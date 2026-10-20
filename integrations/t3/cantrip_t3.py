#!/usr/bin/python3 -I
"""Talk to a T3 agent thread through Cantrip and hear its final reply.

`send` is a Cantrip handoff command: it posts the transcript on stdin to one
T3 thread over authenticated HTTP MCP, idempotently on CANTRIP_TAKE_ID, and
exits as soon as T3 acknowledges. `collect` is a long-running user service that
waits for the reply run of each sent take and speaks its final assistant
message once. `mute`, `stop` and `talk` are keyboard actions.

Never logs transcript or reply text; state files hold IDs and timestamps only.
Secrets come from the environment (T3_MCP_TOKEN, CANTRIP_T3_SPEECH_KEY) so a
launcher such as pass-env owns credential lookup.
"""

import argparse
from datetime import datetime
import fcntl
import json
import os
import queue
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import tomllib
import urllib.error
import urllib.request

PROTOCOL = "2025-06-18"
TERMINAL = {"completed", "failed", "cancelled", "interrupted", "rolled_back"}
WAIT_MS = 45_000
HTTP_SECONDS = 75
SEND_SECONDS = 30
STALE_SECONDS = 15 * 60
SPOKEN_LIMIT = 6000
CHUNK_LIMIT = 1200
FIRST_CHUNK_LIMIT = 300
SPEECH_STALL_SECONDS = 20
PCM_RATE = 24_000

DEFAULTS = {
    "label": "Home",
    "prefix": "[voice] ",
    "handoff": "home",
    "speech": {
        "endpoint": "https://openrouter.ai/api/v1",
        "model": "microsoft/mai-voice-2.1-flash",
        "voice": "en-US-Iris:MAI-Voice-2.1-Flash",
        "local_voice": "en-us",
    },
}


def log(message):
    print("[T3Voice] " + message, file=sys.stderr, flush=True)


def config_path():
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "cantrip" / "t3.toml"


def load_config(path=None):
    path = Path(path) if path else config_path()
    with open(path, "rb") as handle:
        data = tomllib.load(handle)
    config = dict(DEFAULTS)
    config["speech"] = {**DEFAULTS["speech"], **data.pop("speech", {})}
    config.update(data)
    for key in ("mcp_url", "thread_id"):
        if not isinstance(config.get(key), str) or not config[key]:
            raise SystemExit("cantrip-t3: " + key + " is required in " + str(path))
    return config


def state_dir():
    base = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local/state")
    path = Path(base) / "cantrip-t3"
    for child in ("pending", "done"):
        (path / child).mkdir(parents=True, exist_ok=True, mode=0o700)
    return path


def runtime_dir():
    base = os.environ.get("XDG_RUNTIME_DIR") or str(state_dir())
    path = Path(base) / "cantrip-t3"
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    return path


def write_json(path, value):
    temporary = path.with_name("." + path.name + ".tmp")
    temporary.write_text(json.dumps(value, sort_keys=True))
    os.replace(temporary, path)


def take_name(take):
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,128}", take or ""):
        raise ValueError("CANTRIP_TAKE_ID is missing or not a plain identifier")
    return take


class McpError(RuntimeError):
    pass


class Mcp:
    """Minimal streamable-HTTP MCP client for one bearer-authenticated server."""

    def __init__(self, url, token, opener=None):
        self.url = url
        self.token = token
        self.session = None
        self.sequence = 0
        self.open = opener or urllib.request.urlopen
        self.rpc("initialize", {"protocolVersion": PROTOCOL, "capabilities": {},
                                "clientInfo": {"name": "cantrip-t3", "version": "1"}})
        self.rpc("notifications/initialized", {}, notify=True)

    def rpc(self, method, params, notify=False, timeout=HTTP_SECONDS):
        payload = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notify:
            self.sequence += 1
            payload["id"] = self.sequence
        headers = {"Authorization": "Bearer " + self.token,
                   "Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream",
                   "MCP-Protocol-Version": PROTOCOL}
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        request = urllib.request.Request(self.url, data=json.dumps(payload).encode(), headers=headers)
        try:
            with self.open(request, timeout=timeout) as response:
                self.session = response.headers.get("Mcp-Session-Id", self.session)
                raw = response.read().decode()
        except urllib.error.HTTPError as error:
            raise McpError("http-" + str(error.code)) from None
        if notify or not raw.strip():
            return None
        if raw.lstrip().startswith("{"):
            frames = [json.loads(raw)]
        else:
            frames = [json.loads(line[5:]) for line in raw.splitlines() if line.startswith("data:")]
        data = next((frame for frame in frames if frame.get("id") == payload["id"]), None)
        if data is None:
            raise McpError("no-response")
        if "error" in data:
            raise McpError("rpc-" + str(data["error"].get("code")))
        return data.get("result")

    def call(self, name, arguments, timeout=HTTP_SECONDS):
        result = self.rpc("tools/call", {"name": name, "arguments": arguments}, timeout=timeout)
        content = result.get("structuredContent")
        if content is None:
            texts = [block.get("text", "") for block in result.get("content", []) if block.get("type") == "text"]
            try:
                content = json.loads(texts[0]) if len(texts) == 1 else {}
            except ValueError:
                content = {}
        if result.get("isError"):
            code = content.get("code") if isinstance(content, dict) else None
            raise McpError("tool-" + (code or "error"))
        return content


