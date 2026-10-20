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


class EndpointerTest(unittest.TestCase):
    def test_speech_then_silence_makes_one_utterance(self):
        endpointer = converse.Endpointer(converse.CONVERSE_DEFAULTS)
        events = [endpointer.feed(f) for f in quiet(20) + tone(6000, 40) + quiet(30)]
        kinds = [e[0] for e in events if e]
        self.assertEqual(kinds, ["start", "utterance"])
        pcm = next(e[1] for e in events if e and e[0] == "utterance")
        self.assertGreater(len(pcm), 40 * converse.FRAME_BYTES)

    def test_a_click_is_not_an_utterance(self):
        endpointer = converse.Endpointer(converse.CONVERSE_DEFAULTS)
        events = [endpointer.feed(f) for f in quiet(20) + tone(6000, 3) + quiet(40)]
        self.assertEqual([e for e in events if e], [])

    def test_long_speech_is_cut_at_the_maximum(self):
        settings = {**converse.CONVERSE_DEFAULTS, "max_utterance_s": 1}
        endpointer = converse.Endpointer(settings)
        events = [endpointer.feed(f) for f in tone(6000, 80)]
        self.assertEqual(sum(1 for e in events if e and e[0] == "utterance"), 2)


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
        return conversation

    def speak(self, conversation, frames):
        for frame in frames:
            self.clock[0] += converse.FRAME_MS / 1000
            conversation.feed(frame)

    def test_each_utterance_becomes_its_own_take(self):
        conversation = self.conversation()
        self.speak(conversation, quiet(20) + tone(6000, 30) + quiet(30) + tone(6000, 30) + quiet(30))
        self.assertEqual([text for text, _ in self.sent], [self.heard, self.heard])
        self.assertEqual(len({take for _, take in self.sent}), 2)
        self.assertTrue(all(take.startswith("converse-") for _, take in self.sent))

    def test_empty_transcript_sends_nothing(self):
        self.heard = ""
        conversation = self.conversation()
        self.speak(conversation, quiet(20) + tone(6000, 30) + quiet(30))
        self.assertEqual(self.sent, [])

    def test_on_speakers_the_reply_is_never_heard_as_input(self):
        conversation = self.conversation()
        conversation.speaking = lambda: True
        with mock.patch.object(converse.t3, "stop_speaking") as stop:
            self.speak(conversation, tone(6000, 60) + quiet(30))
        self.assertEqual(self.sent, [])
        stop.assert_not_called()

    def test_with_headphones_speaking_over_a_reply_stops_it(self):
        conversation = self.conversation(headphones=True)
        conversation.speaking = lambda: True
        with mock.patch.object(converse.t3, "stop_speaking") as stop:
            self.speak(conversation, quiet(20) + tone(6000, 30) + quiet(30))
        stop.assert_called_once()
        self.assertEqual([text for text, _ in self.sent], [self.heard])

    def test_stt_failure_asks_to_repeat(self):
        conversation = self.conversation()

        def broken(*args):
            raise converse.t3.McpError("stt-http-402")

        conversation.transcribe = broken
        self.speak(conversation, quiet(20) + tone(6000, 30) + quiet(30))
        self.assertEqual(self.sent, [])
        self.assertIn("say it again", self.notified.call_args.args[0])

    def test_idle_session_ends(self):
        conversation = self.conversation(idle_minutes=1)
        self.assertFalse(conversation.idle())
        self.clock[0] += 61
        self.assertTrue(conversation.idle())


if __name__ == "__main__":
    unittest.main()
