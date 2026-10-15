"""Release gate regressions; real files/git/shell, GitHub transport only mocked."""

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


class ReleaseGates(unittest.TestCase):
    def test_title_check_uses_literal_environment_input(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
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

    def test_native_approval_is_exact_scoped_and_idempotent(self):
        approve = RELEASE["approve_release_checks"]
        revision = "a" * 40
        pr = {
            "state": "open", "user": {"login": "github-actions[bot]"},
            "base": {"ref": "master"},
            "head": {"ref": "landmark/release", "sha": revision, "repo": {"full_name": "misty-step/cantrip"}},
        }
        check = {
            "id": 123, "event": "pull_request", "head_sha": revision,
            "head_repository": {"full_name": "misty-step/cantrip"},
            "pull_requests": [{"number": 133}],
            "status": "completed", "conclusion": "action_required",
        }

        def exercise(candidate_pr, runs, moved=False):
            reads = 0

            def github(path):
                nonlocal reads
                if path == "pulls/133":
                    reads += 1
                    return {**candidate_pr, "head": {**candidate_pr["head"], "sha": "b" * 40}} if moved and reads > 1 else candidate_pr
                return {"workflow_runs": runs}

            transport = mock.Mock()
            with mock.patch.dict(approve.__globals__, github=github, run=transport):
                approve(133, revision)
            return transport

        self.assertEqual(exercise(pr, [check]).call_count, 3)
        for conclusion in ("success", "failure", "skipped", "cancelled"):
            self.assertEqual(exercise(pr, [{**check, "conclusion": conclusion}]).call_count, 0)
        self.assertEqual(exercise(pr, [{**check, "status": "in_progress", "conclusion": None}]).call_count, 0)
        bad_pr = copy.deepcopy(pr)
        bad_pr["head"]["repo"]["full_name"] = "other/cantrip"
        cases = [(bad_pr, [check], False), (pr, [], False), (pr, [check], True)]
        for changes in (
            {"head_sha": "b" * 40}, {"event": "workflow_dispatch"},
            {"head_repository": {"full_name": "other/cantrip"}}, {"pull_requests": []},
        ):
            cases.append((pr, [{**check, **changes}], False))
        for candidate_pr, runs, moved in cases:
            with self.subTest(candidate_pr=candidate_pr, runs=runs, moved=moved):
                with self.assertRaises(RELEASE["ReleaseError"]):
                    exercise(candidate_pr, runs, moved)

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

    def test_superseded_source_cannot_start_publication(self):
        publish = RELEASE["publish"]
        transport = mock.Mock()
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "release.json").write_text(json.dumps({"source_revision": "a" * 40, "tag": "v0.1.4"}))
            with mock.patch.dict(publish.__globals__, head=lambda: "a" * 40, version_at=lambda: "0.1.4",
                                 github=lambda _: {"object": {"sha": "b" * 40}}, run=transport):
                with self.assertRaisesRegex(RELEASE["ReleaseError"], "superseded"):
                    publish(directory)
            transport.assert_not_called()

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
