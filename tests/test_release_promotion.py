"""Overmind PR105 regression: real git/files; substitute only provider transport."""

from contextlib import redirect_stdout
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile
from typing import Any
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
RELEASE = runpy.run_path(str(ROOT / "scripts/release"))


class MonotonicPromotion(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.git("init", "--quiet", "--initial-branch=master")
        (self.root / "Cargo.toml").write_text('[package]\nname="cantrip"\nversion.workspace=true\n[workspace.package]\nversion="0.1.3"\n')
        lock = '\n'.join(f'[[package]]\nname="{name}"\nversion="0.1.3"\n' for name in ("cantrip", "cantrip-engine"))
        (self.root / "Cargo.lock").write_text(lock)
        (self.root / "site").mkdir()
        (self.root / "site/wrangler.jsonc").write_text('{"account_id":"fixture-account","name":"fixture-site"}')
        self.live = self.commit("fix: published baseline")
        self.git("tag", "v0.1.3")
        (self.root / "Cargo.toml").write_text((self.root / "Cargo.toml").read_text().replace('"0.1.3"', '"0.1.4"'))
        (self.root / "Cargo.lock").write_text(lock.replace('"0.1.3"', '"0.1.4"'))
        (self.root / "docs/releases").mkdir(parents=True)
        (self.root / "docs/releases/v0.1.4.md").write_text("Public fixture notes\n")
        technical = b"Technical fixture notes\n"
        (self.root / "docs/releases/v0.1.4-technical.md").write_bytes(technical)
        (self.root / ".landmark").mkdir()
        plan = {"schema_version": 1, "version": "0.1.4", "tag": "v0.1.4", "base_revision": self.live,
                "previous_tag": "v0.1.3", "technical_changelog_sha256": hashlib.sha256(technical).hexdigest()}
        (self.root / RELEASE["PLAN"]).write_text(json.dumps(plan))
        self.candidate = self.commit("chore(release): prepare 0.1.4")
        (self.root / "later-code").write_text("unverified later merge\n")
        self.newer = self.commit("fix: later unverified change")
        self.git("switch", "--quiet", "--detach", self.live)
        (self.root / "divergent-code").write_text("divergent source\n")
        self.divergent = self.commit("fix: divergent fixture")
        self.git("switch", "--quiet", "--detach", self.candidate)
        self.latest = {"tag_name": "v0.1.3"}
        self.draft: Any = None
        self.remote = {}
        self.writes = []
        self.reads = []
        self.interrupt = False
        self.active = {"created_on": "2026-10-14T00:00:00Z", "versions": [{"version_id": "fixture-version", "percentage": 100}]}
        self.deployed = {"id": "fixture-version", "annotations": {"workers/message": f"git:{self.live} run:41"}}
        original_run = RELEASE["run"]

        def command(*args, **kwargs):
            if args[0] != "gh":
                return original_run(*args, **kwargs)
            if args[1:3] == ("release", "create"):
                self.writes.append(args)
                self.draft = {"tag_name": "v0.1.4", "draft": True, "target_commitish": self.candidate}
            elif args[1:3] == ("release", "upload"):
                if self.interrupt and self.remote:
                    self.interrupt = False
                    raise RELEASE["ReleaseError"]("fixture interrupted upload")
                self.writes.append(args)
                path = Path(args[4])
                self.remote[path.name] = path.read_bytes()
            elif args[1:3] == ("release", "download"):
                destination = Path(args[args.index("--dir") + 1])
                for name, content in self.remote.items():
                    (destination / name).write_bytes(content)
            elif args[1:3] == ("attestation", "verify"):
                self.assertEqual(args[args.index("--source-digest") + 1], self.candidate)
            elif args[1:3] == ("release", "edit"):
                self.writes.append(args)
                self.draft.update(draft=False, published_at="2026-10-15T00:00:00Z", html_url="https://example.invalid/v0.1.4")
            else:
                self.fail(f"unexpected provider command: {args}")
            return subprocess.CompletedProcess(args, 0, "", "")

        self.addCleanup(mock.patch.stopall)
        mock.patch.dict(RELEASE["decide"].__globals__, ROOT=self.root, github=self.github,
                        release_by_tag=self.release_by_tag, run=command).start()
        mock.patch.dict(os.environ, GITHUB_SHA=self.candidate, CLOUDFLARE_API_TOKEN="fixture-not-a-secret").start()
        mock.patch("urllib.request.urlopen", side_effect=self.cloudflare).start()
        self.assets = self.root / "assets"
        self.assets.mkdir()
        (self.assets / "release.json").write_text(json.dumps({"source_revision": self.candidate,
            "tag": "v0.1.4", "archive": {"name": "archive.tar.gz"}}))
        (self.assets / "archive.tar.gz").write_bytes(b"verified fixture archive bytes")
        (self.assets / "provenance.json").write_text("fixture provenance transport\n")
        (self.assets / "v0.1.4.md").write_text("Public fixture notes\n")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, text=True, capture_output=True, check=True).stdout.strip()

    def commit(self, message):
        self.git("add", ".")
        self.git("-c", "user.name=Promotion Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", message)
        return self.git("rev-parse", "HEAD")

    def github(self, path, *, missing=False) -> Any:
        self.reads.append(path)
        if path == "git/ref/heads/master":
            return {"object": {"sha": self.git("rev-parse", "master")}}
        if path == "releases/latest":
            return self.latest
        if path.startswith("commits/"):
            return {"sha": self.git("rev-parse", path.split("/", 1)[1])}
        if path.startswith("compare/"):
            older, newer = path.split("/", 1)[1].split("...")
            base = self.git("merge-base", older, newer)
            status = "identical" if older == newer else "ahead" if base == older else "behind" if base == newer else "diverged"
            return {"status": status, "base_commit": {"sha": older}, "merge_base_commit": {"sha": base}}
        if path == "git/ref/tags/v0.1.4" and missing:
            return None
        self.fail(f"unexpected provider read: {path}")

    def release_by_tag(self, tag):
        self.assertEqual(tag, "v0.1.4")
        if self.draft is None:
            return None
        result = copy.deepcopy(self.draft)
        result["assets"] = [{"name": name, "digest": "sha256:" + hashlib.sha256(data).hexdigest()}
                            for name, data in self.remote.items()]
        return result

    def cloudflare(self, request, timeout):
        self.assertEqual(timeout, 30)
        self.assertTrue(request.full_url.startswith("https://api.cloudflare.com/client/v4/accounts/fixture-account/workers/scripts/fixture-site/"))
        self.assertEqual(request.get_method(), "GET")
        data = {"deployments": [self.active]} if request.full_url.endswith("/deployments") else self.deployed
        return io.BytesIO(json.dumps({"success": True, "result": data}).encode())

    def invoke(self, name):
        with redirect_stdout(io.StringIO()) as output:
            if name == "publish":
                RELEASE[name](self.assets)
            else:
                RELEASE[name]()
        return output.getvalue()

    def test_verified_ancestor_selects_and_publishes_after_master_moves(self):
        self.assertNotEqual(self.git("rev-parse", "master"), self.candidate)
        selection = json.loads(self.invoke("decide"))
        self.assertEqual(selection["mode"], "publish")
        self.assertEqual(selection["source_revision"], self.candidate)
        self.invoke("publish")
        self.assertFalse(self.draft["draft"])
        self.assertEqual(self.remote, {path.name: path.read_bytes() for path in self.assets.iterdir()})

    def test_newer_published_source_refuses_selection_and_publication_without_writes(self):
        self.git("tag", "v0.1.5", self.newer)
        self.latest = {"tag_name": "v0.1.5"}
        for action in ("decide", "publish"):
            with self.subTest(action=action), self.assertRaisesRegex(RELEASE["ReleaseError"], "older than or divergent"):
                self.invoke(action)
        self.assertEqual(self.writes, [])

    def test_published_version_cannot_regress_even_with_matching_source(self):
        self.latest = {"tag_name": "v0.1.5"}
        self.git("tag", "v0.1.5", self.candidate)
        for action in ("decide", "publish"):
            with self.subTest(action=action), self.assertRaisesRegex(RELEASE["ReleaseError"], "newer published version"):
                self.invoke(action)
        self.assertEqual(self.writes, [])

    def test_interrupted_draft_resumes_original_source_after_master_moves(self):
        self.interrupt = True
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "interrupted"):
            self.invoke("publish")
        self.assertTrue(self.draft["draft"])
        partial = copy.deepcopy(self.remote)
        self.assertTrue(partial)
        self.assertEqual(json.loads(self.invoke("decide"))["source_revision"], self.candidate)
        self.invoke("publish")
        self.assertFalse(self.draft["draft"])
        self.assertEqual(partial, {name: self.remote[name] for name in partial})
        self.assertEqual(sum(call[1:3] == ("release", "create") for call in self.writes), 1)
        before = copy.deepcopy(self.remote)
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "already public"):
            self.invoke("publish")
        self.assertEqual(before, self.remote)

    def test_later_source_cannot_retarget_an_interrupted_draft(self):
        self.draft = {"tag_name": "v0.1.4", "draft": True, "target_commitish": self.candidate}
        self.git("switch", "--quiet", "--detach", self.newer)
        with mock.patch.dict(os.environ, GITHUB_SHA=self.newer):
            with self.assertRaisesRegex(RELEASE["ReleaseError"], "original workflow"):
                self.invoke("decide")
        self.assertEqual(self.writes, [])

    def test_draft_asset_mismatch_is_never_overwritten(self):
        self.draft = {"tag_name": "v0.1.4", "draft": True, "target_commitish": self.candidate}
        self.remote = {"archive.tar.gz": b"different prior bytes"}
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "draft asset differs"):
            self.invoke("publish")
        self.assertEqual(self.remote, {"archive.tar.gz": b"different prior bytes"})
        self.assertEqual(self.writes, [])

    def test_website_ancestor_can_promote_after_master_advances(self):
        receipt = json.loads(self.invoke("check_site_promotion"))
        self.assertEqual(receipt["deployed_source"], self.live)
        self.assertEqual(receipt["candidate_source"], self.candidate)
        self.assertEqual(self.writes, [])

    def test_website_reverse_completion_and_divergent_live_source_refuse(self):
        for source in (self.newer, self.divergent):
            self.deployed["annotations"]["workers/message"] = f"git:{source} run:43"
            with self.subTest(source=source), self.assertRaisesRegex(RELEASE["ReleaseError"], "older than or divergent"):
                self.invoke("check_site_promotion")
        self.assertEqual(self.writes, [])

    def test_non_master_source_refuses_both_paths(self):
        self.git("switch", "--quiet", "--detach", self.divergent)
        with mock.patch.dict(os.environ, GITHUB_SHA=self.divergent):
            for action in ("decide", "check_site_promotion"):
                with self.subTest(action=action), self.assertRaisesRegex(RELEASE["ReleaseError"], "older than or divergent"):
                    self.invoke(action)
        self.assertEqual(self.writes, [])

    def test_unknown_split_or_mismatched_website_identity_refuses(self):
        for annotation in ("", "git:short run:42", "git:" + self.live + " run:0"):
            self.deployed["annotations"]["workers/message"] = annotation
            with self.subTest(annotation=annotation), self.assertRaisesRegex(RELEASE["ReleaseError"], "no trusted source"):
                self.invoke("check_site_promotion")
        self.deployed["id"] = "another-version"
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "identity disagrees"):
            self.invoke("check_site_promotion")
        self.active["versions"][0]["percentage"] = 50
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "split website"):
            self.invoke("check_site_promotion")

    def test_only_observed_legacy_version_gets_proven_baseline(self):
        prior = "7737d4a8-38a9-458a-b9fc-2443ef90492f"
        self.active["versions"][0]["version_id"] = prior
        self.deployed = {"id": prior, "annotations": {}}
        original_github = self.github
        baseline = "ad4fac9f31ce0acebee1bcdab5117fb45a6d5127"

        def transport(path, **kwargs):
            # Bind the historical external SHA to this fixture's real baseline topology.
            return original_github(path.replace(baseline, self.live), **kwargs)

        # Exercise exact source validation separately; API response must name the exact requested SHA.
        def comparison(path, **kwargs):
            result = transport(path, **kwargs)
            if path.startswith("compare/" + baseline):
                result["base_commit"]["sha"] = baseline
                result["merge_base_commit"]["sha"] = baseline
            return result

        with mock.patch.dict(RELEASE["check_site_promotion"].__globals__, github=comparison):
            self.assertEqual(json.loads(self.invoke("check_site_promotion"))["deployed_source"], baseline)
        self.deployed["annotations"] = {"workers/message": "malformed"}
        with self.assertRaisesRegex(RELEASE["ReleaseError"], "no trusted source"):
            self.invoke("check_site_promotion")

    def test_workflow_keeps_one_serialized_promotion_and_source_annotation(self):
        workflow = (ROOT / ".github/workflows/site.yml").read_text()
        self.assertIn("cancel-in-progress: false", workflow)
        self.assertIn("python3 ../scripts/release check-site-promotion", workflow)
        self.assertIn('npm run deploy -- --message "git:$(git rev-parse HEAD) run:$GITHUB_RUN_ID"', workflow)
        self.assertNotIn("Superseded source", workflow)


if __name__ == "__main__":
    unittest.main()
