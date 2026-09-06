#!/usr/bin/env python3
"""Install only Cantrip's Omarchy assets; dry-run unless --apply is supplied.

Grounded in Omarchy 4.0.0.alpha's shell/Ui/BarWidget.qml,
shell/services/PluginRegistry.qml, shell/plugins/menu/MenuModel.js and
config/omarchy/shell.json. No Hyprland bindings, services or notifications change.
"""

import argparse
import copy
import ctypes
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile
import time

PLUGIN_ID = "cantrip.dictation"
BEGIN = "\n  // cantrip:begin managed menu\n"
END = "\n  // cantrip:end managed menu\n"


def tokens(text):
    """Token spans let insertion preserve every existing comment and value."""
    decoder = json.JSONDecoder()
    index = 0
    while index < len(text):
        char = text[index]
        if char.isspace():
            index += 1
        elif text.startswith("//", index):
            newline = text.find("\n", index)
            index = len(text) if newline < 0 else newline + 1
        elif char == '"':
            value, end = decoder.raw_decode(text, index)
            yield ("string", value, index, end)
            index = end
        elif char in "{}[]:,":
            yield (char, char, index, index + 1)
            index += 1
        else:
            end = index + 1
            while end < len(text) and not text[end].isspace() and text[end] not in "{}[]:,":
                end += 1
            yield ("value", text[index:end], index, end)
            index = end


def parse_jsonc(text):
    parts = list(tokens(text))
    # Omarchy's menu accepts full-line // comments and trailing commas. Only
    # discard comma tokens, never comma-like bytes inside a command string.
    clean = " ".join(text[start:end] for number, (kind, _, start, end) in enumerate(parts)
                    if not (kind == "," and number + 1 < len(parts) and parts[number + 1][0] in ("}", "]")))
    return json.loads(clean)


def menu_insert(text, additions):
    if (BEGIN in text) != (END in text):
        raise ValueError("Incomplete Cantrip menu markers; original menu was not changed")
    if BEGIN in text:
        if text.count(BEGIN) != 1 or text.count(END) != 1:
            raise ValueError("Multiple Cantrip menu blocks; original menu was not changed")
        start = text.index(BEGIN)
        end = text.index(END, start)
        managed = parse_jsonc("{" + text[start + len(BEGIN):end] + "}")
        unowned = [key for key in managed if key != "cantrip" and not key.startswith("cantrip.")]
        if unowned:
            raise ValueError("Managed menu block contains unrelated IDs; move them outside its markers: "
                             + ", ".join(sorted(unowned)))
        text = text[:start] + text[end + len(END):]
    parsed = parse_jsonc(text)
    if not isinstance(parsed, dict):
        raise ValueError("Menu must be a JSONC object")
    wrapped = isinstance(parsed.get("items"), dict)
    existing = parsed["items"] if wrapped else parsed
    collisions = existing.keys() & additions.keys()
    if collisions:
        raise ValueError("Menu already owns these IDs outside the managed block: " + ", ".join(sorted(collisions)))
    parts = list(tokens(text))
    opening = parts[0][2]
    if wrapped:
        stack = []
        for number, (kind, value, start, _) in enumerate(parts):
            if kind == "string" and value == "items" and stack == ["{"]:
                if parts[number + 1][0] == ":" and parts[number + 2][0] == "{":
                    opening = parts[number + 2][2]
                    break
            if kind in ("{", "["):
                stack.append(kind)
            elif kind in ("}", "]"):
                stack.pop()
        else:
            raise ValueError("Cannot locate menu items object")
    entries = json.dumps(additions, ensure_ascii=False, indent=2)[2:-2]
    block = BEGIN + entries + ("," if existing else "") + END
    result = text[:opening + 1] + block + text[opening + 1:]
    parse_jsonc(result)
    return result


