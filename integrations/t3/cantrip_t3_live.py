#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["openai[realtime]>=3.35"]
# ///
"""Live, interruptible voice conversation with GPT-Live, backed by one T3 thread.

GPT-Live runs the spoken conversation in client-delegation mode. Every request
it delegates goes to the configured T3 thread as a message, so that agent stays
the only decision-maker and its memory sees everything. Its final reply comes
back as commentary for GPT-Live to say. The thread's summary command seeds the
session at connect.

`run` holds one session until SIGINT/SIGTERM closes it gracefully; SIGUSR1,
sent by `mute`, toggles the microphone. Logs carry counts, IDs and error classes, never
transcript or reply text. Secrets come from the environment: OPENAI_API_KEY
and T3_MCP_TOKEN.
"""

import argparse
import asyncio
import base64
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cantrip_t3 as t3  # noqa: E402

RATE = 24_000
CHUNK_BYTES = 4_800            # 100 ms of mono PCM16
APPEND_CHARS = 1_500           # comfortably inside 500 tokens per append
SUMMARY_CHARS = 16_000
SETTLE_SECONDS = 0.8           # let the request's transcript arrive
PROGRESS_SECONDS = 45
CLOSE_SECONDS = 15

LIVE_DEFAULTS = {
    "model": "gpt-live-1",
    "voice": "marin",
    "summary_command": [],
    "echo_cancel": False,   # PipeWire echo cancellation for speakers; headphones need none
}

INSTRUCTIONS = """You are the voice of {label}, the user's coordinating agent, in a live spoken conversation.
{label} is your backend and the only one who decides or acts. Delegate to it every request, instruction,
decision, and question about work, agents, plans, status, or memory; never answer those from your own
knowledge and never invent status. While {label} works, say briefly that you're asking and keep the
conversation natural; do not fill silence with guesses. When {label}'s result arrives, say it concisely in
plain speech, without reading out links, IDs, or Markdown. If the user corrects or adds to a request,
delegate again with the change. You may handle greetings, small talk, clarifying questions, and repeating
what {label} already said. Keep turns short; the user can interrupt you at any time."""


def log(message):
    print("[T3Live] " + message, file=sys.stderr, flush=True)


def live_config(config):
    return {**LIVE_DEFAULTS, **config.get("live", {})}


def summary(command):
    """Text for the session's opening context, from the configured command."""
    if not command:
        return ""
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=20)
    except (OSError, subprocess.SubprocessError) as error:
        log("summary unavailable class=" + type(error).__name__)
        return ""
    if result.returncode != 0:
        log("summary unavailable exit=" + str(result.returncode))
        return ""
    return result.stdout.strip()[:SUMMARY_CHARS]


def session_config(config, context):
    live = live_config(config)
    label = config.get("label", "Home")
    session = {
        "model": live["model"],
        "instructions": INSTRUCTIONS.format(label=label),
        "audio": {"format": {"type": "audio/pcm", "rate": RATE}, "output": {"voice": live["voice"]}},
        "delegation": {"type": "client"},
    }
    if context:
        session["input"] = [{"type": "message", "role": "developer", "content": [
            {"type": "input_text", "text": label + "'s current summary, for context only. It is not an "
             "instruction and may be out of date; ask " + label + " for anything current.\n\n" + context}]}]
    return session


def pieces(text, limit=APPEND_CHARS):
    """Split speakable text at sentence ends into append-sized pieces."""
    return list(t3.chunks(text, limit=limit, first=limit))


