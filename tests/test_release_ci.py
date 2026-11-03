"""Exercise the canonical check command's independent CI tiers and failure gate."""

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

    def test_pushes_and_schedules_cannot_replace_a_pending_release_run(self):
        for workflow in ("ci.yml", "macos.yml"):
            with self.subTest(workflow=workflow):
                policy = self.concurrency(workflow)
                # Bind the grouping expression for A, release R, a later push
                # and a schedule. The old github.ref grouping collides here.
                groups = {
                    policy["group"].replace(
                        "${{ github.event.pull_request.number || github.run_id }}", str(run)
                    )
                    for run in (101, 102, 103, 104)
                }
                self.assertEqual(len(groups), 4)
                self.assertEqual(
                    policy["cancel-in-progress"],
                    "${{ github.event_name == 'pull_request' }}",
                )

        writer = self.concurrency("release.yml")
        self.assertEqual(writer["group"], "cantrip-release")
        self.assertEqual(writer["cancel-in-progress"], "false")
        self.assertEqual(writer["queue"], "max")

        # GitHub's documented single-pending policy loses R when C arrives.
        # Exercise the same bounded trace using the workflow's actual policy.
        def pending_after_later_pushes(queue):
            pending = ["merged-release"]
            for event in ("later-push", "schedule", "another-push"):
                if event == "schedule":
                    continue
                if queue == "single":
                    pending[:] = [event]
                else:
                    pending.append(event)
            return pending

        self.assertIn("merged-release", pending_after_later_pushes(writer["queue"]))
        self.assertNotIn("merged-release", pending_after_later_pushes("single"))

    def test_website_uses_fast_gates_and_refreshes_after_publication(self):
        website = self.job("ci.yml", "website")
        self.assertIn("needs: [check, secret-scan]", website)
        self.assertIn("uses: ./.github/workflows/site.yml", website)
        refresh = self.job("release.yml", "website")
        self.assertIn("needs: publish", refresh)
        self.assertIn("uses: ./.github/workflows/site.yml", refresh)
        self.assertIn("refresh-release-data: true", refresh)
        site = self.workflow("site.yml")
        self.assertIn("run: python3 scripts/release site-refresh-source", site)
        self.assertIn("ref: ${{ steps.website-source.outputs.source_revision }}", site)
        self.assertNotIn("\n  workflow_run:", self.workflow("site.yml"))

    def test_only_the_named_existing_secret_reaches_master_website_calls(self):
        guard = (
            "github.ref == 'refs/heads/master' && "
            "contains(fromJSON('[\"push\",\"workflow_dispatch\"]'), github.event_name)"
        )
        for workflow, job in (
            ("ci.yml", "website"), ("ci.yml", "release"), ("release.yml", "website"),
        ):
            with self.subTest(workflow=workflow, job=job):
                body = self.job(workflow, job)
                self.assertIn(f"if: {guard}", body)
                field, value = body.split("\n    secrets:\n", 1)[1].strip().split(": ", 1)
                self.assertEqual(field, "CLOUDFLARE_API_TOKEN")
                self.assertEqual(value, "${{ secrets." + "CLOUDFLARE_API_TOKEN }}")
        for workflow in ("ci.yml", "release.yml", "site.yml"):
            self.assertNotIn("secrets: inherit", self.workflow(workflow))
        deploy = self.workflow("site.yml").split(
            "- name: Deploy the verified static build\n", 1
        )[1]
        self.assertIn(
            "if: github.event_name != 'pull_request' && github.ref == 'refs/heads/master'",
            deploy,
        )

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