def merge_layout(shell, defaults, replace_widget):
    result = copy.deepcopy(shell)
    bar = result.setdefault("bar", {})
    layout = bar.setdefault("layout", {})
    if not isinstance(layout, dict):
        raise ValueError("bar.layout must be an object")
    default_layout = defaults.get("bar", {}).get("layout", {})
    matches = []
    installed = []
    for section, rows in layout.items():
        if not isinstance(rows, list):
            raise ValueError("bar.layout sections must be arrays")
        for index, row in enumerate(rows):
            identity = row.get("id") if isinstance(row, dict) else row
            if identity == PLUGIN_ID:
                installed.append((section, index))
            if replace_widget and identity == replace_widget:
                matches.append((section, index))
    if len(installed) > 1 or len(matches) > 1 or (installed and matches):
        raise ValueError("Multiple Cantrip widget entries; choose one manually before installing")
    if installed:
        return result
    if replace_widget:
        if not matches:
            raise ValueError("Requested widget was not found: " + replace_widget)
        section, index = matches[0]
        old = layout[section][index]
        layout[section][index] = dict(old, id=PLUGIN_ID) if isinstance(old, dict) else {"id": PLUGIN_ID}
    else:
        if "right" not in layout:
            layout["right"] = copy.deepcopy(default_layout.get("right", []))
        layout["right"].append({"id": PLUGIN_ID})
    return result


def safe_path(path):
    for candidate in (path, *path.parents):
        if candidate.is_symlink():
            raise ValueError("Refusing symlink path: " + str(candidate))
    if path.exists() and not path.is_file():
        raise ValueError("Expected a regular file: " + str(path))


def read_optional(path, fallback):
    safe_path(path)
    return path.read_bytes().decode() if path.exists() else fallback


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def write_private(path, contents):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(contents)
        stream.flush()
        os.fsync(stream.fileno())


class PreparedWrite:
    """Prepare bytes and their private backup before any reference changes."""

    def __init__(self, path, contents, expected):
        safe_path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        if read_optional(path, None) != expected:
            raise ValueError("File changed since planning; refused to overwrite: " + str(path))
        self.path = path
        self.contents = contents
        self.expected = expected
        self.temporary = None
        if expected is not None:
            backup = path.with_name(path.name + ".before-cantrip-" + str(time.time_ns()) + ".bak")
            write_private(backup, expected.encode())
        descriptor, temporary = tempfile.mkstemp(prefix=".cantrip-install-", dir=path.parent)
        self.temporary = Path(temporary)
        try:
            with os.fdopen(descriptor, "wb") as stream:
                stream.write(contents.encode())
                stream.flush()
                os.fsync(stream.fileno())
            sync_directory(path.parent)
        except BaseException:
            self.close()
            raise

    def publish(self):
        if read_optional(self.path, None) != self.expected:
            raise ValueError("File changed during installation; refused to overwrite: " + str(self.path))
        os.replace(self.temporary, self.path)
        self.temporary = None
        sync_directory(self.path.parent)

    def rollback(self):
        current = read_optional(self.path, None)
        if current == self.expected:
            return
        if current != self.contents:
            raise ValueError("External edit preserved; manual rollback needed: " + str(self.path))
        if self.expected is None:
            self.path.unlink()
            sync_directory(self.path.parent)
        else:
            atomic_write(self.path, self.expected, self.contents)

    def close(self):
        if self.temporary is not None and self.temporary.exists():
            self.temporary.unlink()


def atomic_write(path, contents, expected):
    if contents == expected:
        return
    prepared = PreparedWrite(path, contents, expected)
    try:
        prepared.publish()
    finally:
        prepared.close()


