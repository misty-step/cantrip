import asyncio
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest import mock

here = Path(__file__).resolve().parent
sys.path.insert(0, str(here))
spec = importlib.util.spec_from_file_location("cantrip_t3_live", here / "cantrip_t3_live.py")
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)
from test_cantrip_t3 import FakeT3  # noqa: E402

CONFIG = {**live.t3.DEFAULTS, "mcp_url": "https://t3.example/mcp", "thread_id": "mcp:home"}


def heard(start, end, text):
    return {"type": "session.input_transcript.delta", "start_ms": start, "end_ms": end, "delta": text}


def said(start, end, text):
    return {"type": "session.output_transcript.delta", "start_ms": start, "end_ms": end, "delta": text}


def delegation(item, offset):
    return {"type": "session.delegation.created", "offset_ms": offset,
            "delegation": {"id": item, "type": "delegation", "target": "client"}}


class BridgeTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        settle = mock.patch.multiple(live, SETTLE_SECONDS=0.05, SETTLE_MAX_SECONDS=0.5,
                                     SETTLE_UNCOVERED_SECONDS=0.3)
        settle.start()
        self.addCleanup(settle.stop)
        self.sent = []
        self.submitted = []
        self.answer = ("completed", "## Status\n\nThree agents are running. [PR](https://x/y)")

        async def send(event):
            self.sent.append(event)

        def submit(delegation_id, message):
            self.submitted.append((delegation_id, message))
            return {"delegation": delegation_id}

        def collect(record, progress):
            return self.answer

        self.bridge = live.Bridge(CONFIG, send, submit, collect)

    async def finish(self):
        await asyncio.gather(*self.bridge.tasks)

    def commentary(self):
        return [event["content"] for event in self.sent if event["type"] == "session.commentary.append"]

    async def test_delegation_sends_its_speech_and_returns_commentary(self):
        await self.bridge.handle(heard(0, 400, "What are"))
        await self.bridge.handle(heard(400, 900, " the agents doing?"))
        await self.bridge.handle(delegation("item_1", 1000))
        await self.finish()
        self.assertEqual(self.submitted, [("item_1", "[voice live] What are the agents doing?")])
        kinds = [(event["type"], event["delegation_id"]) for event in self.sent]
        self.assertEqual(kinds, [("session.thinking.append", "item_1"), ("session.commentary.append", "item_1")])
        self.assertEqual(self.commentary(), ["Status. Three agents are running. PR."])

    async def test_close_delegations_each_get_their_own_speech(self):
        await self.bridge.handle(heard(0, 500, "Pause Tach."))
        await self.bridge.handle(delegation("item_1", 600))
        await self.bridge.handle(heard(700, 1400, "And ship the docs."))
        await self.bridge.handle(delegation("item_2", 1500))
        await self.finish()
        self.assertEqual(sorted(self.submitted), [("item_1", "[voice live] Pause Tach."),
                                                  ("item_2", "[voice live] And ship the docs.")])

    async def test_a_late_fragment_joins_its_own_request(self):
        await self.bridge.handle(heard(0, 400, "Merge the"))
        await self.bridge.handle(delegation("item_1", 900))
        await asyncio.sleep(0.02)
        await self.bridge.handle(heard(400, 900, " docs PR."))   # transcribed after the delegation
        await self.finish()
        self.assertEqual(self.submitted, [("item_1", "[voice live] Merge the docs PR.")])

    async def test_a_transcript_arriving_after_the_delegation_is_still_its_request(self):
        await self.bridge.handle(delegation("item_1", 1500))
        await asyncio.sleep(0.12)
        await self.bridge.handle(heard(0, 700, "Merge the"))
        await asyncio.sleep(0.12)
        await self.bridge.handle(heard(700, 1400, " docs PR."))   # well after the first, still before the point
        await self.finish()
        self.assertEqual(self.submitted, [("item_1", "[voice live] Merge the docs PR.")])

    async def test_a_straggler_joins_the_next_request_instead_of_vanishing(self):
        await self.bridge.handle(heard(0, 400, "Merge the"))
        await self.bridge.handle(heard(400, 850, " docs PR."))
        await self.bridge.handle(delegation("item_1", 900))
        await self.finish()
        await self.bridge.handle(heard(850, 880, " Now."))     # arrives after item_1 was sent
        await self.bridge.handle(heard(1000, 1400, " And ship it."))
        await self.bridge.handle(delegation("item_2", 1500))
        await self.finish()
        self.assertEqual([m for _, m in self.submitted],
                         ["[voice live] Merge the docs PR.", "[voice live] Now. And ship it."])

    async def test_a_repeated_delegation_is_not_sent_twice(self):
        await self.bridge.handle(heard(0, 400, "Ship it."))
        await self.bridge.handle(delegation("item_1", 500))
        await self.bridge.handle(delegation("item_2", 500))
        await self.finish()
        self.assertEqual(len(self.submitted), 1)
        self.assertIn("already", self.commentary()[-1] + self.commentary()[0])

    async def test_a_short_reply_carries_the_question_it_answers(self):
        await self.bridge.handle(said(0, 2000, "Should I merge the docs PR now?"))
        await self.bridge.handle(heard(2500, 2800, "Yes."))
        await self.bridge.handle(delegation("item_1", 2900))
        await self.finish()
        message = self.submitted[0][1]
        self.assertTrue(message.startswith("[voice live] Yes."))
        self.assertIn("Should I merge the docs PR now?", message)

    async def test_empty_request_is_not_sent(self):
        await self.bridge.handle(delegation("item_1", 100))
        await self.finish()
        self.assertEqual(self.submitted, [])
        self.assertIn("didn't catch", self.commentary()[0])

    async def test_failures_are_reported_by_what_may_have_happened(self):
        cases = [
            (live.t3.McpError("tool-capability_denied"), None, "Nothing was sent"),
            (TimeoutError(), None, "not sure"),
            (None, live.t3.McpError("http-502"), "has the request"),
            (None, AttributeError("surprise"), "has the request"),
        ]
        for n, (send_error, wait_error, expected) in enumerate(cases):
            def submit(delegation_id, message, error=send_error):
                if error:
                    raise error
                return {}

            def collect(record, progress, error=wait_error):
                raise error

            self.bridge.submit, self.bridge.collect = submit, collect
            await self.bridge.handle(heard(n * 1000, n * 1000 + 400, "Ship it."))
            await self.bridge.handle(delegation("item_%d" % n, n * 1000 + 500))
            await self.finish()
            self.assertIn(expected, self.commentary()[-1], (send_error, wait_error))

    async def test_an_unexpected_error_still_answers_gpt_live(self):
        def submit(*args):
            raise KeyError("surprise")

        self.bridge.submit = submit
        await self.bridge.handle(heard(0, 400, "Ship it."))
        await self.bridge.handle(delegation("item_1", 500))
        await self.finish()
        self.assertIn("not sure", self.commentary()[-1])

    async def test_failed_run_is_reported(self):
        self.answer = ("failed", "")
        await self.bridge.handle(heard(0, 500, "Ship it."))
        await self.bridge.handle(delegation("item_1", 600))
        await self.finish()
        self.assertIn("couldn't finish", self.commentary()[-1])

    async def test_long_reply_is_split_into_append_sized_pieces(self):
        self.answer = ("completed", "One long sentence here. " * 300)
        await self.bridge.handle(heard(0, 500, "Tell me everything."))
        await self.bridge.handle(delegation("item_1", 600))
        await self.finish()
        self.assertGreater(len(self.commentary()), 1)
        self.assertTrue(all(len(piece) <= live.APPEND_CHARS for piece in self.commentary()))

    async def test_close_records_usage(self):
        await self.bridge.handle({"type": "session.usage.updated", "usage": {"seconds": 12}})
        await self.bridge.handle({"type": "session.closed", "reason": "close_requested", "usage": {"seconds": 15}})
        self.assertTrue(self.bridge.closed.is_set())
        self.assertEqual(self.bridge.usage_seconds, 15)

    async def test_closing_never_waits_for_a_long_run(self):
        def slow(record, progress):
            time.sleep(3)
            return ("completed", "late")

        self.bridge.collect = slow
        await self.bridge.handle(heard(0, 400, "Ship it."))
        await self.bridge.handle(delegation("item_1", 500))
        await asyncio.sleep(0.4)
        began = time.monotonic()
        for task in list(self.bridge.tasks):
            task.cancel()
        await asyncio.gather(*self.bridge.tasks, return_exceptions=True)
        self.assertLess(time.monotonic() - began, 0.5)


class HomeCallsTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        patch = mock.patch.dict(os.environ, {"XDG_STATE_HOME": self.tmp.name, "XDG_RUNTIME_DIR": self.tmp.name})
        patch.start()
        self.addCleanup(patch.stop)
        self.addCleanup(self.tmp.cleanup)

    def test_request_is_idempotent_on_delegation_and_returns_final_reply(self):
        fake = FakeT3()
        fake.reply("Working.", "All quiet.")
        record = live.submit_request(CONFIG, fake, "item_1", "[voice live] status?")
        live.submit_request(CONFIG, fake, "item_1", "[voice live] status?")
        self.assertEqual(live.collect_reply(fake, record, lambda note: None), ("completed", "All quiet."))
        self.assertEqual(fake.sends[0], {"threadId": "mcp:home", "message": "[voice live] status?",
                                         "mode": "queue", "clientRequestId": "cantrip-live-item_1"})
        self.assertEqual(fake.sends[0], fake.sends[1])

    def test_waiting_behind_other_work_does_not_spin(self):
        fake = FakeT3()
        calls, sleeps = [], []
        real = fake.call

        def call(name, arguments, timeout=None):
            calls.append(name)
            if len(calls) > 6:
                fake.reply("Done.")
                fake.items.append({"position": len(fake.items), "type": "user_message", "runId": "run:1",
                                   "messageId": "message:mcp:client%3Ax:thread-send:cantrip-live-q"})
            return real(name, arguments, timeout)

        fake.call = call
        record = {"thread_id": "mcp:home", "request_id": "cantrip-live-q", "status": "sent", "run_id": None}
        with mock.patch.object(live.t3, "LOCATE_SECONDS", 0):
            status, reply = live.collect_reply(fake, record, lambda note: None, sleep=sleeps.append)
        self.assertEqual((status, reply), ("completed", "Done."))
        self.assertTrue(sleeps)
        self.assertLess(len(calls), 50)

    def test_mute_never_signals_a_stale_pid(self):
        import subprocess
        other = subprocess.Popen(["sleep", "5"])
        self.addCleanup(other.kill)
        (live.t3.runtime_dir() / "live.pid").write_text(str(other.pid))
        self.assertIsNone(live.live_pid())


