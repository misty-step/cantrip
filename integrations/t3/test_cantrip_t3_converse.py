import array
import importlib.util
import math
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

here = Path(__file__).resolve().parent
sys.path.insert(0, str(here))
spec = importlib.util.spec_from_file_location("cantrip_t3_converse", here / "cantrip_t3_converse.py")
converse = importlib.util.module_from_spec(spec)
spec.loader.exec_module(converse)

CONFIG = {**converse.t3.DEFAULTS, "mcp_url": "https://t3.example/mcp", "thread_id": "mcp:home"}
SAMPLES = converse.FRAME_BYTES // 2


def tone(amplitude, frames=1):
    out = []
    for _ in range(frames):
        out.append(array.array("h", (int(amplitude * math.sin(i / 3)) for i in range(SAMPLES))).tobytes())
    return out


def quiet(frames=1):
    return tone(30, frames)


def speech(frames, scale=1.0):
    """Syllables: loud runs of varying level with short dips, like a voice."""
    out = []
    for n in range(frames):
        out += quiet() if n % 12 in (10, 11) else tone(scale * (6000, 2500, 4500, 1800)[n % 4])
    return out


def clicks(frames):
    return [tone(8000)[0] if n % 4 == 0 else quiet()[0] for n in range(frames)]


class EndpointerTest(unittest.TestCase):
    def events(self, frames, **settings):
        endpointer = converse.Endpointer({**converse.CONVERSE_DEFAULTS, **settings})
        return [e for e in (endpointer.feed(f) for f in frames) if e], endpointer

    def test_speech_then_silence_makes_one_utterance(self):
        events, _ = self.events(quiet(20) + speech(40) + quiet(40))
        self.assertEqual([e[0] for e in events], ["start", "utterance"])
        self.assertGreater(len(events[1][1]), 40 * converse.FRAME_BYTES)

    def test_a_short_burst_or_typing_is_not_an_utterance(self):
        self.assertEqual(self.events(quiet(20) + speech(3) + quiet(40))[0], [])
        self.assertEqual(self.events(quiet(20) + clicks(200) + quiet(40))[0], [])

    def test_long_speech_is_cut_at_the_maximum(self):
        events, _ = self.events(speech(80), max_utterance_s=1)
        self.assertEqual(sum(1 for e in events if e[0] == "utterance"), 2)

    def test_steady_noise_is_dropped_and_learned_as_background(self):
        events, endpointer = self.events(quiet(20) + tone(3000, 400) + quiet(10) + speech(10) + tone(3000, 2000))
        self.assertEqual([e[0] for e in events if e[0] != "start"], ["noise"])
        self.assertGreater(endpointer.threshold(), rms(tone(3000)[0]))
        self.assertNotIn("utterance", [e[0] for e in events])

    def test_speech_over_learned_noise_is_still_heard(self):
        # a fan at ~1,000 RMS, then speech about ten times louder, as a voice near the mic is
        events, _ = self.events(tone(1500, 400) + speech(40, scale=4) + tone(1500, 40))
        self.assertIn("utterance", [e[0] for e in events])


def rms(frame):
    return converse.rms(frame)


def espeak(text):
    """Real synthesized speech at 16 kHz, or None where espeak-ng is missing."""
    import shutil, subprocess, wave, io
    if not shutil.which("espeak-ng"):
        return None
    raw = subprocess.run(["espeak-ng", "--stdout", text], capture_output=True, check=True).stdout
    with wave.open(io.BytesIO(raw)) as w:
        rate, samples = w.getframerate(), array.array("h", w.readframes(w.getnframes()))
    out = array.array("h", (samples[int(i * rate / 16000)] for i in range(int(len(samples) * 16000 / rate))))
    b = out.tobytes()
    return [b[i:i + converse.FRAME_BYTES] for i in range(0, len(b) - converse.FRAME_BYTES + 1, converse.FRAME_BYTES)]