def connect(config):
    token = os.environ.get("T3_MCP_TOKEN", "").strip()
    if not token:
        raise SystemExit("cantrip-t3: T3_MCP_TOKEN is not set")
    return Mcp(config["mcp_url"], token)


# ── send ──────────────────────────────────────────────────────────────────


def request_id(take):
    return "cantrip-voice-" + take


def send(config, text, take, mcp=None, now=time.time):
    take = take_name(take)
    if not text.strip():
        raise ValueError("empty transcript")
    pending = state_dir() / "pending" / (take + ".json")
    done = state_dir() / "done" / (take + ".json")
    if done.exists():
        return json.loads(done.read_text())
    mcp = mcp or connect(config)
    before = mcp.call("t3_thread_read", {"threadId": config["thread_id"], "limit": 1,
                                         "maxCharsPerItem": 1}).get("thread", {})
    result = mcp.call("t3_thread_send", {"threadId": config["thread_id"],
                                         "message": config["prefix"] + text.strip(),
                                         "mode": "queue", "clientRequestId": request_id(take)},
                      timeout=SEND_SECONDS)
    record = {"take": take, "thread_id": config["thread_id"], "request_id": request_id(take),
              "sent_at": now(), "scan_from": max(0, int(before.get("itemCount") or 0) - 2),
              "run_id": find_key(result, "runId"), "message_id": find_key(result, "messageId"),
              "status": "sent"}
    if pending.exists():
        record = {**record, **{k: v for k, v in json.loads(pending.read_text()).items() if v}}
    write_json(pending, record)
    log("sent take=" + take + " chars=" + str(len(text.strip())))
    return record


def find_key(value, key):
    if isinstance(value, dict):
        if isinstance(value.get(key), str):
            return value[key]
        for item in value.values():
            found = find_key(item, key)
            if found:
                return found
    elif isinstance(value, list):
        for item in value:
            found = find_key(item, key)
            if found:
                return found
    return None


# ── collect ───────────────────────────────────────────────────────────────


def read_items(mcp, thread_id, after):
    while True:
        page = mcp.call("t3_thread_read", {"threadId": thread_id, "afterPosition": after,
                                           "limit": 100, "maxCharsPerItem": 50_000})
        items = page.get("items", [])
        yield from items
        if not page.get("hasMore") or not items:
            return
        after = page.get("nextPosition", items[-1]["position"])


def full_text(mcp, thread_id, item):
    text = item.get("text") or ""
    offset = item.get("nextTextOffset")
    while offset:
        page = mcp.call("t3_thread_read", {"threadId": thread_id, "itemId": item["itemId"],
                                           "textOffset": offset, "maxCharsPerItem": 50_000})
        part = page.get("item") or next(iter(page.get("items", [])), {})
        text += part.get("text") or ""
        offset = part.get("nextTextOffset")
    return text


def locate_run(mcp, record):
    """Find the run that carries this take's message."""
    if record.get("run_id"):
        return record["run_id"]
    marker = "thread-send:" + record["request_id"]
    for item in read_items(mcp, record["thread_id"], record.get("scan_from") or 0):
        message = item.get("messageId") or ""
        if item.get("type") == "user_message" and (message == record.get("message_id") or message.endswith(marker)):
            return item.get("runId")
    return None


