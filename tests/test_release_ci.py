"""Exercise the canonical check command's independent CI tiers and failure gate."""

import os
from pathlib import Path
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


if __name__ == "__main__":
    unittest.main()
