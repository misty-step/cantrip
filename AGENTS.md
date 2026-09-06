# Cantrip

Cantrip is a local-first Linux dictation app: one Rust crate and the `cantrip` binary. Read [`VISION.md`](VISION.md) before changing product behavior; add an ADR in `docs/adr/` for a non-obvious architectural decision.

## Code map

- `src/main.rs` — clap CLI (daemon and client subcommands)
- `src/daemon.rs` — Idle/Recording/Processing state machine, socket server, worker
- `src/ipc.rs` — Unix-socket `Command`/`Reply` protocol
- `src/capture.rs` — `pw-record` child process
- `src/stt.rs` — Parakeet via transcribe-rs
- `src/models.rs` — model download and verification (`~/.local/share/cantrip/models`)
- `src/inject.rs`, `src/desktop.rs` — bounded native Wayland delivery and verified focus/session permits
- `src/config.rs`, `src/paths.rs` — TOML config and XDG paths
- `src/postproc.rs` — OpenAI-compatible transcript cleanup
- `src/keys.rs` — OS keyring API-key access
- `src/pipeline.rs` — shared STT/postproc pipeline for the daemon and `transcribe`
- `src/hud.rs` — layer-shell status HUD (`cantrip hud`)
- `src/actions.rs` — explicit recording recovery and setup window
- `src/archive.rs`, `src/recovery.rs` — owner-private per-take history and retained audio
- `src/theme.rs` — shared desktop palette
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
- In-flight recordings live under `$XDG_RUNTIME_DIR/cantrip`. Stopped takes are durably retained under their own IDs in the transcript history before STT. Failed, partial, cancelled, or undelivered takes remain independent; complete durable text plus successful delivery permits audio removal. Storage failures preserve runtime audio where possible and must be surfaced.
- Successful transcripts are owner-only local history under `$XDG_STATE_HOME/cantrip/transcripts` and are never uploaded automatically.
- Dismissal acknowledges feedback, never deletes artifacts. Forget requires explicit confirmation and removes only the selected take's retained audio and incomplete text; complete archived text stays.
- Automatic delivery requires uninterrupted verified destination/session history. Unknown focus, lock, suspend, or reconnection defers; potentially completed handoffs are uncertain and never retried through another backend.
- Keep the std-thread + mpsc process model; do not add an async runtime or a second durable work ledger.
- Use `anyhow` context for fallible operations and reserve `unwrap()` for tests.
- Use Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`) on the `master` branch.

## Work tracking and secrets

Work from the operator's current request. Check current code and overlapping work before starting; report the result and verification evidence in the session or pull request. Historical issues are context, not a queue. Never commit secrets.
