#!/usr/bin/python3 -I
"""Hands-free turn-taking with a T3 thread: listen, transcribe, send, and let the collector speak.

`run` keeps the microphone open until SIGINT/SIGTERM or `idle_minutes` without
speech, replies or owed replies. Local energy-based end-of-speech detection
cuts utterances; each one is transcribed by OpenRouter's JSON
`/audio/transcriptions` endpoint and sent
through `cantrip_t3.send` as its own take, so `cantrip-t3 collect` speaks the
reply exactly as for push-to-talk. With `headphones = true`, speaking over a
reply stops it (barge-in); on speakers the microphone is ignored while a reply
plays, so the agent never hears its own voice.
Audio leaves the machine only for detected speech. Logs carry durations,
counts and error codes, never audio or text.
"""

import argparse
import array
import base64
import io
import json
import math
import os
import queue
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time
import uuid
import wave

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cantrip_t3 as t3  # noqa: E402

RATE = 16_000
FRAME_MS = 30
FRAME_BYTES = RATE * FRAME_MS // 1000 * 2

CONVERSE_DEFAULTS = {
    "stt_endpoint": "https://openrouter.ai/api/v1",
    "stt_model": "nvidia/parakeet-tdt-0.6b-v3",
    "language": "en",
    "end_silence_ms": 1000,     # this much quiet ends an utterance (shorter splits normal pauses)
    "min_speech_ms": 350,       # shorter bursts are noise
    "max_utterance_s": 60,
    "idle_minutes": 5,
    "headphones": False,        # True enables barge-in; on speakers input pauses while a reply plays
}
PLAYBACK_TAIL_S = 0.6           # room echo after a reply stops


def log(message):
    print("[T3Converse] " + message, file=sys.stderr, flush=True)


def converse_config(config):
    return {**CONVERSE_DEFAULTS, **config.get("converse", {})}


def rms(frame):
    samples = array.array("h", frame)
    return math.sqrt(sum(x * x for x in samples) / len(samples)) if samples else 0.0