def final_reply(mcp, record):
    reply = None
    for item in read_items(mcp, record["thread_id"], record.get("scan_from") or 0):
        if item.get("runId") == record["run_id"] and item.get("type") == "assistant_message":
            reply = item
    return full_text(mcp, record["thread_id"], reply) if reply else ""


def advance(mcp, record, now=time.time):
    """Advance one pending take. Returns ("wait", record) or ("reply"|"failed", record, text)."""
    if not record.get("run_id"):
        record["run_id"] = locate_run(mcp, record)
        if not record["run_id"]:
            return ("wait", record, "")
    waited = mcp.call("t3_thread_wait", {"threadId": record["thread_id"], "runId": record["run_id"],
                                         "timeoutMs": WAIT_MS}, timeout=WAIT_MS / 1000 + 30)
    status = find_key(waited, "status")
    if status not in TERMINAL:
        return ("wait", record, "")
    record["run_status"] = status
    record["finished_at"] = finished_at(waited) or now()
    if status != "completed":
        return ("failed", record, "")
    return ("reply", record, final_reply(mcp, record))


def finished_at(waited):
    stamp = find_key(waited, "completedAt")
    try:
        return datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp() if stamp else None
    except ValueError:
        return None


def pending_records():
    records = []
    for path in sorted((state_dir() / "pending").glob("*.json")):
        try:
            records.append((path, json.loads(path.read_text())))
        except ValueError:
            log("unreadable pending record " + path.name)
    return sorted(records, key=lambda pair: pair[1].get("sent_at", 0))


def collect(config, once=False, mcp=None, speaker=None, now=time.time):
    with open(state_dir() / "collector.lock", "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("cantrip-t3: another collector is running") from None
        run_collector(config, once, mcp, speaker, now)


def run_collector(config, once, mcp, speaker, now):
    speaker = speaker or Speaker(config)
    backoff = 5
    while True:
        records = pending_records()
        if not records:
            if once:
                return
            time.sleep(1)
            continue
        try:
            mcp = mcp or connect(config)
            for path, record in records:
                outcome, record, text = advance(mcp, record, now=now)
                if outcome == "wait":
                    write_json(path, record)
                    continue
                finish(path, record, outcome, text, speaker, now)
            backoff = 5
        except (McpError, OSError, ValueError) as error:
            log("collector error class=" + type(error).__name__ + " detail=" + str(error)[:80])
            mcp = None
            if once:
                return
            time.sleep(backoff)
            backoff = min(backoff * 2, 120)
        if once:
            return


def finish(path, record, outcome, text, speaker, now):
    """Move the record to done *before* speaking: a reply is spoken at most once."""
    stale = now() - record.get("finished_at", now()) > STALE_SECONDS
    record["outcome"] = outcome
    record["reply_chars"] = len(text)
    if outcome == "failed":
        spoken = config_label(speaker) + " could not finish that request."
    elif not text.strip():
        record["outcome"] = "empty"
        spoken = ""
    else:
        spoken = text
    if is_muted():
        record["speech"] = "muted"
    elif stale:
        record["speech"] = "stale"
    else:
        record["speech"] = "spoken" if spoken else "none"
    write_json(state_dir() / "done" / path.name, record)
    path.unlink(missing_ok=True)
    log("reply take=" + record["take"] + " outcome=" + record["outcome"] + " chars="
        + str(len(text)) + " speech=" + record["speech"])
    if record["speech"] == "spoken":
        speaker.say(spoken)
    elif record["speech"] in ("muted", "stale"):
        notify(config_label(speaker) + " replied (" + record["speech"] + "); see the thread.")


def config_label(speaker):
    return speaker.config.get("label", "Home")


# ── speech ────────────────────────────────────────────────────────────────


def speakable(markdown):
    """Turn a Markdown reply into plain sentences for speech."""
    text = re.sub(r"```.*?```", " (code omitted) ", markdown, flags=re.S)
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", " ", text)
    text = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"<?(https?|t3-thread)://\S+>?", "a link", text)
    text = re.sub(r"`([^`]*)`", r"\1", text)
    text = re.sub(r"^\s*\|?\s*:?-{3,}.*$", " ", text, flags=re.M)
    text = text.replace("|", ", ")
    text = re.sub(r"^\s{0,3}(#{1,6}|>|[-*+]|\d+[.)])\s+", "", text, flags=re.M)
    text = re.sub(r"(\*\*|__|\*|_|~~)(\S.*?\S|\S)\1", r"\2", text)
    lines = [line.strip() for line in text.splitlines()]
    text = " ".join(line if re.search(r"[.!?:;,]$", line) else line + "." for line in lines if line)
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) > SPOKEN_LIMIT:
        cut = text.rfind(". ", 0, SPOKEN_LIMIT)
        text = text[: cut + 1 if cut > 0 else SPOKEN_LIMIT] + " The rest is in the thread."
    return text


