#!/usr/bin/env python3
"""US-005: walk Settings and the real clipboard in a disposable headless session.

Requires sway, grim, wtype, wl-copy, wl-paste, tesseract, and wf-recorder on PATH.
Pass --fixture only after reviewing its opening words for recording disclosure.
No operator configuration, daemon, compositor, clipboard, or archive is changed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fixture", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True, mode=0o700)
    if any(out.iterdir()):
        parser.error("--out must be an empty evidence directory")
    for tool in ("sway", "swaymsg", "grim", "wtype", "wl-copy", "wl-paste", "tesseract", "wf-recorder"):
        if not shutil.which(tool):
            parser.error(f"Missing prerequisite: {tool}")
    fixture = args.fixture.read_bytes() if args.fixture else json.dumps({
        "schema_version": 2, "session_id": "old-settings-qa", "source": "dictation",
        "completed_at_unix_ms": 1759276800000, "audio": {"duration_ms": 49128},
        "stt": {"partial": False}, "raw_transcript": "Raw words before cleanup.",
        "postprocessed_transcript": "An old transcript with cleaned words.\n\n" + "Full text, not a preview. " * 12,
    }).encode()
    record = json.loads(fixture)
    take_id = record["session_id"]
    if not re.fullmatch(r"[A-Za-z0-9_-]+", take_id):
        parser.error("Fixture must have a valid archived session_id")
    expected = record.get("postprocessed_transcript")
    if not isinstance(expected, str):
        expected = record["raw_transcript"]
    if not expected.strip():
        parser.error("Fixture must have usable text")
    processes = []
    logs = []
    with tempfile.TemporaryDirectory(prefix="cantrip-settings-qa-", dir=os.environ.get("TMPDIR")) as scratch:
        root = Path(scratch)
        env = dict(os.environ)
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "SWAYSOCK", "HYPRLAND_INSTANCE_SIGNATURE", "DBUS_SESSION_BUS_ADDRESS", "XDG_SESSION_ID", "XDG_SEAT_PATH", "XDG_SESSION_PATH"):
            env.pop(key, None)
        for key in ("HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"):
            path = root / key
            path.mkdir(mode=0o700)
            env[key] = str(path)
        env.update(BROWSER="none", CI="1", WLR_BACKENDS="headless", WLR_RENDERER="pixman", LIBGL_ALWAYS_SOFTWARE="1", XDG_SESSION_TYPE="wayland", XDG_CURRENT_DESKTOP="sway")
        history = Path(env["XDG_STATE_HOME"]) / "cantrip/transcripts"
        history.mkdir(parents=True, mode=0o700)
        archive = history / f"{take_id}.json"
        archive.write_bytes(fixture)
        archive.chmod(0o600)
        config = root / "sway.conf"
        config.write_text("output HEADLESS-1 mode 1100x900\nseat seat0 fallback true\nxwayland disable\ndefault_border pixel 0\nfor_window [app_id=\".*\"] floating enable\n")

        def launch(command, name):
            log = open(out / f"{name}.log", "wb")
            logs.append(log)
            process = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            processes.append(process)
            return process

        def run(*command, check=True):
            return subprocess.run(command, env=env, capture_output=True, check=check, timeout=15)

        def until(predicate, description):
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if predicate():
                    return
                time.sleep(0.2)
            raise RuntimeError(f"Timed out: {description}")

        def capture(name):
            path = out / f"{name}.png"
            run("grim", str(path))
            return run("tesseract", str(path), "stdout").stdout.decode()

        def visible(text, phrase):
            return phrase in " ".join(text.split())

        recorder = None
        try:
            compositor = launch(["sway", "-c", str(config)], "compositor")
            runtime = Path(env["XDG_RUNTIME_DIR"])
            until(lambda: any(runtime.glob("wayland-*.lock")), "isolated Wayland socket")
            env["WAYLAND_DISPLAY"] = next(runtime.glob("wayland-*.lock")).name.removesuffix(".lock")
            env["SWAYSOCK"] = str(runtime / f"sway-ipc.{os.getuid()}.{compositor.pid}.sock")
            until(lambda: Path(env["SWAYSOCK"]).exists(), "isolated compositor IPC")
            recorder = launch(["wf-recorder", "--no-dmabuf", "-D", "-r", "20", "-c", "libx264", "-p", "preset=ultrafast", "-p", "threads=2", "-x", "yuv420p", "-f", str(out / "settings-history.mp4")], "recorder")
            # Keep a virtual keyboard present so the headless seat has input
            # capabilities; no real keyboard or pointer is connected.
            launch(["wtype", "-s", "3600000"], "virtual-keyboard")
            launch([str(binary), "settings"], "settings")
            time.sleep(2)
            rows = capture("old-transcript")
            if not visible(rows, "Copy"):
                raise RuntimeError("Saved transcript was not discoverable in Settings")
            # The overflowing page's drag-scroll handle is the first Tab stop.
            run("wtype", "-k", "Tab", "-s", "250", "-k", "Tab", "-s", "250", "-k", "Tab", "-s", "250")
            capture("copy-focused")
            run("wtype", "-k", "space")
            def clipboard_matches():
                result = run("wl-paste", "--no-newline", check=False)
                return result.returncode == 0 and result.stdout == expected.encode()
            until(clipboard_matches, "full saved text on actual clipboard")
            time.sleep(2)
            if not visible(capture("copied"), "Copied transcript"):
                raise RuntimeError("Settings did not confirm clipboard success")
            if archive.read_bytes() != fixture:
                raise RuntimeError("Copy changed the archive")
            # A stale row must fail honestly instead of copying its cached preview.
            archive.unlink()
            capture("stale-row")
            run("wtype", "-k", "space")
            time.sleep(2)
            if not visible(capture("copy-unavailable"), "not copied"):
                raise RuntimeError("Settings did not report unavailable text")
            if not clipboard_matches():
                raise RuntimeError("Failed copy replaced the previous clipboard")
            capture("before-refresh")
            run("wtype", "-M", "shift", "-k", "Tab", "-m", "shift", "-s", "250")
            capture("refresh-focused")
            run("wtype", "-k", "space")
            time.sleep(1)
            if not visible(capture("empty-history"), "No saved transcripts yet"):
                raise RuntimeError("Refresh did not show empty history")
            report = {
                "story": "US-005", "fixture_class": "provided archived transcript" if args.fixture else "synthetic old-schema cleaned transcript",
                "take_id": take_id, "schema_version": record.get("schema_version"),
                "clipboard_bytes": len(expected.encode()), "clipboard_sha256": hashlib.sha256(expected.encode()).hexdigest(),
                "checks": ["native Settings old archive discovery", "exact full text through wl-copy/wl-paste", "copy does not mutate archive", "stale archive copy preserves previous clipboard", "refresh to empty history"],
                "isolation": "headless Sway pixman, independent HOME/XDG/runtime/clipboard, no daemon",
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            }
            (out / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report, indent=2))
        finally:
            if recorder and recorder.poll() is None:
                recorder.send_signal(signal.SIGINT)
                recorder.wait(timeout=15)
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            for log in logs:
                log.close()


if __name__ == "__main__":
    main()