class RealSpeechTest(unittest.TestCase):
    """Regression for the review at 64ce49a: tone fixtures hid speech being judged as noise."""

    def test_synthesized_speech_is_uploaded_whole_even_when_long(self):
        frames = espeak("This is a voice test. I want to know what the agents are doing right now, "
                        "which pull requests are waiting for review, and whether the release went out "
                        "this afternoon as planned, because I need to decide what to work on next.")
        if frames is None:
            self.skipTest("espeak-ng not installed")
        endpointer = converse.Endpointer(converse.CONVERSE_DEFAULTS)
        events = [e for e in (endpointer.feed(f) for f in quiet(20) + frames + quiet(40)) if e]
        self.assertNotIn("noise", [e[0] for e in events])
        uploaded = sum(len(e[1]) for e in events if e[0] == "utterance")
        self.assertGreater(uploaded, 0.9 * len(frames) * converse.FRAME_BYTES)

    def test_the_floor_never_learns_the_voice(self):
        frames = espeak("Keep talking for a while so the floor would climb if it learned from speech. " * 3)
        if frames is None:
            self.skipTest("espeak-ng not installed")
        endpointer = converse.Endpointer(converse.CONVERSE_DEFAULTS)
        for f in quiet(20) + frames:
            endpointer.feed(f)
        self.assertLess(endpointer.floor, 200)


class ConversationTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        patch = mock.patch.dict(os.environ, {"XDG_STATE_HOME": self.tmp.name, "XDG_RUNTIME_DIR": self.tmp.name})
        patch.start()
        self.addCleanup(patch.stop)
        self.addCleanup(self.tmp.cleanup)
        notify = mock.patch.object(converse.t3, "notify")
        self.notified = notify.start()
        self.addCleanup(notify.stop)
        self.sent = []
        self.heard = "What are the agents doing?"
        self.clock = [0.0]

    def conversation(self, **settings):
        config = {**CONFIG, "converse": settings}
        conversation = converse.Conversation(config, "k", transcribe=lambda s, k, pcm: self.heard,
                                             send=lambda c, text, take: self.sent.append((text, take)),
                                             now=lambda: self.clock[0])
        conversation.speaking = lambda: False
        conversation.recording = lambda: False
        conversation.pending = lambda: False
        return conversation

    def test_speech_in_progress_holds_the_reply(self):
        conversation = self.conversation()
        self.speak(conversation, quiet(20) + speech(20))
        self.assertTrue(converse.t3.user_speaking())
        self.speak(conversation, speech(10) + quiet(40))
        self.assertFalse(converse.t3.user_speaking())

    def test_send_failure_message_follows_whether_anything_may_have_arrived(self):
        conversation = self.conversation()

        def kept(config, text, take):
            path = converse.t3.state_dir() / "pending" / (take + ".json")
            path.write_text("{}")
            raise TimeoutError()

        conversation.send = kept
        self.speak(conversation, quiet(20) + speech(30) + quiet(40))
        self.assertIn("Not sure", self.notified.call_args.args[0])
        conversation.send = lambda *args: (_ for _ in ()).throw(OSError("unreachable"))
        self.speak(conversation, speech(30) + quiet(40))
        self.assertIn("say it again", self.notified.call_args.args[0])

    def test_a_failing_turn_never_kills_the_worker(self):
        import queue, threading
        conversation = self.conversation()
        conversation.work = queue.Queue()
        threading.Thread(target=conversation.worker, daemon=True).start()
        calls = []

        def flaky(*args):
            calls.append(1)
            if len(calls) == 1:
                raise AttributeError("boom")
            return self.heard

        conversation.transcribe = flaky
        self.speak(conversation, quiet(20) + speech(30) + quiet(40) + speech(30) + quiet(40))
        for _ in range(100):
            if self.sent:
                break
            __import__("time").sleep(0.02)
        self.assertEqual(len(self.sent), 1)

    def speak(self, conversation, frames):
        for frame in frames:
            self.clock[0] += converse.FRAME_MS / 1000
            conversation.feed(frame)

    def test_each_utterance_becomes_its_own_take(self):
        conversation = self.conversation()
        self.speak(conversation, quiet(20) + speech(30) + quiet(40) + speech(30) + quiet(40))
        self.assertEqual([text for text, _ in self.sent], [self.heard, self.heard])
        self.assertEqual(len({take for _, take in self.sent}), 2)
        self.assertTrue(all(take.startswith("converse-") for _, take in self.sent))

    def test_empty_transcript_sends_nothing(self):
        self.heard = ""
        conversation = self.conversation()
        self.speak(conversation, quiet(20) + speech(30) + quiet(40))
        self.assertEqual(self.sent, [])

    def test_on_speakers_the_reply_is_never_heard_as_input(self):
        conversation = self.conversation()
        conversation.speaking = lambda: True
        with mock.patch.object(converse.t3, "stop_speaking") as stop:
            self.speak(conversation, speech(60) + quiet(40))
        self.assertEqual(self.sent, [])
        stop.assert_not_called()

    def test_with_headphones_speaking_over_a_reply_stops_it(self):
        conversation = self.conversation(headphones=True)
        conversation.speaking = lambda: True
        with mock.patch.object(converse.t3, "stop_speaking") as stop:
            self.speak(conversation, quiet(20) + speech(30) + quiet(40))
        stop.assert_called_once()
        self.assertEqual([text for text, _ in self.sent], [self.heard])

    def test_transient_stt_failure_asks_to_repeat_and_refusal_stops(self):
        conversation = self.conversation()

        def broken(*args):
            raise converse.t3.McpError("stt-http-503")

        conversation.transcribe = broken
        self.speak(conversation, quiet(20) + speech(30) + quiet(40))
        self.assertEqual(self.sent, [])
        self.assertIn("say it again", self.notified.call_args.args[0])
        self.assertIsNone(conversation.refused)

        def refused(*args):
            raise converse.SttRefused("stt-http-402")

        conversation.transcribe = refused
        self.speak(conversation, speech(30) + quiet(40))
        self.assertEqual(conversation.refused, "stt-http-402")

    def test_malformed_stt_reply_loses_only_that_utterance(self):
        conversation = self.conversation()

        def malformed(*args):
            raise ValueError("Expecting value")

        conversation.transcribe = malformed
        self.speak(conversation, quiet(20) + speech(30) + quiet(40))
        conversation.transcribe = lambda *args: self.heard
        self.speak(conversation, speech(30) + quiet(40))
        self.assertEqual(len(self.sent), 1)

    def test_push_to_talk_speech_is_not_heard_twice(self):
        conversation = self.conversation()
        conversation.recording = lambda: True
        self.speak(conversation, quiet(20) + speech(30) + quiet(40))
        self.assertEqual(self.sent, [])

    def test_a_stale_player_pid_does_not_deafen_the_session(self):
        (converse.t3.runtime_dir() / "player.pid").write_text(str(os.getpid()))
        self.assertFalse(converse.speaking())

    def test_threaded_capture_keeps_listening_while_a_turn_is_processed(self):
        import threading
        gate = threading.Event()
        conversation = self.conversation()
        conversation.work = __import__("queue").Queue()
        threading.Thread(target=conversation.worker, daemon=True).start()
        entered = threading.Event()
        conversation.transcribe = lambda *args: (entered.set(), gate.wait(2), self.heard)[2]
        self.speak(conversation, quiet(20) + speech(30) + quiet(40))
        self.assertTrue(entered.wait(2))   # the worker is now busy with the first turn
        self.speak(conversation, speech(30) + quiet(40))
        self.assertEqual((conversation.work.qsize(), len(self.sent)), (1, 0))   # second captured meanwhile
        gate.set()
        for _ in range(100):
            if len(self.sent) == 2:
                break
            __import__("time").sleep(0.02)
        self.assertEqual(len(self.sent), 2)

    def test_idle_session_ends_but_not_while_a_reply_is_owed(self):
        conversation = self.conversation(idle_minutes=1)
        self.assertFalse(conversation.idle())
        conversation.pending = lambda: True
        self.clock[0] += 61
        self.assertFalse(conversation.idle())
        conversation.pending = lambda: False
        self.clock[0] += 61
        self.assertTrue(conversation.idle())


if __name__ == "__main__":
    unittest.main()
