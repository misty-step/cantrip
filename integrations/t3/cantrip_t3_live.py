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

`run` holds one session until SIGINT/SIGTERM, `idle_minutes` without speech,
or `max_minutes` closes it gracefully; SIGUSR1, sent by `mute`, toggles the
microphone and stops sending its audio. Logs carry counts, IDs and error classes, never
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
SETTLE_SECONDS = 0.6           # quiet in the request's transcript before it is complete
SETTLE_MAX_SECONDS = 2.5
SETTLE_UNCOVERED_SECONDS = 1.5 # stop waiting for coverage sooner when the user paused before delegating
COVER_MS = 600                 # transcript ending this close to the delegation point covers it
CONTEXT_CHARS = 300
SHORT_WORDS = 6                # a reply this short ("yes", "the second one") travels with its question
PROGRESS_SECONDS = 45
CLOSE_SECONDS = 15
PREBUFFER_SECONDS = 0.4        # jitter buffer before each spoken turn (measured: 107 gaps → 11)

LIVE_DEFAULTS = {
    "model": "gpt-live-1",
    "voice": "marin",
    "summary_command": [],
    "echo_cancel": False,
    "input_device": "",         # PipeWire source to listen on; default input when empty
    "output_device": "",        # PipeWire sink to speak on; default output when empty
    # Close after this long with no speech either way and nothing pending, and after this long in total.
    "idle_minutes": 5,
    "max_minutes": 60,   # PipeWire echo cancellation for speakers; headphones need none
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


def in_daemon_thread(function, *args):
    """Run blocking work on a daemon thread, so closing the session never waits for Home's run."""
    loop = asyncio.get_running_loop()
    future = loop.create_future()

    def settle(outcome, failed):
        if not future.done():
            (future.set_exception if failed else future.set_result)(outcome)

    def work():
        try:
            outcome, failed = function(*args), False
        except Exception as error:  # handed to the awaiting coroutine, never lost
            outcome, failed = error, True
        except BaseException as error:  # e.g. SystemExit: report it as a failure, never end the session here
            outcome, failed = RuntimeError(type(error).__name__), True
        try:
            loop.call_soon_threadsafe(settle, outcome, failed)
        except RuntimeError:
            pass   # the loop already closed: the session ended while this ran

    threading.Thread(target=work, daemon=True).start()
    return future


def definite_refusal(error):
    """True when T3 certainly did not accept the message; anything else may have arrived."""
    return isinstance(error, t3.McpError) and (error.refused() or str(error).startswith(("http-4", "rpc-")))


class Bridge:
    """Conversation state and delegation handling, independent of the transport.

    `submit(delegation_id, message)` sends one request and returns its record;
    `collect(record, progress)` waits for that run and returns (status, reply).
    Both block and run off the event loop.
    """

    def __init__(self, config, send, submit, collect, now=time.monotonic):
        self.config = config
        self.label = config.get("label", "Home")
        self.send = send            # async callable taking one event dict
        self.submit = submit
        self.collect = collect
        self.now = now
        self.heard = []             # (start_ms, end_ms, text, arrived) user transcript fragments
        self.said = []              # (start_ms, end_ms, text) GPT-Live's own speech
        self.boundaries = [-1]      # offsets of earlier delegations: each request starts at the last one
        self.claimed = set()        # indexes of fragments already sent
        self.tasks = set()
        self.sequence = 0
        self.usage_seconds = 0
        self.active_at = now()
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
            self.active_at = self.now()
            self.heard.append((event.get("start_ms", 0), event.get("end_ms", 0), event.get("delta", ""), self.now()))
        elif kind == "session.output_transcript.delta":
            self.said.append((event.get("start_ms", 0), event.get("end_ms", 0), event.get("delta", "")))
        elif kind == "session.delegation.created":
            self.active_at = self.now()
            delegation = event.get("delegation") or {}
            offset = event.get("offset_ms")
            task = asyncio.create_task(self.delegate(delegation.get("id"), offset))
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

    def window(self, offset_ms):
        """The session-time span this delegation's request was spoken in."""
        if offset_ms is None:
            offset_ms = max((end for _, end, _, _ in self.heard), default=0)
        earlier = max((b for b in self.boundaries if b < offset_ms), default=-1)
        self.boundaries.append(offset_ms)
        return earlier, offset_ms   # transcript times share the session timeline with offset_ms

    def covered(self, end):
        """Speech reaching close to the delegation point has been transcribed."""
        return any(e >= end - COVER_MS for s, e, _, _ in self.heard if s < end)

    async def settle(self, start, end):
        """Wait for the request's transcript: until it reaches the delegation point and has gone
        quiet, or SETTLE_MAX_SECONDS."""
        began = self.now()
        while self.now() - began < SETTLE_MAX_SECONDS:
            arrived = [at for s, _, _, at in self.heard if s < end]
            quiet = self.now() - max(arrived, default=began) >= SETTLE_SECONDS
            if quiet and (self.covered(end) or self.now() - began >= SETTLE_UNCOVERED_SECONDS):
                return
            await asyncio.sleep(0.1)

    def claim(self, end):
        """Every not-yet-sent fragment spoken before the delegation point, each sent exactly once.
        A straggler that missed its own request joins the next one instead of being lost."""
        mine = sorted(i for i, (s, _, _, _) in enumerate(self.heard) if s < end and i not in self.claimed)
        self.claimed.update(mine)
        return "".join(self.heard[i][2] for i in mine).strip()

    def context(self, start, end):
        """What GPT-Live said just before, so a bare "yes" reaches the thread with its question."""
        spoken = "".join(text for s, _, text in sorted(self.said) if start - 30_000 <= s < end).strip()
        return spoken[-CONTEXT_CHARS:]

    def message(self, text, context):
        prefix = self.config.get("live_prefix", "[voice live] ")
        if context and len(text.split()) <= SHORT_WORDS:
            return prefix + text + "\n\n(Replying to the voice assistant, which had just said: " + context + ")"
        return prefix + text

    async def delegate(self, delegation_id, offset_ms=None):
        if not delegation_id:
            return
        try:
            await self.run_delegation(delegation_id, offset_ms)
        except asyncio.CancelledError:
            raise
        except Exception as error:  # never leave GPT-Live waiting forever
            log("delegation broke id=" + delegation_id + " class=" + type(error).__name__)
            await self.append("commentary", delegation_id, "Something went wrong on my side. "
                              "Ask the user to check " + self.label + "'s thread before repeating anything.")

    async def run_delegation(self, delegation_id, offset_ms):
        start, end = self.window(offset_ms)
        await self.settle(start, end)
        text = self.claim(end)
        log("delegation id=" + delegation_id + " chars=" + str(len(text)))
        if not text:
            already = offset_ms is not None and self.boundaries.count(offset_ms) > 1
            await self.append("commentary", delegation_id, "That request is already with " + self.label + "."
                              if already else "I didn't catch the request. Ask the user to repeat it.")
            return
        try:
            record = await in_daemon_thread(self.submit, delegation_id, self.message(text, self.context(start, end)))
        except Exception as error:
            log("send failed id=" + delegation_id + " code=" + t3.describe(error))
            if definite_refusal(error):
                await self.append("commentary", delegation_id,
                                  "I couldn't reach " + self.label + " just now. Nothing was sent to it.")
            else:
                await self.append("commentary", delegation_id, "I'm not sure " + self.label + " received that. "
                                  "Ask the user to check its thread before repeating it.")
            return
        await self.append("thinking", delegation_id,
                          "Sent to " + self.label + ". Waiting for its answer; nothing is done yet.")
        loop = asyncio.get_running_loop()

        def progress(note):
            asyncio.run_coroutine_threadsafe(self.append("thinking", delegation_id, note), loop)

        try:
            status, reply = await in_daemon_thread(self.collect, record, progress)
        except Exception as error:
            log("reply lost id=" + delegation_id + " code=" + t3.describe(error))
            await self.append("commentary", delegation_id, self.label + " has the request, but I lost track "
                              "of its answer. It will be in " + self.label + "'s thread.")
            return
        self.active_at = self.now()
        if status != "completed":
            await self.append("commentary", delegation_id,
                              self.label + " couldn't finish that request; the details are in its thread.")
            return
        spoken = t3.speakable(reply)
        log("result id=" + delegation_id + " chars=" + str(len(reply)) + " pieces=" + str(len(pieces(spoken))))
        for piece in pieces(spoken) or [self.label + " finished without a reply."]:
            await self.append("commentary", delegation_id, piece)


def submit_request(config, mcp, delegation_id, message):
    """Send one request to the thread, idempotent on the delegation."""
    request = "cantrip-live-" + delegation_id
    result = mcp.call("t3_thread_send", {"threadId": config["thread_id"], "message": message,
                                         "mode": "queue", "clientRequestId": request},
                      timeout=t3.SEND_SECONDS)
    return {"thread_id": config["thread_id"], "request_id": request, "status": "sent", "label": config.get("label"),
            "run_id": t3.find_key(result, "runId"), "message_id": t3.find_key(result, "messageId")}


def collect_reply(mcp, record, progress, now=time.monotonic, sleep=time.sleep):
    """Wait for the request's run and return (status, final reply)."""
    reported = now()
    while True:
        began = now()
        outcome, record, reply = t3.advance(mcp, record, now=time.time)
        if outcome != "wait":
            return record.get("run_status", outcome), reply
        if now() - began < 1:
            sleep(1)   # queued behind other work: no run to wait on yet, so don't spin
        if now() - reported > PROGRESS_SECONDS:
            label = record.get("label") or "Home"
            progress(label + (" is waiting for the user in its thread." if record.get("waiting_noticed")
                              else " is still working on it."))
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
    """Writes PCM to pw-cat off the event loop, through a small jitter buffer.

    GPT-Live sends 100 ms blocks at about real time with arrival gaps up to ~0.3 s.
    Written as they arrive, every late block was an audible gap. Each turn (and any
    underrun) now starts only once PREBUFFER_SECONDS is queued, or a short reply has
    fully arrived.
    """

    def __init__(self, target=None, clock=time.monotonic):
        super().__init__(daemon=True)
        argv = ["pw-cat", "--playback", "--raw", "--format", "s16", "--rate", str(RATE), "--channels", "1"]
        if target:
            argv += ["--target", target]
        self.process = subprocess.Popen(argv + ["-"], stdin=subprocess.PIPE,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.audio = queue.Queue()
        self.clock = clock
        self.underruns = 0

    def write(self, pcm):
        self.process.stdin.write(pcm)
        self.process.stdin.flush()

    def run(self):
        pending, first, plays_until = bytearray(), None, None
        while True:
            try:
                block = self.audio.get(timeout=0.02)
            except queue.Empty:
                block = b""
            if block is None:
                break
            now = self.clock()
            if plays_until is not None and now > plays_until:   # ran dry: re-prime before resuming
                if now - plays_until < 1.0:
                    self.underruns += 1
                plays_until = None
            if block:
                pending += block
                first = first if first is not None else now
            if not pending:
                continue
            seconds = len(pending) / (RATE * 2)
            if plays_until is None and seconds < PREBUFFER_SECONDS and now - first < PREBUFFER_SECONDS + 0.2:
                continue
            try:
                self.write(bytes(pending))
            except BrokenPipeError:
                return
            plays_until = (plays_until or now) + seconds
            pending, first = bytearray(), None
        if pending:
            try:
                self.write(bytes(pending))
            except BrokenPipeError:
                pass

    def close(self):
        self.audio.put(None)
        self.join(timeout=2)
        log("player underruns=" + str(self.underruns))
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


def limits(live):
    """Idle and maximum session seconds; a bad value falls back to the default rather than disabling them."""
    def minutes(key):
        try:
            value = float(live[key])
        except (TypeError, ValueError):
            value = 0
        return (value if value > 0 else LIVE_DEFAULTS[key]) * 60
    return minutes("idle_minutes"), minutes("max_minutes")


def audioop_rms(pcm):
    samples = memoryview(pcm).cast("h") if len(pcm) % 2 == 0 else memoryview(pcm[:-1]).cast("h")
    return int((sum(x * x for x in samples) / max(1, len(samples))) ** 0.5)


def notify_later(message):
    """Desktop notice without blocking the event loop."""
    threading.Thread(target=t3.notify, args=(message,), daemon=True).start()


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
    echo = player = recorder = None
    try:
        pid_file.write_text(str(os.getpid()))
        echo = EchoCancel() if live["echo_cancel"] else None
        devices = (echo.__enter__() if echo else None) or (live["input_device"] or None, live["output_device"] or None)
        chunks = asyncio.Queue()
        player = Player(devices[1])
        player.start()
        recorder = microphone(loop, chunks, devices[0])
        async with AsyncOpenAI() as client, client.live.connect() as connection:
            async def send(event):
                await connection.send(event)

            # One MCP client per call: delegations run concurrently on their own threads.
            bridge = Bridge(config, send,
                            lambda delegation, message: submit_request(config, t3.connect(config), delegation, message),
                            lambda record, progress: collect_reply(t3.connect(config), record, progress))
            await connection.session.start(session=session_config(config, context), event_id="start")
            started = asyncio.Event()
            muted = False
            opened = time.monotonic()
            idle_limit, max_limit = limits(live)
            trace_path = os.environ.get("CANTRIP_T3_LIVE_TRACE")
            trace = open(trace_path, "a", buffering=1) if trace_path else None

            async def pump_audio():
                pending = b""
                await started.wait()
                while True:
                    block = pending + await chunks.get()
                    whole = len(block) - len(block) % 2
                    pending = block[whole:]
                    if whole and not stopping.is_set():
                        # Muted: keep the input stream going with silence; the room's audio never leaves.
                        audio = bytes(whole) if muted else block[:whole]
                        await connection.session.input_audio.append(audio=base64.b64encode(audio).decode("ascii"))

            async def controls():
                nonlocal muted
                await started.wait()
                while True:
                    await toggled.wait()
                    toggled.clear()
                    muted = not muted
                    if muted:
                        await connection.session.input_audio.mute(event_id=bridge.event_id("mute"))
                    else:
                        await connection.session.input_audio.unmute(event_id=bridge.event_id("unmute"))
                    notify_later("Live microphone " + ("muted" if muted else "on"))

            async def watchdog():
                """Close a forgotten session: it bills every connected second."""
                warned = False
                while not stopping.is_set():
                    await asyncio.sleep(5)
                    idle = time.monotonic() - bridge.active_at > idle_limit and not bridge.tasks
                    if not warned and time.monotonic() - opened > max_limit - 60:
                        warned = True
                        await bridge.append("instructions", None, "The session ends in about a minute. "
                                            "Tell the user briefly at the next natural pause.")
                    if idle or time.monotonic() - opened > max_limit:
                        log("closing reason=" + ("idle" if idle else "max-duration"))
                        notify_later("Live session closing (" + ("idle" if idle else "time limit") + ")")
                        stopping.set()

            async def close_on_stop():
                await stopping.wait()
                await connection.session.close()
                try:
                    await asyncio.wait_for(bridge.closed.wait(), CLOSE_SECONDS)
                except TimeoutError:
                    log("close unconfirmed")
                    await connection.close()

            helpers = [asyncio.create_task(job()) for job in (pump_audio, controls, watchdog, close_on_stop)]
            try:
                async for event in connection:
                    data = event.model_dump(exclude_none=True)
                    if data.get("type") == "session.started":
                        log("started session=" + str((data.get("session") or {}).get("id")))
                        notify_later("Live with " + bridge.label)
                        started.set()
                    elif data.get("type") == "session.output_audio.delta":
                        bridge.active_at = time.monotonic()
                        pcm = base64.b64decode(data["delta"])
                        if trace:
                            level = audioop_rms(pcm)
                            trace.write("%.4f %d %d\n" % (time.monotonic(), len(pcm), level))   # timing, size, level only
                        player.audio.put(pcm)
                        continue
                    await bridge.handle(data)
                    if bridge.closed.is_set():
                        break
            finally:
                for task in helpers + list(bridge.tasks):
                    task.cancel()
    finally:
        pid_file.unlink(missing_ok=True)
        if recorder:
            recorder.terminate()
        if player:
            player.close()
        if echo:
            echo.__exit__(None, None, None)
        t3.notify("Live session ended")   # synchronous: it is the sign that billing stopped


def live_pid():
    """The running session's pid, only if it really is this bridge (a stale pid is never signalled)."""
    try:
        pid = int((t3.runtime_dir() / "live.pid").read_text())
        argv = Path("/proc", str(pid), "cmdline").read_bytes().decode(errors="replace")
    except (OSError, ValueError):
        return None
    return pid if "cantrip_t3_live" in argv or "cantrip-t3-live" in argv else None


def main(argv=None):
    parser = argparse.ArgumentParser(prog="cantrip-t3-live", description=__doc__.split("\n\n")[0])
    parser.add_argument("--config", type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("run", help="hold one live session until SIGINT/SIGTERM")
    commands.add_parser("session", help="print the session.start configuration, without connecting")
    commands.add_parser("mute", help="toggle the running session's microphone")
    args = parser.parse_args(argv)
    if args.command == "mute":
        pid = live_pid()
        if pid is None:
            print("no live session", file=sys.stderr)
            return 1
        os.kill(pid, signal.SIGUSR1)
        return 0
    config = t3.load_config(args.config)
    if args.command == "session":
        print(json.dumps(session_config(config, summary(live_config(config)["summary_command"])), indent=2))
        return 0
    os.environ["OPENAI_API_KEY"] = t3.secret("OPENAI_API_KEY")   # first line, as `pass` stores it
    if not os.environ["OPENAI_API_KEY"]:
        raise SystemExit("cantrip-t3-live: OPENAI_API_KEY is not set")
    asyncio.run(run(config))
    return 0


if __name__ == "__main__":
    sys.exit(main())
