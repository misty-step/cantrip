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
- Microphone dictation needs a working PipeWire user session and `pw-record`;
  selecting a specific microphone also needs `pw-dump` from PipeWire.
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
a source-build route, not a promise of a downloadable Mac release.

### Build prerequisites

- A source checkout, Git, rustup, and the exact Rust toolchain pinned in
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
for Intel, with two build jobs and a default
`target/native-runtime/<target>/1.24.2` cache outside CI. When `RUNNER_TEMP` is
set, standalone preparation and packaging instead share
`$RUNNER_TEMP/native-runtime/<target>/1.24.2`, outside the restored Cargo target
cache. A supplied `--output` still takes precedence. It explicitly builds the complete
CPU dependency closure and combines its static archives into `libonnxruntime.a`,
avoiding `ort-sys`' incomplete decomposed dependency list. Source revision and
every archive are checked before cache reuse; no system ORT install or downgrade
is needed. `--source-runtime` selects the same source build on arm64.

Native macOS CI persists the Intel runtime marker, static archives, and source
tree with an exact-key `actions/cache` entry. Source (including `.git` and headers)
is retained for the revision/clean-source checks; build objects, dependency source,
and CMake intermediates are not cached. The key includes the runtime version/revision, target,
minimum macOS, runner image, Xcode/SDK/compiler and CMake identities, and the
preparation script hash. Preparation still runs after restoration and rejects
an identity mismatch, modified source, or changed archive; there is no partial-key
fallback. The validated runtime is saved immediately, so a later app-verification
failure does not discard a successful source build. Both architectures retain
formatting, Clippy for every workspace target, workspace tests, the eval example,
Python release tests, and relocated-app proof.
For CI tiers, scheduling and publication policy, see
[Decision 0035](decisions/0035-bounded-ci-feedback.md).
Clippy also compiles the targets previously compiled by a separate `cargo check`.
Smoke-command failures report the command, exit status, and at most the last
4096 bytes of stderr, never transcript-bearing stdout.

Cache schema 2 rejects older/incomplete caches. For standalone preparation,
choose a fresh `--output`; packaging uses the same default cache, so
remove only your own obsolete cache deliberately before rebuilding there.

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

**Development signing is not hardened, Developer ID signed, or notarized.**
Without a Developer ID team, hardened library validation would reject the
contained third-party dylibs. Neither development mode is a production
distribution or a guarantee of Gatekeeper acceptance. Do not disable Gatekeeper
or fabricate signing metadata to turn it into one.

Ad-hoc signing binds macOS microphone access to that exact code. Replacing it
with a different build can return access to `not-determined`, requiring Allow
again. CI deliberately stays on `--ad-hoc`. For development updates on your own
Mac, create a stable local code-signing identity once and reuse it:

1. Open Keychain Access with `open -a "Keychain Access"`. Optionally create a
   separate keychain through File › New Keychain and keep its password private.
2. Select Keychain Access › Certificate Assistant › Create a Certificate. Give
   it your own development name, select **Self Signed Root** for Identity Type
   and **Code Signing** for Certificate Type, and save it in your chosen
   keychain. Retain both the certificate and its private key across updates.
3. Find its full certificate SHA-1. A self-signed identity may be listed as
   untrusted; that does not prevent local signing. Do not use `-v`, which hides
   identities that are not trusted:

   ```sh
   keychain="$HOME/Library/Keychains/YOUR_DEVELOPMENT.keychain-db"
   security find-identity -p codesigning "$keychain"
   ```

4. Unlock that keychain yourself, then build to a new output directory:

   ```sh
   security unlock-keychain "$keychain" # prompts for the password
   python3 scripts/package-macos --target "$target" \
     --dev-sign-identity YOUR_CERTIFICATES_FORTY_CHARACTER_SHA1 \
     --keychain "$keychain" --output dist/mac-local-development
   ```

`--keychain` is optional when the identity is already on the normal keychain
search list. The script never reads or stores a keychain password. The local
mode signs without hardened runtime or notarization, verifies a non-ad-hoc
signature whose designated requirement pins the selected certificate, and
records `local-identity-development` plus the public certificate SHA-1 in
`build.json` and `manifest.json`. Reuse the same identity and bundle identifier
to preserve the microphone grant when replacing a development app. Creating a
new identity is a new permission identity, even if its display name is the same.

