"""Exercise the canonical check tiers and release-only workflow boundary."""

import itertools
import os
from pathlib import Path
import re
import runpy
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class CheckTiers(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.log = self.root / "commands"
        for name in ("cargo", "python3"):
            command = self.root / name
            command.write_text(
                '#!/bin/sh\n'
                'printf "%s %s\\n" "${0##*/}" "$*" >> "$CHECK_COMMANDS"\n'
                '[ "${0##*/} $*" != "${CHECK_FAIL:-}" ]\n'
            )
            command.chmod(0o755)
        self.environment = {
            **os.environ,
            "PATH": f"{self.root}{os.pathsep}{os.environ['PATH']}",
            "CHECK_COMMANDS": str(self.log),
        }

    def run_check(self, *arguments, failure=""):
        self.log.unlink(missing_ok=True)
        result = subprocess.run(
            [str(ROOT / "scripts/check"), *arguments], cwd=ROOT,
            env={**self.environment, "CHECK_FAIL": failure},
            text=True, capture_output=True,
        )
        commands = self.log.read_text().splitlines() if self.log.exists() else []
        return result, commands

    def test_ci_tiers_partition_the_complete_local_gate(self):
        result, all_commands = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        rust, rust_commands = self.run_check("rust")
        contracts, contract_commands = self.run_check("contracts")
        self.assertEqual(rust.returncode, 0, rust.stderr)
        self.assertEqual(contracts.returncode, 0, contracts.stderr)
        self.assertEqual(all_commands, rust_commands + contract_commands)
        self.assertEqual(rust_commands, [
            "cargo fmt --all --check",
            "cargo clippy --workspace --locked --all-targets -- -D warnings",
            "cargo test --workspace --locked",
            "cargo test --locked --package cantrip --example eval",
        ])
        self.assertEqual(contract_commands, [
            "python3 -m unittest discover -s integrations/omarchy -p test_*.py",
            "python3 -m unittest discover -s tests -p test_release_*.py",
            "python3 -m unittest discover -s tests -p test_install_from_source.py",
            "python3 -m unittest discover -s tests -p test_verify_site.py",
        ])

    def test_failed_rust_gate_stops_before_tests(self):
        result, commands = self.run_check("rust", failure="cargo clippy --workspace --locked --all-targets -- -D warnings")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(commands), 2)

    def test_invalid_tier_never_runs_a_partial_gate(self):
        for arguments in (("unknown",), ("rust", "contracts"), ("",)):
            with self.subTest(arguments=arguments):
                result, commands = self.run_check(*arguments)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(commands, [])

    def test_required_check_rejects_every_incomplete_tier(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        command = next(
            line.strip().removeprefix("run: ")
            for line in workflow.splitlines()
            if 'run: test "$RUST_RESULT"' in line
        )
        for rust in ("success", "failure", "cancelled", "skipped"):
            for contracts in ("success", "failure", "cancelled", "skipped"):
                with self.subTest(rust=rust, contracts=contracts):
                    result = subprocess.run(
                        ["bash", "-c", command], capture_output=True,
                        env={**os.environ, "RUST_RESULT": rust, "CONTRACTS_RESULT": contracts},
                    )
                    self.assertEqual(result.returncode == 0, rust == contracts == "success")


class WorkflowDispatch(unittest.TestCase):
    @staticmethod
    def workflow(name):
        return (ROOT / ".github/workflows" / name).read_text()

    @classmethod
    def concurrency(cls, name):
        block = cls.workflow(name).split("\nconcurrency:\n", 1)[1].split("\n\n", 1)[0]
        return dict(
            line.strip().split(": ", 1)
            for line in block.splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        )

    @classmethod
    def job(cls, workflow, name):
        match = re.search(
            rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [^\s]|\Z)",
            cls.workflow(workflow),
        )
        if match is None:
            raise AssertionError(f"{workflow} has no {name} job")
        return match.group(1)

    def test_no_workflow_runs_ci_on_a_pr_or_push(self):
        expected = {
            "ci.yml": {"workflow_dispatch", "workflow_call"},
            "macos.yml": {"workflow_dispatch", "workflow_call", "schedule"},
            "release.yml": {"workflow_dispatch"},
            "site.yml": {"workflow_dispatch", "workflow_call"},
        }
        for path in (ROOT / ".github/workflows").glob("*.y*ml"):
            with self.subTest(workflow=path.name):
                events = path.read_text().split("\non:\n", 1)[1].split("\n\n", 1)[0]
                triggers = set(re.findall(r"(?m)^  (\w+):", events))
                self.assertNotIn("push", triggers)
                self.assertNotIn("pull_request", triggers)
                self.assertNotIn("pull_request_target", triggers)
                if path.name in expected:
                    self.assertEqual(triggers, expected[path.name])
        self.assertFalse((ROOT / ".github/workflows/pr-title.yml").exists())

    def test_dispatches_and_warmups_cannot_replace_a_pending_release(self):
        for workflow in ("ci.yml", "macos.yml"):
            with self.subTest(workflow=workflow):
                policy = self.concurrency(workflow)
                groups = {
                    policy["group"].replace("${{ github.run_id }}", str(run))
                    for run in (101, 102, 103, 104)
                }
                self.assertEqual(len(groups), 4)
                self.assertEqual(policy["cancel-in-progress"], "false")
        writer = self.concurrency("release.yml")
        self.assertEqual(writer["group"], "cantrip-release")
        self.assertEqual(writer["cancel-in-progress"], "false")
        self.assertEqual(writer["queue"], "max")

        def pending_after_later_dispatches(queue):
            pending = ["release-candidate"]
            for event in ("later-release", "warmup", "another-release"):
                if event == "warmup":
                    continue
                if queue == "single":
                    pending[:] = [event]
                else:
                    pending.append(event)
            return pending

        self.assertIn("release-candidate", pending_after_later_dispatches(writer["queue"]))
        self.assertNotIn("release-candidate", pending_after_later_dispatches("single"))

    def test_release_gate_rejects_every_incomplete_qualification(self):
        gate = self.job("ci.yml", "release-gate")
        self.assertIn("needs: [check, secret-scan, native]", gate)
        self.assertIn("if: always()", gate)
        command = next(
            line.strip().removeprefix("run: ") for line in gate.splitlines()
            if 'run: test "$CHECK_RESULT"' in line
        )
        for results in itertools.product(("success", "failure", "cancelled", "skipped"), repeat=3):
            with self.subTest(results=results):
                result = subprocess.run(
                    ["bash", "-c", command], capture_output=True,
                    env={**os.environ, **dict(zip(
                        ("CHECK_RESULT", "SCAN_RESULT", "NATIVE_RESULT"), results,
                    ))},
                )
                self.assertEqual(result.returncode == 0, all(item == "success" for item in results))

    def test_publication_qualifies_one_candidate_and_reuses_its_artifact(self):
        qualification = self.job("release.yml", "qualification")
        self.assertIn("needs: plan", qualification)
        self.assertIn("if: needs.plan.outputs.mode == 'publish'", qualification)
        self.assertIn("uses: ./.github/workflows/ci.yml", qualification)
        self.assertIn("uses: ./.github/workflows/macos.yml", self.job("ci.yml", "native"))
        publish = self.job("release.yml", "publish")
        self.assertIn("needs: [build, verify, qualification]", publish)
        for consumer in ("verify", "publish"):
            self.assertIn(
                "artifact-ids: ${{ needs.build.outputs.artifact_id }}",
                self.job("release.yml", consumer),
            )
        self.assertIn("artifact-ids: ${{ needs.verify.outputs.proof_id }}", publish)
        self.assertNotIn("cargo build", publish)

    def test_schedule_only_prewarms_intel_and_never_qualifies_a_candidate(self):
        native = self.job("macos.yml", "native")
        self.assertIn(
            """fromJSON(github.event_name == 'schedule' && '["macos-15-intel"]' || '["macos-15","macos-15-intel"]')""",
            native,
        )
        steps = re.split(r"(?m)^      - ", native.split("    steps:\n", 1)[1])
        for step in steps:
            if any(command in step for command in (
                "rust-cache@", "cargo fmt", "scripts/package-macos",
                "scripts/verify-macos", "upload-artifact@",
            )):
                self.assertIn("if: github.event_name != 'schedule'", step)
        self.assertIn("scripts/prepare-macos-runtime", native)
        self.assertIn("uses: actions/cache/save@", native)
        self.assertNotIn("schedule", self.workflow("ci.yml"))
        self.assertNotIn("schedule", self.workflow("release.yml"))

    def test_only_the_named_existing_secret_reaches_master_website_calls(self):
        guard = "github.ref == 'refs/heads/master' && github.event_name == 'workflow_dispatch'"
        refresh = self.job("release.yml", "website")
        self.assertIn("needs: publish", refresh)
        self.assertIn(f"if: {guard}", refresh)
        self.assertIn("uses: ./.github/workflows/site.yml", refresh)
        self.assertIn("refresh-release-data: true", refresh)
        field, value = refresh.split("\n    secrets:\n", 1)[1].strip().split(": ", 1)
        self.assertEqual(field, "CLOUDFLARE_API_TOKEN")
        self.assertEqual(value, "${{ secrets." + "CLOUDFLARE_API_TOKEN }}")
        self.assertNotIn("CLOUDFLARE_API_TOKEN", self.workflow("ci.yml"))
        self.assertNotIn("actions: write", self.workflow("release.yml"))
        for workflow in ("ci.yml", "release.yml", "site.yml"):
            self.assertNotIn("secrets: inherit", self.workflow(workflow))
        site = self.workflow("site.yml")
        self.assertIn("run: python3 scripts/release site-refresh-source", site)
        self.assertIn("ref: ${{ steps.website-source.outputs.source_revision }}", site)
        deploy = site.split("- name: Deploy the verified static build\n", 1)[1]
        self.assertIn(f"if: {guard}", deploy)

    def test_website_watchdog_exceeds_all_edge_wait_budgets(self):
        verify = runpy.run_path(str(ROOT / "scripts/verify-site"))
        wait = verify["wait_for_page"].__defaults__[0]
        fetch = verify["fetch"].__defaults__[0]
        site = self.job("site.yml", "site")
        watchdog = int(re.search(r"(?m)^    timeout-minutes: (\d+)$", site).group(1)) * 60
        pages = len(re.findall(r"(?m)^\s+scripts/verify-site ", site))
        self.assertGreater(pages, 0)
        self.assertGreater(watchdog, pages * (wait + fetch) + 5 * 60)


if __name__ == "__main__":
    unittest.main()
