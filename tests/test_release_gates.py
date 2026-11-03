"""Release gate regressions; real files/git/shell and a fake GitHub transport."""

import hashlib
import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
RELEASE = runpy.run_path(str(ROOT / "scripts/release"))


class ReleaseGates(unittest.TestCase):
    def test_release_pr_preparation_neither_waits_for_nor_approves_pr_ci(self):
        for existing in ([], [{"number": 133, "url": "https://github.com/misty-step/cantrip/pull/133"}]):
            with self.subTest(existing=bool(existing)), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                log = root / "commands.jsonl"
                gh = root / "gh"
                gh.write_text(
                    "#!/usr/bin/env python3\n"
                    "import json, os, sys\n"
                    "from pathlib import Path\n"
                    "args = sys.argv[1:]\n"
                    "with Path(os.environ['RELEASE_GH_LOG']).open('a') as log:\n"
                    "    log.write(json.dumps(args) + '\\n')\n"
                    "if args[:2] == ['pr', 'list']:\n"
                    "    print(os.environ['RELEASE_EXISTING_PRS'])\n"
                    "elif args[:2] == ['pr', 'create']:\n"
                    "    print('https://github.com/misty-step/cantrip/pull/133')\n"
                    "elif args[:2] != ['pr', 'edit']:\n"
                    "    raise SystemExit('unexpected GitHub effect: ' + repr(args))\n"
                )
                gh.chmod(0o755)
                with mock.patch.dict(os.environ, {
                    "PATH": f"{root}{os.pathsep}{os.environ['PATH']}",
                    "RELEASE_GH_LOG": str(log),
                    "RELEASE_EXISTING_PRS": json.dumps(existing),
                }):
                    RELEASE["ensure_release_pr"]("0.1.4")
                commands = [json.loads(line) for line in log.read_text().splitlines()]
                self.assertEqual([command[:2] for command in commands], [
                    ["pr", "list"], ["pr", "edit" if existing else "create"],
                ])
                if not existing:
                    body = commands[1][commands[1].index("--body") + 1]
                    self.assertIn("without PR CI", body)
                    self.assertIn("dispatch `release.yml` on master", body)

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
