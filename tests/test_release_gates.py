"""Release gate regressions; real files/git/shell, fake GitHub transport and clock."""

import copy
import hashlib
import json
import os
from pathlib import Path
import runpy
import shlex
import subprocess
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
RELEASE = runpy.run_path(str(ROOT / "scripts/release"))


class NativeReleaseChecks(unittest.TestCase):
    def setUp(self):
        self.approve = RELEASE["approve_release_checks"]
        self.revision = "a" * 40
        self.pr = {
            "state": "open", "user": {"login": "github-actions[bot]"},
            "base": {"ref": "master"},
            "head": {"ref": "landmark/release", "sha": self.revision,
                     "repo": {"full_name": "misty-step/cantrip"}},
        }
        self.checks = {
            workflow: {
                "id": identifier, "event": "pull_request", "head_sha": self.revision,
                "head_repository": {"full_name": "misty-step/cantrip"},
                "pull_requests": [{"number": 133}],
                "status": "completed", "conclusion": "action_required",
            }
            for identifier, workflow in enumerate(("ci.yml", "macos.yml", "site.yml", "pr-title.yml"), 100)
        }
        self.responses = {workflow: [[check]] for workflow, check in self.checks.items()}
        self.reads = []
        self.seconds = 0
        self.clock = mock.Mock()
        self.clock.monotonic.side_effect = lambda: self.seconds
        self.clock.sleep.side_effect = self.advance
        self.transport = mock.Mock()
        patch = mock.patch.dict(self.approve.__globals__, github=self.github,
                                run=self.transport, time=self.clock,
                                RELEASE_CHECK_WAIT_SECONDS=5)
        patch.start()
        self.addCleanup(patch.stop)

    def advance(self, seconds):
        self.seconds += seconds

    def query(self, workflow):
        return f"actions/workflows/{workflow}/runs?event=pull_request&head_sha={self.revision}&per_page=100"

    def github(self, path, *, timeout):
        self.assertGreater(timeout, 0)
        self.assertLessEqual(timeout, 5)
        self.reads.append(path)
        if path == "pulls/133":
            return self.pr
        for workflow, responses in self.responses.items():
            if path == self.query(workflow):
                response = responses.pop(0) if len(responses) > 1 else responses[0]
                if isinstance(response, Exception):
                    raise response
                return {"workflow_runs": response}
        self.fail(f"unexpected GitHub lookup: {path}")

    def test_waits_for_all_native_runs_and_approves_each_once(self):
        extra = {**self.checks["ci.yml"], "id": 104}
        self.responses["ci.yml"] = [[], [self.checks["ci.yml"], extra]]
        self.responses["site.yml"] = [[], [], [self.checks["site.yml"]]]
        self.approve(133, self.revision)
        self.assertEqual(self.clock.sleep.call_args_list, [mock.call(2), mock.call(2)])
        self.assertEqual(
            [call.args for call in self.transport.call_args_list],
            [("gh", "api", "--method", "POST",
              f"repos/misty-step/cantrip/actions/runs/{identifier}/approve")
             for identifier in (101, 103, 100, 104, 102)],
        )
        self.assertEqual([call.kwargs["timeout"] for call in self.transport.call_args_list],
                         [5, 5, 3, 3, 1])
        self.assertEqual(self.reads.count(self.query("ci.yml")), 2)
        self.assertEqual(self.reads.count(self.query("macos.yml")), 1)
        self.assertEqual(self.reads.count(self.query("site.yml")), 3)

    def test_times_out_without_reapproving_discovered_runs(self):
        self.responses["site.yml"] = [[]]
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "timed out.*site.yml"):
            self.approve(133, self.revision)
        self.assertEqual(self.seconds, 5)
        self.assertEqual(self.clock.sleep.call_args_list,
                         [mock.call(2), mock.call(2), mock.call(1)])
        self.assertEqual(self.transport.call_count, 3)
        self.assertEqual(self.reads.count(self.query("site.yml")), 3)

    def test_existing_approvals_and_failed_runs_are_not_retried(self):
        for status, conclusion in (
            ("completed", "success"), ("completed", "failure"),
            ("completed", "skipped"), ("completed", "cancelled"),
            ("completed", "timed_out"), ("in_progress", None), ("queued", None),
        ):
            with self.subTest(status=status, conclusion=conclusion):
                self.responses = {
                    workflow: [[{**check, "status": status, "conclusion": conclusion}]]
                    for workflow, check in self.checks.items()
                }
                self.approve(133, self.revision)
                self.transport.assert_not_called()
                self.clock.sleep.assert_not_called()

    def test_rejects_checks_outside_the_exact_native_pr_head(self):
        for changes in (
            {"head_sha": "b" * 40}, {"event": "workflow_dispatch"},
            {"head_repository": {"full_name": "other/cantrip"}},
            {"pull_requests": []}, {"pull_requests": [{"number": 134}]},
        ):
            with self.subTest(changes=changes):
                self.responses["ci.yml"] = [[{**self.checks["ci.yml"], **changes}]]
                with self.assertRaisesRegex(RELEASE["ReleaseError"], "exact generator PR head"):
                    self.approve(133, self.revision)
                self.transport.assert_not_called()
                self.clock.sleep.assert_not_called()

    def test_rejects_foreign_generator_pr(self):
        self.pr["head"]["repo"]["full_name"] = "other/cantrip"
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "same-repository generator PR"):
            self.approve(133, self.revision)
        self.transport.assert_not_called()
        self.clock.sleep.assert_not_called()

    def test_rechecks_pr_identity_while_waiting(self):
        self.responses = {workflow: [[]] for workflow in self.checks}

        def move_head(seconds):
            self.advance(seconds)
            self.pr["head"]["sha"] = "b" * 40

        self.clock.sleep.side_effect = move_head
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "PR identity moved"):
            self.approve(133, self.revision)
        self.transport.assert_not_called()
        self.assertEqual(self.clock.sleep.call_count, 1)

    def test_rechecks_pr_identity_before_approval(self):
        moved = copy.deepcopy(self.pr)
        moved["head"]["sha"] = "b" * 40
        lookup = mock.Mock(side_effect=[
            self.pr, {"workflow_runs": [self.checks["ci.yml"]]}, moved,
        ])
        with mock.patch.dict(self.approve.__globals__, github=lookup):
            with self.assertRaisesRegex(RELEASE["ReleaseError"], "PR identity moved"):
                self.approve(133, self.revision)
        self.transport.assert_not_called()
        self.clock.sleep.assert_not_called()

    def test_discovery_api_error_stops_waiting_without_retry(self):
        self.responses["ci.yml"] = [[], RELEASE["ReleaseError"]("GitHub lookup refused")]
        for workflow in ("macos.yml", "site.yml", "pr-title.yml"):
            self.responses[workflow] = [[{**self.checks[workflow], "conclusion": "success"}]]
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "GitHub lookup refused"):
            self.approve(133, self.revision)
        self.transport.assert_not_called()
        self.assertEqual(self.clock.sleep.call_count, 1)
        self.assertEqual(self.reads.count(self.query("ci.yml")), 2)

    def test_approval_api_error_is_not_retried(self):
        self.transport.side_effect = RELEASE["ReleaseError"]("GitHub approval refused")
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "GitHub approval refused"):
            self.approve(133, self.revision)
        self.assertEqual(self.transport.call_count, 1)
        self.clock.sleep.assert_not_called()