def tree_snapshot(root):
    if not root.exists():
        return None
    if root.is_symlink() or not root.is_dir():
        raise ValueError("Plugin must be a real directory: " + str(root))
    result = {}
    for directory, dirs, files in os.walk(root, followlinks=False):
        for name in [".", *sorted(dirs), *sorted(files)]:
            path = Path(directory) if name == "." else Path(directory) / name
            info = path.lstat()
            relative = str(path.relative_to(root))
            mode = stat.S_IMODE(info.st_mode)
            if stat.S_ISLNK(info.st_mode):
                result[relative] = ("link", os.readlink(path), mode)
            elif stat.S_ISDIR(info.st_mode):
                result[relative] = ("directory", mode)
            elif stat.S_ISREG(info.st_mode):
                digest = hashlib.sha256()
                with path.open("rb") as stream:
                    for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                        digest.update(chunk)
                result[relative] = ("file", digest.hexdigest(), mode)
            else:
                raise ValueError("Unsupported file in plugin: " + str(path))
    return result


def rename_atomic(source, destination, exchange=False):
    # Linux renameat2 is required by this Linux/Omarchy integration. An
    # unsupported filesystem fails without deleting or hiding the old plugin.
    library = ctypes.CDLL(None, use_errno=True)
    rename = library.renameat2
    rename.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    flags = 2 if exchange else 1  # RENAME_EXCHANGE / RENAME_NOREPLACE
    if rename(-100, os.fsencode(source), -100, os.fsencode(destination), flags) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(destination))


@dataclass
class InstallPlan:
    plugin: Path
    writes: list
    previous_plugin: object


def apply_plan(installation):
    changes = [(path, content, old) for path, content, old in installation.writes if content != old]
    if not changes:
        return
    plugin = installation.plugin
    plugin_changes = [change for change in changes if change[0].parent == plugin]
    references = [change for change in changes if change[0].parent != plugin]
    prepared = []
    workspace = None
    staged = None
    published = False
    preserve_workspace = False
    new_snapshot = None
    try:
        # Recheck the plan and stage all reference files before publication.
        for path, _, expected in changes:
            if read_optional(path, None) != expected:
                raise ValueError("File changed since planning: " + str(path))
        if tree_snapshot(plugin) != installation.previous_plugin:
            raise ValueError("Plugin changed since planning; installation was not published")
        for path, contents, expected in references:
            prepared.append(PreparedWrite(path, contents, expected))
        if plugin_changes:
            plugin.parent.mkdir(parents=True, exist_ok=True)
            # Outside plugins/: registry discovery can never see the candidate.
            workspace = Path(tempfile.mkdtemp(prefix=".cantrip-backup-", dir=plugin.parent.parent))
            staged = workspace / "plugin"
            if installation.previous_plugin is None:
                staged.mkdir(mode=0o700)
            else:
                shutil.copytree(plugin, staged, symlinks=True)
            for path, contents, _ in installation.writes:
                if path.parent == plugin:
                    candidate = staged / path.name
                    if candidate.exists():
                        candidate.unlink()
                    write_private(candidate, contents.encode())
            for directory, _, files in os.walk(staged, topdown=False, followlinks=False):
                for name in files:
                    path = Path(directory) / name
                    if not path.is_symlink():
                        with path.open("rb") as stream:
                            os.fsync(stream.fileno())
                sync_directory(Path(directory))
            new_snapshot = tree_snapshot(staged)
            if tree_snapshot(plugin) != installation.previous_plugin:
                raise ValueError("Plugin changed during staging; installation was not published")
            rename_atomic(staged, plugin, exchange=installation.previous_plugin is not None)
            published = True
            sync_directory(plugin.parent)
            sync_directory(workspace)
        for change in prepared:
            change.publish()
        if published and installation.previous_plugin is not None:
            preserve_workspace = True
            print("Previous complete plugin preserved at " + str(workspace))
    except BaseException as original:
        rollback_errors = []
        for change in reversed(prepared):
            try:
                change.rollback()
            except (OSError, ValueError) as error:
                rollback_errors.append(str(error))
        if published and not rollback_errors:
            try:
                if tree_snapshot(plugin) != new_snapshot:
                    raise ValueError("External plugin edits preserved; manual rollback needed")
                if installation.previous_plugin is None:
                    rename_atomic(plugin, staged)
                else:
                    rename_atomic(staged, plugin, exchange=True)
                sync_directory(plugin.parent)
                published = False
            except (OSError, ValueError) as error:
                rollback_errors.append(str(error))
        if rollback_errors:
            preserve_workspace = True
            location = " at " + str(workspace) if workspace is not None else ""
            raise RuntimeError("Rollback needs attention; current plugin/configuration and private backups retained"
                               + location + ": " + "; ".join(rollback_errors)) from original
        raise
    finally:
        for change in prepared:
            change.close()
        if workspace is not None and not preserve_workspace:
            shutil.rmtree(workspace)


