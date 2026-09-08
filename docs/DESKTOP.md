# Desktop setup and troubleshooting

Install the [release binary](INSTALLATION.md), then follow
[first dictation](USAGE.md#first-dictation). A login service and Omarchy badge
are optional conveniences, not requirements for trying Cantrip. Keep one
startup owner, and do not replace a personal setup just to follow this guide.

Examples use `"$HOME/.local/bin/cantrip"`. Substitute your actual executable path
when using another prefix. A shell's `PATH`, a compositor's `PATH`, and the
systemd user manager's environment are separate; the installer changes none of
them.

## Supported desktops

| Capability | Requirement and boundary |
|---|---|
| Local file transcription | Supported Linux x86-64 runtime and explicitly downloaded Parakeet model; no running daemon, microphone, GPU, or compositor needed |
| Microphone dictation | Working PipeWire user session and `pw-record`; use the attended Wayland session for the desktop workflow |
| Clipboard delivery | Wayland clipboard support and `wl-copy` from `wl-clipboard`; choose `injection = "clipboard"` for manual paste |
| Passive HUD | Wayland layer-shell support; compositor support is separate from clipboard and keyboard delivery |
| Automatic paste or typing | Direct Hyprland with the Lua focus/layer APIs, virtual-keyboard support, and a verifiable active logind session |
| Native Settings and Actions | A working graphical session; Actions offers deliberate copy/recovery without promising keyboard-delivery support |
| Optional user service | systemd 246+, one Wayland session per Unix user, and a session manager that owns the graphical-session lifecycle and environment |

The project's documented automatic-delivery baseline is **Hyprland 0.56.2**.
Authenticated UWSM-managed direct desktops are supported. Nested, ambiguous, or
unverified desktops fail closed. Having a Wayland display or virtual-keyboard
protocol is not enough to establish destination safety. Other compositors can
use explicit clipboard delivery or Copy in Actions; there is no bypass that
makes automatic delivery guess.

The delivery guard tracks focus changes, layer surfaces, session locks, suspend,
and compositor/session reconnection. Unknown or changed state defers keyboard
delivery. Keep the intended editor focused when stopping the recording and
throughout processing. A successful paste-helper acknowledgement does not prove
the application accepted the text.

The HUD is bottom-anchored, pointer-transparent, and does not take focus. Its
presence does not establish that the microphone or destination works. The
[attended trial](USAGE.md#4a-dictate-into-an-editor-on-supported-hyprland), not a
compositor name or a successful diagnostic exit, establishes your actual result.

## Dictation shortcut

Cantrip does not install or change hotkeys. Bind one unused shortcut to the
absolute installed executable path followed by `toggle`. Print the command to
use from your graphical terminal:

```sh
printf '"%s" toggle\n' "$HOME/.local/bin/cantrip"
```

The shortcut must execute that command directly, **without opening a terminal**.
Press it once to start and once to stop; disable key-repeat for the binding. The
same command works with either guarded automatic delivery or explicit clipboard
mode. Hold-to-talk is a separate choice: bind key-down to `start` and key-up to
`stop` only when the compositor supports separate transitions.

### Hyprland Lua configuration

Inspect existing bindings with `hyprctl binds` and choose an unused combination.
In a Lua configuration loaded by your Hyprland session, this is the binding
shape from the [Hyprland binding guide](https://wiki.hypr.land/Configuring/Basics/Binds/):

```lua
hl.bind("SUPER + CTRL + D", hl.dsp.exec_cmd('"/home/YOUR_USER/.local/bin/cantrip" toggle'))
```

Replace the example home directory with the actual path printed above and change
the key if it is already assigned. This is Lua configuration, **not a shell
command**. Do not add locked-session or repeating flags. Back up the specific
user configuration file before editing; do not replace a whole desktop config.

On Omarchy, user bindings belong in `~/.config/hypr/bindings.lua`, not packaged
files. Inspect `omarchy menu keybindings --print` first. Its existing binding
helper can express the same shortcut:

```lua
o.bind("SUPER + CTRL + D", "Dictation", '"/home/YOUR_USER/.local/bin/cantrip" toggle')
```

Use **one** of these forms, not both. Prefer an unused key. If deliberately
replacing an existing binding, record its previous action and unbind that exact
combination with `hl.unbind(...)` before adding the replacement.

After saving your user configuration, check the compositor's configuration:

```sh
hyprctl reload
hyprctl configerrors
```

Resolve any reported error before trying the shortcut. Return to the
[first-dictation editor step](USAGE.md#4a-dictate-into-an-editor-on-supported-hyprland).
Do not trigger `stop` from a diagnostic terminal while expecting text in another
window.

## Keep one startup owner

A foreground terminal, a personal/package service, or compositor autostart can
own the daemon. `cantrip actions` uses an installed `cantrip.service` even when
that unit is disabled; it does not enable it or launch a competing daemon.
Without a service, Actions can start a direct process. Neither path adds hotkeys.

Inspect the existing service from your attended, unlocked graphical session:

```sh
systemctl --user show cantrip.service \
  --property=LoadState,FragmentPath,DropInPaths,UnitFileState,ActiveState
```

Also inspect any personal/compositor autostart you already use. Keep that owner
or deliberately migrate it; service discovery alone cannot rule out every
custom autostart. If Cantrip is already reachable, investigate its owner rather
than running another `daemon` command.

## User service (graphical session)

The bundled `cantrip.service` is optional. The binary installer neither copies
nor enables it. Supported service use requires systemd 246 or newer, one
Wayland session per Unix user, and a session manager that refreshes its
environment before starting `graphical-session.target` and stops that target on
logout.

```sh
systemctl --user is-active graphical-session.target
```

If the target is inactive or the desktop does not manage its login/logout
lifecycle, keep Actions or the foreground terminal instead. **Do not manually
start the target or enable lingering to bypass missing session integration.**

### Fresh installation

First complete an attended dictation and stop its daemon through the existing
owner. `cantrip stop` only ends a recording; use Ctrl+C for the foreground daemon
and wait for its clean exit. An Actions-started direct process must be identified
by its executable and PID before sending that specific PID SIGTERM and waiting
for exit. Do not use broad `pkill` or delete the socket.

For an archive installation, open a terminal **in the extracted release directory**.
The binary is already installed; choose the bundled unit:

```sh
unit_source="$PWD/cantrip.service"
```

Source builders use the same procedure after the
[separate source-binary installation](https://github.com/misty-step/cantrip#build-from-source),
setting `unit_source="$PWD/contrib/cantrip.service"` from the repository root
instead. No binary is copied by the shared service setup below.

Only proceed when the owner review above shows no existing owner. This block
refuses an existing service, mask, unit symlink, or personal drop-in directory:

```sh
(
  set -eu
  unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  if [ "$(systemctl --user show cantrip.service --property=LoadState --value)" != not-found ]; then
    printf '%s\n' 'Existing or unknown service owner; inspect it before replacing it.' >&2
    exit 1
  fi
  for path in "$unit_dir/cantrip.service" "$unit_dir/cantrip.service.d"; do
    if [ -e "$path" ] || [ -L "$path" ]; then
      printf 'Refusing to replace %s\n' "$path" >&2
      exit 1
    fi
  done
  test -f "$unit_source"
  mkdir -p "$unit_dir"
  install -m644 "$unit_source" "$unit_dir/cantrip.service"
  systemctl --user daemon-reload
  systemctl --user cat cantrip.service
)
```

Review the effective unit and any inherited drop-ins before enabling anything.
It runs `%h/.local/bin/cantrip`, not a login-shell `PATH` lookup. Confirm that
this is your installed executable. To retain another location, use
`systemctl --user edit cantrip.service` and set:

```ini
[Service]
ExecStart=
ExecStart=/absolute/path/to/cantrip daemon
```

Use your real executable path and review the resulting effective unit again.
Do not replace an existing personal unit or drop-ins to make this example fit.

### Environment and readiness

Services inherit the **user manager's** environment, not the invoking terminal's.
The session manager must supply current `WAYLAND_DISPLAY` and, on Hyprland,
`HYPRLAND_INSTANCE_SIGNATURE` before startup. Do not hard-code or guess them.
To repair this login, run from its graphical terminal before starting Cantrip:

```sh
systemctl --user import-environment WAYLAND_DISPLAY
# On Hyprland:
systemctl --user import-environment HYPRLAND_INSTANCE_SIGNATURE
```

A one-time import does not configure future logins. The session manager must
refresh these values each login and retire them on logout. Import
`XDG_CURRENT_DESKTOP` and `XDG_SESSION_TYPE` if supplied by that session.
`WAYLAND_SOCKET` must be absent. Keep the manager's runtime directory and D-Bus
address; never import the entire shell environment or put API keys in a unit.

Custom XDG config/data/state paths and tool `PATH` must agree with the existing
installation so configuration, models, credentials, and recordings do not appear
missing. This includes `pw-record` and `wl-copy` availability. Environment
changes affect newly started processes, not an already-running daemon.

After reviewing the effective unit and environment:

```sh
systemctl --user enable cantrip.service
systemctl --user start cantrip.service
"$HOME/.local/bin/cantrip" ping
"$HOME/.local/bin/cantrip" status --json
"$HOME/.local/bin/cantrip" doctor
```

Use the overridden executable path when applicable. Enablement selects login
startup; start launches it now. An active service is **not dictation readiness**:
wait for IPC, address diagnostic findings, and make another
[attended editor or clipboard trial](USAGE.md#first-dictation). Do not start a
second daemon to diagnose the first.

### Stop, update, and remove

Finish or cancel the take and let processing settle before routine maintenance:

```sh
systemctl --user stop cantrip.service          # stop now
systemctl --user disable --now cantrip.service # also remove login enablement, if intended
```

These are separate choices. After stopping, inspect the result and require a
clean inactive service, not a failed or timed-out shutdown:

```sh
systemctl --user show cantrip.service --property=ActiveState,SubState,Result
```

The unit follows the [graphical-session lifecycle](https://www.freedesktop.org/software/systemd/man/latest/systemd.special.html#graphical-session.target).
Explicit stops are not failure-restarted. `KillMode=mixed` lets the daemon stop
`pw-record` with SIGINT and retain audio before terminating remaining children.
A stuck shutdown is force-killed after 90 seconds; inspect failures rather than
assuming runtime-only audio became durable. No unit action deletes retained data.

For a binary update or rollback, record the current enabled/running state and
follow [release maintenance](INSTALLATION.md#update-and-roll-back). Keep the
existing owner stopped throughout replacement. Leave unchanged units and
personal drop-ins alone. If deliberately replacing a unit, disable its old
enablement first, preserve and review overrides, install the reviewed replacement,
reload the manager, and restore only the intended enablement. Restart that owner,
check its environment and IPC, and make an attended dictation. Binary rollback
does not prove history-schema compatibility.

To remove only the unit installed by this guide, disable it, remove its reviewed
file at `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/cantrip.service`, and run
`systemctl --user daemon-reload`. Inspect whether a lower-priority packaged unit
becomes visible. Remove only drop-ins you deliberately created. Keep the binary
if bindings still use it, or remove those bindings before uninstalling it.
Configuration, models, keyring entries, runtime leftovers, and history remain.

## Diagnose without changing ownership

```sh
"$HOME/.local/bin/cantrip" doctor
"$HOME/.local/bin/cantrip" actions --doctor
```

Read the reported actions, not just the command exit. `doctor` checks effective
file configuration and discoverable prerequisites: finding `pw-record` does
not prove the microphone captures sound; a model directory does not prove a
successful inference; a configured provider does not prove credentials or its
endpoint work; available injection backends do not certify focus safety. The
HUD's layer-shell support is checked when it starts.

For a user service:

```sh
journalctl --user -u cantrip.service -b --no-pager
```

For any daemon owner, operational details are also in
`${XDG_STATE_HOME:-$HOME/.local/state}/cantrip/daemon.log`, without transcript
text. Check [configuration](CONFIGURATION.md) for invalid settings, model setup,
cleanup, and audio source selection. Check [recording recovery](USAGE.md#recovery)
before repeating a failed take. Never delete runtime or history directories as
a setup repair.

After repairing repeated service startup failures, run
`systemctl --user reset-failed cantrip.service` before starting that owner again.
If clipboard mode works but automatic delivery defers, investigate the supported
Hyprland/logind session conditions; do not weaken the guard.

## Omarchy integration

The optional badge and menu installer is a **source-checkout integration**, not
part of the binary installer. It does not install a daemon, service, or hotkey.
Its commands invoke `cantrip` by name, so the chosen executable's directory must
be on the **Omarchy desktop session's** `PATH`; exporting it only in a terminal
does not update an already-running shell.

From the repository root, while attending the unlocked graphical session,
review the dry run before applying:

```sh
python3 integrations/omarchy/install.py
python3 integrations/omarchy/install.py --apply
omarchy menu summon cantrip
```

Live `--apply` fails closed unless bounded read-only probes identify the same
Hyprland/Omarchy session and both report unlocked, with no requested or pending
lock. Locked, unavailable, ambiguous, malformed, or timed-out state refuses
installation before staging or backups. Run from that graphical session's
terminal: the installer will not guess a display from SSH/TTY, disable locking,
unlock automatically, or provide a live bypass. Passing `--config-dir` for the
live configuration does not skip these checks.

The installer rechecks before publishing the plugin and each changed config
file. If safety changes during staging/publication, rollback preserves installed
content and may retain private backups. These checks are defense in depth,
**not a guarantee against a check-to-lock race**; only the shell can coordinate
hot reload with locking. Do not use unattended live deployment. Dry runs and
already-current no-ops do not mutate or require a session probe; a distinct
offline `--config-dir` fixture can be installed without a running desktop.

When replacing a personal badge, add `--replace-widget OLD_PLUGIN_ID` to both
installer commands. Left-click keeps raw dictation; right-click opens Actions.
Existing hotkeys are not redefined. Status failures show unknown, not Ready;
the badge retains only the last confirmed pending count.

The installer preserves unrelated shell/menu content, stages complete plugin
updates, and reports private rollback backups. To roll back, disable
`cantrip.dictation`, restore the previous bar widget, and remove only the managed
Cantrip menu block. Move the plugin directory outside `omarchy/plugins` rather
than deleting personal extras. Never restore whole backups over subsequent edits.
