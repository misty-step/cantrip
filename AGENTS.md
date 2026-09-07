# Cantrip

Cantrip is a local-first Linux dictation app: one Rust crate and the `cantrip`
binary. Start with [`README.md`](README.md) and the contract relevant to the
request. `VISION.md` is optional context, not a required first read or product
lock. Preserve non-obvious architectural decisions in `docs/adr/`.


## Contracts

- Local STT is the default. Cloud STT and cleanup are opt-in OpenAI-compatible HTTP lanes.
- Configured-cloud STT errors, partial text, or empty recognition of nonempty audio get one automatic whole-take attempt with installed default local Parakeet. Never download a model or switch cloud providers implicitly; do not concatenate alternate passes. Archive the selected backend honestly.
- Never put transcript text or audio in operational logs or telemetry. Log character counts, durations, model/backend names, and error classes only; `cantrip transcribe` stdout is the sole transcript-output exception. Keep the existing tags: `[Daemon]`, `[Capture]`, `[STT]`, `[Postproc]`, `[Inject]`, `[Models]`, `[HUD]`, `[Telemetry]`.
- API keys belong in the OS keyring via `cantrip key`, never in files, logs, or git.
- Stop `pw-record` with SIGINT and wait; SIGKILL can corrupt the WAV.
- Type-mode injection never touches the clipboard. Paste-first delivery may use `wl-copy`; clipboard mode does not restore prior contents.
- In-flight recordings live under `$XDG_RUNTIME_DIR/cantrip`. Stopped takes are durably retained under their own IDs in transcript history before STT, including cancellation and graceful shutdown. Successful, failed, partial, empty, cancelled, and undelivered takes remain independent; only confirmed Forget deletes retained audio. Import trusted finalized runtime leftovers on startup, consuming originals only after matching durable audio is confirmed. Surface storage failures and preserve runtime originals when durability is uncertain; live/runtime-only audio cannot survive reboot.
- Successful transcripts are owner-only local history under `$XDG_STATE_HOME/cantrip/transcripts` and are never uploaded automatically.
- Dismissal acknowledges feedback, never deletes artifacts. Forget requires explicit confirmation and removes only the selected take's retained audio and incomplete text; complete archived text stays.
- Automatic delivery requires uninterrupted verified destination/session history. Unknown focus, lock, suspend, or reconnection defers; potentially completed handoffs are uncertain and never retried through another backend.
- Keep the std-thread + mpsc process model; do not add an async runtime or a second durable work ledger.

## Proof

`./scripts/check` is the canonical CI-equivalent local gate and owns its
contents. Choose a smaller command only for a named, changed surface.

## Work tracking and secrets

Work from the operator's current request. Check current code and overlapping
work before starting. Linear owns selected work and priorities; link the result
and sanitized verification evidence from the work record and PR/session.
Historical issues are context, not an intake queue. Never commit secrets.
