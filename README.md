# Cantrip

Local-first dictation for Linux and macOS. Press a shortcut, speak, press it again.
Cantrip transcribes on your CPU. Linux can deliver to a verified destination;
macOS uses explicit clipboard delivery and manual paste, never automatic keys.

[Website](https://cantrip.mistystep.io) ·
[Download a Linux release](https://github.com/misty-step/cantrip/releases/latest) ·
[Install](docs/INSTALLATION.md) ·
[First dictation](docs/USAGE.md#first-dictation)

## Features

- **Local by default.** CPU-only Parakeet speech recognition through
  [transcribe-rs](https://github.com/cjpais/transcribe-rs). Download the model
  deliberately once; it is not bundled in the executable.
- **Quiet native feedback.** A passive pixel HUD shows microphone activity and
  processing without taking focus or inventing progress. Native Settings and
  Actions handle configuration and selected-recording recovery.
- **Guarded delivery.** Linux automatic paste/typing requires supported
  Hyprland/logind focus and session history. Other Linux desktops and macOS use
  clipboard/manual paste. An uncertain handoff is not automatically retried;
  existing macOS Auto/Paste/Type choices defer without clipboard or key effects.
- **Explicit choices.** Cleanup and cloud providers are optional; cleanup and
  telemetry are off by default. Stopped audio and plaintext transcript history
  are retained locally until deliberately removed.

## Quickstart

### Download and install

Use the [published Linux x86-64 archive](https://github.com/misty-step/cantrip/releases/latest):
no checkout, Rust, GPU, or CUDA is needed. The baseline is Ubuntu 24.04 / glibc
2.39. Follow [installation and provenance verification](docs/INSTALLATION.md),
then [first attended dictation](docs/USAGE.md#first-dictation). Models, shortcuts,
and any startup service are separate explicit setup steps; the installer changes
only the executable.

### Build Linux from source

Source development is separate from release installation. With rustup and a C
linker/toolchain installed, use the exact Rust version pinned in
[`rust-toolchain.toml`](rust-toolchain.toml). On Ubuntu, native **build** packages
are installed with:

```sh
sudo apt install build-essential libdbus-1-dev libssl-dev pkg-config
```

From the repository root, compile without installing:

```sh
rustup install
cargo build --release --locked
./target/release/cantrip --version
```

To install a source-built binary, one command builds **this tree** and atomically
replaces `$HOME/.local/bin/cantrip`. It never copies a previously compiled
`target/` artifact, so a git change cannot sneak in between build and install.
It refuses a symlink destination and a live daemon. First install and later
updates are the same command. See [ADR 0025](docs/adr/0025-source-install-builds-this-tree.md).

```sh
# Stop the existing owner first: docs/DESKTOP.md#stop-update-and-remove
./scripts/install-from-source
```

Now use the same [first-dictation guide](docs/USAGE.md#first-dictation) as archive
users. Do not reinitialize existing config, download models implicitly, or start
a second daemon. The optional [shared service setup](docs/DESKTOP.md#user-service-graphical-session)
uses `contrib/cantrip.service` for source builders and does not copy the binary.

Use the package owner's procedure for package-managed binaries. Leave unchanged
units, personal drop-ins, configuration, models, keyring credentials, and history
alone. Restart only the intended owner and repeat the attended dictation. A
backed-up executable can restore code; it does not reverse history-schema
changes. For published archives, use the installer's
[update and rollback operations](docs/INSTALLATION.md#update-and-roll-back)
instead of this source-build procedure.

### Build a native macOS app

macOS 13.3+ has a native menu-bar app, microphone capture, passive HUD, and
login Keychain integration. There is **no published macOS release**. Build on a
matching native Apple Silicon or Intel Mac using the
[macOS source-build procedure](docs/INSTALLATION.md#macos-native-app-from-source).
It produces a self-contained `Cantrip.app`; Rust, Python, and Xcode are build
tools, not runtime requirements. An explicit ad-hoc **development** build is
neither hardened nor notarized. Production packaging requires Developer ID
signing, accepted notarization, stapling, and a passing Gatekeeper assessment;
missing prerequisites fail closed.

## Requirements

See [binary runtime prerequisites](docs/INSTALLATION.md#runtime-prerequisites-and-support)
and [desktop capabilities](docs/DESKTOP.md#supported-desktops). On Linux, a
working PipeWire session with `pw-record` is needed for capture;
clipboard/paste needs `wl-copy`. Automatic keys and the passive HUD have
separate compositor requirements. On macOS, capture uses AVAudioEngine with
explicit microphone permission; delivery is clipboard/manual paste. `doctor`
and an idle daemon are not proof of a microphone-to-editor dictation.

## User service (graphical session)

This section is the Linux service route; macOS startup belongs to the native app.

The optional [user-service procedure](docs/DESKTOP.md#user-service-graphical-session)
owns service installation, one-owner checks, session environment, and readiness.
Archive users need no source checkout or source-build binary-copy step.

### Fresh installation

See [fresh service installation](docs/DESKTOP.md#fresh-installation), after an
attended first dictation. The binary is installed separately.

### Environment and readiness

See [session environment and readiness](docs/DESKTOP.md#environment-and-readiness).
Do not manually start the graphical-session target or enable lingering as a bypass.

### Stop, update, and remove

See [startup-owner maintenance](docs/DESKTOP.md#stop-update-and-remove) and
[binary maintenance](docs/INSTALLATION.md#stop-before-maintenance).

## CLI

The [CLI reference](docs/USAGE.md#cli-reference) and
[everyday recording controls](docs/USAGE.md#everyday-controls) live in the usage
guide. Examples use explicit installed paths; installation does not alter `PATH`.

## Recovery

[Use and recover selected recordings](docs/USAGE.md#recovery). Copy never sends
keys. Confirmed Forget deletes retained audio and incomplete text but
[keeps complete archived transcript text](docs/PRIVACY.md#what-forget-deletes).

## Omarchy integration

See the optional [Omarchy badge and menu integration](docs/DESKTOP.md#omarchy-integration).
It is checkout-based, attended, and separate from binary/service/shortcut setup.

## Configuration

See [configuration](docs/CONFIGURATION.md) for local/cloud STT, opt-in cleanup,
delivery, HUD accessibility, and telemetry. `config show` serializes the
configuration loaded from disk with defaults, not the original file/comments
or the running daemon's snapshot. Native Settings saves and reloads the file.

## Privacy

Read [privacy and retained data](docs/PRIVACY.md) before speaking sensitive
material. Default local recognition does not upload content, but successful,
cancelled, and undelivered audio and plaintext history are retained. Cloud
features, metadata telemetry, clipboard exposure, and exact deletion limits
are separate choices described there.

## Evaluation gauntlet

`examples/eval` scores STT and cleanup lanes for WER/CER, latency, and cost.
The `living-speech` command compares at least five current models on explicitly
reviewed real dictation, retains text-free run receipts, and enforces a durable
$4.50 commission spending cap. [Published speech results](https://cantrip.mistystep.io/evals)
keep our measurements separate from cited independent and vendor benchmarks.
Reproduction, privacy, and scheduling procedures: [evaluation guide](docs/EVALUATION.md).

## Architecture

The Rust workspace has two crates and one version authority in
`workspace.package.version`:

- [`cantrip-engine`](crates/engine/) owns the dictation workflow, cancellation,
  durable per-take archive/recovery, configuration, IPC, models, STT, cleanup,
  and telemetry. There is one workflow and one history store, not one per OS.
- The root `cantrip` crate is the CLI/native host. Its
  [`Platform`, `Recorder`, and `DeliveryPermit` ports](crates/engine/src/ports.rs)
  supply capture, stop-time session/destination history, and native delivery
  mechanisms; policy and durable transitions remain in the engine.
- Linux retains PipeWire capture, Hyprland/logind guarded keys, and its
  supervised Wayland HUD process. The macOS accessory app owns AppKit on the
  main thread and the shared engine on a std thread, or attaches to an existing
  engine without taking ownership. Its `daemon` command is engine-only, with
  no menu bar or HUD.

Both hosts keep the std-thread + mpsc model; neither introduces an async
runtime or another durable work ledger. See
[ADR 0031](docs/adr/0031-shared-engine-native-macos.md) for boundaries, manual-paste
policy, packaging, and verification limits.

## Development

For Linux development, after installing the pinned toolchain as above:

```sh
cargo build --locked
./scripts/check
```

`scripts/check` is the Linux clone-to-green command: it runs
`cargo fmt --all --check`,
`cargo clippy --workspace --locked --all-targets -- -D warnings`,
`cargo test --workspace --locked`, the offline
`cargo test --locked --package cantrip --example eval` suite, and
installer/release safety tests (Python 3.11+), stopping at the first red gate.
The example tests do not contact providers. On macOS, use the native
[development and verification procedures](docs/INSTALLATION.md#macos-development-and-verification),
including the Intel source-runtime preparation before Cargo gates.

- **Local git hooks** (format + clippy on commit, tests + secret scan on push):
  `.githooks/install.sh`. After installing hooks, `gitleaks` and `trufflehog`
  must be on `PATH`; the hooks fail closed when either scanner is missing.
- **CI** ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)): fmt, clippy
  `-D warnings`, tests, secret scan, and on `master` the Landmark-prepared
  verified Linux release.
- **Native macOS CI** ([`.github/workflows/macos.yml`](.github/workflows/macos.yml)):
  matching arm64/Intel workspace gates, explicit development packaging, and
  relocated public-fixture verification. This workflow was not dispatched as
  part of the native smoke; its definition is not a claim of an Intel pass.
- **Website and documentation source:** [`site/`](site/). The five user guides
  below are canonical Markdown, rendered by the site rather than copied into
  a second web manual. Website release data comes from published release assets,
  not an unreleased Cargo version or local technical changelog.
- **Architecture decisions:** [`docs/adr/`](docs/adr/). Operational log tags:
  `[Daemon]` `[Capture]` `[STT]` `[Postproc]` `[Inject]` `[Models]` `[HUD]`.

### Verify Settings transcript copy headlessly

US-005 has an isolated native Settings journey. Build the binary, then run:

```sh
BROWSER=none CI=1 python3 scripts/verify-settings-history.py \
  --binary target/debug/cantrip --out /path/to/empty/evidence-directory
```

It requires `sway`, `grim`, `wtype`, `wl-copy`, `wl-paste`, `tesseract`, and
`wf-recorder` on `PATH`. The runner creates its own headless software-rendered
compositor and HOME/XDG directories; it never uses the live desktop or daemon.
It checks exact full-text clipboard bytes, unchanged archive contents, stale-row
failure, and refresh to empty history. Screenshots, a screen recording, and a
content-free proof report are saved under `--out`.

To verify a real old archive, pass `--fixture /path/to/TAKE_ID.json`. The source is
read-only; the runner copies it into disposable history. Review its opening words
before sharing the recording or screenshots. Private transcripts are not fixtures
to commit or upload automatically.

### Review the Linux native HUD

Open the local developer gallery without starting dictation:

```sh
cargo run --locked -- hud-gallery
cargo run --locked -- hud-gallery --screenshot /tmp/cantrip-hud-gallery.png
```

The gallery uses fixture status events with the production HUD state machine,
pixel renderer, font, and palette—not a browser imitation. Browse the full state
catalog or replay transition journeys; pause, scrub, step frames, and zoom to
inspect the pixels. Labels and reduced-motion controls affect only the gallery.
It does not start the daemon, capture audio, run speech models, make provider
requests, or read recording history or credentials.
Fixture stage durations are illustrative, not an STT latency benchmark; native
transition, acknowledgement-hold and fade timing come from the production code.
The “Three chunks, fast cleanup” journey exercises rapid backend handoffs.
Its event markers describe fixture inputs; the visible phase may deliberately
trail them while the production renderer finishes its bounded transition.

`hud-gallery` is intentionally hidden from ordinary CLI help. It is a local
native window, not a public website route or a separate development server.
The existing `hud --screenshot PATH --state NAME` command still captures the
actual layer-shell surface for compositor-specific verification.

### Verify the relocated macOS app

After building on the matching native Mac:

```sh
python3 scripts/verify-macos \
  --app dist/mac-arm64-dev/Cantrip.app \
  --target aarch64-apple-darwin \
  --output /path/to/new/evidence-directory
```

The helper relocates the real bundle, verifies its signature/Mach-O identity,
and runs with disposable HOME/XDG directories and a system-only runtime PATH.
It explicitly downloads the public local model and transcribes the checked JFK
WAV, checks missing-model refusal without implicit downloads, tests denied or
invalid-device capture without live audio, and replays explicit Auto/Paste/Type
to terminal Deferred without rewriting config. It also checks graceful daemon
termination and saves five states of the production macOS HUD as **offscreen
NSView pixels**, plus a sanitized `verification.json`. A logged-in WindowServer
session is needed for those pixels; `--skip-hud` requires an explicit
`--hud-unavailable-reason` and records the omission rather than a pass.

The native arm64 development-app smoke completed this helper, workspace
clippy/tests, offline evaluation, and real Mach-O relocation/signing tests.
Linux `scripts/check` and the isolated Settings full-text/stale-copy journey
also passed. These are distinct proofs: offscreen NSView images do **not**
prove panel placement/focus or attended microphone-to-editor success.
Live microphone permission/capture, global shortcut/menu actions, Open At Login,
general macOS clipboard/manual destination paste, Intel execution, and production
signing/notarization were not exercised. No operator permissions or live
microphone/general clipboard were touched by the helper.

## Work and documentation ownership

Work starts from the user's current request, checked against live code and
overlapping work. Follow the existing work record when available; neither an old
issue nor a document authorizes automatic intake.
Powder is retired. [ADR 0017](docs/adr/0017-powder-board-of-record.md) preserves
the earlier migration and its rejection of duplicate boards, not live routing.
GitHub issue history remains context rather than a second backlog.

The repository owns version-bound product/system contracts, accepted technical
decisions, portable procedures, and curated public/synthetic eval inputs and
baselines. Work records hold safe summaries and links to proof; raw, large, or
sensitive run output belongs in approved retained artifact storage, subject to
the existing privacy boundary. Owner-private recording history stays local and
never becomes a repository fixture or work attachment automatically.
[VISION.md](VISION.md) is optional product rationale, not mandatory reading or
a higher authority than the request.

## Prior art & credits

Design informed by [Handy](https://github.com/cjpais/Handy) and its
[`transcribe-rs`](https://github.com/cjpais/transcribe-rs) engine, which Cantrip
uses directly, and by [Vox](https://github.com/misty-step/vox), our macOS
predecessor whose pipeline architecture and privacy rules carry over.

## License

[MIT](LICENSE) © Misty Step.

## Docs

- [Install](docs/INSTALLATION.md) — verified Linux releases, native Mac builds, and lifecycle.
- [Use](docs/USAGE.md) — first dictation, controls, outcomes, recovery, and CLI.
- [Configure](docs/CONFIGURATION.md) — settings and explicit provider choices.
- [Desktop](docs/DESKTOP.md) — support, shortcuts, startup owners, and diagnostics.
- [Privacy](docs/PRIVACY.md) — network boundaries, retained audio/text, and deletion.
- [Published release notes](https://github.com/misty-step/cantrip/releases).
- [Evaluation gauntlet](docs/EVALUATION.md).
- [Architecture decisions](docs/adr/) — chronological rationale, not a flat list
  of current implementation requirements. Older decisions retain their original
  evidence and link forward where superseded.
  - [Per-take recovery and verified delivery](docs/adr/0019-per-take-recovery-and-verified-delivery.md),
    refined by [local completion and explicit audio deletion](docs/adr/0020-local-completion-and-explicit-audio-deletion.md)
    and the [signed waveform contract](docs/adr/0021-signed-pixel-waveform.md),
    painted as a [persistent pixel field](docs/adr/0023-persistent-pixel-field.md).
  - [Private history and fixture promotion](docs/adr/0013-local-transcript-history.md)
    and [eval-driven post-processing](docs/adr/0012-eval-driven-postprocessing.md).
  - [Shared engine and native macOS](docs/adr/0031-shared-engine-native-macos.md) —
    platform ports, explicit manual paste, packaging, and proof boundaries.
