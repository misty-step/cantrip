# Cantrip

Cantrip is a local-first Linux dictation app: one Rust crate and the `cantrip` binary. Read [`VISION.md`](VISION.md) before changing product behavior; add an ADR in `docs/adr/` for a non-obvious architectural decision.

## Code map

- `src/main.rs` — clap CLI (daemon and client subcommands)
- `src/daemon.rs` — Idle/Recording/Processing state machine, socket server, worker
- `src/ipc.rs` — Unix-socket `Command`/`Reply` protocol
- `src/capture.rs` — `pw-record` child process
- `src/stt.rs` — Parakeet via transcribe-rs
- `src/models.rs` — model download and verification (`~/.local/share/cantrip/models`)
- `src/inject.rs` — `wtype` → `ydotool` → `wl-copy` delivery
- `src/config.rs`, `src/paths.rs` — TOML config and XDG paths
- `src/postproc.rs` — OpenAI-compatible transcript cleanup
- `src/keys.rs` — OS keyring API-key access
- `src/pipeline.rs` — shared STT/postproc pipeline for the daemon and `transcribe`
- `src/hud.rs` — layer-shell status HUD (`cantrip hud`)
- `src/telemetry.rs` — opt-in Langfuse OTLP export

## Commands

The toolchain is pinned to stable in `rust-toolchain.toml` with rustfmt and clippy components.

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run -- transcribe samples/jfk.wav
./scripts/check
```

`scripts/check` is the CI-equivalent sequence: fmt check, clippy with `-D warnings`, then tests.

## Contracts

- Local STT is the default. Cloud STT and cleanup are opt-in OpenAI-compatible HTTP lanes.
- Never put transcript text or audio in operational logs or telemetry. Log character counts, durations, model/backend names, and error classes only; `cantrip transcribe` stdout is the sole transcript-output exception. Keep the existing tags: `[Daemon]`, `[Capture]`, `[STT]`, `[Postproc]`, `[Inject]`, `[Models]`, `[HUD]`, `[Telemetry]`.
- API keys belong in the OS keyring via `cantrip key`, never in files, logs, or git.
- Stop `pw-record` with SIGINT and wait; SIGKILL can corrupt the WAV.
- Type-mode injection never touches the clipboard. Paste-first delivery may use `wl-copy`; clipboard mode does not restore prior contents.
- In-flight recordings live under `$XDG_RUNTIME_DIR/cantrip` and are removed after processing. A complete STT failure may retain one owner-only `~/.local/state/cantrip/last-failed.wav` for `cantrip recover`.
- Successful transcripts are owner-only local history under `$XDG_STATE_HOME/cantrip/transcripts` and are never uploaded automatically.
- Keep the std-thread + mpsc process model; do not add an async runtime or a second durable work ledger.
- Use `anyhow` context for fallible operations and reserve `unwrap()` for tests.
- Use Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`) on the `master` branch.

## Work tracking and secrets

The Powder ledger for `misty-step/cantrip` is the work board of record. GitHub Issues are a read-only archive of pre-2026-08-21 decisions. Never commit secrets.
