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
    """Stands in for the T3 MCP tools, with the response shapes the real server returns."""

    def __init__(self):
        self.items = [{"position": 0, "type": "user_message", "runId": "run:0", "messageId": "m0"}]
        self.sends = []
        self.status = "running"
        self.completed_at = "2026-10-20T19:00:00.000Z"
        self.refuse = {}

    def call(self, name, arguments, timeout=None):
        if name in self.refuse:
            raise t3.McpError(self.refuse[name])
        if name == "t3_thread_read":
            after = arguments.get("afterPosition")
            items = [item for item in self.items if after is None or item["position"] > after]
            runs = [{"runId": "run:1", "status": self.status,
                     "completedAt": self.completed_at if self.status in t3.TERMINAL else None}]
            return {"thread": {"itemCount": len(self.items)}, "recentRuns": runs,
                    "items": items[: arguments.get("limit", 100)], "hasMore": False,
                    "nextPosition": len(self.items)}
        if name == "t3_thread_send":
            self.sends.append(arguments)
            message = "message:mcp:client%3Ax:thread-send:" + arguments["clientRequestId"]
            if not any(item.get("messageId") == message for item in self.items):
                self.items.append({"position": len(self.items), "type": "user_message",
                                   "runId": "run:1", "messageId": message})
            return {"threadId": arguments["threadId"], "runId": "run:1", "messageId": message}
        if name == "t3_thread_wait":
            return {"threadId": arguments["threadId"], "runId": arguments["runId"],
                    "status": self.status, "timedOut": self.status not in t3.TERMINAL}
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
        self.plays = True

    def say(self, text):
        self.said.append(text)
        return "spoken" if self.plays else "unplayable"


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

    @staticmethod
    def finished():
        return t3.datetime.fromisoformat("2026-10-20T19:00:01+00:00").timestamp()

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
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=self.finished)
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
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=self.finished)
        self.assertEqual(self.speaker.said, [])
        self.assertEqual(self.records("done")[0]["speech"], "muted")
        self.assertTrue(self.notified.called)
        t3.set_mute("toggle")
        self.assertFalse(t3.is_muted())

    def test_repeat_refetches_last_reply_unless_muted(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("All quiet.")
        t3.set_mute("on")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=self.finished)
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
        finished = t3.datetime.fromisoformat("2026-10-20T19:00:00+00:00").timestamp()
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=lambda: finished)
        self.assertEqual(self.speaker.said, ["Home could not finish that request."])

    def test_transport_error_keeps_take_pending(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        broken = mock.Mock(call=mock.Mock(side_effect=t3.McpError("http-502")))
        t3.collect(CONFIG, once=True, mcp=broken, speaker=self.speaker)
        self.assertEqual(len(self.records("pending")), 1)


class ReviewRegressionTest(Base):
    """One test per finding of the review at b9b56c0."""

    def test_stale_rule_uses_the_runs_real_completion_time(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("Old news.")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker,
                   now=lambda: self.finished() + 7200)
        self.assertEqual(self.speaker.said, [])
        self.assertEqual(self.records("done")[0]["speech"], "stale")

    def test_a_refused_record_does_not_block_later_takes(self):
        write = t3.write_json
        write(t3.state_dir() / "pending" / "old.json", {"take": "old", "thread_id": "mcp:gone",
              "request_id": "cantrip-voice-old", "run_id": "run:gone", "sent_at": 1, "status": "sent"})
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("Fine.")
        real = self.t3.call

        def call(name, arguments, timeout=None):
            if arguments.get("threadId") == "mcp:gone":
                raise t3.McpError("tool-thread_not_found")
            return real(name, arguments, timeout)

        self.t3.call = call
        for _ in range(t3.TOOL_FAILURES):
            t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=lambda: 1000)
        self.assertEqual(self.speaker.said, ["Fine."])
        outcomes = sorted(record["outcome"] for record in self.records("done"))
        self.assertEqual(outcomes, ["error", "reply"])
        self.assertEqual(self.records("pending"), [])

    def test_unauthorized_collector_notifies_and_exits_for_restart(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.refuse["t3_thread_wait"] = "http-401"
        with self.assertRaises(SystemExit) as stopped:
            t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker)
        self.assertEqual(stopped.exception.code, t3.AUTH_EXIT)
        self.assertTrue(self.notified.called)
        self.assertEqual(len(self.records("pending")), 1)

    def test_mute_or_stop_while_held_for_recording_silences_the_reply(self):
        for action in ("mute", "stop"):
            recording = iter([True, True, False, False])
            speaker = t3.Speaker(CONFIG, recording=lambda: next(recording, False))
            remote = mock.patch.object(speaker, "remote", return_value=1)
            local = mock.patch.object(speaker, "local")
            with remote as played, local as spoke, mock.patch.object(t3.time, "sleep", side_effect=lambda _: (
                    t3.set_mute("on") if action == "mute" else t3.stop_speaking())), \
                    mock.patch.dict(os.environ, {"CANTRIP_T3_SPEECH_KEY": "k"}):
                speaker.say("Here is the answer.")
            self.assertFalse(played.called or spoke.called, action)
            t3.set_mute("off")

    def test_pending_is_written_before_t3_so_a_killed_handoff_is_collected(self):
        real = self.t3.call
        seen = []

        def call(name, arguments, timeout=None):
            if name == "t3_thread_send":
                seen.append([p.name for p in (t3.state_dir() / "pending").glob("*.json")])
            return real(name, arguments, timeout)

        self.t3.call = call
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.assertEqual(seen, [["take-1.json"]])

    def test_uncertain_send_is_found_later_and_definite_refusal_is_dropped(self):
        self.t3.refuse["t3_thread_send"] = "tool-capability_denied"
        with self.assertRaises(t3.McpError):
            t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.assertEqual(self.records("pending"), [])
        self.assertEqual(json.loads((t3.state_dir() / "last-error.json").read_text())["code"],
                         "tool-capability_denied")
        del self.t3.refuse["t3_thread_send"]
        t3.write_json(t3.state_dir() / "pending" / "take-2.json", {
            "take": "take-2", "thread_id": THREAD, "request_id": "cantrip-voice-take-2",
            "sent_at": 0, "status": "sending"})
        clock = [t3.SENDING_GRACE_SECONDS + 1]
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=lambda: clock[0])
        self.assertEqual(self.records("pending")[0]["run_id"], None)   # still queued, not given up
        self.t3.items.append({"position": len(self.t3.items), "type": "user_message", "runId": "run:1",
                              "messageId": "message:mcp:client%3Ax:thread-send:cantrip-voice-take-2"})
        self.t3.reply("Found it.")
        clock[0] += t3.LOCATE_SECONDS
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=lambda: clock[0])
        self.assertEqual(self.speaker.said, ["Found it."])
        t3.write_json(t3.state_dir() / "pending" / "take-3.json", {
            "take": "take-3", "thread_id": THREAD, "request_id": "cantrip-voice-take-3",
            "sent_at": 0, "status": "sending"})
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker,
                   now=lambda: t3.EXPIRE_SECONDS + 1)
        self.assertIn("expired", [record["outcome"] for record in self.records("done")])

    def test_retry_racing_completion_does_not_resurrect_the_take(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("Done.")
        real = self.t3.call

        def call(name, arguments, timeout=None):
            result = real(name, arguments, timeout)
            if name == "t3_thread_send":
                t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=self.finished)
            return result

        (t3.state_dir() / "done" / "take-1.json").unlink(missing_ok=True)
        self.t3.call = call
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.assertEqual(self.records("pending"), [])
        self.assertEqual(self.speaker.said, ["Done."])

    def test_unplayable_reply_is_recorded_and_notified(self):
        self.speaker.plays = False
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("Fine.")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=self.finished)
        self.assertEqual(self.records("done")[0]["speech"], "unplayable")
        self.assertIn("could not be played", self.notified.call_args.args[0])

    def test_missing_players_fall_back_then_report_failure(self):
        speaker = t3.Speaker(CONFIG, opener=mock.Mock(side_effect=OSError()), recording=lambda: False)
        with mock.patch.object(t3.subprocess, "Popen", side_effect=FileNotFoundError()), \
                mock.patch.dict(os.environ, {"CANTRIP_T3_SPEECH_KEY": "k"}):
            self.assertEqual(speaker.say("Hello."), "unplayable")

    def test_stop_never_signals_a_reused_pid(self):
        (t3.runtime_dir() / "player.pid").write_text(str(os.getpid()))
        with mock.patch.object(t3.os, "killpg") as killpg:
            self.assertFalse(t3.stop_speaking())
        killpg.assert_not_called()
        self.assertFalse((t3.runtime_dir() / "player.pid").exists())

    def test_waiting_run_is_surfaced_once(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.status = "waiting"
        for _ in range(2):
            t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker)
        self.assertEqual(self.notified.call_count, 1)
        self.assertIn("waiting", self.notified.call_args.args[0])

    def test_collector_never_regresses_a_record_send_just_acknowledged(self):
        path = t3.state_dir() / "pending" / "take-1.json"
        t3.write_json(path, {"take": "take-1", "thread_id": THREAD, "request_id": "cantrip-voice-take-1",
                             "sent_at": 0, "status": "sending"})
        stale_copy = json.loads(path.read_text())
        t3.write_json(path, {**stale_copy, "status": "sent", "run_id": "run:1", "message_id": "m"})
        t3.save(path, {**stale_copy, "located_at": 5})
        saved = json.loads(path.read_text())
        self.assertEqual((saved["status"], saved["run_id"]), ("sent", "run:1"))

    def test_a_fresh_sending_record_is_left_alone(self):
        t3.write_json(t3.state_dir() / "pending" / "take-1.json", {
            "take": "take-1", "thread_id": THREAD, "request_id": "cantrip-voice-take-1",
            "sent_at": 100, "status": "sending"})
        calls = mock.Mock(side_effect=AssertionError("no T3 call during the grace period"))
        t3.collect(CONFIG, once=True, mcp=mock.Mock(call=calls), speaker=self.speaker, now=lambda: 110)
        calls.assert_not_called()

    def test_refused_retry_keeps_an_earlier_attempts_record(self):
        t3.write_json(t3.state_dir() / "pending" / "take-1.json", {
            "take": "take-1", "thread_id": THREAD, "request_id": "cantrip-voice-take-1",
            "sent_at": 0, "status": "sending"})
        self.t3.refuse["t3_thread_send"] = "http-401"
        with self.assertRaises(t3.McpError):
            t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.assertEqual(len(self.records("pending")), 1)

    def test_reply_silenced_while_held_is_recorded_and_notified(self):
        self.speaker.say = lambda text: "muted"
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("Fine.")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=self.finished)
        self.assertEqual(self.records("done")[0]["speech"], "muted")
        self.assertIn("replied (muted)", self.notified.call_args.args[0])

    def test_repeat_finds_the_reply_by_position_after_later_runs(self):
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("The first answer.")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=self.finished)
        for n in range(t3.RECENT_ITEMS + 50):
            self.t3.items.append({"position": len(self.t3.items), "type": "assistant_message",
                                  "runId": "run:later", "itemId": "x" + str(n), "text": "later"})
        self.speaker.said.clear()
        self.assertTrue(t3.repeat(CONFIG, mcp=self.t3, speaker=self.speaker))
        self.assertEqual(self.speaker.said, ["The first answer."])

    def test_missing_player_costs_no_speech_request(self):
        opener = mock.Mock(side_effect=AssertionError("speech requested without a player"))
        speaker = t3.Speaker(CONFIG, opener=opener, recording=lambda: False)
        with mock.patch.object(t3.subprocess, "Popen", side_effect=FileNotFoundError()), \
                mock.patch.dict(os.environ, {"CANTRIP_T3_SPEECH_KEY": "k"}):
            speaker.say("Hello.")
        opener.assert_not_called()

    def test_pcm_is_flushed_per_block(self):
        flushed = []
        player = mock.Mock(pid=1, poll=mock.Mock(return_value=0))
        player.stdin.write = lambda block: flushed.append(("write", len(block)))
        player.stdin.flush = lambda: flushed.append(("flush",))

        class Response(io.BytesIO):
            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

        speaker = t3.Speaker(CONFIG, opener=lambda request, timeout: Response(b"\x00" * 20000),
                             recording=lambda: False)
        with mock.patch.object(t3.subprocess, "Popen", return_value=player), \
                mock.patch.dict(os.environ, {"CANTRIP_T3_SPEECH_KEY": "k"}):
            speaker.say("Hello.")
        writes = [i for i, entry in enumerate(flushed) if entry[0] == "write"]
        self.assertTrue(writes and all(flushed[i + 1] == ("flush",) for i in writes))

    def test_damaged_records_are_moved_aside_not_crash_looped(self):
        pending = t3.state_dir() / "pending"
        (pending / "garbage.json").write_text("{not json")
        (pending / "list.json").write_text("[1, 2]")
        (pending / "partial.json").write_text('{"take": "x"}')
        (t3.state_dir() / "done" / "bad.json").write_text("[]")
        t3.send(CONFIG, "status?", "take-1", mcp=self.t3)
        self.t3.reply("Fine.")
        t3.collect(CONFIG, once=True, mcp=self.t3, speaker=self.speaker, now=self.finished)
        self.assertEqual(self.speaker.said, ["Fine."])
        self.assertEqual(sorted(p.name for p in (t3.state_dir() / "damaged").iterdir()),
                         ["garbage.json", "list.json", "partial.json"])
        self.assertTrue(t3.repeat(CONFIG, mcp=self.t3, speaker=self.speaker))

    def test_secrets_use_the_first_line_and_never_reach_logs(self):
        with mock.patch.dict(os.environ, {"T3_MCP_TOKEN": "tok-123\nlogin: me\n"}):
            self.assertEqual(t3.secret("T3_MCP_TOKEN"), "tok-123")
        error = ValueError("Invalid header value b'Bearer tok-123\\nlogin'")
        self.assertEqual(t3.describe(error), "ValueError")
        self.assertEqual(t3.describe(t3.McpError("http-401")), "http-401")

    def test_unreachable_t3_drops_the_record_it_created(self):
        with mock.patch.object(t3, "connect", side_effect=OSError("network down")), \
                mock.patch.dict(os.environ, {"T3_MCP_TOKEN": "tok"}):
            with self.assertRaises(OSError):
                t3.send(CONFIG, "status?", "take-1")
        self.assertEqual(self.records("pending"), [])

    def test_missing_token_leaves_no_record(self):
        with mock.patch.dict(os.environ, {"T3_MCP_TOKEN": ""}):
            with self.assertRaises(ValueError):
                t3.send(CONFIG, "status?", "take-1")
        self.assertEqual(self.records("pending"), [])

    def test_speech_cut_short_is_recorded_as_stopped(self):
        speaker = t3.Speaker(CONFIG, recording=lambda: False)

        def remote(parts, key, started):
            t3.stop_speaking()
            return 1

        with mock.patch.object(speaker, "remote", side_effect=remote), \
                mock.patch.dict(os.environ, {"CANTRIP_T3_SPEECH_KEY": "k"}):
            self.assertEqual(speaker.say("Hello there."), "stopped")

    def test_redirects_are_refused(self):
        handler = t3.NoRedirect()
        self.assertIsNone(handler.redirect_request(None, None, 302, "Found", {}, "https://elsewhere.example/"))

    def test_identifiers_keep_their_underscores(self):
        self.assertEqual(t3.speakable("Run foo_bar_baz with **care** and _style_."),
                         "Run foo_bar_baz with care and style.")


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