def chunks(text, limit=CHUNK_LIMIT, first=FIRST_CHUNK_LIMIT):
    """Split at sentence ends; the first chunk is short so speech starts quickly."""
    pieces = []
    for sentence in re.split(r"(?<=[.!?])\s+", text):
        while len(sentence) > limit:
            pieces.append(sentence[:limit])
            sentence = sentence[limit:]
        if sentence:
            pieces.append(sentence)
    current, cap = "", min(first, limit)
    for piece in pieces:
        if current and len(current) + 1 + len(piece) > cap:
            yield current
            current, cap = piece, limit
        else:
            current = (current + " " + piece).strip()
    if current:
        yield current


def cantrip_recording():
    try:
        status = subprocess.run(["cantrip", "status", "--json"], capture_output=True, text=True, timeout=5)
        return json.loads(status.stdout).get("state") == "recording"
    except (OSError, ValueError, subprocess.SubprocessError):
        return False


def is_muted():
    return (state_dir() / "muted").exists()


def notify(message):
    if shutil.which("notify-send"):
        subprocess.run(["notify-send", "-a", "Cantrip", "-t", "2500", message],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


class Speaker:
    """Speak text once: natural remote voice when configured, else espeak-ng."""

    def __init__(self, config, opener=None, recording=cantrip_recording):
        self.config = config
        self.open = opener or urllib.request.urlopen
        self.recording = recording

    def say(self, text):
        text = speakable(text)
        if not text:
            return
        held = time.time()
        while self.recording() and time.time() - held < 600:
            time.sleep(0.5)
        started = time.time()
        self.checked = (0.0, False)
        parts = list(chunks(text))
        played = 0
        key = os.environ.get("CANTRIP_T3_SPEECH_KEY", "").strip()
        if key:
            played = self.remote(parts, key, started)
        if played < len(parts) and not self.interrupted(started):
            log("speech fallback=local chunks=" + str(len(parts) - played))
            self.local(" ".join(parts[played:]), started)

    def stopped(self, started):
        marker = runtime_dir() / "stop"
        return marker.exists() and marker.stat().st_mtime >= started - 0.01

    def interrupted(self, started):
        """Stopped explicitly, or a Cantrip take began recording (checked once a second)."""
        if self.stopped(started):
            return True
        checked, recording = getattr(self, "checked", (0.0, False))
        if time.time() - checked >= 1:
            recording = self.recording()
            self.checked = (time.time(), recording)
        return recording

    def player(self, argv):
        process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, start_new_session=True)
        (runtime_dir() / "player.pid").write_text(str(process.pid))
        return process

    def release(self, process, started):
        if self.interrupted(started) and process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
        try:
            process.stdin.close()
        except BrokenPipeError:
            pass
        process.wait()
        (runtime_dir() / "player.pid").unlink(missing_ok=True)

    def fetch(self, parts, key, audio, cancelled):
        """Producer thread: request each chunk in turn and queue its PCM blocks."""
        speech = self.config["speech"]
        for index, part in enumerate(parts):
            body = json.dumps({"model": speech["model"], "voice": speech["voice"],
                               "input": part, "response_format": "pcm"}).encode()
            request = urllib.request.Request(speech["endpoint"].rstrip("/") + "/audio/speech", data=body,
                                             headers={"Authorization": "Bearer " + key,
                                                      "Content-Type": "application/json"})
            try:
                with self.open(request, timeout=SPEECH_STALL_SECONDS) as response:
                    while block := response.read(8192):
                        if cancelled.is_set():
                            return
                        audio.put((index, block))
            except (OSError, ValueError) as error:
                audio.put((index, error))
                return
            audio.put((index, None))

    def remote(self, parts, key, started):
        """Play natural speech; return how many chunks were fully handed to the player."""
        audio = queue.Queue()
        cancelled = threading.Event()
        threading.Thread(target=self.fetch, args=(parts, key, audio, cancelled), daemon=True).start()
        process = self.player(["pw-cat", "--playback", "--raw", "--format", "s16",
                               "--rate", str(PCM_RATE), "--channels", "1", "-"])
        played = 0
        last = time.time()
        try:
            while played < len(parts) and not self.interrupted(started):
                try:
                    index, block = audio.get(timeout=0.1)
                except queue.Empty:
                    if time.time() - last > SPEECH_STALL_SECONDS:
                        log("speech stalled chunk=" + str(played))
                        break
                    continue
                last = time.time()
                if isinstance(block, Exception):
                    log("speech error class=" + type(block).__name__ + " chunk=" + str(index))
                    break
                if block is None:
                    played = index + 1
                else:
                    process.stdin.write(block)
        except BrokenPipeError:
            pass
        finally:
            cancelled.set()
            self.release(process, started)
        return played

    def local(self, text, started):
        process = self.player(["espeak-ng", "-v", self.config["speech"]["local_voice"], "-s", "170"])
        try:
            process.stdin.write(text.encode())
            process.stdin.close()
        except BrokenPipeError:
            pass
        while process.poll() is None and not self.interrupted(started):
            time.sleep(0.1)
        self.release(process, started)


