# Cantrip

Cantrip is a local-first Linux/macOS dictation app: the `cantrip` native host
and `cantrip-engine` shared workflow crate in one Rust workspace, with
`workspace.package.version` as the version authority. Start with
[`README.md`](README.md) and the contract relevant to the request. `VISION.md`
is optional context, not a required first read or product lock. Preserve
non-obvious architectural decisions in `docs/adr/`; the platform boundary is
[ADR 0031](docs/adr/0031-shared-engine-native-macos.md).


## Contracts

- Local STT is the default. Cloud STT and cleanup are opt-in OpenAI-compatible HTTP lanes.
- Configured-cloud STT errors, partial text, or empty recognition of nonempty audio get one automatic whole-take attempt with installed default local Parakeet. Never download a model or switch cloud providers implicitly; do not concatenate alternate passes. Archive the selected backend honestly.
- Never put transcript text or audio in operational logs or telemetry. Log character counts, durations, model/backend names, and error classes only; `cantrip transcribe` stdout is the sole transcript-output exception. Keep the existing tags: `[Daemon]`, `[Capture]`, `[STT]`, `[Postproc]`, `[Inject]`, `[Models]`, `[HUD]`, `[Telemetry]`.
- API keys belong in the OS keyring via `cantrip key`, never in files, logs, or git.
- Linux capture stops `pw-record` with SIGINT and waits; SIGKILL can corrupt the WAV. macOS capture owns AVAudioEngine on its native thread. Startup failure must quiesce producers and finalize any accepted WAV prefix before returning; remove only conclusively empty, unaccepted headers.
- Linux Type-mode injection never touches the clipboard; paste-first may use `wl-copy`. macOS supports explicit clipboard/manual paste only: existing Auto/Paste/Type must defer without clipboard or keyboard effects, never silently downgrade or rewrite the choice. Clipboard mode on either OS does not read/restore prior contents.
- In-flight recordings live in the platform runtime directory (`$XDG_RUNTIME_DIR/cantrip` on Linux; private temporary runtime on macOS). Stopped takes are durably retained under their own IDs in transcript history before STT, including cancellation, startup failure with surviving audio, and graceful shutdown. Successful, failed, partial, empty, cancelled, and undelivered takes remain independent; only confirmed Forget deletes retained audio. Import trusted finalized runtime leftovers on startup, consuming originals only after matching durable audio is confirmed. Surface storage failures and preserve runtime originals when durability is uncertain; runtime-only audio is not guaranteed to survive reboot.
- Successful transcripts are owner-only local plaintext history (`$XDG_STATE_HOME/cantrip/transcripts` on Linux; `~/Library/Application Support/cantrip/state/transcripts` on macOS) and are never uploaded automatically. macOS defaults use Application Support and login Keychain; absolute XDG overrides provide isolation. Private permissions/ACL stripping are not encryption.
- Dismissal acknowledges feedback, never deletes artifacts. Forget requires explicit confirmation and removes only the selected take's retained audio and incomplete text; complete archived text stays.
- Automatic delivery requires uninterrupted verified destination/session history. Unknown focus, lock, suspend, or reconnection defers; potentially completed handoffs are uncertain and never retried through another backend.
- The engine owns workflow, cancellation, config/IPC, and the durable archive/recovery store. The native host supplies `Platform`, `Recorder`, and `DeliveryPermit` mechanisms, not alternate policy or a per-OS workflow. Linux keeps its supervised Wayland HUD; the macOS app owns AppKit on the main thread and an engine std thread, or attaches without owning an existing daemon. macOS `daemon` is engine-only; app quit must not kill an externally owned daemon.
- macOS permission setup is explicit: microphone access only; no Accessibility, Input Monitoring, or Screen Recording grants for supported capture/HUD/Carbon shortcut behavior. Open At Login is an explicit SMAppService choice, never enabled automatically.
- Keep the std-thread + mpsc process model; do not add an async runtime or a second durable work ledger.

## Proof

`./scripts/check` is the canonical Linux CI-equivalent local gate and owns its
contents. Native macOS workspace/runtime gates and relocated app verification
are defined in `.github/workflows/macos.yml` and documented in
`docs/INSTALLATION.md`; offscreen NSView pixels are not live panel/focus proof.
Choose a smaller command only for a named, changed surface.

## Work tracking and secrets

Work from the operator's current request. Check current code and overlapping
work before starting. Link the result and sanitized verification evidence from
the existing work record when available and from the PR/session.
Historical issues are context, not an intake queue. Never commit secrets.
