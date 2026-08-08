# Repository Guidelines

Cantrip is a local-first Linux dictation app: one Rust crate, binary `cantrip`.

Product north star: [`VISION.md`](VISION.md). Read it before inventing scope.

## Layout

- `src/main.rs` — clap CLI (daemon + client subcommands)
- `src/daemon.rs` — state machine (Idle/Recording/Processing), socket server, worker thread
- `src/ipc.rs` — Unix-socket line protocol (`Command`, `Reply`)
- `src/capture.rs` — `pw-record` child-process recorder
- `src/stt.rs` — Parakeet via transcribe-rs
- `src/models.rs` — model download/verify (`~/.local/share/cantrip/models`)
- `src/inject.rs` — wtype → ydotool → wl-copy chain
- `src/config.rs` / `src/paths.rs` — TOML config, XDG paths
- `src/postproc.rs` — OpenAI-compatible transcript cleanup
- `src/keys.rs` — OS keyring API key access
- `src/pipeline.rs` — shared STT + postproc job pipeline (daemon + `transcribe`)
- `src/hud.rs` — layer-shell status HUD (`cantrip hud`)
- `docs/adr/` — architecture decisions; add an ADR before non-obvious changes

## Commands

- Toolchain pinned in `rust-toolchain.toml` (channel `stable`, rustfmt + clippy
  components); install: `rustup toolchain install stable --profile minimal
  --component rustfmt --component clippy`.
- `cargo build` / `cargo test`
- `cargo clippy --all-targets -- -D warnings` (CI-enforced)
- `cargo fmt --check` (CI-enforced)
- Smoke test: `cargo run -- transcribe samples/jfk.wav`

- Never log transcript content — character counts only. `cantrip transcribe`
  stdout is the single exemption. Postproc/cloud HTTP errors log status codes
  only, never bodies.
- Log tags: `[Daemon]` `[Capture]` `[STT]` `[Postproc]` `[Inject]` `[Models]` `[HUD]`.
- Stop `pw-record` with SIGINT and wait; SIGKILL corrupts the WAV.
- Type-mode injection must never touch the clipboard.
- In-flight recordings live under `$XDG_RUNTIME_DIR/cantrip` and are deleted
  after processing. On a full STT failure the daemon may keep one copy at
  `~/.local/state/cantrip/last-failed.wav` for `cantrip recover`.
- anyhow errors with `.context()`; no `unwrap()` outside tests.
- No async runtime; std threads + mpsc.
- Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`); branch `master`.
- Never commit secrets; API keys live in the OS keyring via `cantrip key`, not files.

Organization root context: @~/Development/misty-step/AGENTS.md
