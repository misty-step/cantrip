import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("cantrip_t3", Path(__file__).with_name("cantrip_t3.py"))
t3 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(t3)

THREAD = "mcp:home"
CONFIG = {**t3.DEFAULTS, "mcp_url": "https://t3.example/mcp", "thread_id": THREAD}


class FakeT3:
    """Stands in for the T3 MCP tools the integration uses."""

    def __init__(self):
        self.items = [{"position": 0, "type": "user_message", "runId": "run:0", "messageId": "m0"}]
        self.sends = []
        self.status = "running"
        self.completed_at = "2026-10-20T19:00:00.000Z"

    def call(self, name, arguments, timeout=None):
        if name == "t3_thread_read":
            after = arguments.get("afterPosition") or 0
            items = [item for item in self.items if item["position"] >= after]
            return {"thread": {"itemCount": len(self.items)}, "items": items[: arguments.get("limit", 100)],
                    "hasMore": False, "nextPosition": len(self.items)}
        if name == "t3_thread_send":
            self.sends.append(arguments)
            message = "message:mcp:client%3Ax:thread-send:" + arguments["clientRequestId"]
            if not any(item.get("messageId") == message for item in self.items):
                self.items.append({"position": len(self.items), "type": "user_message",
                                   "runId": "run:1", "messageId": message})
            return {"commandId": "c1"}
        if name == "t3_thread_wait":
            return {"run": {"runId": arguments["runId"], "status": self.status,
                            "completedAt": self.completed_at}}
        raise AssertionError(name)

    def reply(self, *texts):
        self.status = "completed"
        for text in texts:
            self.items.append({"position": len(self.items), "type": "assistant_message", "runId": "run:1",
                               "itemId": "i" + str(len(self.items)), "text": text})


class FakeSpeaker:
    def __init__(self):
        self.config = CONFIG
        self.said = []

    def say(self, text):
        self.said.append(text)


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        patch = mock.patch.dict(os.environ, {"XDG_STATE_HOME": self.tmp.name,
                                             "XDG_RUNTIME_DIR": self.tmp.name})
        patch.start()
        self.addCleanup(patch.stop)
        self.addCleanup(self.tmp.cleanup)
        self.t3 = FakeT3()
        self.speaker = FakeSpeaker()
        notify = mock.patch.object(t3, "notify")
        self.notified = notify.start()
        self.addCleanup(notify.stop)

    def records(self, kind):
        return [json.loads(p.read_text()) for p in (t3.state_dir() / kind).glob("*.json")]


class SendTest(Base):
    def test_send_queues_one_prefixed_message_idempotently_on_take(self):
        t3.send(CONFIG, "what are the agents doing?\n", "take-1", mcp=self.t3)
        t3.send(CONFIG, "what are the agents doing?\n", "take-1", mcp=self.t3)
        self.assertEqual(self.t3.sends[0], {"threadId": THREAD, "message": "[voice] what are the agents doing?",
                                            "mode": "queue", "clientRequestId": "cantrip-voice-take-1"})
        self.assertEqual(self.t3.sends[0], self.t3.sends[1])
        self.assertEqual(sum(item["type"] == "user_message" for item in self.t3.items), 2)
        self.assertEqual(len(self.records("pending")), 1)

    def test_state_never_holds_transcript(self):
        t3.send(CONFIG, "secret words here", "take-1", mcp=self.t3)
        stored = "".join(p.read_text() for p in t3.state_dir().rglob("*.json"))
        self.assertNotIn("secret", stored)

    def test_refuses_missing_take_and_empty_text(self):
        with self.assertRaises(ValueError):
            t3.send(CONFIG, "hi", "", mcp=self.t3)
        with self.assertRaises(ValueError):
            t3.send(CONFIG, "hi", "../x", mcp=self.t3)
        with self.assertRaises(ValueError):
            t3.send(CONFIG, "  \n", "take-1", mcp=self.t3)
        self.assertEqual(self.t3.sends, [])

    def test_already_answered_take_is_not_resent(self):
        t3.send(CONFIG, "hello", "take-1", mcp=self.t3)
        self.t3.reply("Hi.")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker)
        t3.send(CONFIG, "hello", "take-1", mcp=self.t3)
        self.assertEqual(len(self.t3.sends), 1)

    def test_main_send_exits_nonzero_on_t3_failure(self):
        with mock.patch.object(t3, "load_config", return_value=CONFIG), \
                mock.patch.object(t3, "connect", side_effect=t3.McpError("http-401")), \
                mock.patch.dict(os.environ, {"CANTRIP_TAKE_ID": "take-1"}), \
                mock.patch("sys.stdin", io.StringIO("hello")):
            self.assertEqual(t3.main(["send"]), 1)


