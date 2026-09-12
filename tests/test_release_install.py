"""Filesystem/process contracts for the downloaded bundle's binary installer."""

from contextlib import contextmanager
import hashlib
import os
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "contrib" / "install.sh"


@unittest.skipUnless(sys.platform == "linux", "release installer requires Linux /proc")
class ReleaseInstallationContracts(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cantrip-release-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.prefix = self.root / "selected prefix"
        self.target = self.prefix / "bin" / "cantrip"
        self.backups = self.root / "backups"
        self.backups.mkdir(mode=0o700)
        self.environment = os.environ.copy()
        self.environment.update(
            HOME=str(self.root / "home"),
            XDG_CONFIG_HOME=str(self.root / "config"),
            XDG_DATA_HOME=str(self.root / "data"),
            XDG_STATE_HOME=str(self.root / "state"),
            XDG_RUNTIME_DIR=str(self.root / "runtime"),
            DBUS_SESSION_BUS_ADDRESS=f"unix:path={self.root}/no-session-bus",
            PATH=os.defpath,
        )
        for name in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"):
            Path(self.environment[name]).mkdir(mode=0o700)
        self.protected = {
            "config/cantrip/config.toml": b'injection = "clipboard"\n',
            "config/systemd/user/cantrip.service": b"personal startup owner\n",
            "config/systemd/user/cantrip.service.d/personal.conf": b"personal override\n",
            "config/desktop/shortcuts.conf": b"personal shortcut\n",
            "data/cantrip/models/weights.bin": b"installed model\x00weights",
            "data/keyrings/login.keyring": b"opaque keyring storage\x00",
            "state/cantrip/transcripts/saved.json": b'{"transcript":"keep me"}\n',
            "state/cantrip/transcripts/saved.wav": b"retained recording\x00",
            "state/cantrip/daemon.log": b"existing diagnostic history\n",
            "runtime/cantrip/interrupted.wav": b"runtime recovery audio\x00",
        }
        for name, content in self.protected.items():
            path = self.root / name
            path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            path.write_bytes(content)
            path.chmod(0o600)
        self.enablement = self.root / "config/systemd/user/graphical-session.target.wants/cantrip.service"
        self.enablement.parent.mkdir(mode=0o700)
        self.enablement.symlink_to("../cantrip.service")

    def bundle(self, version):
        directory = self.root / f"release {version}"
        directory.mkdir(mode=0o700)
        shutil.copyfile(INSTALLER, directory / "install.sh")
        (directory / "install.sh").chmod(0o755)
        executable = directory / "cantrip"
        executable.write_text(f"#!/bin/sh\nprintf '%s\\n' '{version}'\n")
        executable.chmod(0o755)
        checksums = "".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
            for path in sorted(directory.iterdir())
        )
        (directory / "checksums.txt").write_text(checksums)
        return directory

    def invoke(self, bundle, operation, backup=None, prefix=None, environment=None):
        command = [str(bundle / "install.sh"), operation, "--prefix", str(prefix or self.prefix)]
        if backup is not None:
            command.extend(["--backup", str(backup)])
        return subprocess.run(
            command,
            cwd=self.root,
            env=environment or self.environment,
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
        )

    def succeeds(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def refuses(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def version(self, path):
        return subprocess.check_output([str(path)], env=self.environment, text=True, timeout=5).strip()

    def assert_operator_state_preserved(self):
        for name, content in self.protected.items():
            path = self.root / name
            self.assertEqual(path.read_bytes(), content, name)
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600, name)
        self.assertTrue(self.enablement.is_symlink())
        self.assertEqual(os.readlink(self.enablement), "../cantrip.service")

    def installed(self):
        original = self.bundle("original")
        self.succeeds(self.invoke(original, "install"))
        return original

    def failing_command_environment(self, command):
        directory = self.root / f"failed-{command}"
        directory.mkdir(mode=0o700)
        executable = directory / command
        executable.write_text("#!/bin/sh\nexit 73\n")
        executable.chmod(0o755)
        return {**self.environment, "PATH": f"{directory}{os.pathsep}{os.defpath}"}

    @contextmanager
    def running_executable(self, path):
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        shutil.copyfile(shutil.which("sleep"), path)
        path.chmod(0o755)
        process = subprocess.Popen([str(path), "60"], env=self.environment)
        try:
            self.assertIsNone(process.poll())
            yield process
        finally:
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=5)

    def test_lifecycle_retains_backup_operator_data_and_existing_startup_owner(self):
        original = self.installed()
        self.assertEqual(self.version(self.target), "original")
        self.assert_operator_state_preserved()

        updated = self.bundle("updated")
        backup = self.backups / "before update"
        self.succeeds(self.invoke(updated, "update", backup))
        self.assertEqual(self.version(self.target), "updated")
        self.assertEqual(self.version(backup), "original")
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o700)
        self.assert_operator_state_preserved()

        self.succeeds(self.invoke(original, "rollback", backup))
        self.assertEqual(self.version(self.target), "original")
        self.assertEqual(self.version(backup), "original")
        self.assert_operator_state_preserved()

        self.succeeds(self.invoke(original, "uninstall"))
        self.assertFalse(self.target.exists())
        self.assertEqual(self.version(backup), "original")
        self.assert_operator_state_preserved()

    def test_existing_install_and_occupied_backup_are_never_overwritten(self):
        self.installed()
        updated = self.bundle("updated")
        backup = self.backups / "occupied"
        backup.write_bytes(b"an unrelated existing file\n")
        self.refuses(self.invoke(updated, "install"))
        self.refuses(self.invoke(updated, "update", backup))
        self.assertEqual(self.version(self.target), "original")
        self.assertEqual(backup.read_bytes(), b"an unrelated existing file\n")
        self.assert_operator_state_preserved()

    def test_symlink_binary_is_not_followed_or_removed(self):
        bundle = self.bundle("new")
        victim = self.root / "unrelated executable"
        victim.write_bytes(b"keep this file\n")
        victim.chmod(0o755)
        self.target.parent.mkdir(mode=0o700, parents=True)
        self.target.symlink_to(victim)
        self.refuses(self.invoke(bundle, "install"))
        self.refuses(self.invoke(bundle, "update", self.backups / "new"))
        self.refuses(self.invoke(bundle, "uninstall"))
        self.assertTrue(self.target.is_symlink())
        self.assertEqual(os.readlink(self.target), str(victim))
        self.assertEqual(victim.read_bytes(), b"keep this file\n")
        self.assertFalse((self.backups / "new").exists())
        self.assert_operator_state_preserved()

    def test_symlink_ancestors_and_backup_paths_are_refused(self):
        self.installed()
        updated = self.bundle("updated")
        alias = self.root / "prefix alias"
        alias.symlink_to(self.prefix, target_is_directory=True)
        backup_alias = self.root / "backup alias"
        backup_alias.symlink_to(self.backups, target_is_directory=True)
        dangling = self.backups / "dangling"
        dangling.symlink_to(self.root / "missing unrelated file")
        self.refuses(self.invoke(updated, "update", self.backups / "prefix-backup", prefix=alias))
        self.refuses(self.invoke(updated, "update", backup_alias / "ancestor-backup"))
        self.refuses(self.invoke(updated, "update", dangling))
        self.assertEqual(self.version(self.target), "original")
        self.assertFalse((self.backups / "prefix-backup").exists())
        self.assertFalse((self.backups / "ancestor-backup").exists())
        self.assertTrue(dangling.is_symlink())
        self.assertFalse((self.root / "missing unrelated file").exists())
        self.assert_operator_state_preserved()

    def test_hardlinked_target_cannot_be_updated_or_uninstalled(self):
        self.installed()
        alias = self.root / "shared executable"
        os.link(self.target, alias)
        updated = self.bundle("updated")
        backup = self.backups / "before update"
        self.refuses(self.invoke(updated, "update", backup))
        self.refuses(self.invoke(updated, "uninstall"))
        self.assertTrue(self.target.samefile(alias))
        self.assertEqual(self.version(alias), "original")
        self.assertFalse(backup.exists())
        self.assert_operator_state_preserved()

    def test_unsafe_writable_target_and_destination_are_refused(self):
        self.installed()
        updated = self.bundle("updated")
        backup = self.backups / "before update"
        self.target.chmod(0o777)
        self.refuses(self.invoke(updated, "update", backup))
        self.assertEqual(self.version(self.target), "original")
        self.assertEqual(stat.S_IMODE(self.target.stat().st_mode), 0o777)
        self.target.chmod(0o755)
        self.target.parent.chmod(0o777)
        self.refuses(self.invoke(updated, "update", backup))
        self.assertEqual(self.version(self.target), "original")
        self.assertFalse(backup.exists())
        self.assert_operator_state_preserved()

    def test_user_owned_sticky_world_writable_ancestor_is_accepted(self):
        sticky = self.root / "private-tmp"
        sticky.mkdir()
        os.chmod(sticky, 0o1777)
        prefix = sticky / "selected prefix"
        bundle = self.bundle("new")
        self.succeeds(self.invoke(bundle, "install", prefix=prefix))
        self.assertEqual(self.version(prefix / "bin" / "cantrip"), "new")
        self.assert_operator_state_preserved()

    def test_user_owned_world_writable_ancestor_without_sticky_is_refused(self):
        writable = self.root / "shared"
        writable.mkdir()
        os.chmod(writable, 0o0777)
        prefix = writable / "selected prefix"
        bundle = self.bundle("new")
        result = self.invoke(bundle, "install", prefix=prefix)
        self.refuses(result)
        self.assertIn("Directory is group/world-writable:", result.stderr)
        self.assertFalse((prefix / "bin" / "cantrip").exists())
        self.assert_operator_state_preserved()

    def test_corrupt_bundle_fails_before_backup_or_replacement(self):
        self.installed()
        updated = self.bundle("updated")
        (updated / "cantrip").write_bytes(b"corrupted after checksum generation\n")
        backup = self.backups / "before update"
        self.refuses(self.invoke(updated, "update", backup))
        self.assertEqual(self.version(self.target), "original")
        self.assertFalse(backup.exists())
        self.assert_operator_state_preserved()

    def test_failed_staging_keeps_old_binary_and_allows_a_clean_retry(self):
        self.installed()
        updated = self.bundle("updated")
        backup = self.backups / "before update"
        self.refuses(self.invoke(updated, "update", backup, environment=self.failing_command_environment("install")))
        self.assertEqual(self.version(self.target), "original")
        self.assertFalse(backup.exists())
        self.assert_operator_state_preserved()
        self.succeeds(self.invoke(updated, "update", backup))
        self.assertEqual(self.version(self.target), "updated")
        self.assertEqual(self.version(backup), "original")

    def test_failed_replacement_preserves_published_backup_and_old_binary(self):
        self.installed()
        updated = self.bundle("updated")
        backup = self.backups / "before update"
        self.refuses(self.invoke(updated, "update", backup, environment=self.failing_command_environment("mv")))
        self.assertEqual(self.version(self.target), "original")
        self.assertEqual(self.version(backup), "original")
        self.assert_operator_state_preserved()
        self.refuses(self.invoke(updated, "update", backup))
        self.assertEqual(self.version(self.target), "original")
        self.assertEqual(self.version(backup), "original")
        self.succeeds(self.invoke(updated, "rollback", backup))

    def test_backup_appearing_during_staging_is_not_overwritten(self):
        self.installed()
        updated = self.bundle("updated")
        backup = self.backups / "chosen backup"
        commands = self.root / "concurrent-backup"
        commands.mkdir(mode=0o700)
        wrapper = commands / "install"
        wrapper.write_text(
            '#!/bin/sh\n"$REAL_INSTALL" "$@" || exit "$?"\n'
            'if [ ! -e "$RACE_BACKUP" ]; then\n'
            '  (set -C; printf "%s\\n" "another caller owns this file" > "$RACE_BACKUP")\n'
            'fi\n'
        )
        wrapper.chmod(0o755)
        environment = {
            **self.environment,
            "PATH": f"{commands}{os.pathsep}{os.defpath}",
            "REAL_INSTALL": shutil.which("install"),
            "RACE_BACKUP": str(backup),
        }
        self.refuses(self.invoke(updated, "update", backup, environment=environment))
        self.assertEqual(self.version(self.target), "original")
        self.assertEqual(backup.read_text(), "another caller owns this file\n")
        self.assert_operator_state_preserved()

    def test_target_replaced_by_symlink_during_staging_is_not_overwritten(self):
        self.installed()
        updated = self.bundle("updated")
        backup = self.backups / "chosen backup"
        victim = self.root / "unrelated file"
        victim.write_bytes(b"unrelated contents\n")
        commands = self.root / "concurrent-target"
        commands.mkdir(mode=0o700)
        wrapper = commands / "install"
        wrapper.write_text(
            '#!/bin/sh\n"$REAL_INSTALL" "$@" || exit "$?"\n'
            'rm -- "$RACE_TARGET"\nln -s -- "$RACE_VICTIM" "$RACE_TARGET"\n'
        )
        wrapper.chmod(0o755)
        environment = {
            **self.environment,
            "PATH": f"{commands}{os.pathsep}{os.defpath}",
            "REAL_INSTALL": shutil.which("install"),
            "RACE_TARGET": str(self.target),
            "RACE_VICTIM": str(victim),
        }
        self.refuses(self.invoke(updated, "update", backup, environment=environment))
        self.assertTrue(self.target.is_symlink())
        self.assertEqual(os.readlink(self.target), str(victim))
        self.assertEqual(victim.read_bytes(), b"unrelated contents\n")
        self.assertFalse(backup.exists())
        self.assert_operator_state_preserved()

    def test_live_destination_process_is_not_stopped_or_replaced(self):
        bundle = self.bundle("new")
        rollback = self.backups / "rollback source"
        shutil.copyfile(bundle / "cantrip", rollback)
        rollback.chmod(0o700)
        backup = self.backups / "before update"
        with self.running_executable(self.target) as process:
            original = self.target.read_bytes()
            self.refuses(self.invoke(bundle, "update", backup))
            self.refuses(self.invoke(bundle, "rollback", rollback))
            self.refuses(self.invoke(bundle, "uninstall"))
            self.assertIsNone(process.poll())
            self.assertEqual(self.target.read_bytes(), original)
            self.assertFalse(backup.exists())
            self.assertEqual(self.version(rollback), "new")
            self.assert_operator_state_preserved()

    def test_live_selected_socket_is_refused_but_stale_socket_is_preserved(self):
        self.installed()
        updated = self.bundle("updated")
        backup = self.backups / "before update"
        path = Path(self.environment["XDG_RUNTIME_DIR"]) / "cantrip/cantrip.sock"
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(path))
            listener.listen(1)
            self.refuses(self.invoke(updated, "update", backup))
            self.assertEqual(self.version(self.target), "original")
            self.assertFalse(backup.exists())
            self.assertTrue(stat.S_ISSOCK(path.stat().st_mode))
            self.assert_operator_state_preserved()
        self.succeeds(self.invoke(updated, "update", backup))
        self.assertEqual(self.version(self.target), "updated")
        self.assertTrue(stat.S_ISSOCK(path.stat().st_mode))
        self.assert_operator_state_preserved()

    def test_unrelated_live_prefix_and_runtime_remain_independent(self):
        bundle = self.bundle("new")
        other_binary = self.root / "another installation/bin/cantrip"
        other_socket = self.root / "another-runtime/cantrip/cantrip.sock"
        other_socket.parent.mkdir(mode=0o700, parents=True)
        with self.running_executable(other_binary) as process, socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(other_socket))
            listener.listen(1)
            self.succeeds(self.invoke(bundle, "install"))
            self.assertEqual(self.version(self.target), "new")
            self.succeeds(self.invoke(bundle, "uninstall"))
            self.assertIsNone(process.poll())
            self.assertTrue(stat.S_ISSOCK(other_socket.stat().st_mode))
            self.assert_operator_state_preserved()


if __name__ == "__main__":
    unittest.main()