class ReleaseGates(unittest.TestCase):
    def test_title_check_uses_literal_environment_input(self):
        workflow = (ROOT / ".github/workflows/pr-title.yml").read_text()
        command = next(line.strip() for line in workflow.splitlines() if "python3 -c" in line)
        self.assertNotIn("${{", command)
        for title, expected in (
            ("fix(stt): preserve completed chunks", 0),
            ("feat!: change the supported interface", 0),
            ("chore(release): prepare 0.1.4", 0),
            ("docs: describe $(false) literally", 0),
            ("Add an unclassified change", 1),
            ("fix: first\nfeat: second", 1),
        ):
            with self.subTest(title=title):
                result = subprocess.run(shlex.split(command), env={**os.environ, "PR_TITLE": title}, capture_output=True)
                self.assertEqual(result.returncode, expected)

    def test_github_transport_timeout_fails_without_command_output(self):
        with mock.patch.object(subprocess, "run",
                               side_effect=subprocess.TimeoutExpired("gh", 1, output="private")) as transport:
            with self.assertRaisesRegex(RELEASE["ReleaseError"], "^gh timed out$"):
                RELEASE["github"]("pulls/133", timeout=1)
        self.assertEqual(transport.call_count, 1)
        self.assertEqual(transport.call_args.kwargs["timeout"], 1)

    def test_failed_skipped_or_foreign_runtime_proof_cannot_finalize(self):
        finalize = RELEASE["finalize"]
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "archive.tar.gz").write_bytes(b"candidate bytes")
            digest = hashlib.sha256(b"candidate bytes").hexdigest()
            data = {"source_revision": "a" * 40, "archive": {"name": "archive.tar.gz", "sha256": digest}}
            (directory / "release.json").write_text(json.dumps(data))
            proof = directory / "proof.json"
            for status, revision, archive in (
                ("failed", "a" * 40, digest), ("skipped", "a" * 40, digest),
                ("passed", "b" * 40, digest), ("passed", "a" * 40, "0" * 64),
            ):
                proof.write_text(json.dumps({"status": status, "source_revision": revision, "archive_sha256": archive}))
                with mock.patch.dict(finalize.__globals__, head=lambda: "a" * 40):
                    with self.assertRaises(RELEASE["ReleaseError"]):
                        finalize(directory, proof)
                self.assertFalse((directory / "verification.json").exists())

    def test_release_plan_rejects_code_changes_after_preparation(self):
        validate = RELEASE["validate_plan"]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def git(*args, check=True):
                return subprocess.run(["git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false",
                                       "-c", "user.name=Release Fixture", "-c", "user.email=fixture@example.invalid", *args],
                                      cwd=root, capture_output=True, text=True, check=check)

            git("init", "--quiet")
            (root / "Cargo.toml").write_text('[package]\nname="cantrip"\nversion.workspace=true\n[workspace.package]\nversion="0.1.4"\n')
            git("add", ".")
            git("commit", "--quiet", "-m", "chore: base")
            base = git("rev-parse", "HEAD").stdout.strip()
            git("tag", "v0.1.3")
            (root / "scripts").mkdir()
            (root / "scripts/release").write_text("unexpected code change\n")
            git("add", ".")
            git("commit", "--quiet", "-m", "fix: moved source")
            revision = git("rev-parse", "HEAD").stdout.strip()
            with mock.patch.dict(validate.__globals__, ROOT=root, git=git):
                with self.assertRaisesRegex(RELEASE["ReleaseError"], "source changed after release preparation"):
                    validate({"schema_version": 1, "version": "0.1.4", "tag": "v0.1.4", "base_revision": base, "previous_tag": "v0.1.3"}, revision)
            baseline = RELEASE["validate_baseline"]
            with mock.patch.dict(baseline.__globals__, git=git):
                baseline("v0.1.3", revision)
                for tag in (None, "", "--unsafe", "v0.1.2"):
                    with self.subTest(tag=tag):
                        with self.assertRaises(RELEASE["ReleaseError"]):
                            baseline(tag, revision)


if __name__ == "__main__":
    unittest.main()