class CollectTest(Base):
    def test_waits_then_speaks_final_reply_once(self):
        t3.send(CONFIG, "what are the agents doing?", "take-1", mcp=self.t3)
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker)
        self.assertEqual(self.speaker.said, [])
        self.assertEqual(self.records("pending")[0]["run_id"], "run:1")
        self.t3.reply("Working on it.", "Three agents are running.")
        finished = t3.datetime.fromisoformat("2026-10-20T19:00:01+00:00").timestamp()
        for _ in range(2):
            t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=lambda: finished)
        self.assertEqual(self.speaker.said, ["Three agents are running."])
        done = self.records("done")[0]
        self.assertEqual((done["outcome"], done["speech"], done["reply_chars"]), ("reply", "spoken", 25))
        self.assertEqual(self.records("pending"), [])

    def test_muted_reply_is_recorded_not_spoken(self):
        t3.set_mute("on")
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("All quiet.")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker)
        self.assertEqual(self.speaker.said, [])
        self.assertEqual(self.records("done")[0]["speech"], "muted")
        self.assertTrue(self.notified.called)
        t3.set_mute("toggle")
        self.assertFalse(t3.is_muted())

    def test_repeat_refetches_last_reply_unless_muted(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("All quiet.")
        t3.set_mute("on")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker)
        self.assertTrue(t3.repeat(CONFIG, mcp=self.t3, speaker=self.speaker))
        self.assertEqual(self.speaker.said, [])
        t3.set_mute("off")
        t3.repeat(CONFIG, mcp=self.t3, speaker=self.speaker)
        self.assertEqual(self.speaker.said, ["All quiet."])

    def test_reply_finished_long_ago_is_not_spoken(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("Old news.")
        finished = t3.datetime.fromisoformat("2026-10-20T19:00:00+00:00").timestamp()
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker,
                   now=lambda: finished + t3.STALE_SECONDS + 1)
        self.assertEqual(self.speaker.said, [])
        self.assertEqual(self.records("done")[0]["speech"], "stale")

    def test_failed_run_is_announced(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.status = "failed"
        self.t3.completed_at = None
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker)
        self.assertEqual(self.speaker.said, ["Home could not finish that request."])

    def test_transport_error_keeps_take_pending(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        broken = mock.Mock(call=mock.Mock(side_effect=t3.McpError("http-502")))
        t3.collect(CONFIG, once=True, mcp=broken, speaker=self.speaker)
        self.assertEqual(len(self.records("pending")), 1)


class SpeechTest(Base):
    def test_markdown_becomes_plain_speech(self):
        spoken = t3.speakable("## Status\n\n- **Overmind**: design lane [open](t3-thread://v1/abc)\n"
                              "- `cantrip` PR at https://github.com/x/y/pull/1\n\n```sh\nls\n```\n")
        self.assertEqual(spoken, "Status. Overmind: design lane open. cantrip PR at a link. (code omitted).")

    def test_long_text_is_capped_and_chunked(self):
        text = t3.speakable("This is a sentence. " * 1000)
        self.assertLess(len(text), t3.SPOKEN_LIMIT + 40)
        self.assertTrue(text.endswith("The rest is in the thread."))
        parts = list(t3.chunks(text))
        self.assertTrue(all(len(part) <= t3.CHUNK_LIMIT for part in parts))
        self.assertLessEqual(len(parts[0]), t3.FIRST_CHUNK_LIMIT)
        self.assertGreater(len(parts[1]), t3.FIRST_CHUNK_LIMIT)
        self.assertEqual(" ".join(parts), text)

    def test_stalled_remote_speech_falls_back_to_local(self):
        def opener(request, timeout):
            raise TimeoutError()

        speaker = t3.Speaker(CONFIG, opener=opener, recording=lambda: False)
        with mock.patch.object(speaker, "player"), mock.patch.object(speaker, "release"), \
                mock.patch.object(speaker, "local") as local, \
                mock.patch.dict(os.environ, {"CANTRIP_T3_SPEECH_KEY": "k"}):
            speaker.say("Hello there.")
        self.assertEqual(local.call_args.args[0], "Hello there.")

    def test_stop_marks_current_speech_stopped(self):
        speaker = t3.Speaker(CONFIG, recording=lambda: False)
        started = __import__("time").time()
        self.assertFalse(speaker.stopped(started))
        t3.stop_speaking()
        self.assertTrue(speaker.stopped(started))

    def test_remote_speech_streams_pcm_until_stopped(self):
        written = []

        class Player:
            stdin = mock.Mock(write=written.append)
            pid = 1

            def wait(self):
                return 0

            def poll(self):
                return 0

        class Response(io.BytesIO):
            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

        requests = []

        def opener(request, timeout):
            requests.append(json.loads(request.data))
            return Response(b"\x00\x01" * 10)

        speaker = t3.Speaker(CONFIG, opener=opener, recording=lambda: False)
        with mock.patch.object(t3.subprocess, "Popen", return_value=Player()), \
                mock.patch.dict(os.environ, {"CANTRIP_T3_SPEECH_KEY": "k"}):
            speaker.say("Hello there. All good.")
        self.assertEqual(requests, [{"model": "microsoft/mai-voice-2.1-flash",
                                     "voice": "en-US-Iris:MAI-Voice-2.1-Flash",
                                     "input": "Hello there. All good.", "response_format": "pcm"}])
        self.assertEqual(b"".join(written), b"\x00\x01" * 10)


if __name__ == "__main__":
    unittest.main()
