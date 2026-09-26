"""Bar status contract: what the Cantrip mark shows for a daemon snapshot.

Status.js is plain JavaScript shared with the QML widget; node evaluates it here.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import unittest

STATUS = Path(__file__).with_name("Status.js")
KAYLEE = {"name": "kaylee", "label": "Kaylee", "color": "#e68bd5"}


def snapshot(state="idle", **fields):
    value = {"epoch": "e", "state": state, "pending_recordings": 13, "attention": False,
             "signal": {"level": 40} if state == "recording" else None, "outcome": None,
             "notice": None, "handoff": None, "capabilities": {"cancel": state != "idle"}}
    value.update(fields)
    return value


def outcome(message, **fields):
    value = {"event_id": 9, "message": message, "completeness": "complete", "delivery": "pasted",
             "error": None, "dismissed": False, "handoff": None}
    value.update(fields)
    return value


@unittest.skipUnless(shutil.which("node") or os.environ.get("CI"), "node is not installed")
class BarStatusContract(unittest.TestCase):
    def view(self, value):
        script = ("const S = require(process.argv[1]); const s = JSON.parse(process.argv[2]);"
                  "const h = S.handoff(s);"
                  "console.log(JSON.stringify({tone: S.tone(s), tooltip: S.tooltip(s), color: h && h.color,"
                  " dismiss: S.dismissCommand(s)}))")
        out = subprocess.run(["node", "-e", script, str(STATUS), json.dumps(value)],
                             check=True, capture_output=True, text=True, timeout=10).stdout
        return json.loads(out)

    def test_rest_is_quiet_and_never_reports_the_backlog(self):
        view = self.view(snapshot())
        self.assertEqual(view["tone"], "rest")
        self.assertNotIn("13", view["tooltip"])
        self.assertNotIn("recording(s)", view["tooltip"])

    def test_a_take_is_colored_by_its_route_and_named_only_for_a_handoff(self):
        default = self.view(snapshot("recording"))
        routed = self.view(snapshot("recording", handoff=KAYLEE))
        self.assertEqual((default["tone"], default["color"]), ("recording", None))
        self.assertEqual(default["tooltip"], "Cantrip: Recording.")
        self.assertEqual((routed["tone"], routed["color"]), ("recording", "#e68bd5"))
        self.assertEqual(routed["tooltip"], "Cantrip: Recording to Kaylee.")
        # A malformed color falls back to the default route rather than reaching QML.
        broken = self.view(snapshot("recording", handoff={**KAYLEE, "color": "magenta"}))
        self.assertIsNone(broken["color"])

    def test_attention_follows_the_daemon_and_a_new_take_supersedes_it(self):
        failed = outcome("Handoff failed.", delivery="failed", error="handoff-failed", handoff=KAYLEE)
        view = self.view(snapshot(attention=True, outcome=failed))
        self.assertEqual(view["tone"], "attention")
        self.assertIn("Handoff failed. (to Kaylee)", view["tooltip"])
        self.assertIn("Middle-click: dismiss", view["tooltip"])
        # Dismiss targets exactly the shown outcome, never an independent notice.
        self.assertEqual(view["dismiss"], "cantrip dismiss --event-id 9")
        self.assertIsNone(self.view(snapshot(outcome=failed))["dismiss"])
        # Dismissed or replaced outcomes are no longer flagged by the daemon.
        self.assertEqual(self.view(snapshot(outcome={**failed, "dismissed": True}))["tone"], "rest")
        self.assertEqual(self.view(snapshot("recording", attention=True))["tone"], "recording")

    def test_unknown_status_never_claims_ready(self):
        for value in (None, snapshot("starting")):
            view = self.view(value)
            self.assertEqual(view["tone"], "unavailable")
            self.assertNotIn("Ready", view["tooltip"])


if __name__ == "__main__":
    unittest.main()