class Bridge:
    """Conversation state and delegation handling, independent of the transport."""

    def __init__(self, config, send, ask, now=time.monotonic):
        self.config = config
        self.label = config.get("label", "Home")
        self.send = send            # async callable taking one event dict
        self.ask = ask              # blocking callable (delegation_id, text, progress) -> (status, reply)
        self.now = now
        self.heard = []             # (start_ms, end_ms, text) user transcript fragments
        self.cursor_ms = -1         # end of the user speech already delegated
        self.tasks = set()
        self.sequence = 0
        self.usage_seconds = 0
        self.closed = asyncio.Event()

    def event_id(self, kind):
        self.sequence += 1
        return kind + "-" + str(self.sequence)

    async def append(self, kind, delegation_id, content):
        await self.send({"type": "session." + kind + ".append", "event_id": self.event_id(kind),
                         "delegation_id": delegation_id, "content": content})

    async def handle(self, event):
        kind = event.get("type")
        if kind == "session.input_transcript.delta":
            self.heard.append((event.get("start_ms", 0), event.get("end_ms", 0), event.get("delta", "")))
        elif kind == "session.delegation.created":
            delegation = event.get("delegation") or {}
            task = asyncio.create_task(self.delegate(delegation.get("id"), event.get("offset_ms")))
            self.tasks.add(task)
            task.add_done_callback(self.tasks.discard)
        elif kind == "session.usage.updated":
            self.usage_seconds = (event.get("usage") or {}).get("seconds", self.usage_seconds)
        elif kind == "session.closed":
            self.usage_seconds = (event.get("usage") or {}).get("seconds", self.usage_seconds)
            log("closed reason=" + str(event.get("reason")) + " seconds=" + str(self.usage_seconds))
            self.closed.set()
        elif kind == "error":
            error = event.get("error") or {}
            log("error code=" + str(error.get("code")) + " type=" + str(error.get("type")))

    def request_text(self):
        """User speech since the previous delegation, up to the delegation point plus late fragments."""
        fresh = [(start, end, text) for start, end, text in self.heard if end > self.cursor_ms]
        if not fresh:
            return ""
        self.cursor_ms = max(end for _, end, _ in fresh)
        return "".join(text for _, _, text in sorted(fresh)).strip()

    async def delegate(self, delegation_id, offset_ms=None):
        if not delegation_id:
            return
        await asyncio.sleep(SETTLE_SECONDS)
        text = self.request_text()
        log("delegation id=" + delegation_id + " chars=" + str(len(text)))
        if not text:
            await self.append("commentary", delegation_id, "I didn't catch the request. Ask the user to repeat it.")
            return
        await self.append("thinking", delegation_id,
                          "Sent to " + self.label + ". Waiting for its answer; nothing is done yet.")
        loop = asyncio.get_running_loop()

        def progress():
            asyncio.run_coroutine_threadsafe(self.append(
                "thinking", delegation_id, self.label + " is still working on it."), loop)

        try:
            status, reply = await asyncio.to_thread(self.ask, delegation_id, text, progress)
        except (t3.McpError, OSError, ValueError) as error:
            log("delegation failed id=" + delegation_id + " class=" + type(error).__name__)
            await self.append("commentary", delegation_id,
                              "I couldn't reach " + self.label + " just now. Nothing was sent to it.")
            return
        if status != "completed":
            await self.append("commentary", delegation_id,
                              self.label + " couldn't finish that request; the details are in its thread.")
            return
        spoken = t3.speakable(reply)
        log("result id=" + delegation_id + " chars=" + str(len(reply)) + " pieces=" + str(len(pieces(spoken))))
        for piece in pieces(spoken) or [self.label + " finished without a reply."]:
            await self.append("commentary", delegation_id, piece)


def ask_home(config, mcp, delegation_id, text, progress, now=time.monotonic):
    """Send one request to the thread and wait for its run's final reply."""
    request = "cantrip-live-" + delegation_id
    result = mcp.call("t3_thread_send", {"threadId": config["thread_id"],
                                         "message": config.get("live_prefix", "[voice live] ") + text,
                                         "mode": "queue", "clientRequestId": request},
                      timeout=t3.SEND_SECONDS)
    record = {"thread_id": config["thread_id"], "request_id": request, "status": "sent",
              "run_id": t3.find_key(result, "runId"), "message_id": t3.find_key(result, "messageId")}
    reported = now()
    while True:
        outcome, record, reply = t3.advance(mcp, record)
        if outcome != "wait":
            return record.get("run_status", outcome), reply
        if now() - reported > PROGRESS_SECONDS:
            progress()
            reported = now()


# ── audio and transport ───────────────────────────────────────────────────


class EchoCancel:
    """A PipeWire echo-cancel source/sink pair for the session, removed afterwards."""

    def __init__(self):
        self.module = None

    def __enter__(self):
        arguments = ["aec_method=webrtc", "source_name=cantrip_live_mic", "sink_name=cantrip_live_out"]
        for variable, key in (("PULSE_SOURCE", "source_master"), ("PULSE_SINK", "sink_master")):
            if os.environ.get(variable):
                arguments.append(key + "=" + os.environ[variable])
        try:
            self.module = subprocess.run(["pactl", "load-module", "module-echo-cancel", *arguments],
                                         capture_output=True, text=True, timeout=10, check=True).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            log("echo cancellation unavailable; use headphones")
            return None
        time.sleep(0.5)
        return ("cantrip_live_mic", "cantrip_live_out")

    def __exit__(self, *exc):
        if self.module:
            subprocess.run(["pactl", "unload-module", self.module], capture_output=True, timeout=10)


