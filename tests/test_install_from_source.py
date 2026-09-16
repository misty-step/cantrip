"""Contracts for scripts/install-from-source: build this tree, then replace."""

import os
from pathlib import Path
import shlex
import socket
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "scripts" / "install-from-source"


@unittest.skipUnless(sys.platform == "linux", "source installer inspects Linux /proc")
class InstallFromSourceContracts(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cantrip-src-install-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        (self.repo / "Cargo.toml").write_text("[package]\nname = \"cantrip\"\nversion = \"0.0.0\"\n")
        (self.repo / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.98.1"\n')
        self.prefix = self.root / "prefix"
        self.dest = self.prefix / "bin" / "cantrip"
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.runtime = self.root / "runtime"
        self.runtime.mkdir()
        self.environment = os.environ.copy()
        self.environment.update(
            HOME=str(self.root / "home"),
            XDG_RUNTIME_DIR=str(self.runtime),
            PATH=f"{self.tools}{os.pathsep}{os.defpath}",
        )
        (self.root / "home").mkdir()
        self.install_fake_cargo(b"#!/bin/sh\nprintf 'fake-cantrip\\n'\n")

    def install_fake_cargo(self, payload: bytes) -> None:
        payload_path = self.tools / "payload"
        payload_path.write_bytes(payload)
        cargo = self.tools / "cargo"
        cargo.write_text(
            "\n".join(
                [
                    "#!/bin/sh",
                    "set -e",
                    'printf "%s\\n" "$*" > cargo.args',
                    "mkdir -p target/release",
                    f"cp -- {shlex.quote(str(payload_path))} target/release/cantrip",
                    "chmod 755 target/release/cantrip",
                ]
            )
            + "\n"
        )
        cargo.chmod(0o755)

    def invoke(self, extra_env=None, cwd=None):
        env = self.environment if extra_env is None else extra_env
        return subprocess.run(
            [str(INSTALLER), "--prefix", str(self.prefix)],
            cwd=cwd or self.repo,
            env=env,
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
        )

    def succeeds(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def refuses(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_builds_release_locked_then_installs(self):
        result = self.invoke()
        self.succeeds(result)
        args = (self.repo / "cargo.args").read_text()
        self.assertIn("--release", args)
        self.assertIn("--locked", args)
        self.assertTrue(self.dest.is_file())
        self.assertFalse(self.dest.is_symlink())
        self.assertEqual(stat.S_IMODE(self.dest.stat().st_mode) & 0o777, 0o755)
        self.assertEqual(
            subprocess.check_output([str(self.dest)], text=True, timeout=5).strip(),
            "fake-cantrip",
        )
        self.assertIn("installed:", result.stdout)

    def test_update_replaces_a_regular_file(self):
        self.succeeds(self.invoke())
        self.install_fake_cargo(b"#!/bin/sh\nprintf 'second\\n'\n")
        result = self.invoke()
        self.succeeds(result)
        self.assertEqual(
            subprocess.check_output([str(self.dest)], text=True, timeout=5).strip(),
            "second",
        )

    def test_symlink_bin_directory_is_refused(self):
        victim = self.root / "checkout-bin"
        victim.mkdir()
        self.dest.parent.parent.mkdir(parents=True)
        self.dest.parent.symlink_to(victim, target_is_directory=True)
        result = self.invoke()
        self.refuses(result)
        self.assertIn("Refusing symlink path", result.stderr)
        self.assertTrue(self.dest.parent.is_symlink())
        self.assertFalse((self.repo / "cargo.args").exists())
        self.assertEqual(list(victim.iterdir()), [])

    def test_symlink_destination_is_refused_and_left_in_place(self):
        victim = self.root / "checkout-binary"
        victim.write_bytes(b"keep me\n")
        victim.chmod(0o755)
        self.dest.parent.mkdir(parents=True)
        self.dest.symlink_to(victim)
        result = self.invoke()
        self.refuses(result)
        self.assertIn("Refusing symlink destination", result.stderr)
        self.assertTrue(self.dest.is_symlink())
        self.assertEqual(victim.read_bytes(), b"keep me\n")
        self.assertFalse((self.repo / "cargo.args").exists())

    def test_running_destination_is_refused(self):
        self.succeeds(self.invoke())
        shutil.copyfile(shutil.which("sleep"), self.dest)
        self.dest.chmod(0o755)
        process = subprocess.Popen([str(self.dest), "60"], env=self.environment)
        try:
            self.assertIsNone(process.poll())
            result = self.invoke()
        finally:
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=5)
        self.refuses(result)
        self.assertIn("still running", result.stderr)

    def test_live_socket_is_refused(self):
        sock_dir = self.runtime / "cantrip"
        sock_dir.mkdir()
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(sock.close)
        sock.bind(str(sock_dir / "cantrip.sock"))
        sock.listen(1)
        result = self.invoke()
        self.refuses(result)
        self.assertIn("live Cantrip socket", result.stderr)
        self.assertFalse(self.dest.exists())

    def test_refuses_outside_the_repository_root(self):
        result = self.invoke(cwd=self.root)
        self.refuses(result)
        self.assertIn("repository root", result.stderr)

    def test_failed_build_leaves_destination_untouched(self):
        self.succeeds(self.invoke())
        original = self.dest.read_bytes()
        cargo = self.tools / "cargo"
        cargo.write_text("#!/bin/sh\nexit 17\n")
        cargo.chmod(0o755)
        result = self.invoke()
        self.refuses(result)
        self.assertEqual(self.dest.read_bytes(), original)
