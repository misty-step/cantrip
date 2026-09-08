# Cantrip

Local-first dictation for Linux. Press a shortcut, speak, press it again.
Cantrip transcribes on your CPU and delivers the finished text when the
destination can be verified; explicit clipboard delivery is available elsewhere.

[Website](https://cantrip.mistystep.io) ·
[Download a release](https://github.com/misty-step/cantrip/releases/latest) ·
[Install](docs/INSTALLATION.md) ·
[First dictation](docs/USAGE.md#first-dictation)

## Features

- **Local by default.** CPU-only Parakeet speech recognition through
  [transcribe-rs](https://github.com/cjpais/transcribe-rs). Download the model
  deliberately once; it is not bundled in the executable.
- **Quiet native feedback.** A passive pixel HUD shows microphone activity and
  processing without taking focus or inventing progress. Native Settings and
  Actions handle configuration and selected-recording recovery.
- **Guarded delivery.** Automatic paste/typing requires supported Hyprland/logind
  focus and session history. Other desktops can use clipboard/manual paste.
  An uncertain handoff is not automatically retried.
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

### Build from source

Source development is separate from release installation. With rustup and a C
linker/toolchain installed, use the exact Rust version pinned in
[`rust-toolchain.toml`](rust-toolchain.toml). On Ubuntu, native **build** packages
are installed with:

```sh
sudo apt install build-essential libdbus-1-dev libssl-dev pkg-config
```

From the repository root:

```sh
rustup install
cargo build --release --locked
./target/release/cantrip --version
```

For a first source-built binary at the default location, the following copies
**only the executable** and refuses an existing file or symlink. Inspect an
existing installation and its startup owner instead of overwriting it.

```sh
(
  set -eu
  binary="$HOME/.local/bin/cantrip"
  if [ -e "$binary" ] || [ -L "$binary" ]; then
    printf 'Existing installation at %s; use its maintenance procedure.\n' "$binary" >&2
    exit 1
  fi
  mkdir -p "$HOME/.local/bin"
  install -m755 ./target/release/cantrip "$binary"
)
```

Now use the same [first-dictation guide](docs/USAGE.md#first-dictation) as archive
users. Do not reinitialize existing config, download models implicitly, or start
a second daemon. The optional [shared service setup](docs/DESKTOP.md#user-service-graphical-session)
uses `contrib/cantrip.service` for source builders and does not copy the binary.

For later source-built updates, build first, record the current enabled/running
state, make a private backup of the installed binary, and
[stop its existing owner cleanly](docs/DESKTOP.md#stop-update-and-remove). Keep
that owner stopped during replacement. For a regular binary at the default
location, stage beside it and rename atomically:

```sh
(
  set -eu
  binary="$HOME/.local/bin/cantrip"
  [ -f "$binary" ] && [ ! -L "$binary" ]
  staged="$(mktemp "$HOME/.local/bin/.cantrip.XXXXXX")"
  trap 'rm -f -- "$staged"' EXIT
  install -m755 ./target/release/cantrip "$staged"
  mv -T -- "$staged" "$binary"
)
```

Use the package owner's procedure for package-managed binaries. Leave unchanged
units, personal drop-ins, configuration, models, keyring credentials, and history
alone. Restart only the intended owner and repeat the attended dictation. A
backed-up executable can restore code; it does not reverse history-schema
changes. For published archives, use the installer's
[update and rollback operations](docs/INSTALLATION.md#update-and-roll-back)
instead of this source-build procedure.

## Requirements

See [binary runtime prerequisites](docs/INSTALLATION.md#runtime-prerequisites-and-support)
and [desktop capabilities](docs/DESKTOP.md#supported-desktops). A working
PipeWire session with `pw-record` is needed for capture; clipboard/paste needs
`wl-copy`. Automatic keys and the passive HUD have separate compositor
requirements. `doctor` and an idle daemon are not proof of a successful
microphone-to-editor dictation.

## User service (graphical session)

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

`examples/eval` is a reproducible harness that scores configured STT and cleanup
lanes over a five-clip reference set (WER/CER, latency, cost) and ranks
arrangements. Reproduction procedures, accepted contracts, and clearly separated
historical findings/design proposals: [evaluation guide](docs/EVALUATION.md).

## Development

After installing the pinned toolchain as above:

```sh
cargo build --locked
./scripts/check
```

`scripts/check` is the clone-to-green command: it runs `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`,
the offline `cargo test --locked --example eval` suite, and installer/release
safety tests (Python 3.11+), stopping at the first red gate. The example tests
do not contact providers.

- **Local git hooks** (format + clippy on commit, tests + secret scan on push):
  `.githooks/install.sh`. After installing hooks, `gitleaks` and `trufflehog`
  must be on `PATH`; the hooks fail closed when either scanner is missing.
- **CI** ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)): fmt, clippy
  `-D warnings`, tests, secret scan, and on `master` the Landmark-prepared
  verified Linux release.
- **Website and documentation source:** [`site/`](site/). The five user guides
  below are canonical Markdown, rendered by the site rather than copied into
  a second web manual. Website release data comes from published release assets,
  not an unreleased Cargo version or local technical changelog.
- **Architecture decisions:** [`docs/adr/`](docs/adr/). Operational log tags:
  `[Daemon]` `[Capture]` `[STT]` `[Postproc]` `[Inject]` `[Models]` `[HUD]`.

## Work and documentation ownership

Work starts from the user's current request, checked against live code and
overlapping work. Linear owns current work, prioritization, and selected unresolved
opportunities; neither an old issue nor a document authorizes automatic intake.
Powder is retired. [ADR 0017](docs/adr/0017-powder-board-of-record.md) preserves
the earlier migration and its rejection of duplicate boards, not live routing.
GitHub issue history remains context rather than a second backlog.

The repository owns version-bound product/system contracts, accepted technical
decisions, portable procedures, and curated public/synthetic eval inputs and
baselines. Linear holds safe work summaries and links to proof; raw, large, or
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

- [Install](docs/INSTALLATION.md) — verified releases and binary lifecycle.
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
    and the [signed waveform contract](docs/adr/0021-signed-pixel-waveform.md).
  - [Private history and fixture promotion](docs/adr/0013-local-transcript-history.md)
    and [eval-driven post-processing](docs/adr/0012-eval-driven-postprocessing.md).
