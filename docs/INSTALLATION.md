# Install a Cantrip release

The Linux x86-64 archive contains a CPU-only executable, this guide, `install.sh`,
the optional `cantrip.service`, `LICENSE`, the public JFK `sample.wav`,
`manifest.json`, and `checksums.txt`. No Rust installation, GPU, or CUDA runtime
is needed. Installation itself uses Bash and GNU coreutils, without networking.

## Runtime prerequisites and support

- Linux x86-64, glibc **2.39 or newer** (release baseline: Ubuntu 24.04).
- An ordinary user session with readable Linux `/proc`; run the installer as
  the intended Cantrip user, **not through sudo**.
- For microphone dictation: PipeWire with `pw-record`, a Wayland compositor,
  and `wl-copy` from `wl-clipboard` for clipboard/paste delivery.
- Local transcription needs the Parakeet model, downloaded separately with
  `cantrip models pull`. Downloads need network access; subsequent local
  transcription does not. Cloud credentials need an unlocked Secret Service
  keyring and its user-session D-Bus connection. Cleanup remains opt-in.
- Automatic keyboard delivery currently requires the documented direct
  Hyprland desktop (Lua focus/layer APIs verified on 0.56.2), virtual-keyboard
  support, and a verifiable active logind session. Authenticated UWSM sessions
  are supported; nested or unverified desktops fail closed. Elsewhere, select
  `injection = "clipboard"` or explicitly copy from `cantrip actions`.
  The HUD separately needs Wayland layer-shell support.

