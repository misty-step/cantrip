# Repository Guidelines

Cantrip is a local-first Linux dictation app: one Rust crate, binary `cantrip`.

## Layout

- `src/main.rs` — clap CLI (daemon + client subcommands)
- `src/daemon.rs` — state machine (Idle/Recording/Processing), socket server, worker thread
- `src/ipc.rs` — Unix-socket line protocol (`Command`, `Reply`)
- `src/capture.rs` — `pw-record` child-process recorder
- `src/stt.rs` — Parakeet via transcribe-rs
- `src/models.rs` — model download/verify (`~/.local/share/cantrip/models`)
- `src/inject.rs` — wtype → ydotool → wl-copy chain
- `src/config.rs` / `src/paths.rs` — TOML config, XDG paths
- `docs/adr/` — architecture decisions; add an ADR before non-obvious changes

## Commands

- `cargo build` / `cargo test`
- `cargo clippy --all-targets -- -D warnings` (CI-enforced)
- `cargo fmt --check` (CI-enforced)
- Smoke test: `cargo run -- transcribe samples/jfk.wav`

## Rules

- Never log transcript content — character counts only. `cantrip transcribe`
  stdout is the single exemption.
- Log tags: `[Daemon]` `[Capture]` `[STT]` `[Inject]` `[Models]`.
- Stop `pw-record` with SIGINT and wait; SIGKILL corrupts the WAV.
- Type-mode injection must never touch the clipboard.
- Recordings live only under `$XDG_RUNTIME_DIR/cantrip` and are deleted after
  processing, success or failure.
- anyhow errors with `.context()`; no `unwrap()` outside tests.
- No async runtime; std threads + mpsc.
- Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`); branch `master`.
- Never commit secrets; BYOK keys (future) go through the OS keyring, not files.
