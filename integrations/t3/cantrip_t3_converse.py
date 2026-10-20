#!/usr/bin/python3 -I
"""Hands-free turn-taking with a T3 thread: listen, transcribe, send, and let the collector speak.

`run` keeps the microphone open until SIGINT/SIGTERM or `idle_minutes` without
speech. Local energy-based end-of-speech detection cuts utterances; each one is
transcribed by an OpenAI-compatible `/audio/transcriptions` endpoint and sent
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
from pathlib import Path
import signal
import subprocess
import sys
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
    "end_silence_ms": 700,      # this much quiet ends an utterance
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

    While a reply is playing the threshold also tracks its echo, so only speech
    clearly louder than the reply counts as the user (headphones need none of this).
    """

    def __init__(self, settings, floor=200.0):
        self.end_frames = settings["end_silence_ms"] // FRAME_MS
        self.min_frames = settings["min_speech_ms"] // FRAME_MS
        self.max_frames = settings["max_utterance_s"] * 1000 // FRAME_MS
        self.floor = floor
        self.echo = 0.0
        self.frames = []
        self.voiced = 0
        self.quiet = 0

    def threshold(self, playing):
        base = max(self.floor * 3, 400.0)
        return max(base, self.echo * 2.0) if playing else base

    def feed(self, frame, playing=False):
        """Returns ("start", None) when speech begins, ("utterance", pcm) when it ends, else None."""
        level = rms(frame)
        loud = level > self.threshold(playing)
        if not self.frames:
            if playing and not loud:
                self.echo = 0.9 * self.echo + 0.1 * level
            if not loud:
                self.floor = 0.95 * self.floor + 0.05 * max(level, 50.0)
                return None
            self.frames, self.voiced, self.quiet = [frame], 1, 0
            return None
        self.frames.append(frame)
        if loud:
            self.voiced += 1
            self.quiet = 0
            if self.voiced == self.min_frames:
                return ("start", None)
        else:
            self.quiet += 1
        if self.quiet >= self.end_frames or len(self.frames) >= self.max_frames:
            frames, voiced = self.frames, self.voiced
            self.frames, self.voiced, self.quiet = [], 0, 0
            if voiced >= self.min_frames:
                return ("utterance", b"".join(frames))
        return None


def wav_bytes(pcm):
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(RATE)
        out.writeframes(pcm)
    return buffer.getvalue()


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
        raise t3.McpError("stt-http-" + str(error.code)) from None


def speaking():
    return (t3.runtime_dir() / "player.pid").exists()


class Conversation:
    """Turns utterances into takes; independent of audio capture for testing."""

    def __init__(self, config, key, transcribe=transcribe, send=t3.send, now=time.monotonic):
        self.config = config
        self.settings = converse_config(config)
        self.key = key
        self.transcribe = transcribe
        self.send = send
        self.now = now
        self.endpointer = Endpointer(self.settings)
        self.last_speech = now()
        self.turns = 0
        self.played_until = -1.0
        self.speaking = speaking

    def feed(self, frame):
        playing = self.speaking()
        if playing:
            self.played_until = self.now()
        if not self.settings["headphones"] and (playing or self.now() - self.played_until < PLAYBACK_TAIL_S):
            self.endpointer = Endpointer(self.settings, self.endpointer.floor)   # drop echo, keep the floor
            return
        event = self.endpointer.feed(frame, playing=playing)
        if not event:
            return
        self.last_speech = self.now()
        if event[0] == "start":
            if playing:
                t3.stop_speaking()
                log("barge-in")
            return
        self.utterance(event[1])

    def utterance(self, pcm):
        seconds = len(pcm) / 2 / RATE
        started = time.monotonic()
        try:
            text = self.transcribe(self.settings, self.key, pcm)
        except (t3.McpError, OSError, ValueError) as error:
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
            log("send failed code=" + t3.describe(error))
            t3.notify(self.config.get("label", "Home") + " didn't get that; please say it again.")
            return
        self.turns += 1
        log("turn=%d seconds=%.1f stt_s=%.2f chars=%d" % (self.turns, seconds, time.monotonic() - started, len(text)))

    def idle(self):
        return self.now() - self.last_speech > self.settings["idle_minutes"] * 60


def run(config):
    key = t3.secret("CANTRIP_T3_SPEECH_KEY")
    if not key:
        raise SystemExit("cantrip-t3-converse: CANTRIP_T3_SPEECH_KEY is not set")
    t3.connect(config, timeout=t3.SEND_STEP_SECONDS)   # fail now if T3 refuses the sign-in
    conversation = Conversation(config, key)
    stopping = []
    signal.signal(signal.SIGTERM, lambda *_: stopping.append(1))
    signal.signal(signal.SIGINT, lambda *_: stopping.append(1))
    recorder = subprocess.Popen(["pw-record", "--raw", "--format", "s16", "--rate", str(RATE),
                                 "--channels", "1", "-"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    t3.notify("Listening for " + config.get("label", "Home"))
    log("listening model=" + conversation.settings["stt_model"])
    try:
        while not stopping and not conversation.idle():
            frame = recorder.stdout.read(FRAME_BYTES)
            if len(frame) < FRAME_BYTES:
                break
            conversation.feed(frame)
    finally:
        recorder.terminate()
        recorder.wait()
        t3.notify("Stopped listening for " + config.get("label", "Home"))
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