class PlayerTest(unittest.TestCase):
    def test_turns_start_only_after_the_jitter_buffer_fills(self):
        writes, clock = [], [0.0]
        with mock.patch.object(live.subprocess, "Popen"):
            player = live.Player(clock=lambda: clock[0])
        player.write = lambda pcm: writes.append((clock[0], len(pcm)))
        block = bytes(live.RATE * 2 // 10)   # 100 ms
        player.start()
        for n in range(6):
            clock[0] = n * 0.1
            player.audio.put(block)
            time.sleep(0.06)
        player.audio.put(None)
        player.join(2)
        self.assertGreaterEqual(writes[0][0], live.PREBUFFER_SECONDS - 0.11)   # held until ~0.4 s queued
        self.assertGreaterEqual(writes[0][1], len(block) * 4)
        self.assertEqual(sum(size for _, size in writes), len(block) * 6)


class SessionStateTest(unittest.TestCase):
    def test_state_file_follows_the_session_and_is_removed(self):
        with tempfile.TemporaryDirectory() as run, mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": run}):
            state = live.SessionState()
            state.update("listening", False)
            first = state.path.stat().st_mtime_ns
            state.update("listening", False)   # unchanged: not rewritten
            self.assertEqual(state.path.stat().st_mtime_ns, first)
            state.update("speaking", True)
            import json
            self.assertEqual(json.loads(state.path.read_text()),
                             {"mode": "live", "pid": os.getpid(), "phase": "speaking", "muted": True})
            state.clear()
            self.assertFalse(state.path.exists())


class LimitsTest(unittest.TestCase):
    def test_bad_limits_fall_back_to_defaults(self):
        self.assertEqual(live.limits({"idle_minutes": "x", "max_minutes": 0}), (300, 3600))
        self.assertEqual(live.limits({"idle_minutes": 2, "max_minutes": 30}), (120, 1800))


class SessionTest(unittest.TestCase):
    def test_client_delegation_session_seeds_summary_as_developer_context(self):
        session = live.session_config(CONFIG, "NOW: three lanes.")
        self.assertEqual(session["delegation"], {"type": "client"})
        self.assertEqual(session["audio"]["format"], {"type": "audio/pcm", "rate": 24000})
        self.assertEqual(session["input"][0]["role"], "developer")
        self.assertIn("NOW: three lanes.", session["input"][0]["content"][0]["text"])
        self.assertNotIn("input", live.session_config(CONFIG, ""))


if __name__ == "__main__":
    unittest.main()
