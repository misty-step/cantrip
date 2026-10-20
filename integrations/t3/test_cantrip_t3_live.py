import asyncio
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
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


class BridgeTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.sent = []
        self.asked = []
        self.answer = ("completed", "## Status\n\nThree agents are running. [PR](https://x/y)")
        live.SETTLE_SECONDS = 0

        async def send(event):
            self.sent.append(event)

        def ask(delegation_id, text, progress):
            self.asked.append((delegation_id, text))
            return self.answer

        self.bridge = live.Bridge(CONFIG, send, ask)

    async def delegate(self, delegation_id):
        await self.bridge.handle({"type": "session.delegation.created", "offset_ms": 0,
                                  "delegation": {"id": delegation_id, "type": "delegation", "target": "client"}})
        await asyncio.gather(*self.bridge.tasks)

    async def test_delegation_sends_new_speech_to_home_and_returns_commentary(self):
        await self.bridge.handle(heard(0, 400, "What are"))
        await self.bridge.handle(heard(400, 900, " the agents doing?"))
        await self.delegate("item_1")
        self.assertEqual(self.asked, [("item_1", "What are the agents doing?")])
        kinds = [(event["type"], event["delegation_id"]) for event in self.sent]
        self.assertEqual(kinds, [("session.thinking.append", "item_1"), ("session.commentary.append", "item_1")])
        self.assertEqual(self.sent[-1]["content"], "Status. Three agents are running. PR.")
        self.assertEqual(len({event["event_id"] for event in self.sent}), 2)

    async def test_follow_up_carries_only_speech_after_previous_request(self):
        await self.bridge.handle(heard(0, 500, "Pause Tach."))
        await self.delegate("item_1")
        await self.bridge.handle(heard(2000, 2600, "Actually, Thursday not Friday."))
        await self.delegate("item_2")
        self.assertEqual([text for _, text in self.asked], ["Pause Tach.", "Actually, Thursday not Friday."])

    async def test_empty_request_is_not_sent(self):
        await self.delegate("item_1")
        self.assertEqual(self.asked, [])
        self.assertEqual(self.sent[0]["type"], "session.commentary.append")

    async def test_failed_run_and_transport_error_are_reported_honestly(self):
        self.answer = ("failed", "")
        await self.bridge.handle(heard(0, 500, "Ship it."))
        await self.delegate("item_1")
        self.assertIn("couldn't finish", self.sent[-1]["content"])

        def broken(*args):
            raise live.t3.McpError("http-401")

        self.bridge.ask = broken
        await self.bridge.handle(heard(1000, 1500, "Ship it."))
        await self.delegate("item_2")
        self.assertIn("Nothing was sent", self.sent[-1]["content"])

    async def test_long_reply_is_split_into_append_sized_pieces(self):
        self.answer = ("completed", "One long sentence here. " * 300)
        await self.bridge.handle(heard(0, 500, "Tell me everything."))
        await self.delegate("item_1")
        pieces = [event["content"] for event in self.sent if event["type"] == "session.commentary.append"]
        self.assertGreater(len(pieces), 1)
        self.assertTrue(all(len(piece) <= live.APPEND_CHARS for piece in pieces))

    async def test_close_records_usage(self):
        await self.bridge.handle({"type": "session.usage.updated", "usage": {"seconds": 12}})
        await self.bridge.handle({"type": "session.closed", "reason": "close_requested", "usage": {"seconds": 15}})
        self.assertTrue(self.bridge.closed.is_set())
        self.assertEqual(self.bridge.usage_seconds, 15)


class AskHomeTest(unittest.TestCase):
    def test_request_is_idempotent_on_delegation_and_returns_final_reply(self):
        fake = FakeT3()
        fake.reply("Working.", "All quiet.")
        fake.status = "completed"
        with tempfile.TemporaryDirectory() as state, mock.patch.dict(os.environ, {"XDG_STATE_HOME": state}):
            first = live.ask_home(CONFIG, fake, "item_1", "status?", lambda: None)
            live.ask_home(CONFIG, fake, "item_1", "status?", lambda: None)
        self.assertEqual(first, ("completed", "All quiet."))
        self.assertEqual(fake.sends[0], {"threadId": "mcp:home", "message": "[voice live] status?",
                                         "mode": "queue", "clientRequestId": "cantrip-live-item_1"})
        self.assertEqual(fake.sends[0], fake.sends[1])


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