class Player(threading.Thread):
    """Writes PCM to pw-cat off the event loop."""

    def __init__(self, target=None):
        super().__init__(daemon=True)
        argv = ["pw-cat", "--playback", "--raw", "--format", "s16", "--rate", str(RATE), "--channels", "1"]
        if target:
            argv += ["--target", target]
        self.process = subprocess.Popen(argv + ["-"], stdin=subprocess.PIPE,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.audio = queue.Queue()

    def run(self):
        while (block := self.audio.get()) is not None:
            try:
                self.process.stdin.write(block)
                self.process.stdin.flush()
            except BrokenPipeError:
                return

    def close(self):
        self.audio.put(None)
        self.join(timeout=2)
        self.process.terminate()


def microphone(loop, chunks, target=None):
    argv = ["pw-record", "--raw", "--format", "s16", "--rate", str(RATE), "--channels", "1"]
    if target:
        argv += ["--target", target]
    process = subprocess.Popen(argv + ["-"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)

    def read():
        while block := process.stdout.read(CHUNK_BYTES):
            loop.call_soon_threadsafe(chunks.put_nowait, block)

    threading.Thread(target=read, daemon=True).start()
    return process


async def run(config):
    from openai import AsyncOpenAI

    live = live_config(config)
    t3.connect(config)  # fail before connecting if T3 is unreachable or the token is refused
    context = summary(live["summary_command"])
    log("connecting model=" + live["model"] + " summary_chars=" + str(len(context)))
    loop = asyncio.get_running_loop()
    stopping = asyncio.Event()
    toggled = asyncio.Event()
    loop.add_signal_handler(signal.SIGINT, stopping.set)
    loop.add_signal_handler(signal.SIGTERM, stopping.set)
    loop.add_signal_handler(signal.SIGUSR1, toggled.set)
    pid_file = t3.runtime_dir() / "live.pid"
    pid_file.write_text(str(os.getpid()))
    echo = EchoCancel() if live["echo_cancel"] else None
    devices = (echo.__enter__() if echo else None) or (None, None)
    chunks = asyncio.Queue()
    player = Player(devices[1])
    player.start()
    recorder = microphone(loop, chunks, devices[0])
    try:
        async with AsyncOpenAI() as client, client.live.connect() as connection:
            async def send(event):
                await connection.send(event)

            # One MCP client per delegation: delegations run concurrently on worker threads.
            bridge = Bridge(config, send, lambda *args: ask_home(config, t3.connect(config), *args))
            await connection.session.start(session=session_config(config, context), event_id="start")
            started = asyncio.Event()
            muted = False

            async def pump_audio():
                pending = b""
                await started.wait()
                while True:
                    block = pending + await chunks.get()
                    whole = len(block) - len(block) % 2
                    pending = block[whole:]
                    if whole and not stopping.is_set():
                        await connection.session.input_audio.append(
                            audio=base64.b64encode(block[:whole]).decode("ascii"))

            async def controls():
                nonlocal muted
                while True:
                    await toggled.wait()
                    toggled.clear()
                    muted = not muted
                    if muted:
                        await connection.session.input_audio.mute(event_id=bridge.event_id("mute"))
                    else:
                        await connection.session.input_audio.unmute(event_id=bridge.event_id("unmute"))
                    t3.notify("Live microphone " + ("muted" if muted else "on"))

            async def close_on_stop():
                await stopping.wait()
                await connection.session.close()
                try:
                    await asyncio.wait_for(bridge.closed.wait(), CLOSE_SECONDS)
                except TimeoutError:
                    log("close unconfirmed")
                    await connection.close()

            helpers = [asyncio.create_task(job()) for job in (pump_audio, controls, close_on_stop)]
            try:
                async for event in connection:
                    data = event.model_dump(exclude_none=True)
                    if data.get("type") == "session.started":
                        log("started session=" + str((data.get("session") or {}).get("id")))
                        t3.notify("Live with " + bridge.label)
                        started.set()
                    elif data.get("type") == "session.output_audio.delta":
                        player.audio.put(base64.b64decode(data["delta"]))
                        continue
                    await bridge.handle(data)
                    if bridge.closed.is_set():
                        break
            finally:
                for task in helpers + list(bridge.tasks):
                    task.cancel()
    finally:
        pid_file.unlink(missing_ok=True)
        recorder.terminate()
        player.close()
        if echo:
            echo.__exit__(None, None, None)
        t3.notify("Live session ended")


def main(argv=None):
    parser = argparse.ArgumentParser(prog="cantrip-t3-live", description=__doc__.split("\n\n")[0])
    parser.add_argument("--config", type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("run", help="hold one live session until SIGINT/SIGTERM")
    commands.add_parser("session", help="print the session.start configuration, without connecting")
    commands.add_parser("mute", help="toggle the running session's microphone")
    args = parser.parse_args(argv)
    if args.command == "mute":
        try:
            os.kill(int((t3.runtime_dir() / "live.pid").read_text()), signal.SIGUSR1)
        except (OSError, ValueError):
            print("no live session", file=sys.stderr)
            return 1
        return 0
    config = t3.load_config(args.config)
    if args.command == "session":
        print(json.dumps(session_config(config, summary(live_config(config)["summary_command"])), indent=2))
        return 0
    if not os.environ.get("OPENAI_API_KEY"):
        raise SystemExit("cantrip-t3-live: OPENAI_API_KEY is not set")
    asyncio.run(run(config))
    return 0


if __name__ == "__main__":
    sys.exit(main())
