# Install Cantrip

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

The published-release, provenance, installer, service, update, rollback, and
uninstall procedures below are **Linux-only** and remain the supported binary
distribution route. For macOS 13.3+, use
[native source packaging](#macos-native-app-from-source); no public macOS
release has been published.

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

## macOS native app from source

The native app baseline is **macOS 13.3+**, with separate Apple Silicon
(`aarch64-apple-darwin`) and Intel (`x86_64-apple-darwin`) builds. Build each on
its matching native Mac with a matching native Python process, not through
Rosetta or cross-compilation. Both packaging and runtime preparation check the
calling process's architecture and translation state before building. This is
a source-build route, not a promise of a downloadable Mac release or an exercised
Intel build.

### Build prerequisites

- A source checkout, Git, rustup, and the exact Rust **1.98.1** pinned in
  [`rust-toolchain.toml`](../rust-toolchain.toml). Run `rustup install` from the
  repository root; Cargo reads that pin automatically. Keep `Cargo.lock` and
  use `--locked`; do not downgrade shared inference dependencies for Intel.
- Selected Xcode command-line tools providing Clang, `lipo`, `otool`,
  `install_name_tool`, `codesign`, `ditto`, and `xcrun`. Obtain them from Apple
  if missing; `xcrun --find clang` must resolve the selected toolchain. There is
  no exact Xcode-version pin in this repository; the scripts verify the
  selected tools and packaged minimum OS rather than claiming an immutable
  Xcode image.
- **Python 3.11+** on `PATH` as `python3`, or invoke the scripts with the full
  path to an existing/private 3.11+ interpreter. Apple's system Python may be
  too old. Native CI selects Python 3.12; 3.11 is the scripts' minimum.
- **CMake 3.28+** and `make` for Intel, or for an explicit arm64
  `--source-runtime` build. They may be supplied privately on `PATH`; the
  bootstrap does not install global tools.
- Network access for Cargo/native build dependencies. Model downloads are
  separate, explicit actions and are not part of packaging.

Apple Silicon normally uses `ort-sys`' checksum-pinned CPU runtime. Its locked
`ort-sys 2.0.0-rc.12` inventory has no Intel Mac prebuilt, so
[`scripts/prepare-macos-runtime`](../scripts/prepare-macos-runtime) builds CPU
ONNX Runtime **1.24.2** from immutable commit
`058787ceead760166e3c50a0a4cba8a833a6f53f`. Packaging invokes this automatically
for Intel, with two build jobs and a project-local
`target/native-runtime/<target>/1.24.2` cache. It verifies cached source and
archive identity before reuse; no system ORT install or version downgrade is
needed. `--source-runtime` explicitly selects the same source build on arm64.

### Obtain a development app

From the repository root, select **one** matching architecture:

```sh
# On a native Apple Silicon Mac:
target=aarch64-apple-darwin
output=dist/mac-arm64-dev
```

```sh
# Instead, on a native Intel Mac:
target=x86_64-apple-darwin
output=dist/mac-intel-dev
```

Then build and package this tree into a **new, nonexistent** output directory:

```sh
rustup install
python3 scripts/package-macos --target "$target" --ad-hoc --output "$output"
binary="$PWD/$output/Cantrip.app/Contents/MacOS/cantrip"
"$binary" --version
```

The command produces `Cantrip.app`, a `-development.zip`, `manifest.json`, and
`SHA256SUMS`. It snapshots the executable and every non-system dylib, rewrites
their load paths inside the bundle, checks Mach-O architecture/minimum OS, and
verifies the signature and bundled executable version. System Apple libraries
remain OS dependencies. The stable app identifier is `com.misty-step.cantrip`;
the bundle contains the microphone purpose string and audio-input entitlement.

**Ad-hoc means DEVELOPMENT ONLY: not hardened, not Developer ID signed, and not
notarized.** Without a Team ID, hardened library validation would reject the
contained third-party dylibs, so the development mode deliberately does not
claim hardened runtime. It is not a production distribution or a guarantee of
Gatekeeper acceptance. Do not disable Gatekeeper or fabricate signing metadata
to turn it into one.

The generated app is real and self-contained: build tools and the source
checkout are not runtime requirements. Open `Cantrip.app` deliberately in
Finder (or move the whole app, not just its executable, to your chosen app
location). No arguments, or the explicit `app` command, starts the menu-bar
host; `daemon` is an engine-only process without menu bar/HUD. Keep one startup
owner. Packaging does not launch the app, grant microphone access, download
models, enable Open At Login, or change user config/history/credentials.

The **bundle CLI path** is `Cantrip.app/Contents/MacOS/cantrip`, not a binary
under `Contents/Resources` and not a Linux installer destination. If deliberately
moved to `/Applications`, for example:

```sh
"/Applications/Cantrip.app/Contents/MacOS/cantrip" --version
```

Use the [usage](USAGE.md), [desktop](DESKTOP.md), and
[privacy](PRIVACY.md) contracts for explicit setup and attended use. New Mac
configuration selects clipboard/manual paste. Existing Auto/Paste/Type choices
are preserved and defer without opening the pasteboard or sending keys, never
silently downgraded.

### Production prerequisites and fail-closed packaging

Production mode is the default when `--ad-hoc` is absent. It requires all of:

- A clean committed source checkout; the packaged version/toolchain/metadata
  come from that revision, and the checkout is rechecked before output.
- A valid **Developer ID Application** signing certificate **with its private
  key** in the build user's Keychain. `--sign-identity` must select exactly one
  valid identity by certificate name or SHA-1.
- Its matching **ten-character Apple Developer Team ID**, passed as `--team-id`.
- An existing authenticated `notarytool store-credentials` Keychain profile
  passed as `--notary-profile`, plus Apple network access and the Xcode
  `notarytool`/`stapler` tools. Profile authentication is checked before building.

After obtaining those real prerequisites, substitute their exact values and
choose another new output directory:

```sh
python3 scripts/package-macos --target "$target" --output dist/mac-production \
  --sign-identity 'Developer ID Application: ORGANIZATION (TEAMID)' \
  --team-id TEAMID --notary-profile YOUR_EXISTING_PROFILE
```

`ORGANIZATION`, `TEAMID`, and `YOUR_EXISTING_PROFILE` above are placeholders,
not supplied certificates or credentials. The script signs nested libraries
and the app with hardened runtime and secure timestamps, verifies identity and
minimal entitlements, requires Apple notarization **Accepted** with no error
issues, staples and validates the ticket, and requires a passing Gatekeeper
assessment before publishing output locally. It then writes the final ZIP,
manifest, and checksums. There is **no unsigned, unnotarized, or ad-hoc fallback**;
missing identity/profile, rejected notarization, invalid stapling, or failed
assessment prevents distributable output. An existing output is never replaced.
The script does not publish a GitHub release.

Production signing/notarization has not been exercised here, and no signing
identity, Apple grant, or public Mac asset is asserted. Unlike Linux releases,
these locally produced Mac assets do not come with the published Linux
`release.json`/GitHub attestation contract. Checksums detect corruption, not
publisher identity. Review the manifest's architecture, source revision, dirty
state, runtime identity, signing mode, and notarization status honestly.

### macOS development and verification

Before ordinary Intel workspace Cargo commands, prepare the same pinned CPU
runtime and set its project-local link environment:

```sh
python3 scripts/prepare-macos-runtime --target x86_64-apple-darwin
export ORT_LIB_PATH="$PWD/target/native-runtime/x86_64-apple-darwin/1.24.2/build/Release"
export ORT_PREFER_DYNAMIC_LINK=0
```

Run from the repository root on the matching native host:

```sh
cargo fmt --all --check
cargo check --workspace --locked --all-targets
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo test --locked --package cantrip --example eval
python3 -m unittest discover -s tests -p 'test_release_*.py'
python3 scripts/verify-macos --app "$output/Cantrip.app" \
  --target "$target" --output /path/to/new/evidence-directory
```

The native helper relocates the real app with a system-only runtime PATH and
private HOME/XDG directories, explicitly downloads the public local model, and
transcribes only the checked public JFK WAV. It verifies missing-model refusal
without implicit downloads, denied/invalid-UID capture without live audio,
terminal Deferred for explicit Auto/Paste/Type without config migration,
graceful shutdown, and five offscreen production NSView HUD states. It saves
sanitized `verification.json` and own-view PNGs, not private transcripts.
A logged-in WindowServer session is needed for NSView rendering. An explicit
`--skip-hud --hud-unavailable-reason 'REASON'` records an omission, not a pass.

The native arm64 development app passed that relocated helper, workspace
clippy/tests, offline evaluation, and six real Mach-O relocation/signing tests.
The corresponding Intel CI definition is not evidence of an Intel run. Offscreen
views do not prove live panel positioning/focus. Attended microphone permission
and capture, shortcut/menu actions, Open At Login, general clipboard/manual
destination paste, Intel runtime execution, and production signing/notarization
remain unexercised. A file-STT smoke is not an attended microphone-to-editor
trial.