# ── keyboard actions ──────────────────────────────────────────────────────


def repeat(config, mcp=None, speaker=None):
    """Speak the most recent reply again, fetched from T3; no reply text is stored locally."""
    done = [json.loads(p.read_text()) for p in (state_dir() / "done").glob("*.json")]
    done = [record for record in done if record.get("outcome") == "reply"]
    if not done:
        return False
    record = max(done, key=lambda item: item.get("finished_at", 0))
    text = final_reply(mcp or connect(config), record)
    log("repeat take=" + record["take"] + " chars=" + str(len(text)))
    if text and not is_muted():
        (speaker or Speaker(config)).say(text)
    return bool(text)


def stop_speaking():
    (runtime_dir() / "stop").write_text(str(time.time()))
    pid_file = runtime_dir() / "player.pid"
    try:
        os.killpg(int(pid_file.read_text()), signal.SIGTERM)
        return True
    except (OSError, ValueError):
        return False


def set_mute(action):
    marker = state_dir() / "muted"
    muted = marker.exists()
    target = {"on": True, "off": False, "toggle": not muted}[action]
    if target:
        marker.write_text("muted\n")
        stop_speaking()
    else:
        marker.unlink(missing_ok=True)
    return target


def main(argv=None):
    parser = argparse.ArgumentParser(prog="cantrip-t3", description=__doc__.split("\n\n")[0])
    parser.add_argument("--config", type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("send", help="Cantrip handoff: send stdin to the configured thread")
    collector = commands.add_parser("collect", help="wait for replies and speak them")
    collector.add_argument("--once", action="store_true")
    mute = commands.add_parser("mute", help="mute, unmute or toggle reply speech")
    mute.add_argument("action", nargs="?", choices=("on", "off", "toggle"), default="toggle")
    commands.add_parser("stop", help="stop speaking the current reply")
    commands.add_parser("repeat", help="speak the most recent reply again")
    commands.add_parser("talk", help="stop speech, then toggle a Cantrip take to this handoff")
    commands.add_parser("status", help="print mute state and pending take count")
    args = parser.parse_args(argv)

    if args.command == "stop":
        stop_speaking()
        return 0
    if args.command == "mute":
        muted = set_mute(args.action)
        notify("Spoken replies " + ("muted" if muted else "on"))
        print("muted" if muted else "unmuted")
        return 0
    if args.command == "status":
        pending = len(list((state_dir() / "pending").glob("*.json")))
        print(json.dumps({"muted": is_muted(), "pending": pending}))
        return 0
    config = load_config(args.config)
    if args.command == "talk":
        stop_speaking()
        os.execvp("cantrip", ["cantrip", "toggle", "--handoff", config["handoff"]])
    if args.command == "repeat":
        try:
            return 0 if repeat(config) else 1
        except (McpError, OSError, ValueError) as error:
            log("repeat failed class=" + type(error).__name__)
            return 1
    if args.command == "send":
        try:
            send(config, sys.stdin.read(), os.environ.get("CANTRIP_TAKE_ID", ""))
        except (McpError, OSError, ValueError) as error:
            log("send failed class=" + type(error).__name__ + " detail=" + str(error)[:80])
            return 1
        return 0
    collect(config, once=args.once)
    return 0


if __name__ == "__main__":
    sys.exit(main())
