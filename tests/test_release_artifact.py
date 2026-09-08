"""Release boundaries exercised with real Git checkouts and small native ELF files."""

import hashlib
import json
import os
from pathlib import Path
import platform
import runpy
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest


PACKAGER = Path(__file__).resolve().parents[1] / "scripts" / "package-release"
PACKAGE = runpy.run_path(str(PACKAGER))
VERSION = "0.1.0"
EPOCH = 1710000000
ROOT_NAME = f"cantrip-v{VERSION}-x86_64-unknown-linux-gnu"
PAYLOAD = {
    "cantrip", "install.sh", "cantrip.service", "LICENSE", "INSTALLATION.md",
    "USAGE.md", "CONFIGURATION.md", "DESKTOP.md", "PRIVACY.md",
    "sample.wav", "manifest.json", "checksums.txt",
}


def write_elf(path, version=VERSION, requirements=None):
    """A native syscall-only --version fixture; optional real ELF ABI tables."""
    message = f"cantrip {version}\n".encode()
    code = (
        b"\xb8\x01\x00\x00\x00"  # mov eax, SYS_write
        b"\xbf\x01\x00\x00\x00"  # mov edi, STDOUT_FILENO
        b"\x48\x8d\x35\x10\x00\x00\x00"  # lea rsi, [rip + 16]
        b"\xba" + struct.pack("<I", len(message)) + b"\x0f\x05"
        b"\xb8\x3c\x00\x00\x00\x31\xff\x0f\x05"  # exit(0)
    )
    base = 0x400000
    program_count = 3 if requirements else 1
    code_offset = 64 + program_count * 56
    data = bytearray(code_offset)
    data.extend(code + message)
    sections = [(0,) * 10]
    program_headers = []
    section_offset = 0
    section_names_index = 0

    if requirements:
        names = b"\0.dynstr\0.gnu.version_r\0.dynamic\0.interp\0.shstrtab\0"
        strings = bytearray(b"\0")
        string_offsets = {}
        for library, versions in requirements.items():
            for name in [library, *versions]:
                string_offsets[name] = len(strings)
                strings.extend(name.encode() + b"\0")

        def add_section(name, contents, kind, flags, link=0, info=0, alignment=1, entry_size=0):
            data.extend(b"\0" * (-len(data) % alignment))
            offset = len(data)
            data.extend(contents)
            sections.append((
                names.index(name.encode() + b"\0"), kind, flags,
                base + offset if flags & 2 else 0, offset, len(contents),
                link, info, alignment, entry_size,
            ))
            return offset

        strings_offset = add_section(".dynstr", strings, 3, 2)
        needs = bytearray()
        for library_index, (library, versions) in enumerate(requirements.items()):
            size = 16 + len(versions) * 16
            next_library = size if library_index + 1 < len(requirements) else 0
            needs.extend(struct.pack("<HHIII", 1, len(versions), string_offsets[library], 16, next_library))
            for version_index, name in enumerate(versions):
                next_version = 16 if version_index + 1 < len(versions) else 0
                needs.extend(struct.pack("<IHHII", 0, 0, version_index + 2, string_offsets[name], next_version))
        needs_offset = add_section(".gnu.version_r", needs, 0x6FFFFFFE, 2, 1, len(requirements), 8)
        dynamic = [(1, string_offsets[library]) for library in requirements]
        dynamic += [
            (5, base + strings_offset), (10, len(strings)),
            (0x6FFFFFFE, base + needs_offset), (0x6FFFFFFF, len(requirements)), (0, 0),
        ]
        dynamic_bytes = b"".join(struct.pack("<QQ", *entry) for entry in dynamic)
        dynamic_offset = add_section(".dynamic", dynamic_bytes, 6, 2, 1, alignment=8, entry_size=16)
        interpreter = b"/lib64/ld-linux-x86-64.so.2\0"
        interpreter_offset = add_section(".interp", interpreter, 1, 2)
        section_names_index = len(sections)
        add_section(".shstrtab", names, 3, 0)
        data.extend(b"\0" * (-len(data) % 8))
        section_offset = len(data)
        for section in sections:
            data.extend(struct.pack("<IIQQQQIIQQ", *section))
        program_headers = [
            (3, 4, interpreter_offset, base + interpreter_offset, base + interpreter_offset, len(interpreter), len(interpreter), 1),
            (2, 4, dynamic_offset, base + dynamic_offset, base + dynamic_offset, len(dynamic_bytes), len(dynamic_bytes), 8),
        ]

    header = struct.pack(
        "<16sHHIQQQIHHHHHH", b"\x7fELF\x02\x01\x01" + b"\0" * 9,
        2, 62, 1, base + code_offset, 64, section_offset, 0,
        64, 56, program_count, 64, len(sections) if requirements else 0, section_names_index,
    )
    program_headers.insert(1 if requirements else 0, (1, 5, 0, base, base, len(data), len(data), 4096))
    data[:code_offset] = header + b"".join(struct.pack("<IIQQQQQQ", *entry) for entry in program_headers)
    path.write_bytes(data)
    path.chmod(0o755)