class Endpointer:
    """Energy endpointing with an adaptive noise floor.

    The floor learns only between utterances, never from the user's voice.
    Speech needs a run of consecutive loud frames, so typing never counts.
    Sound that stays loud for three seconds without the dips that syllables
    make (a fan, a hum) is steady noise: it is dropped, never uploaded, and
    becomes the new floor.
    """

    RUN_FRAMES = 5          # 150 ms of continuous sound: a syllable, not a click
    STEADY_FRAMES = 100     # judge steadiness after 3 s of sound
    DIP_WINDOW = 10         # a dip is a frame below 40% of the loudest of the last 300 ms
    DIP_RATIO = 0.4
    MIN_DIPS_PER_S = 0.5    # speech dips several times a second; a fan almost never

    def __init__(self, settings, floor=200.0):
        self.end_frames = settings["end_silence_ms"] // FRAME_MS
        self.min_frames = settings["min_speech_ms"] // FRAME_MS
        self.max_frames = settings["max_utterance_s"] * 1000 // FRAME_MS
        self.floor = floor
        self.reset()

    def reset(self):
        self.frames, self.levels = [], []
        self.voiced = self.quiet = self.run = self.longest = self.dips = 0
        self.started = False

    def threshold(self):
        return max(self.floor * 3, 400.0)

    def speech(self):
        return self.voiced >= self.min_frames and self.longest >= self.RUN_FRAMES

    def steady(self):
        if len(self.frames) < self.STEADY_FRAMES:
            return False
        return self.dips < self.MIN_DIPS_PER_S * len(self.frames) * FRAME_MS / 1000

    def feed(self, frame):
        """("start", None) once speech is established, ("utterance", pcm) when it ends,
        ("noise", None) for dropped steady sound, else None."""
        level = rms(frame)
        loud = level > self.threshold()
        if not self.frames:
            if not loud:
                self.floor = 0.95 * self.floor + 0.05 * max(level, 50.0)
                return None
        recent = self.levels[-self.DIP_WINDOW:]
        if recent and level < self.DIP_RATIO * max(recent):
            self.dips += 1
        self.frames.append(frame)
        self.levels.append(level)
        if loud:
            self.voiced += 1
            self.quiet = 0
            self.run += 1
            self.longest = max(self.longest, self.run)
        else:
            self.quiet += 1
            self.run = 0
        if self.steady():
            self.floor = max(self.floor, sorted(self.levels)[len(self.levels) // 2] / 2)
            self.reset()
            return ("noise", None)
        if not self.started and self.speech():
            self.started = True
            return ("start", None)
        if self.quiet >= self.end_frames or len(self.frames) >= self.max_frames:
            pcm, speech = b"".join(self.frames), self.speech()
            self.reset()
            if speech:
                return ("utterance", pcm)
        return None


def wav_bytes(pcm):
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(RATE)
        out.writeframes(pcm)
    return buffer.getvalue()


class SttRefused(Exception):
    """The transcription service refused the key or model: retrying cannot help."""


def transcribe(settings, key, pcm, opener=None):
    body = json.dumps({"model": settings["stt_model"], "language": settings["language"],
                       "input_audio": {"data": base64.b64encode(wav_bytes(pcm)).decode(), "format": "wav"}})
    request = t3.urllib.request.Request(settings["stt_endpoint"].rstrip("/") + "/audio/transcriptions",
                                        data=body.encode(), headers={"Authorization": "Bearer " + key,
                                                                     "Content-Type": "application/json"})
    try:
        with (opener or t3.OPEN)(request, timeout=30) as response:
            return (json.loads(response.read()).get("text") or "").strip()
    except t3.urllib.error.HTTPError as error:
        if error.code in (401, 402, 403, 404):
            raise SttRefused("stt-http-" + str(error.code)) from None
        raise t3.McpError("stt-http-" + str(error.code)) from None


def speaking():
    """True only while a live reply player runs; a stale pid file never deafens the session."""
    try:
        pid = int((t3.runtime_dir() / "player.pid").read_text())
        argv0 = Path("/proc", str(pid), "cmdline").read_bytes().split(b"\0")[0].decode()
    except (OSError, ValueError):
        return False
    return Path(argv0).name in t3.PLAYERS


OWED_SECONDS = 10 * 60          # a reply owed longer than this no longer holds the mic open


def busy_elsewhere():
    """A reply this session is still owed, recently enough to be worth staying for."""
    now = time.time()
    for path in (t3.state_dir() / "pending").glob("converse-*.json"):
        record = t3.readable(path)
        if record and now - (record.get("sent_at") or 0) < OWED_SECONDS:
            return True
    return False


class Conversation:
    """Turns utterances into takes; independent of audio capture for testing.

    Transcription and sending run on one worker thread, so capture never stalls
    and the next sentence is not lost while the previous one is processed.
    """

    def __init__(self, config, key, transcribe=transcribe, send=t3.send, now=time.monotonic, threaded=False):
        self.config = config
        self.label = config.get("label", "Home")
        self.settings = converse_config(config)
        self.key = key
        self.transcribe = transcribe
        self.send = send
        self.now = now
        self.endpointer = Endpointer(self.settings)
        self.active_at = now()
        self.turns = 0
        self.played_until = -1.0
        self.speaking = speaking
        self.recording = t3.cantrip_recording
        self.pending = busy_elsewhere
        self.take_recording = False
        self.refused = None
        self.work = queue.Queue() if threaded else None
        if threaded:
            threading.Thread(target=self.worker, daemon=True).start()
            threading.Thread(target=self.watch_cantrip, daemon=True).start()

    def watch_cantrip(self):
        """Poll Cantrip off the capture thread, so a slow daemon never stalls capture."""
        while True:
            self.take_recording = self.recording()
            time.sleep(1)

    def cantrip_take(self):
        """A push-to-talk take is recording: its speech is not ours."""
        if self.work is None:   # synchronous (tests): ask directly
            return self.recording()
        return self.take_recording

    def feed(self, frame):
        playing = self.speaking()
        if playing:
            self.played_until = self.now()
            self.active_at = self.now()
        deaf = not self.settings["headphones"] and (playing or self.now() - self.played_until < PLAYBACK_TAIL_S)
        if deaf or self.cantrip_take():
            self.endpointer.reset()   # on speakers, never hear the reply; keep the floor
            return
        event = self.endpointer.feed(frame)
        self.mark_speaking(self.endpointer.started)
        if not event:
            return
        if event[0] == "noise":
            log("steady noise dropped floor=%d" % self.endpointer.floor)
        elif event[0] == "start":
            self.active_at = self.now()
            if playing:
                t3.stop_speaking()
                log("barge-in")
        else:
            self.active_at = self.now()
            if self.work:
                self.work.put(event[1])
            else:
                self.utterance(event[1])

    def mark_speaking(self, speaking):
        """Tell the collector the user is mid-utterance, so a reply never starts over them."""
        marker = t3.runtime_dir() / "user-speaking"
        marked = getattr(self, "marked_at", None)
        if speaking:
            if marked is None or self.now() - marked >= 1:   # refresh once a second
                marker.write_text("")
                self.marked_at = self.now()
        elif marked is not None:
            marker.unlink(missing_ok=True)
            self.marked_at = None

    def worker(self):
        while True:
            pcm = self.work.get()
            try:
                self.utterance(pcm)
            except Exception as error:  # never let one turn end the worker
                log("turn failed class=" + type(error).__name__)
                t3.notify("Something went wrong with that; please say it again.")

    def utterance(self, pcm):
        seconds = len(pcm) / 2 / RATE
        started = time.monotonic()
        try:
            text = self.transcribe(self.settings, self.key, pcm)
        except SttRefused as error:
            log("stt refused code=" + str(error))
            t3.notify("Speech-to-text refused the request (" + str(error) + "); hands-free has stopped.")
            self.refused = str(error)
            return
        except Exception as error:  # a malformed reply or transport failure loses only this utterance
            log("stt failed code=" + t3.describe(error))
            t3.notify("Couldn't transcribe that; please say it again.")
            return
        if len(text) < 2:
            log("stt empty seconds=%.1f" % seconds)
            return
        take = "converse-" + uuid.uuid4().hex[:16]
        try:
            self.send(self.config, text, take)
        except (t3.McpError, OSError, ValueError) as error:
            # `send` keeps the pending record only when the message may have arrived.
            log("send failed code=" + t3.describe(error))
            if (t3.state_dir() / "pending" / (take + ".json")).exists():
                t3.notify("Not sure " + self.label + " got that; if it did, you'll hear the reply.")
            else:
                t3.notify(self.label + " didn't get that; please say it again.")
            return
        self.turns += 1
        self.active_at = self.now()
        log("turn=%d seconds=%.1f stt_s=%.2f chars=%d" % (self.turns, seconds, time.monotonic() - started, len(text)))

    def idle(self):
        """Idle only after `idle_minutes` with no speech, no reply playing and no reply owed."""
        if self.pending():
            self.active_at = self.now()
        return self.now() - self.active_at > self.settings["idle_minutes"] * 60


def check_key(settings, key, opener=None):
    """Fail at start rather than on the first utterance when the STT key is refused (OpenRouter's /key)."""
    request = t3.urllib.request.Request(settings["stt_endpoint"].rstrip("/") + "/key",
                                        headers={"Authorization": "Bearer " + key})
    try:
        with (opener or t3.OPEN)(request, timeout=10):
            return True
    except t3.urllib.error.HTTPError as error:
        if error.code in (401, 403):
            return False
        return True   # not OpenRouter, or no such route: find out on the first utterance
    except OSError:
        return True


def run(config):
    label = config.get("label", "Home")
    try:
        key = t3.secret("CANTRIP_T3_SPEECH_KEY")
        if not key:
            raise ValueError("CANTRIP_T3_SPEECH_KEY is not set")
        t3.connect(config, timeout=t3.SEND_STEP_SECONDS)   # fail now if T3 refuses the sign-in
        if not check_key(converse_config(config), key):
            raise ValueError("speech-to-text key refused")
    except (t3.McpError, OSError, ValueError, SystemExit) as error:
        t3.notify("Hands-free couldn't start (" + t3.describe(error) + ").")
        raise SystemExit("cantrip-t3-converse: could not start: " + t3.describe(error)) from None
    conversation = Conversation(config, key, threaded=True)
    stopping = []
    signal.signal(signal.SIGTERM, lambda *_: stopping.append(1))
    signal.signal(signal.SIGINT, lambda *_: stopping.append(1))
    recorder = subprocess.Popen(["pw-record", "--raw", "--format", "s16", "--rate", str(RATE),
                                 "--channels", "1", "-"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    t3.notify("Listening for " + label)
    log("listening model=" + conversation.settings["stt_model"])
    try:
        while not stopping and not conversation.idle() and not conversation.refused:
            frame = recorder.stdout.read(FRAME_BYTES)
            if len(frame) < FRAME_BYTES:
                break
            conversation.feed(frame)
    finally:
        conversation.mark_speaking(False)
        recorder.stdout.close()   # a recorder blocked on a full pipe can then exit
        recorder.terminate()
        try:
            recorder.wait(timeout=3)
        except subprocess.TimeoutExpired:
            recorder.kill()
            recorder.wait()
        unsent = conversation.work.qsize()
        t3.notify("Stopped listening for " + label + (" (" + str(unsent) + " unsent)" if unsent else ""))
        log("stopped turns=%d" % conversation.turns)


def main(argv=None):
    parser = argparse.ArgumentParser(prog="cantrip-t3-converse", description=__doc__.split("\n\n")[0])
    parser.add_argument("--config", type=Path)
    parser.add_argument("command", choices=("run",))
    args = parser.parse_args(argv)
    run(t3.load_config(args.config))
    return 0


if __name__ == "__main__":
    sys.exit(main())
