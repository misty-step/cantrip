from contextlib import contextmanager, redirect_stdout
import copy
import errno
import json
from io import StringIO
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import patch

from install import PLUGIN_ID, apply_plan, atomic_write, main, menu_insert, merge_layout, parse_jsonc, plan, safe_path, tree_snapshot, write_private


class InstallationContracts(unittest.TestCase):
    def test_menu_update_preserves_unrelated_jsonc_and_is_repeatable(self):
        original = '{\n  "version": 1,\n  "items": {\n    // My launcher entry stays verbatim.\n    "personal.notes": {"action": "editor https://example.test/notes", "label": "Notes"},\n  }\n}\n'
        additions = {"cantrip": {"label": "Cantrip"}, "cantrip.recordings": {"action": "cantrip actions"}}
        updated = menu_insert(original, additions)
        self.assertIn('    // My launcher entry stays verbatim.\n    "personal.notes": {"action": "editor https://example.test/notes", "label": "Notes"},', updated)
        expected = parse_jsonc(original)
        expected["items"].update(additions)
        self.assertEqual(parse_jsonc(updated), expected)
        self.assertEqual(menu_insert(updated, additions), updated)

    def test_existing_unmanaged_menu_ids_are_not_overwritten(self):
        with self.assertRaises(ValueError):
            menu_insert('{"cantrip":{"action":"my-personal-command"}}', {"cantrip": {"label": "Cantrip"}})

    def test_unrelated_entries_in_managed_menu_are_not_silently_deleted(self):
        additions = {"cantrip": {"label": "Cantrip"}}
        managed = menu_insert("{\n}\n", additions)
        edited = managed.replace('  "cantrip":', '  "personal.notes": {"action": "editor notes"},\n  "cantrip":', 1)
        self.assertIn("personal.notes", parse_jsonc(edited))
        with self.assertRaises(ValueError):
            menu_insert(edited, additions)

    def test_replacing_widget_keeps_its_position_and_other_configuration(self):
        original = {"version": 1, "idle": {"lock": 300}, "disabledPlugins": ["omarchy.menu"], "bar": {"position": "top", "layout": {"left": [{"id": "custom.menu"}], "right": [{"id": "omarchy.tray"}, {"id": "personal.cantrip", "customSetting": 4}, {"id": "omarchy.power"}]}}}
        expected = copy.deepcopy(original)
        expected["bar"]["layout"]["right"][1]["id"] = PLUGIN_ID
        merged = merge_layout(original, {}, "personal.cantrip")
        self.assertEqual(merged, expected)
        self.assertEqual(merge_layout(merged, {}, "personal.cantrip"), merged)
        self.assertEqual(original["bar"]["layout"]["right"][1]["id"], "personal.cantrip")

    def test_partial_shell_override_keeps_default_right_widgets(self):
        defaults = {"bar": {"layout": {"right": [{"id": "omarchy.power"}]}}}
        merged = merge_layout({"idle": {"lock": 123}}, defaults, None)
        self.assertEqual(merged["bar"]["layout"]["right"], [{"id": "omarchy.power"}, {"id": PLUGIN_ID}])
        self.assertEqual(merged["idle"], {"lock": 123})

    def test_concurrent_edit_is_refused_and_replaced_content_is_backed_up(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "shell.json"
            path.write_text("new external edit")
            with self.assertRaises(ValueError):
                atomic_write(path, "planned content", "old original")
            self.assertEqual(path.read_text(), "new external edit")
            atomic_write(path, "planned content", "new external edit")
            self.assertEqual(path.read_text(), "planned content")
            backups = list(Path(directory).glob("shell.json.before-cantrip-*.bak"))
            self.assertEqual([backup.read_text() for backup in backups], ["new external edit"])
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_backup_is_private_before_any_content_is_written(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "shell.json"
            path.write_text("private configuration")
            opened_modes = []
            real_open = os.open

            def observe_creation(path, flags, mode=0o777, *, dir_fd=None):
                descriptor = real_open(path, flags, mode, dir_fd=dir_fd)
                if os.fsdecode(path).endswith(".bak"):
                    opened_modes.append(os.fstat(descriptor).st_mode & 0o777)
                return descriptor

            previous_umask = os.umask(0)
            try:
                with patch("install.os.open", side_effect=observe_creation):
                    atomic_write(path, "replacement", "private configuration")
            finally:
                os.umask(previous_umask)
            self.assertEqual(opened_modes, [0o600])

    def test_symlink_destination_is_never_followed(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            real = parent / "real"
            real.mkdir()
            (parent / "linked").symlink_to(real, target_is_directory=True)
            with self.assertRaises(ValueError):
                safe_path(parent / "linked" / "shell.json")


class PluginPublicationContracts(unittest.TestCase):
    def installation(self, directory, config_name="config"):
        root = Path(directory)
        config = root / config_name
        defaults = root / "omarchy/config/omarchy/shell.json"
        defaults.parent.mkdir(parents=True)
        defaults.write_text('{"bar":{"layout":{"right":[{"id":"omarchy.power"}]}}}')
        plugin = config / "omarchy/plugins" / PLUGIN_ID
        plugin.mkdir(parents=True)
        old_files = {
            plugin / "manifest.json": json.dumps({"id": PLUGIN_ID, "version": "1.0.0"}),
            plugin / "BarWidget.qml": 'Item { property string version: "old" }\n',
            plugin / "Status.js": "function version() { return 'old'; }\n",
            plugin / "Personal.qml": "// Kept by the operator.\n",
            config / "omarchy/shell.json": '{"idle":{"lock":321}}\n',
            config / "omarchy/extensions/omarchy-menu.jsonc": '{\n  // My menu.\n  "items": {}\n}\n',
        }
        for path, contents in old_files.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)
        return plan(config, root / "omarchy", None), old_files

    def test_staging_failure_never_exposes_a_partial_plugin(self):
        with tempfile.TemporaryDirectory() as directory:
            installation, old_files = self.installation(directory)
            real_open = os.open

            def fail_last_asset(path, flags, mode=0o777, *, dir_fd=None):
                if Path(path).name == "Status.js" and flags & os.O_CREAT:
                    raise OSError(errno.EIO, "simulated staging failure")
                return real_open(path, flags, mode, dir_fd=dir_fd)

            with patch("install.os.open", side_effect=fail_last_asset):
                with self.assertRaises(OSError):
                    apply_plan(installation)
            for path, contents in old_files.items():
                self.assertEqual(path.read_text(), contents)
            self.assertEqual({path.name for path in installation.plugin.parent.iterdir()}, {PLUGIN_ID})

    def test_reference_failure_restores_the_complete_previous_installation(self):
        with tempfile.TemporaryDirectory() as directory:
            installation, old_files = self.installation(directory)
            menu = next(path for path in old_files if path.name == "omarchy-menu.jsonc")
            real_replace = os.replace

            def fail_menu(source, destination):
                if Path(destination) == menu:
                    raise OSError(errno.EIO, "simulated menu publication failure")
                return real_replace(source, destination)

            with patch("install.os.replace", side_effect=fail_menu):
                with self.assertRaises(OSError):
                    apply_plan(installation)
            for path, contents in old_files.items():
                self.assertEqual(path.read_text(), contents)
            self.assertEqual({path.name for path in installation.plugin.iterdir()},
                             {"manifest.json", "BarWidget.qml", "Status.js", "Personal.qml"})

    def test_failed_first_install_removes_its_new_references_and_plugin(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            defaults = root / "omarchy/config/omarchy/shell.json"
            defaults.parent.mkdir(parents=True)
            defaults.write_text('{"bar":{"layout":{"right":[{"id":"omarchy.power"}]}}}')
            installation = plan(root / "config", root / "omarchy", None)
            real_replace = os.replace

            def fail_menu(source, destination):
                if Path(destination).name == "omarchy-menu.jsonc":
                    raise OSError(errno.EIO, "simulated menu publication failure")
                return real_replace(source, destination)

            with patch("install.os.replace", side_effect=fail_menu):
                with self.assertRaises(OSError):
                    apply_plan(installation)
            for path, _, _ in installation.writes:
                self.assertFalse(path.exists())
            self.assertFalse(installation.plugin.exists())

    def test_upgrade_preserves_extra_files_and_keeps_the_old_plugin_outside_discovery(self):
        with tempfile.TemporaryDirectory() as directory:
            installation, old_files = self.installation(directory)
            apply_plan(installation)
            for path, contents, _ in installation.writes:
                self.assertEqual(path.read_text(), contents)
            personal = installation.plugin / "Personal.qml"
            self.assertEqual(personal.read_text(), old_files[personal])
            backups = list(installation.plugin.parent.parent.glob(".cantrip-backup-*/plugin"))
            self.assertEqual(len(backups), 1)
            for path, contents in old_files.items():
                if path.parent == installation.plugin:
                    self.assertEqual((backups[0] / path.name).read_text(), contents)
            self.assertEqual({path.name for path in installation.plugin.parent.iterdir()}, {PLUGIN_ID})

    def test_repeat_install_preserves_user_shell_formatting(self):
        with tempfile.TemporaryDirectory() as directory:
            installation, _ = self.installation(directory)
            apply_plan(installation)
            root = Path(directory)
            shell = root / "config/omarchy/shell.json"
            user_contents = json.dumps(json.loads(shell.read_text()), separators=(",", ":")) + "\n"
            shell.write_text(user_contents)
            apply_plan(plan(root / "config", root / "omarchy", None))
            self.assertEqual(shell.read_text(), user_contents)

    @contextmanager
    def live_installation(self, directory):
        root = Path(directory)
        installation, old_files = self.installation(directory, ".config")
        runtime = root / "runtime"
        control_dir = runtime / "hypr/test_session"
        control_dir.mkdir(parents=True)
        environment = {
            "HOME": str(root), "XDG_CONFIG_HOME": str(root / ".config"),
            "XDG_RUNTIME_DIR": str(runtime), "HYPRLAND_INSTANCE_SIGNATURE": "test_session",
            "WAYLAND_DISPLAY": "wayland-test", "OMARCHY_PATH": str(root / "omarchy"),
        }
        state = {
            "compositor": b'{"locked":false}',
            "shell": {key: False for key in ("locked", "requested", "pending", "sessionLocked", "secure")},
            "instances": [{"id": "test-shell", "pid": os.getpid(),
                           "config_path": str(root / "omarchy/shell/shell.qml")}],
            "environment": environment.copy(),
        }

        def qs(command, **kwargs):
            if "error" in state:
                raise state["error"]
            payload = state["instances"] if command[1] == "list" else state["shell"]
            output = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
            return subprocess.CompletedProcess(command, state.get("returncode", 0), output, b"")

        stopped = threading.Event()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as wayland, \
                socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as control:
            wayland.bind(str(runtime / "wayland-test"))
            wayland.listen()
            control.bind(str(control_dir / ".socket.sock"))
            control.listen()
            control.settimeout(0.05)

            def compositor():
                while not stopped.is_set():
                    try:
                        connection, _ = control.accept()
                    except TimeoutError:
                        continue
                    with connection:
                        connection.settimeout(1)
                        try:
                            if connection.recv(128) == b"j/locked":
                                connection.sendall(state["compositor"])
                        except OSError:
                            pass

            server = threading.Thread(target=compositor)
            server.start()
            try:
                with patch.dict(os.environ, environment, clear=True), \
                        patch("install.subprocess.run", side_effect=qs), \
                        patch("install.process_environment", side_effect=lambda pid: state["environment"]):
                    yield installation, old_files, state
            finally:
                stopped.set()
                server.join()

    def test_unsafe_live_states_leave_the_entire_configuration_unchanged(self):
        cases = [
            ("compositor", b'{"locked":true}'),
            ("compositor", b'{"locked":0}'),
            ("compositor", b'{"locked":true,"locked":false}'),
            ("compositor", b"unknown request"),
            ("shell", {}),
            ("shell", b'{"locked":false'),
            ("returncode", 1),
            ("error", FileNotFoundError("qs unavailable")),
            ("error", subprocess.TimeoutExpired("qs", 2)),
        ]
        for key in ("locked", "requested", "pending", "sessionLocked", "secure"):
            cases.append(("shell", {field: field == key for field in
                                   ("locked", "requested", "pending", "sessionLocked", "secure")}))
        for key, value in cases:
            with self.subTest(key=key, value=value), tempfile.TemporaryDirectory() as directory:
                with self.live_installation(directory) as (installation, _, state):
                    state[key] = value
                    config = Path(directory) / ".config"
                    before = tree_snapshot(config)
                    with self.assertRaises(ValueError):
                        apply_plan(installation)
                    self.assertEqual(tree_snapshot(config), before)

    def test_unidentified_live_sessions_leave_the_entire_configuration_unchanged(self):
        for case in ("missing-display", "forwarded-socket", "wrong-shell-display", "ambiguous-shell"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                with self.live_installation(directory) as (installation, _, state):
                    if case == "missing-display":
                        del os.environ["WAYLAND_DISPLAY"]
                    elif case == "forwarded-socket":
                        os.environ["WAYLAND_SOCKET"] = "3"
                    elif case == "wrong-shell-display":
                        state["environment"]["WAYLAND_DISPLAY"] = "wayland-other"
                    else:
                        state["instances"].append(dict(state["instances"][0], id="other-shell"))
                    config = Path(directory) / ".config"
                    before = tree_snapshot(config)
                    with self.assertRaises(ValueError):
                        apply_plan(installation)
                    self.assertEqual(tree_snapshot(config), before)

    def test_initial_live_refusal_does_not_even_create_the_configuration_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            defaults = root / "omarchy/config/omarchy/shell.json"
            defaults.parent.mkdir(parents=True)
            defaults.write_text('{"bar":{"layout":{"right":[]}}}')
            config = root / ".config"
            with patch.dict(os.environ, {"HOME": str(root)}, clear=True):
                with self.assertRaises(ValueError):
                    apply_plan(plan(config, root / "omarchy", None))
            self.assertFalse(config.exists())

    def test_changed_xdg_directory_does_not_make_the_live_home_configuration_offline(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.live_installation(directory) as (installation, _, state):
                os.environ["XDG_CONFIG_HOME"] = str(Path(directory) / "offline")
                state["shell"]["pending"] = True
                before = tree_snapshot(Path(directory) / ".config")
                with self.assertRaises(ValueError):
                    apply_plan(installation)
                self.assertEqual(tree_snapshot(Path(directory) / ".config"), before)

    def test_lock_requested_during_staging_prevents_live_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.live_installation(directory) as (installation, old_files, state):
                def request_lock(path, contents):
                    write_private(path, contents)
                    if path.name == "Status.js":
                        state["shell"]["requested"] = True

                with patch("install.write_private", side_effect=request_lock):
                    with self.assertRaises(ValueError):
                        apply_plan(installation)
                for path, contents in old_files.items():
                    self.assertEqual(path.read_text(), contents)
                self.assertEqual(tree_snapshot(installation.plugin), installation.previous_plugin)
                self.assertEqual({path.name for path in installation.plugin.parent.iterdir()}, {PLUGIN_ID})

    def test_explicitly_unlocked_live_install_and_locked_noop_preserve_their_contracts(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.live_installation(directory) as (installation, old_files, state):
                apply_plan(installation)
                for path, contents, _ in installation.writes:
                    self.assertEqual(path.read_text(), contents)
                personal = installation.plugin / "Personal.qml"
                self.assertEqual(personal.read_text(), old_files[personal])
                state["shell"]["locked"] = True
                config = Path(directory) / ".config"
                before = tree_snapshot(config)
                apply_plan(plan(config, Path(directory) / "omarchy", None))
                self.assertEqual(tree_snapshot(config), before)

    def test_locked_live_dry_run_remains_read_only(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.live_installation(directory) as (_, _, state):
                state["shell"]["locked"] = True
                config = Path(directory) / ".config"
                before = tree_snapshot(config)
                with patch("sys.argv", ["install.py", "--config-dir", str(config)]), redirect_stdout(StringIO()):
                    main()
                self.assertEqual(tree_snapshot(config), before)

    def test_offline_fixture_install_does_not_require_a_desktop_session(self):
        with tempfile.TemporaryDirectory() as directory:
            installation, _ = self.installation(directory)
            with patch.dict(os.environ, {"HOME": str(Path(directory) / "home")}, clear=True):
                apply_plan(installation)
            for path, contents, _ in installation.writes:
                self.assertEqual(path.read_text(), contents)


if __name__ == "__main__":
    unittest.main()
