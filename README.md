# Cantrip

Local-first dictation for Linux. Press a key, speak, let go — your words appear
where your cursor is. A cantrip is a small spell you can always cast; this is
that, for text.

## Features

- **Local transcription by default.** Speech never leaves the machine: an
  int8 ONNX [Parakeet TDT 0.6B v3](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx)
  model via [transcribe-rs](https://github.com/cjpais/transcribe-rs) runs on
  CPU in ~250 ms for short dictations.
- **Optional transcript cleanup.** Cleanup is disabled by default. When enabled,
  the raw transcript can pass through a small language model—such as
  `qwen3:8b` on local Ollama—to remove spoken disfluencies and add punctuation
  and capitalization before delivery.
- **Optional cloud lanes.** Any OpenAI-compatible STT or chat endpoint can
  replace the local models (see `docs/CONFIGURATION.md`). API keys live in
  the OS keyring, never in files.
- **Long dictation without oversized uploads.** Local and cloud STT split long
  recordings into bounded chunks and deliver one finished transcript. Partial
  failures preserve available text and the original audio for recovery.
- **Passive status HUD.** A bottom-anchored, input-transparent track shows measured
  microphone activity and real multi-chunk progress. It never takes focus or
  invents progress. Silence, partial text, deferred delivery, storage failures,
  and lost connections have distinct notices. Labels and reduced motion are
  configurable; HUD and native windows follow the desktop palette.
- **Guarded paste-first delivery.** Finished text is normally copied with
  `wl-copy`, then pasted with one native Wayland `Ctrl+Shift+V` chord.
  The destination and uninterrupted session history must remain verifiable.
  Changed focus, locks, suspend, or ambiguous desktop state defer delivery
  instead of sending text elsewhere. Strict `type` mode never touches the
  clipboard; `clipboard` mode only copies. No `wtype`/`ydotool` subprocesses.
- **Per-recording recovery.** `cantrip actions` opens a deliberate native window
  for selected-recording copy, local or configured-provider recovery, confirmed
  deletion, cancellation, and setup. Metadata never exposes transcript text.
  A new failure or unrelated success cannot replace another pending recording.

```
trigger ──> capture (pw-record) ──> STT (local, default) ──> postproc (Ollama) ──> inject
             16 kHz mono s16          parakeet | cloud    qwen3:8b | ...   paste | type | clip
```

## Requirements

- Linux with PipeWire (`pw-record`) and a Wayland compositor.
- `wl-clipboard` for paste/copy delivery (usually preinstalled).
- Native keyboard delivery currently requires a direct Hyprland desktop with
  the Lua focus/layer APIs (verified on 0.56.2), the virtual-keyboard protocol,
  and a verifiable active logind session. Authenticated UWSM-managed desktops
  are supported; nested or unverified desktops fail closed.
  On other compositors, choose `injection = "clipboard"` or explicitly copy
  from `cantrip actions`; automatic delivery does not guess.
- The HUD needs a compositor with the Wayland layer-shell protocol (COSMIC,
  Sway, Hyprland, wlroots-based).

```sh
sudo apt install libdbus-1-dev pkg-config wl-clipboard  # Debian/Ubuntu build + copy prerequisites
```

## Quickstart

```sh
cargo build --release

# 1. Create the annotated default config.
./target/release/cantrip config init

# 2. Inspect the effective capture, STT, cleanup, injection, HUD, and daemon
# paths. Follow each reported action; the local default will request the model.
./target/release/cantrip doctor
./target/release/cantrip models pull   # when doctor requests it

# 3. Run the daemon in a dedicated terminal for this first session.
./target/release/cantrip daemon

# 4. In another terminal, dictate once and inspect the delivery outcome.
./target/release/cantrip toggle        # start
./target/release/cantrip toggle        # stop, transcribe, and deliver
```

Bind one custom shortcut to the absolute path of `cantrip toggle`; press it once
to start and once to stop. Compositors that support separate key-down and
key-up commands can bind `cantrip start` and `cantrip stop` instead. Unsupported
desktop safety checks require clipboard mode or explicit recovery, as above.

Run `cantrip doctor` again after changing config or installing a prerequisite.
If cleanup is enabled, ensure its configured endpoint is running and its
keyring credential id, when needed, was stored with `cantrip key set`.
`cantrip cancel` discards an active capture without injecting. During processing,
cancellation prevents later chunks and delivery, and retains recoverable audio;
an already-running provider request may need to return before the worker settles.

## CLI

| Command | Purpose |
|---|---|
| `cantrip daemon [--preload]` | Run the dictation daemon |
| `cantrip hud [--screenshot PATH]` | Run the layer-shell status HUD (or dump one frame to a PNG and exit). The daemon spawns and watches it; run manually only to override |
| `cantrip settings [--screenshot PATH]` | Open the configuration window (view, edit, reload; or dump a frame) |
| `cantrip actions [--doctor] [--screenshot PATH]` | Open the recording recovery and setup window |
| `cantrip toggle` / `start` / `stop` / `cancel` | Dictation transitions |
| `cantrip status [--json]` / `ping` | Current state, measured progress, outcome, and capabilities / daemon liveness |
| `cantrip transcribe [--local] <wav>` | One-shot file transcription; transcript-only stdout, diagnostics on stderr |
| `cantrip models pull` / `status` | Manage local STT models |
| `cantrip config show` / `edit` / `init` / `path` | Inspect and edit configuration |
| `cantrip key set` / `rm` / `status <id>` | Store and manage keyring credential ids |
| `cantrip doctor` | Environment report |
| `cantrip recordings [--json]` | List canonical recording IDs, durations, and available artifacts; no transcript text |
| `cantrip copy <id>` | Copy this exact recording's saved transcript; never send keys |
| `cantrip last` | Re-deliver the latest saved transcript, selected once when accepted |
| `cantrip recover [--id ID] [--local] [--clipboard]` | Retry this recording; omitted ID selects the newest retained audio |
| `cantrip dismiss [--event-id ID]` | Acknowledge a notice without deleting recordings |
| `cantrip forget <id> --confirm` | Delete retained audio and incomplete text; keep complete archived text |
| `cantrip reload` | Re-read configuration in the running daemon |

Two hotkeys, one with cleanup and one without: `toggle` and `start` take
`--postproc clean|raw` to force transcript cleanup on or off for that
dictation, overriding `[postproc].enabled`. Bind one key to
`cantrip toggle --postproc clean` and the other to
`cantrip toggle --postproc raw`; each key starts and stops its own dictation
mode (cleanup runs only when the capture was started with `clean`). Without
the flag, `[postproc].enabled` decides.

## Recovery

Open `cantrip actions` to select a recording by its capture time and ID. Copy,
recover, and confirmed Forget always target that selection, even if newer takes
arrive. Keyboard navigation supports arrows, Page Up/Down, and Home/End.
Escape closes the window or confirmation without deleting a recording.

For the CLI, list metadata and choose an exact ID:

```sh
cantrip recordings
cantrip recover --id RECORDING_ID --clipboard
cantrip copy RECORDING_ID           # copy already-saved text without re-transcribing
```

For local recovery without changing your usual configuration:

```sh
cantrip models pull                 # once, if Parakeet is not installed
cantrip recover --id RECORDING_ID --local --clipboard
```

Paste with Ctrl+V in a GUI or Ctrl+Shift+V in a terminal. `--local` uses installed
Parakeet and disables cleanup for that operation; it does not change separately
opted-in count-only telemetry. A partial result is visibly marked incomplete
and leaves the audio available. Retrying processes the whole recording again.

Dismissal only acknowledges feedback. Forget requires confirmation and removes
that take's audio and incomplete text, retaining any complete archived transcript.
Partial or uncertain delivery is not a successful retry: inspect the destination
before trying again to avoid duplicates.

For an audio file outside the managed history:

```sh
(umask 077; cantrip transcribe --local recording.wav > recovered.txt)
```

The CLI exits unsuccessfully if only a partial transcript was produced, while
still printing the available text. Keep the original file until satisfied.

## Omarchy integration

The repository includes a status badge and menu route. Review the dry run first:

```sh
python3 integrations/omarchy/install.py
python3 integrations/omarchy/install.py --apply
omarchy menu summon cantrip
```

If replacing an existing personal badge, add `--replace-widget OLD_PLUGIN_ID`
to both installer commands. Left-click keeps raw dictation; right-click opens
`cantrip actions`. Existing hotkeys are not redefined. Status failures show
unknown state, not Ready; the badge retains only the last confirmed pending count.

The installer preserves unrelated shell/menu content, stages complete plugin
updates, and reports private rollback backups. To roll back, disable
`cantrip.dictation`, restore the previous bar widget, and remove only the managed
Cantrip menu block. Move the plugin directory outside `omarchy/plugins` rather
than deleting personal extras. Never restore whole backups over subsequent edits.

## Configuration

Everything lives in `~/.config/cantrip/config.toml` (or `cantrip config path`);
`cantrip config show` prints the active file, `config edit` opens it, and
`cantrip settings` opens a window you can keep open to view and adjust it
(Save reloads the daemon). The gauntlet-informed recommended setup and every
knob (STT model, cloud STT, postproc model + instructions, cloud postproc) are
documented in [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md).

## Privacy

- Audio and transcripts never leave the machine in the default local lanes.
- In-flight recordings live in `$XDG_RUNTIME_DIR/cantrip` (tmpfs, per-user
  `0700`). Before transcription, each stopped take is retained under its own ID
  in `$XDG_STATE_HOME/cantrip/transcripts` as owner-only recovery audio.
  Failed, partial, cancelled, empty meaningful, or undelivered takes remain
  available independently. Audio is removed after a complete result is durably
  saved and delivered, or after explicit confirmed Forget.
  Storage failures are surfaced; runtime-only audio must be retrieved before reboot.
  Legacy `last-failed.wav` and `last-transcript.txt` migrate independently and
  idempotently, with originals consumed only after durable publication.
- Every successful STT result is saved locally as an owner-only JSON record in
  `$XDG_STATE_HOME/cantrip/transcripts` (normally
  `~/.local/state/cantrip/transcripts`). This history contains sensitive text;
  it is never uploaded or committed automatically.
- Operational logs contain character counts only—never transcript content.
  The single stdout exemption is `cantrip transcribe`.
- Clipboard mode overwrites the clipboard and does not restore the previous
  contents (restoring is racy on Wayland).

## Evaluation gauntlet

`examples/eval` is a reproducible harness that scores any configured STT and
post-proc lane over a 5-clip reference set (WER/CER, latency, cost) and ranks
arrangements. Findings and reproduction steps: [`docs/EVALUATION.md`](docs/EVALUATION.md).

## Development

The Rust toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml)
(channel `stable`). Install it with:

