"""Release adapter contracts that do not contact GitHub or compile Rust."""

import json
from pathlib import Path
import runpy
import tempfile
import unittest


ADAPTER = Path(__file__).resolve().parents[1] / "scripts" / "release"
RELEASE = runpy.run_path(str(ADAPTER))


class ReleaseAdapterContracts(unittest.TestCase):
    def test_package_version_edits_only_the_owned_crate(self):
        with tempfile.TemporaryDirectory(prefix="cantrip-release-meta-") as directory:
            cargo = Path(directory) / "Cargo.toml"
            lock = Path(directory) / "Cargo.lock"
            cargo.write_text(
                '[package]\nname = "cantrip"\nversion = "0.1.0"\nedition = "2021"\n\n'
                '[dependencies]\nanyhow = "1"\n'
            )
            lock.write_text(
                "version = 4\n\n[[package]]\nname = \"anyhow\"\nversion = \"1.0.0\"\n\n"
                "[[package]]\nname = \"cantrip\"\nversion = \"0.1.0\"\n"
            )
            RELEASE["set_package_version"](cargo, "cantrip", "0.1.1")
            RELEASE["set_package_version"](lock, "cantrip", "0.1.1", lock=True)
            self.assertIn('name = "cantrip"\nversion = "0.1.1"', cargo.read_text())
            self.assertIn('name = "anyhow"\nversion = "1.0.0"', lock.read_text())
            self.assertIn('name = "cantrip"\nversion = "0.1.1"', lock.read_text())
            self.assertNotIn('version = "0.1.0"', cargo.read_text())

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
