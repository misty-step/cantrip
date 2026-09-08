# Install a Cantrip release

Use the [latest published Linux x86-64 release](https://github.com/misty-step/cantrip/releases/latest)
for a CPU-only executable. No source checkout, Rust installation, GPU, or CUDA
runtime is needed. The archive contains the executable, user documentation,
`install.sh`, the optional `cantrip.service`, `LICENSE`, the public JFK
`sample.wav`, `manifest.json`, and `checksums.txt`.

Installation is binary-only. It does not download models, enable a service, add
shortcuts, or change configuration, credentials, or retained recordings.
After installing, continue to [your first attended dictation](USAGE.md#first-dictation).
If a retained older bundle lacks a linked companion guide, use the
[online user documentation](https://cantrip.mistystep.io/docs/install); no
checkout is required.

## Runtime prerequisites and support

- **Linux x86-64, Ubuntu 24.04 / glibc 2.39 baseline.** The release is
  dynamically linked, not a static executable. Older glibc systems such as
  Ubuntu 22.04 are outside this binary's supported baseline.
- Run as the intended ordinary user with readable Linux `/proc`, **not through
  sudo**. The installer uses Bash and GNU coreutils without networking.
- Microphone dictation needs a working PipeWire user session and `pw-record`.
  Clipboard/paste delivery needs Wayland and `wl-copy` from `wl-clipboard`.
  On Ubuntu 24.04, the tools are provided by `pipewire-bin` and `wl-clipboard`;
  installing tools alone does not configure a working microphone/session.
- Local transcription needs the Parakeet weights, downloaded separately and
  deliberately with `models pull`. Local inference after that download needs
  no network. Cleanup is disabled until explicitly configured and enabled.
- Cloud credentials require an unlocked Secret Service keyring and the same
  user-session D-Bus connection. No credentials are needed for the default
  local trial.
- Automatic keyboard delivery requires the documented direct Hyprland/logind
  desktop. Other or unverified desktops use explicit clipboard/manual paste.
  The HUD separately requires Wayland layer-shell support. See the
  [desktop capability table](DESKTOP.md#supported-desktops).

After verifying the release metadata below, inspect its `runtime_baseline`:

```sh
jq '.runtime_baseline' release.json
```

It records the distribution baseline, minimum glibc, actual ELF loader,
required libraries/symbol versions, and corresponding Ubuntu runtime packages
for **that artifact**. Use those package requirements rather than assuming
glibc alone is sufficient. They are not a complete desktop installation recipe:
graphical-session integration, fonts, microphone tools, and optional keyring
services depend on your desktop. The release-verifier container also includes
test tools; its package list is not a minimal product requirement.

A command-line transcription smoke does not prove microphone capture, HUD, or
paste delivery on your desktop. Read `doctor` findings and complete the
[attended trial](USAGE.md#first-dictation); its exit code alone is not readiness.

## Verify and install

### Download one published tag

Choose a version from the [published releases](https://github.com/misty-step/cantrip/releases)
and read that tag's public notes. In a new download directory, replace `VERSION`
below with its version **without the leading `v`**. Do not use an unreleased
version from a source checkout.

Download the matching archive, `release.json`, `SHA256SUMS`, and
`provenance.json` from that same release. With the
[GitHub CLI](https://cli.github.com/manual/gh_attestation_verify) installed:

```sh
version=VERSION
archive="cantrip-v${version}-x86_64-unknown-linux-gnu.tar.gz"
gh release download "v${version}" --repo misty-step/cantrip \
  --pattern "$archive" --pattern release.json \
  --pattern SHA256SUMS --pattern provenance.json
```

Downloading those four files through the release page is also fine. Keep the
same `version` and `archive` shell variables for the following steps.
Verification needs `jq`, a recent `gh` with attestation support, and network
access as required by GitHub's verifier; this is separate from the offline
binary installer.

### Check corruption and signed provenance

Run from the directory containing those four files. **Stop on any failure; do
not install an artifact whose identity or attestation does not verify.**

```sh
(
  set -eu
  sha256sum --check --strict --ignore-missing SHA256SUMS
  # Both the selected archive and release.json must report OK.

  jq -e --arg version "$version" --arg archive "$archive" '
    .schema_version == 1 and .version == $version and
    .tag == ("v" + $version) and
    .target == "x86_64-unknown-linux-gnu" and .archive.name == $archive
  ' release.json
  source_revision="$(jq -er '
    .source_revision | select(test("^[0-9a-f]{40}$|^[0-9a-f]{64}$"))
  ' release.json)"

  for subject in release.json "$archive"; do
    gh attestation verify "$subject" \
      --repo misty-step/cantrip \
      --bundle provenance.json \
      --source-digest "$source_revision" \
      --signer-workflow misty-step/cantrip/.github/workflows/release.yml \
      --deny-self-hosted-runners
  done

  jq -r '.archive | "\(.sha256)  \(.name)"' release.json \
    | sha256sum --check --strict
  printf 'Verified source revision: %s\n' "$source_revision"
)
```

`--ignore-missing` allows other release assets named in `SHA256SUMS` to be
absent; it is not permission to skip the archive or `release.json`. The
attestation checks cover both selected files and bind them to the same full
source revision, repository, and release workflow on a GitHub-hosted runner.
Compare that verified revision with the commit behind your chosen published
tag. A signature identifies the build's provenance; it is not a substitute for
trusting the repository and reviewing its release notes.

Checksums alone detect corruption, not who published a file. Obtain metadata
and the provenance bundle through the trusted release channel, not an unrelated
mirror. `release.json` records the archive name and SHA-256 plus its version,
tag, source revision, target, exact Rust toolchain, runtime baseline, and binary
SHA-256. The bundled `manifest.json` records the same build identity without
the archive hash: an archive cannot contain its own hash.

### Install the verified executable

After successful verification and runtime preparation:

```sh
tar -xzf "$archive" &&
cd "${archive%.tar.gz}" &&
sha256sum --check --strict checksums.txt &&
./install.sh install --prefix "$HOME/.local" &&
"$HOME/.local/bin/cantrip" --version
```

Stop if the inner checksum check fails. Keep the verified archive, provenance
files, and release notes for diagnosis or rollback review.

The installer changes only `PREFIX/bin/cantrip`; `--prefix` defaults to
`$HOME/.local`. It never changes `PATH`, shortcuts, services, config, models,
recordings, logs, or the OS keyring. Use the explicit executable path shown
above, or deliberately add its `bin` directory to the relevant shell/desktop
`PATH`. For an existing package-owned installation, use that package owner's
maintenance procedure instead. Existing installations use `update`, not a
forced fresh install.

Continue to [first dictation](USAGE.md#first-dictation) for first-time config
creation, the deliberate model download, clear terminal boundaries, and either
a hotkey-driven editor trial or explicit clipboard/manual paste. Do not
reinitialize existing configuration during an update.

## Keep one startup owner

Service installation is **optional and separate**, after an attended trial.
Keep a personal/package service or compositor autostart if it already owns
Cantrip. Actions uses an installed service even when disabled; otherwise it can
start a direct process. Neither path changes hotkeys.

The [shared desktop service procedure](DESKTOP.md#user-service-graphical-session)
starts with the already-installed binary and uses `./cantrip.service` from the
extracted archive. It covers owner checks, unit review, session environment,
enablement, readiness, and removal without a source checkout or binary-copy
step. Another prefix needs an explicit `ExecStart` override. Do not manually
start `graphical-session.target` or enable lingering to bypass missing desktop
integration.


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
if appropriate, using the [desktop removal procedure](DESKTOP.md#stop-update-and-remove);
account for shortcuts that still refer to the binary. Then:

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