```sh
rustup toolchain install stable --profile minimal --component rustfmt --component clippy
```

```sh
cargo build
./scripts/check
```

`scripts/check` is the clone-to-green command: it runs `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`, and the Omarchy
installer safety tests, stopping at the first red gate.

- **Local git hooks** (format + clippy on commit, tests + secret scan on push):
  `.githooks/install.sh`. After installing hooks, `gitleaks` and `trufflehog`
  must be on `PATH`; the pre-commit and pre-push hooks fail closed when either
  scanner is missing instead of skipping the scan.
- **CI** (`.github/workflows/ci.yml`): fmt, clippy `-D warnings`, tests, and a
  TruffleHog + Gitleaks secret scan on every push/PR.
- Architecture decisions: [`docs/adr/`](docs/adr/). Log tags: `[Daemon]`
  `[Capture]` `[STT]` `[Postproc]` `[Inject]` `[Models]` `[HUD]`.

## Prior art & credits

Design informed by [Handy](https://github.com/cjpais/Handy) (and its
`transcribe-rs` engine, which Cantrip uses directly) and by
[Vox](https://github.com/misty-step/vox), our macOS predecessor whose pipeline
architecture and privacy rules carry over.

## License

[MIT](LICENSE) © Misty Step.

## Docs

- [Configuration](docs/CONFIGURATION.md)
- [Evaluation gauntlet](docs/EVALUATION.md)
- [Architecture decisions](docs/adr/)
- Marketing/docs site scaffold: [`site/`](site/)
