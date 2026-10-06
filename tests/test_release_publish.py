"""Release adapter contracts that do not contact GitHub or compile Rust."""

import json
from pathlib import Path
import runpy
import tomllib
import tempfile
import unittest


ADAPTER = Path(__file__).resolve().parents[1] / "scripts" / "release"
RELEASE = runpy.run_path(str(ADAPTER))


class ReleaseAdapterContracts(unittest.TestCase):
    def test_release_version_updates_one_authority_and_every_owned_lock_entry(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-release-meta-") as directory:
            cargo = Path(directory) / "Cargo.toml"
            lock = Path(directory) / "Cargo.lock"
            cargo.write_text(
                '[package]\nname = "cantrip"\nversion.workspace = true\nedition = "2021"\n\n'
                '[workspace.package]\nversion = "0.1.0"\n\n'
                '[dependencies]\nanyhow = "1"\ncantrip-engine = { path = "crates/engine" }\n'
            )
            third_party = (
                '[[package]]\nname = "anyhow"\nversion = "1.0.0"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n\n'
            )
            lock.write_text(
                "version = 4\n\n" + third_party
                + '[[package]]\nname = "cantrip"\nversion = "0.1.0"\n'
                'dependencies = ["anyhow", "cantrip-engine"]\n\n'
                '[[package]]\nname = "cantrip-engine"\nversion = "0.1.0"\n'
                'dependencies = ["anyhow"]\n'
            )
            RELEASE["set_workspace_version"](cargo, "0.1.1")
            RELEASE["set_locked_versions"](lock, "0.1.1")
            manifest = tomllib.loads(cargo.read_text())
            self.assertEqual(manifest["package"]["version"], {"workspace": True})
            self.assertEqual(manifest["workspace"]["package"]["version"], "0.1.1")
            self.assertEqual(manifest["dependencies"]["cantrip-engine"], {"path": "crates/engine"})
            self.assertIn(third_party, lock.read_text())
            locked = tomllib.loads(lock.read_text())["package"]
            self.assertEqual(
                {entry["name"]: entry["version"] for entry in locked},
                {"anyhow": "1.0.0", "cantrip": "0.1.1", "cantrip-engine": "0.1.1"},
            )

    def test_incomplete_or_ambiguous_owned_lockfile_is_not_partially_updated(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-release-meta-") as directory:
            lock = Path(directory) / "Cargo.lock"
            root = 'version = 4\n\n[[package]]\nname = "cantrip"\nversion = "0.1.0"\n'
            for extra in (
                "",
                '\n[[package]]\nname = "cantrip-engine"\nversion = "0.1.0"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n',
                '\n[[package]]\nname = "cantrip"\nversion = "0.2.0"\n',
            ):
                with self.subTest(extra=extra):
                    lock.write_text(root + extra)
                    with self.assertRaises(RELEASE["ReleaseError"]):
                        RELEASE["set_locked_versions"](lock, "0.1.1")
                    self.assertEqual(lock.read_text(), root + extra)

    def test_attested_checksums_exclude_the_provenance_bundle(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-release-sums-") as directory:
            root = Path(directory)
            (root / "release.json").write_text("{}\n")
            (root / "provenance.json").write_text("attestation-bundle\n")
            (root / "archive.tar.gz").write_bytes(b"archive")
            RELEASE["checksums"](root)
            names = {
                line.split("  ", 1)[1]
                for line in (root / "SHA256SUMS").read_text().splitlines()
            }
            self.assertEqual(names, {"archive.tar.gz", "release.json"})
            self.assertTrue((root / "provenance.json").is_file())


if __name__ == "__main__":
    unittest.main()