def checksum_map(contents):
    return {name: digest for digest, name in (line.split("  ", 1) for line in contents.splitlines())}


@unittest.skipUnless(
    sys.platform.startswith("linux") and platform.machine() in ("x86_64", "AMD64")
    and shutil.which("git") and shutil.which("readelf"),
    "release packaging requires Linux x86-64, git, and readelf",
)
class ReleaseArtifactContracts(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cantrip-release-test-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.root = self.directory / "source"
        self.root.mkdir()
        self.binary = self.directory / "cantrip"
        self.output = self.directory / "release"
        self.environment = {
            **os.environ,
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_DATE": f"{EPOCH} +0000", "GIT_COMMITTER_DATE": f"{EPOCH} +0000",
        }
        sources = {
            "Cargo.toml": '[package]\nname = "cantrip"\nversion = "0.1.0"\n',
            "rust-toolchain.toml": '[toolchain]\nchannel = "1.98.1"\n',
            "src/main.rs": "fn main() {}\n",
            ".gitignore": ".env\nmodels/\ntranscripts/\n",
            "contrib/install.sh": "#!/usr/bin/env bash\nexit 0\n",
            "contrib/cantrip.service": "[Service]\nExecStart=%h/.local/bin/cantrip daemon\n",
            "docs/INSTALLATION.md": "Public installation instructions.\n",
            "docs/USAGE.md": "Recording and recovery instructions.\n",
            "docs/CONFIGURATION.md": "Configuration reference.\n",
            "docs/DESKTOP.md": "Supported desktop setup.\n",
            "docs/PRIVACY.md": "Privacy and retained data.\n",
            "LICENSE": "Public license.\n",
            "samples/jfk.wav": "Public sample bytes.\n",
        }
        for name, contents in sources.items():
            destination = self.root / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(contents)
        (self.root / "scripts").mkdir()
        shutil.copyfile(PACKAGER, self.root / "scripts" / "package-release")
        self.git("init", "--quiet")
        self.commit()
        write_elf(self.binary)

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false",
             "-c", "user.name=Release Fixture", "-c", "user.email=release@example.invalid", *args],
            cwd=self.root, env=self.environment, check=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        ).stdout.strip()

    def commit(self):
        self.git("add", "--all")
        self.git("commit", "--quiet", "-m", "release fixture")
        self.revision = self.git("rev-parse", "HEAD")

    def package(self, *, version=VERSION, revision=None, output=None):
        return subprocess.run(
            [sys.executable, str(self.root / "scripts" / "package-release"),
             "--binary", str(self.binary), "--output", str(output or self.output),
             "--version", version, "--source-revision", revision or self.revision],
            env=self.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )

    def assert_refused(self, result, message):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(message, result.stderr)
        self.assertFalse(self.output.exists(), "a rejected release exposed output assets")

    def test_requested_cargo_and_binary_versions_must_agree(self):
        self.assert_refused(self.package(version="0.2.0"), "Cargo.toml")
        write_elf(self.binary, version="0.2.0")
        self.assert_refused(self.package(), "binary --version")

    def test_source_revision_must_identify_the_checkout(self):
        self.assert_refused(self.package(revision="0" * 40), "checkout HEAD")
        self.assert_refused(self.package(revision=self.revision[:12]), "full lowercase commit SHA")

    def test_dirty_worktree_index_and_untracked_source_cannot_claim_clean_provenance(self):
        source = self.root / "src/main.rs"
        original = source.read_bytes()
        source.write_text("fn main() { panic!(); }\n")
        self.assert_refused(self.package(), "checkout is dirty")
        self.git("add", "src/main.rs")
        self.assert_refused(self.package(), "checkout is dirty")
        source.write_bytes(original)
        self.git("add", "src/main.rs")
        (self.root / "src/untracked.rs").write_text("uncommitted source\n")
        self.assert_refused(self.package(), "checkout is dirty")

    def test_moving_rust_channel_cannot_be_recorded_as_a_pinned_release(self):
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "stable"\n')
        self.commit()
        self.assert_refused(self.package(), "pin an exact Rust release")

    def test_only_public_payload_is_exposed_with_verifiable_deterministic_identity(self):
        private = b"PRIVATE_TOKEN_AND_TRANSCRIPT_NOT_FOR_RELEASE"
        for name in (".env", "models/private.onnx", "transcripts/take.txt"):
            destination = self.root / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(private)
        result = self.package()
        self.assertEqual(result.returncode, 0, result.stderr)
        archive_path = self.output / f"{ROOT_NAME}.tar.gz"
        with tarfile.open(archive_path) as archive:
            members = archive.getmembers()
            self.assertEqual(
                {member.name.rstrip("/") for member in members},
                {ROOT_NAME, *(f"{ROOT_NAME}/{name}" for name in PAYLOAD)},
            )
            self.assertEqual(len(members), len(PAYLOAD) + 1)
            contents = {}
            for member in members:
                self.assertEqual((member.uid, member.gid, member.mtime), (0, 0, EPOCH))
                if member.isdir():
                    self.assertEqual(member.mode, 0o755)
                    continue
                self.assertTrue(member.isfile(), member.name)
                name = Path(member.name).name
                self.assertEqual(member.mode, 0o755 if name in ("cantrip", "install.sh") else 0o644)
                contents[name] = archive.extractfile(member).read()
                self.assertNotIn(private, contents[name])
        self.assertEqual(contents["cantrip"], self.binary.read_bytes())
        self.assertEqual(contents["sample.wav"], (self.root / "samples/jfk.wav").read_bytes())
        expected_checksums = {
            name: hashlib.sha256(data).hexdigest()
            for name, data in contents.items() if name != "checksums.txt"
        }
        self.assertEqual(checksum_map(contents["checksums.txt"].decode()), expected_checksums)
        manifest = json.loads(contents["manifest.json"])
        release = json.loads((self.output / "release.json").read_bytes())
        self.assertEqual(manifest["source_revision"], self.revision)
        self.assertEqual(manifest["binary_sha256"], expected_checksums["cantrip"])
        self.assertEqual(manifest["version"], VERSION)
        self.assertEqual(release.pop("archive"), {
            "name": archive_path.name, "sha256": hashlib.sha256(archive_path.read_bytes()).hexdigest(),
        })
        self.assertEqual(release, manifest)
        self.assertEqual(checksum_map((self.output / "SHA256SUMS").read_text()), {
            path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (archive_path, self.output / "release.json")
        })
        # Neither binary filesystem metadata nor ignored private state may alter
        # a repeated release of the same bytes and committed source identity.
        os.utime(self.binary, (EPOCH + 100, EPOCH + 100))
        self.binary.chmod(0o711)
        repeated = self.directory / "repeated"
        result = self.package(output=repeated)
        self.assertEqual(result.returncode, 0, result.stderr)
        for name in (archive_path.name, "release.json", "SHA256SUMS"):
            self.assertEqual((self.output / name).read_bytes(), (repeated / name).read_bytes())

    def test_missing_installer_payload_is_not_fabricated(self):
        (self.root / "contrib/install.sh").unlink()
        self.commit()
        self.assert_refused(self.package(), "contrib/install.sh")
        self.assertFalse((self.root / "contrib/install.sh").exists())

    def test_committed_symlink_cannot_expose_ignored_private_contents(self):
        (self.root / ".env").write_text("private credential\n")
        installer = self.root / "contrib/install.sh"
        installer.unlink()
        installer.symlink_to("../.env")
        self.commit()
        self.assert_refused(self.package(), "committed regular file")

    def test_preexisting_release_asset_is_preserved_without_partial_publication(self):
        self.output.mkdir()
        asset = self.output / "release.json"
        asset.write_bytes(b"previous release provenance\n")
        result = self.package()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to overwrite", result.stderr)
        self.assertEqual(asset.read_bytes(), b"previous release provenance\n")
        self.assertEqual({path.name for path in self.output.iterdir()}, {"release.json"})

    def test_actual_elf_requirements_cannot_exceed_or_evade_the_baseline(self):
        for requirements, message in (
            ({"libc.so.6": ["GLIBC_2.40"]}, "baseline GLIBC_2.39"),
            ({"libstdc++.so.6": ["GLIBCXX_3.4.34"]}, "baseline GLIBCXX_3.4.33"),
            ({"libc.so.6": ["GLIBC_PRIVATE"]}, "unsupported ABI requirement"),
            ({"libcudart.so.12": ["CUDA_12.0"]}, "CPU-only release baseline"),
        ):
            with self.subTest(requirements=requirements):
                write_elf(self.binary, requirements=requirements)
                self.assert_refused(self.package(), message)
        write_elf(self.binary, requirements={"libc.so.6": ["GLIBC_2.39"]})
        inspected = PACKAGE["inspect_elf"](self.binary)
        self.assertEqual(inspected["elf"]["required_symbol_versions"], {"libc.so.6": ["GLIBC_2.39"]})
        self.assertEqual(inspected["distribution"], "Ubuntu 24.04")
        self.assertEqual(inspected["glibc_min"], "2.39")
        self.assertEqual(inspected["packages"], ["libc6"])


if __name__ == "__main__":
    unittest.main()