A successful binary/transcription smoke is not proof of microphone, HUD, or
paste delivery on a fresh desktop. Follow `cantrip doctor` and make an attended
trial in a safe destination. Full desktop and startup-owner requirements are
in the [README](https://github.com/misty-step/cantrip#requirements).

## Verify and install

Download the archive, `release.json`, and `SHA256SUMS` from the same
[release](https://github.com/misty-step/cantrip/releases). Substitute the
published version below, without its leading `v`:

```sh
version=VERSION
archive="cantrip-v${version}-x86_64-unknown-linux-gnu.tar.gz"
sha256sum --check --strict --ignore-missing SHA256SUMS
# Both the selected archive and release.json must report OK.
tar -xzf "$archive"
cd "${archive%.tar.gz}"
sha256sum --check --strict checksums.txt
./install.sh install --prefix "$HOME/.local"
"$HOME/.local/bin/cantrip" --version
```

Checksums detect corruption; obtain them through the trusted release channel,
not from an unrelated mirror. `manifest.json` records the version, tag, full
source revision, target, exact Rust toolchain, runtime baseline, and binary
SHA-256. `release.json` adds the archive name and SHA-256. The archive cannot
contain its own hash. Keep the archive, its provenance files, and release notes
for later diagnosis or rollback review.

The installer changes only `PREFIX/bin/cantrip`; `--prefix` defaults to
`$HOME/.local`. It never changes `PATH`, shortcuts, services, config, models,
recordings, logs, or the OS keyring. Use the explicit executable path or add its
`bin` directory to your shell's `PATH` yourself. For an existing package-owned
installation, use the package owner's maintenance procedure instead.

For a first installation only, run `cantrip config init`, inspect
`cantrip doctor`, and follow its actions, including `cantrip models pull` when
requested. `cantrip transcribe --local ./sample.wav` exercises local CPU
transcription without starting the daemon. Start `cantrip daemon` in a dedicated
terminal for your first attended dictation, then bind one shortcut to the
absolute executable path plus `toggle`. Existing configurations must not be
reinitialized during an update.

## Keep one startup owner

Service installation is **optional and separate**. `install.sh` neither copies
nor enables the bundled unit. Keep an existing personal/package service or
compositor autostart; do not install a second owner. Actions uses an installed
service, even when disabled, and otherwise can start a direct process.

Use the [README user-service procedure](https://github.com/misty-step/cantrip#user-service-graphical-session)
for the owner checks, unit review, session environment, enablement, and readiness
checks. With an archive, the binary is already installed: do **not** repeat the
repository's fresh-install binary-copy block. Only after its checks show no
existing service, mask, unit symlink, or personal drop-ins, deliberately copy the
bundled `./cantrip.service` to
`${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/cantrip.service`, reload the user
manager, and review the effective unit before enabling anything.

The unit runs `%h/.local/bin/cantrip`. Another prefix requires the documented
explicit `ExecStart` override. Supported service use requires systemd 246+,
one Wayland session per Unix user, and a session manager that owns and refreshes
`graphical-session.target` and its environment. Do not manually start that
target or enable lingering to bypass missing desktop integration.

## Stop before maintenance

Finish or cancel the current take, wait for Idle, then **stop the daemon through
its existing owner and wait for a clean exit**. `cantrip stop` ends recording,
not the daemon. For the documented user service, use
`systemctl --user stop cantrip.service` and require a clean inactive result.
Use Ctrl+C for a foreground daemon. For an Actions-started direct process,
identify its executable and PID, send only that PID SIGTERM, and wait for exit.
Never use a broad `pkill` or delete the socket to pretend the daemon stopped.
Keep the owner stopped throughout maintenance; the installer does not prevent
an external owner from starting a new process between checks.

Use the existing installation's XDG environment. The installer refuses running
processes using the destination executable and a live socket in the selected
`$XDG_RUNTIME_DIR/cantrip/cantrip.sock` (fallback:
`/tmp/cantrip-$UID/cantrip/cantrip.sock`). Idle is still running; a failed ping
is not accepted as proof of shutdown. Unrelated prefixes with distinct runtime
directories are independent. Stale socket files and other runtime data remain
untouched. The checks require visibility of the destination processes and
runtime socket in the caller's Linux process/network namespaces.

## Update and roll back

Verify and extract the new archive first. Read its public/technical notes and
record the current enabled/running state. Back up a unit or drop-ins separately
only if you intend to change them. Stop the existing owner as above, then run
from the **new** extracted bundle:

```sh
mkdir -m700 -p "$HOME/cantrip-backups"
backup="$HOME/cantrip-backups/cantrip-before-VERSION"
./install.sh update --prefix "$HOME/.local" --backup "$backup"
"$HOME/.local/bin/cantrip" --version
```

Choose a new backup filename for every update; an occupied path is refused,
including a dangling symlink or directory. Its parent must already exist and
be owned by you, without group/world write access. The complete old executable
is published privately at that exact path before the staged new executable is
renamed atomically into place. Failure before replacement leaves the old binary
in place; if backup publication already succeeded, the backup remains available.

To restore that binary, stop the owner again and run from a retained bundle:

```sh
./install.sh rollback --prefix "$HOME/.local" --backup "$backup"
"$HOME/.local/bin/cantrip" --version
```

Rollback reads the backup without consuming or overwriting it. It does not
restore whole config/state directories or prove history-schema compatibility;
consult the release notes before using an older binary with newer data. Restore
only unit settings you deliberately changed. Restart only the intended owner,
restore its recorded enablement if needed, and check `ping`, `status --json`,
`doctor`, and an attended dictation. An active service alone is not readiness.

## Uninstall and refusals

Stop the existing owner. Deliberately remove its startup enablement and unit,
if appropriate, using the README's removal procedure; account for shortcuts
that still refer to the binary. Then:

```sh
./install.sh uninstall --prefix "$HOME/.local"
```

Only the regular installed executable is removed. Backups, unchanged units and
drop-ins, config, downloaded models, keyring entries, transcript history, logs,
and runtime leftovers are retained. The prefix and `bin` directories remain.

Symlink targets or path ancestors, multiply linked/foreign-owned executables,
privileged or group/world-writable executables, and unsafe writable destination
directories are refused. Paths may be absolute or relative but must not contain
`..` or newlines. Inspect the existing owner rather than deleting an unexpected
target to bypass a refusal. A `.cantrip-install.lock` in the destination `bin`
serializes installers; after an interrupted run, inspect it and confirm that no
installer is running before removing only that lock and its staged executable.
Never clear runtime or data directories as an installer repair.