The development signing modes are mutually exclusive. Neither accepts
production `--sign-identity`, `--team-id`, or `--notary-profile` inputs.

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

The first dictation Start or shortcut can ask for microphone access, then record
after Allow without another press. Installation, launch, status, polling, and
opening Settings do not ask. See the
[capture-owner authorization decision](decisions/0033-capture-owner-microphone-authorization.md)
and [attended Mac trial](USAGE.md#first-dictation-macos-attended-clipboard-trial)
for cancellation and denied-access guidance.

### Production signing setup on the operator's Mac

The Developer ID private key and notarization credentials stay in this Mac's
**login Keychain**. Nothing exports a private key, imports a `.p12` into CI,
creates a GitHub signing secret, or changes the operator's installed app.
Production signing remains unproved until the real Apple identity/profile
prerequisites below exist and the production path succeeds.

1. **Done.** The individual Apple Developer team is **A56K8A636Z** and its
   agreement is accepted. The expected team is enforced by `package-macos`.
   The public Team ID is visible at
   [Apple Developer Account > Membership details](https://developer.apple.com/account/).
2. **Open.** Install the new local Developer ID Application identity. On this
   Mac, open **Xcode > Settings > Accounts**, add/select that Apple
   Account, select the enrolled team, then **Manage Certificates > + >
   Developer ID Application**. Use a local certificate, not a cloud-managed
   signing service. In **Keychain Access > login > My Certificates**, expand
   the certificate and verify that its private key is underneath it. Do not
   choose Apple Development, Apple Distribution, or Developer ID Installer.
   Developer ID certificate creation requires the team's Account Holder.
   An existing certificate for this team expires 2031-03-03, but its private
   key is elsewhere and it is not a signing identity on this Mac. Do not revoke
   it or import its private key. The operator is creating a new certificate
   from a local CSR; same-team display names are not unique selectors.
3. If Xcode is unavailable, use Apple's
   [CSR procedure](https://developer.apple.com/help/account/certificates/create-a-certificate-signing-request/)
   on this Mac. Open Keychain Access, choose **Certificate Assistant > Request a
   Certificate from a Certificate Authority**, enter your email and a key name,
   leave CA Email Address empty, and choose **Saved to disk**. Then open
   [Certificates, Identifiers & Profiles > Certificates > + > Developer ID
   Application](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/),
   upload only that `.certSigningRequest`, download the public `.cer`, and
   double-click it to install into **login**. The CSR's private key never leaves
   this Mac. Verify the certificate/private-key pair in My Certificates.
4. **Open.** At [account.apple.com](https://account.apple.com), choose **Sign-In and
   Security > App-Specific Passwords > Generate an app-specific password**.
   Give it a Cantrip notarization label. Then run this command yourself,
   substituting only your Apple ID:

   ```sh
   xcrun notarytool store-credentials cantrip-notary \
     --apple-id YOUR_APPLE_ID --team-id A56K8A636Z \
     --keychain "$HOME/Library/Keychains/login.keychain-db"
   ```

   Omit `--password`. `notarytool` securely prompts for the app-specific
   password and validates it before saving. Do not paste it into chat,
   shell arguments, a repository, GitHub secrets, or `--sync`.
5. Run the read-only preflight, with no build or signing:

   ```sh
   python3 scripts/package-macos --preflight \
     --team-id A56K8A636Z --notary-profile cantrip-notary
   ```

   It prints the selected Developer ID Application name, Team ID, full public
   SHA-1, UTC expiry, Xcode tool paths, and whether the stored profile
   authenticates. Only valid identities that have a private key **in this
   Mac's login Keychain** are candidates; an old public certificate alone is
   excluded. Selection and `codesign` use the full SHA-1, never a display name.
   If multiple local private-key identities match, add
   `--sign-identity YOUR_PUBLIC_CERTIFICATE_SHA1`. To list public fingerprints:

   ```sh
   security find-identity -v -p codesigning "$HOME/Library/Keychains/login.keychain-db"
   ```

   The preflight reads only public certificate metadata and uses `notarytool
   history`; it never exports keys or prints the stored profile or its response.
6. **Open.** When `codesign` first asks to use that certificate's **private key**, verify
   that the requester is `/usr/bin/codesign`, enter your login Keychain
   password yourself, and choose **Always Allow**. This authorizes the signing
   tool, not Cantrip's provider key. Do not broaden access to every application.

Apple's [Developer ID guide](https://developer.apple.com/developer-id/) and
[notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow)
describe the certificate, hardened runtime, submission and stapling boundary.
The installed `xcrun notarytool store-credentials --help` also documents the
secure prompt when `--password` is omitted.

### Publish macOS assets for an already released tag

First let the conductor complete the normal release qualification and Linux
publication in [Decision 0035](decisions/0035-bounded-ci-feedback.md). On this
Mac, fetch the released tag into a **separate worktree** and check it out
exactly. Use a release that contains these scripts; do not copy a newer
publisher into an old tagged tree. Replace `vX.Y.Z` with that actual release:

```sh
git fetch origin --tags
git worktree add --detach ../cantrip-wt/publish-vX.Y.Z vX.Y.Z
# In that new worktree:
RUN="$HOME/.cache/tmp/cantrip-publish-vX.Y.Z"
mkdir -p "$RUN/tmp"
CARGO_BUILD_JOBS=4 TMPDIR="$RUN/tmp" python3 scripts/publish-macos \
  --tag vX.Y.Z --target aarch64-apple-darwin \
  --team-id A56K8A636Z --notary-profile cantrip-notary \
  --output "$RUN/arm64"
```

The publisher checks a clean tree, exact local tag/HEAD/workspace-version
agreement, the tag's commit on `misty-step/cantrip`, and an existing published
stable GitHub release. It refuses development or ad-hoc identities, existing
output, and any colliding release assets. There is no unsigned or unnotarized
fallback and no `--clobber`. `gh` uses the existing native GitHub login.

Packaging signs every nested dylib and the app using the login-Keychain
Developer ID identity, hardened runtime, minimal audio-input entitlement and
secure timestamps. It checks signatures, requires notarization **Accepted**
and a clean error log, staples and validates the ticket, and requires
`spctl --assess --type execute`. It zips the **stapled app**, then attaches:

- `cantrip-vX.Y.Z-aarch64-apple-darwin.zip`.
- `cantrip-vX.Y.Z-aarch64-apple-darwin.manifest.json`.
- `cantrip-vX.Y.Z-aarch64-apple-darwin.SHA256SUMS`.

The manifest binds the source, architecture, certificate SHA-1, runtime,
notarization submission ID, executable and final ZIP hashes. The target-specific
checksum file covers the ZIP and manifest. The publisher downloads all three
uploaded assets again and compares their bytes. Linux `release.json`,
`SHA256SUMS` and provenance are untouched. If upload is interrupted, inspect
the existing release; the publisher refuses to replace even a partial set.
These local Mac assets are not covered by the Linux GitHub build attestation.

**Intel is not cross-buildable by this repository's supported toolchain.**
`prepare-macos-runtime` deliberately rejects cross/Rosetta builds, and this
Mac has only `aarch64-apple-darwin` installed. Native CI uses `macos-15-intel`
and source-builds checksum/revision-pinned CPU ONNX Runtime 1.24.2 because
`ort-sys` supplies no Intel prebuilt. That CI app is ad-hoc development output,
not a distributable substitute. Do not move the signing private key or notary
profile to an Intel runner or another Mac. The publisher's generic
`--target x86_64-apple-darwin` option does not remove the native-build guard.
This local path publishes arm64 only on this Mac; no Intel production asset
or second signing home is authorized by this setup.

For packaging without GitHub publication, continue to use:

```sh
python3 scripts/package-macos --target aarch64-apple-darwin \
  --sign-identity YOUR_PUBLIC_CERTIFICATE_SHA1 \
  --team-id A56K8A636Z --notary-profile cantrip-notary \
  --output /path/to/new/production-output
```

### Measure Keychain grants across code updates

[`verify-keychain-persistence-macos`](../scripts/verify-keychain-persistence-macos)
uses the real Cantrip executable in one disposable `macvm`, not the operator's
app, Keychain, clipboard or screen. Finish every host build/signing operation
before booting the VM; this memory-constrained Mac permits one native build or
one VM, never both. Coordinate that slot before running.

The test packages A and never-run B from the same real source with a trivial
sealed resource marker difference and checks distinct cdhashes. Certificate
arms must retain the same designated requirement. It records each signature,
noninteractive read outcome/timing and **only the dummy items' ACL metadata**.
It checks both:

1. A creates its own dummy item with `cantrip key set`, reads it naturally,
   then B attempts a new noninteractive read.
2. An additional ad-hoc Cantrip seed creates a second dummy item. A must fail
   to read it without approval, then the app's real `key allow` opens the
   native prompt. After **Always Allow**, a fresh A read must succeed before
   B is installed. B has never run or been granted access to either item.

The seed is necessary because an item's creator is already trusted and
normally has no initial approval prompt. No fixture uses `security -A`,
adds a trusted helper, or rewrites a partition list. Merely alternating two
cdhashes that have each been granted access does not test an unseen update.

For both controls, unlock the existing development-signing Keychain yourself
and prepare both arms before starting the VM:

```sh
proof="$HOME/.cache/tmp/cantrip-keychain-controls"
CARGO_BUILD_JOBS=4 python3 scripts/verify-keychain-persistence-macos prepare \
  --output "$proof" --arm development --arm ad-hoc \
  --dev-sign-identity YOUR_DEVELOPMENT_CERTIFICATE_SHA1 \
  --keychain /path/to/your/development.keychain-db
```

Once a real Developer ID certificate exists, the exact preparation command is:

```sh
proof="$HOME/.cache/tmp/cantrip-keychain-developer-id"
CARGO_BUILD_JOBS=4 python3 scripts/verify-keychain-persistence-macos prepare \
  --output "$proof" --arm developer-id --team-id A56K8A636Z
```

Add `--sign-identity YOUR_PUBLIC_CERTIFICATE_SHA1` only if needed to disambiguate.
The Developer ID arm needs a signing identity, not notarization credentials;
its experimental bundles are explicitly **not for distribution**.
Then use the same attended phases, setting `arm` to `development`, `ad-hoc`,
or `developer-id` as selected during preparation:

```sh
arm=developer-id
python3 scripts/verify-keychain-persistence-macos start --output "$proof" --arm "$arm"
# Inspect "$proof/$arm-A-approval.png" before choosing the actual button coordinates.
# The password here is ONLY the disposable guest's password, never the host's.
read -s GUEST_KEYCHAIN_PASSWORD
export GUEST_KEYCHAIN_PASSWORD
python3 scripts/verify-keychain-persistence-macos approve --output "$proof" \
  --arm "$arm" --password-env GUEST_KEYCHAIN_PASSWORD --always-allow X Y
unset GUEST_KEYCHAIN_PASSWORD
# Inspect the new after-approval PNG. If a second prompt remains, inspect and repeat approve.
python3 scripts/verify-keychain-persistence-macos update --output "$proof" --arm "$arm"
# For controls, repeat start/approve/update for the other prepared arm in this SAME VM.
python3 scripts/verify-keychain-persistence-macos cleanup --output "$proof"
```

`X Y` must come from the inspected `macvm shot`, not fixed coordinates copied
from another run. `approve` remains pending unless A's noninteractive read
succeeds. `update` records both preservation results without opening an
approval dialog. Inspect its screenshot too. A recorded `false` is a measured
failed persistence claim, not a failed experiment or permission to approve B.
Always run `cleanup`, including after a failed phase. Keep `experiment.json`
and reviewed PNGs outside Git and link retained evidence from the PR.

Apple's [TN2206](https://developer.apple.com/library/archive/technotes/tn2206/_index.html)
explains that trusted-application ACLs track a designated requirement and that
self-signed identities can keep that requirement stable. That is not the whole
modern file-based Keychain check. Apple's published
[`ClientIdentification::partitionIdForProcess`](https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/securityd/src/clientid.cpp)
assigns Apple-verified development/Developer ID code a `teamid:` partition,
but other signed code, including these no-team self-signed and ad-hoc apps,
gets `cdhash:`. Its
[`SecurityServerAcl::validatePartition`](https://github.com/apple-oss-distributions/Security/blob/db15acbe6a7f257a859ad9a3bb86097bfe0679d9/securityd/src/acls.cpp)
checks this **in addition to** ordinary ACL validation. Always Allow can extend
the current client's partition, not pre-authorize every future code hash.
Therefore a certificate-pinned ACL can still match while a changed cdhash
requires approval. Developer ID's stable team partition is the expected remedy,
but only the real Developer ID arm proves it on the current macOS release.

### macOS development and verification

Before ordinary Intel workspace Cargo commands, prepare the same pinned CPU
runtime and set its project-local link environment:

```sh
runtime_environment="$(python3 scripts/prepare-macos-runtime --target x86_64-apple-darwin)" &&
export ORT_LIB_PATH="$(printf '%s\n' "$runtime_environment" | sed -n 's/^ORT_LIB_PATH=//p')"
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
without implicit downloads, denied/restricted or invalid-UID capture refusal
without live audio, terminal Deferred for explicit Auto/Paste/Type without
config migration, graceful shutdown, seven offscreen production NSView HUD states,
each native Settings pane, and the Recordings and Recovery window's Recordings view and its
Setup view after Check Setup, all rendered offscreen. It saves
sanitized `verification.json` and own-view PNGs, not private transcripts.

The capture check reads the owner's authorization first. For undetermined
access it records `not-run` without sending Start or opening a real prompt;
authorized access checks the invalid UID, and denied/restricted access checks
the corresponding authorization refusal class.

A logged-in WindowServer session is needed for NSView rendering. An explicit
`--skip-hud --hud-unavailable-reason 'REASON'` records an omission of those
renders, not a pass.

Record exact-head platform results in the PR. Offscreen views draw the HUD's
pixels, not the live panel's position or focus, and file STT is not a
microphone-to-editor trial. Use
the [attended Mac trial](USAGE.md#first-dictation-macos-attended-clipboard-trial) for microphone,
shortcut/menu, login, and general-clipboard/manual-paste behavior. Intel runtime
execution and production signing/notarization each require their own proof,
not inference from an arm64 development app.

#### Disposable VM proof (no physical headset)

Use the candidate CI-built app in a fresh `macvm` guest, not the operator's Mac.
Install BlackHole 2ch and `switchaudio-osx` in that guest. Explicitly select the
same BlackHole device as the guest's system input and output. The driver refuses
non-`VirtualMac` machines and creates its own private configuration/history.
It changes no system audio routing and uses only `samples/jfk.wav`.

Inside the guest, from this checkout (Python 3.11+):

```sh
APP=/Applications/Cantrip.app
RUN="$HOME/.cache/tmp/cg-proof"
PROOF=/absolute/path/to/cantrip/scripts/verify-capture-setup-macos
python3 "$PROOF" --app "$APP" --run "$RUN" --pull-model prepare
```

`--pull-model` explicitly downloads the public local model into the isolated
data directory. Do not start a second Cantrip app in the same proof session.

1. Focus a guest scratch editor. Press **Control+Option+Space**; answer Allow
   if asked. Inspect the HUD without changing editor focus. Open the Cantrip
   menu and inspect **Open Settings** and **Use BlackHole 2ch instead**.
   Run the driver's `missing` phase to check the failure class, guidance and
   unchanged config.
2. Click **Open Settings** and inspect the General/input pane. Close Settings.
   Reopen the Cantrip menu and click **Use BlackHole 2ch instead**. Run `default`:
   only that explicit action may save the UID, and it must not start capture.
3. Focus the editor and press again. Wait at least three seconds without
   playback; inspect the named no-sound HUD/menu guidance. Run `silence`;
   the take must still be recording with Stop/Cancel available.
4. Run `play` to send the public fixture to BlackHole. While playback is active,
   run `signal` in a second guest command to check measured signal and cleared
   guidance. Inspect the HUD and unchanged editor focus.
5. Press the shortcut to stop after playback. Run `complete` and paste into the
   guest editor. Inspect the public sentence; publish no transcript or audio.
   Repeat the failure and silence inspections in light and dark appearances.

Each phase uses the same arguments, for example:

```sh
python3 "$PROOF" --app "$APP" --run "$RUN" missing
python3 "$PROOF" --app "$APP" --run "$RUN" cleanup
```

Collect only `proof-*.json` and reviewed VM screenshots, not the private run's
history, WAVs or logs. `cleanup` stops only the app PID/start identity this
driver launched. Remove the private run directory and delete the disposable VM
when done. This is an attended proof recipe, not a claim that it has been run.

### Mode picker and live HUD proof in a disposable Mac VM

The [author-run receipt and reviewed screenshots](evidence/verify-cantrip/2026-11-03-modes/run.json)
record the exercised menu, Settings, three modes, capture snapshot and recovery
path, including the app's exact source and installed-binary hash. The recipe
below reproduces that path; dummy providers prove routing and presentation,
not recognition or model quality. The
[cloud evaluation receipt](../eval/baselines/2026-11-03-elevate/run.json)
separately owns real Sol-low behavior and its remaining fidelity failures. The
[mode display captures](evidence/verify-cantrip/2026-11-04-mode-display/README.md)
record the menu, Settings and HUD presentation in Light and Dark, before and
after.

Finish an ad-hoc development package **before** booting the VM. Use a fresh
output path after a code change, and never rebuild while a proof VM is running:

```sh
RUN="$HOME/.cache/tmp/cantrip-modes-$(git rev-parse --short HEAD)"
mkdir -p "$RUN/tmp"
CARGO_BUILD_JOBS=4 TMPDIR="$RUN/tmp" python3 scripts/package-macos \
  --target aarch64-apple-darwin --ad-hoc --output "$RUN/build"
macvm up --share "$RUN" --share "$PWD"
```

Install the bundle inside the disposable guest with `ditto`; launch it with
`open -a /Applications/Cantrip.app`, not as a foreground CLI capture process.
Use the [existing VM audio setup](evidence/verify-cantrip/2026-11-02-macos-hud-omarchy/README.md#setup):
BlackHole 2ch, `audio_source = "BlackHole2ch_UID"`, guest system output routed
to BlackHole, and the public `samples/jfk.wav`. Allow microphone access through
the guest's native prompt. No model downloads, provider credentials, paid
requests, host microphone, or host clipboard are needed.

1. Launch this dummy HTTP provider in a separate guest command, with the shared
   checkout as its working directory. It substitutes synthetic recognizer text
   and fixed per-mode replies. Assertions inspect the real requests, including
   aliases and the operator/addendum bytes; output contains route metadata only.

   ```sh
   python3 - <<'PY'
   import hashlib, json, time
   from http.server import BaseHTTPRequestHandler, HTTPServer
   from pathlib import Path
   operator = Path("crates/engine/src/elevate-prompt.txt").read_bytes().decode()
   fidelity = Path("crates/engine/src/elevate-fidelity.txt").read_bytes().decode()
   raw = "how can we make the vertical space between lines smaller in Gantrip without changing the font size of 16 px"
   aliased = raw.replace("Gantrip", "Cantrip")
   replies = {
       "dummy-clean": "How can we make the vertical space between lines smaller in Cantrip without changing the font size of 16 px?",
       "dummy-elevate": "How can we reduce leading in Cantrip without changing the 16 px font size?",
   }
   class Provider(BaseHTTPRequestHandler):
       def log_message(self, *_): pass
       def do_POST(self):
           body = self.rfile.read(int(self.headers["Content-Length"]))
           event = {"route": self.path}
           if self.path == "/v1/audio/transcriptions":
               assert b"dummy-stt" in body
               reply = {"text": raw}
           elif self.path == "/v1/chat/completions":
               request = json.loads(body)
               model = request["model"]
               assert model in replies
               assert request["reasoning"] == {"effort": "low"}
               assert request["messages"][1]["content"] == "Source:\n" + aliased
               system = request["messages"][0]["content"]
               if model == "dummy-elevate":
                   assert system.startswith(operator + "\n" + fidelity)
               else:
                   assert not system.startswith(operator)
               event.update(model=model, system_sha256=hashlib.sha256(system.encode()).hexdigest())
               reply = {"choices": [{"message": {"content": replies[model]}}]}
           else:
               self.send_error(404); return
           print(json.dumps(event), flush=True)
           time.sleep(7)
           data = json.dumps(reply).encode()
           self.send_response(200)
           self.send_header("Content-Type", "application/json")
           self.send_header("Content-Length", str(len(data)))
           self.end_headers()
           self.wfile.write(data)
   HTTPServer(("127.0.0.1", 9900), Provider).serve_forever()
   PY
   ```

2. In the guest's otherwise disposable configuration, use this setup. The
   legacy `enabled` line intentionally exercises
   [migration](CONFIGURATION.md#migration). The thirty-second budget leaves
   room to inspect the deliberately delayed LLM stage.

   ```toml
   injection = "clipboard"
   keep_warm = false
   audio_source = "BlackHole2ch_UID"
   vocabulary = ["API", {term = "Cantrip", aliases = ["Gantrip"]}, "HUD", "Parakeet", {term = "PipeWire", aliases = ["pipe wire"]}, "WAV"]

   [stt]
   model = "dummy-stt"
   endpoint = "http://127.0.0.1:9900/v1"

   [postproc]
   enabled = false
   endpoint = "http://127.0.0.1:9900/v1"
   api_key_id = ""
   timeout_ms = 30000
   min_chars = 0
   # keep this comment

   [postproc.clean]
   model = "dummy-clean"
   reasoning_effort = "low"

   [postproc.elevate]
   model = "dummy-elevate"
   reasoning_effort = "low"

   [telemetry]
   enabled = false
   ```

3. Open Cantrip's menu. Confirm Raw is the selected radio item. Select each mode
   and reopen the menu to inspect its radio mark, glyph, promise and where the
   take goes; the tooltip lists each stage with its raw model id and host.
   Choosing a mode persists the next-take default: the file must own `mode`,
   have no `[postproc].enabled`, and retain the unrelated comment.
   In this fixture the recognizer is an HTTP lane on loopback, so the menu and
   Settings name “a local server” and the HUD says “on this Mac”.
4. Open **Settings… → Modes**, navigating from the shorter Transcripts pane.
   Select Clean and click **Save & Apply**. Confirm the persisted mode and
   selected menu entry agree. Repeat for Elevate. Inspect exclusive selection,
   each mode's explanation, and under **Details** the raw ids, both model/effort
   fields and the shared endpoint/key ID.
   At 1440×900 with the Dock visible, confirm **Save & Apply** is reachable,
   including after the “Saved and applied” notice appears and with Details open.
5. Complete a take in each mode, playing `samples/jfk.wav` into BlackHole
   while recording. Inspect the live recording and processing HUDs: the line
   names the mode and where the take goes; Raw lights transcribing, and Clean
   and Elevate light transcribing, then refining with the spellings light filled.
   Capture both stages during the seven-second delays.
6. On the Elevate take, choose Raw from the menu **while recording**. The
   picker's next-take default changes, but the active HUD and the projected
   `status --json` pipeline must retain Elevate through both processing stages.
7. Compare each delivered guest clipboard privately with the corresponding
   fixture reply; Raw must match the recognizer text exactly. Do not print
   clipboard contents or unfiltered status/history into logs. Inspect only
   archive metadata: the recorded `mode`/`mode_model`, `alias_substitutions`
   (zero for Raw, one otherwise), and `postproc.status` (`off` versus `applied`).
   Raw must make one STT request and no chat request; Clean and Elevate must
   each make one STT request followed by the correct model's chat request.
8. Set the default back to Elevate. Run
   `cantrip start --postproc raw` inside the guest, wait for the no-sound cue,
   and confirm the HUD still identifies Raw alongside the input guidance.
   Play the public fixture and confirm the guidance clears. Cancel; the take
   must retain its WAV and recorded Raw mode without any provider request.
9. Recover that same take while keeping the default Elevate. This guest command
   waits for cancellation, selects only the take ID from captured status, and
   discards recovery stdout so no transcript reaches logs:

   ```sh
   python3 - <<'PY'
   import json, subprocess, time
   binary = "/Applications/Cantrip.app/Contents/MacOS/cantrip"
   deadline = time.monotonic() + 60
   while True:
       status = json.loads(subprocess.check_output([binary, "status", "--json"]))
       outcome = status.get("outcome") or {}
       if status["state"] == "idle" and outcome.get("completeness") == "cancelled":
           break
       if time.monotonic() >= deadline:
           raise SystemExit("Cancelled take did not settle")
       time.sleep(.1)
   subprocess.run(
       [binary, "recover", "--id", outcome["artifacts"]["take_id"], "--clipboard"],
       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
   )
   PY
   ```

   Recovery must deliver Raw, retain the same take ID and audio, apply zero
   aliases, and make no chat request. Compare the clipboard privately.
10. Repeat representative menu, Modes pane and HUD inspections in Light and
    Dark, changing appearance through guest System Settings. Check wrapping,
    square HUD corners, cleared input guidance and visible save controls.
11. Quit the guest app, stop only this fixture server, and delete the VM with
    `macvm down`. Retain reviewed, transcript-free screenshot crops and sanitized
    revision-bound results, never the guest history, WAVs, clipboard or logs.

