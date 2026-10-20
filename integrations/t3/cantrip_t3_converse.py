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

    The floor follows quiet quickly and rises slowly through sustained sound, so
    a fan, music or a TV becomes background. Speech needs a run of consecutive
    loud frames, so typing never counts. A full-length "utterance" that is loud
    almost throughout is steady noise: it is dropped, never uploaded, and the
    floor jumps to it.
    """

    RUN_FRAMES = 5          # 150 ms of continuous sound: a syllable, not a click
    NOISE_RATIO = 0.9
    STEADY_FRAMES = 100     # judge steadiness over at least 3 s
    STEADY_CV = 0.35        # speech level varies far more than a fan's

    def __init__(self, settings, floor=200.0):
        self.end_frames = settings["end_silence_ms"] // FRAME_MS
        self.min_frames = settings["min_speech_ms"] // FRAME_MS
        self.max_frames = settings["max_utterance_s"] * 1000 // FRAME_MS
        self.floor = floor
        self.reset()

    def reset(self):
        self.frames, self.levels = [], []
        self.voiced = self.quiet = self.run = self.longest = 0
        self.started = False

    def threshold(self):
        return max(self.floor * 3, 400.0)

    def speech(self):
        return self.voiced >= self.min_frames and self.longest >= self.RUN_FRAMES

    def steady(self):
        loud = [level for level in self.levels if level > self.threshold()] or self.levels
        if len(loud) < self.STEADY_FRAMES:
            return False
        mean = sum(loud) / len(loud)
        spread = math.sqrt(sum((level - mean) ** 2 for level in loud) / len(loud))
        return mean > 0 and spread / mean < self.STEADY_CV

    def drop_noise(self):
        self.floor = max(self.floor, sorted(self.levels)[len(self.levels) // 2] / 2.5)
        self.reset()
        return ("noise", None)

    def feed(self, frame):
        """("start", None) once speech is established, ("utterance", pcm) when it ends,
        ("noise", None) for dropped steady sound, else None."""
        level = rms(frame)
        loud = level > self.threshold()
        if loud:
            self.floor = min(self.floor * 1.002, level)
        else:
            self.floor = 0.95 * self.floor + 0.05 * max(level, 50.0)
        if not self.frames and not loud:
            return None
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
        if not self.started and self.speech():
            self.started = True
            return ("start", None)
        full = len(self.frames) >= self.max_frames
        if full and self.voiced / len(self.frames) > self.NOISE_RATIO or self.steady():
            return self.drop_noise()
        if self.quiet >= self.end_frames or full:
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


def busy_elsewhere():
    """Replies still owed or a Cantrip take recording: not idle, and not ours to hear."""
    return any((t3.state_dir() / "pending").glob("*.json"))


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
        self.checked = (-10.0, False)
        self.refused = None
        self.work = queue.Queue() if threaded else None
        if threaded:
            threading.Thread(target=self.worker, daemon=True).start()

    def cantrip_take(self):
        """A push-to-talk take is recording (checked once a second): its speech is not ours."""
        checked, recording = self.checked
        if self.now() - checked >= 1:
            self.checked = (self.now(), self.recording())
        return self.checked[1]

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

    def worker(self):
        while True:
            self.utterance(self.work.get())

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
        except t3.McpError as error:
            log("send failed code=" + t3.describe(error))
            if error.refused() or str(error).startswith(("http-4", "rpc-")):
                t3.notify(self.label + " didn't get that; please say it again.")
            else:
                t3.notify("Not sure " + self.label + " got that; if it did, you'll hear the reply.")
            return
        except (OSError, ValueError) as error:
            log("send failed code=" + t3.describe(error))
            t3.notify("Not sure " + self.label + " got that; if it did, you'll hear the reply.")
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
    key = t3.secret("CANTRIP_T3_SPEECH_KEY")
    if not key:
        raise SystemExit("cantrip-t3-converse: CANTRIP_T3_SPEECH_KEY is not set")
    t3.connect(config, timeout=t3.SEND_STEP_SECONDS)   # fail now if T3 refuses the sign-in
    if not check_key(converse_config(config), key):
        t3.notify("Speech-to-text key was refused; hands-free did not start.")
        raise SystemExit("cantrip-t3-converse: speech-to-text key refused")
    conversation = Conversation(config, key, threaded=True)
    stopping = []
    signal.signal(signal.SIGTERM, lambda *_: stopping.append(1))
    signal.signal(signal.SIGINT, lambda *_: stopping.append(1))
    recorder = subprocess.Popen(["pw-record", "--raw", "--format", "s16", "--rate", str(RATE),
                                 "--channels", "1", "-"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    t3.notify("Listening for " + config.get("label", "Home"))
    log("listening model=" + conversation.settings["stt_model"])
    try:
        while not stopping and not conversation.idle() and not conversation.refused:
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