def plan(config_dir, omarchy_path, replace_widget):
    assets = Path(__file__).parent
    plugin = config_dir / "omarchy/plugins" / PLUGIN_ID
    shell_path = config_dir / "omarchy/shell.json"
    menu_path = config_dir / "omarchy/extensions/omarchy-menu.jsonc"
    plugin_files = [plugin / name for name in ("manifest.json", "BarWidget.qml", "Status.js")]
    originals = {path: read_optional(path, None) for path in [*plugin_files, shell_path, menu_path]}
    previous_plugin = tree_snapshot(plugin)
    current_manifest = originals[plugin / "manifest.json"]
    if current_manifest is None:
        if previous_plugin is not None and len(previous_plugin) > 1:
            raise ValueError("Plugin destination contains unowned files without a Cantrip manifest")
    elif json.loads(current_manifest).get("id") != PLUGIN_ID:
        raise ValueError("Plugin destination belongs to another plugin")
    defaults = json.loads((omarchy_path / "config/omarchy/shell.json").read_text())
    shell = json.loads(originals[shell_path]) if originals[shell_path] is not None else defaults
    updated_shell = merge_layout(shell, defaults, replace_widget)
    menu = originals[menu_path] if originals[menu_path] is not None else "{\n}\n"
    updated_menu = menu_insert(menu, json.loads((assets / "menu.json").read_text()))
    writes = [(path, (assets / path.name).read_text()) for path in plugin_files]
    shell_contents = (originals[shell_path] if updated_shell == shell and originals[shell_path] is not None
                      else json.dumps(updated_shell, ensure_ascii=False, indent=2) + "\n")
    writes.extend(((shell_path, shell_contents), (menu_path, updated_menu)))
    return InstallPlan(plugin, [(destination, contents, originals[destination]) for destination, contents in writes], previous_plugin)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="Write planned changes; existing files get owner-private backups")
    parser.add_argument("--config-dir", type=Path, default=Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))))
    parser.add_argument("--omarchy-path", type=Path, default=Path(os.environ.get("OMARCHY_PATH", "/usr/share/omarchy")))
    parser.add_argument("--replace-widget", help="Replace only this existing layout ID, in place (e.g. phaedrus.cantrip)")
    args = parser.parse_args()
    writes = plan(args.config_dir.absolute(), args.omarchy_path.absolute(), args.replace_widget)
    changes = [(path, content, original) for path, content, original in writes.writes if original != content]
    for path, _, _ in changes:
        print(("Installing " if args.apply else "Would install ") + str(path))
    if args.apply:
        apply_plan(writes)
    if not changes:
        print("Cantrip integration is already current.")
    elif not args.apply:
        print("Dry run only. Add --apply after reviewing these paths.")
    print("No services, Hyprland bindings, clipboard, or microphone were changed.")
    print("Left-click remains raw toggle. Right-click opens native Cantrip actions.")
    print("Existing Super+R and Super+Shift+R bindings are preserved, not redefined.")
    if args.apply:
        print("Omarchy hot-reloads these files. If needed: omarchy-shell shell rescanPlugins")
        print("Open the menu route with: omarchy menu summon cantrip")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, AttributeError, RuntimeError) as error:
        raise SystemExit("Installation stopped: " + str(error) + ". Existing replaced files have backups.") from None
