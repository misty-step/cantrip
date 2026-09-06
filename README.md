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
- **Layer-shell status HUD.** A small always-on-top segmented track shows live
  state. During capture, a measured waveform shows the min/max PCM envelope
  from each new 200 ms input window and eases only between real frames.
  Three seconds of near-digital silence flattens the trace and turns it amber
  without canceling the take. Multi-chunk STT shows measured progress;
  single-chunk STT uses an amber sweep. Delivered text flashes green.
  Amber notices display readable text; actionable failures stay visible until
  another operation replaces them. Pure Rust, no GTK.
- **Atomic paste-first injection.** The default pastes the finished text
  from the clipboard (`wl-copy` then one `Ctrl+Shift+V`), so paragraph breaks
  survive and a focus change mid-dictation cannot interrupt delivery.
  `type` mode (never touching the clipboard) still types via `wtype` or
  `ydotool`, and `clipboard` mode only copies.

```
trigger ──> capture (pw-record) ──> STT (local, default) ──> postproc (Ollama) ──> inject
             16 kHz mono s16          parakeet | cloud    qwen3:8b | ...   paste | wtype | clip
```

## Requirements

- Linux with PipeWire (`pw-record`) and a Wayland compositor.
- `wl-clipboard` for the default paste/copy path (usually preinstalled).
- For direct typing: `wtype` (wlroots/COSMIC) or `ydotool` (needs `ydotoold`).
- The HUD needs a compositor with the Wayland layer-shell protocol (COSMIC,
  Sway, Hyprland, wlroots-based).

```sh
sudo apt install wtype        # or: sudo apt install ydotool
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

# 4. In another terminal, dictate once and confirm the Success capsule.
./target/release/cantrip toggle        # start
./target/release/cantrip toggle        # stop, transcribe, and deliver
```

On COSMIC, first test the two `toggle` calls above. Then add one custom keyboard
shortcut whose command is the absolute path to `cantrip toggle`; press it once
to start and once to stop. Compositors that support separate key-down and
key-up commands can bind `cantrip start` and `cantrip stop` instead.

Run `cantrip doctor` again after changing config or installing a prerequisite.
If cleanup is enabled, ensure its configured endpoint is running and its
keyring credential id, when needed, was stored with `cantrip key set`.
`cantrip cancel` discards the active recording without injecting.

## CLI

| Command | Purpose |
|---|---|
| `cantrip daemon [--preload]` | Run the dictation daemon |
| `cantrip hud [--screenshot PATH]` | Run the layer-shell status HUD (or dump one frame to a PNG and exit). The daemon spawns and watches it; run manually only to override |
| `cantrip settings [--screenshot PATH]` | Open the configuration window (view, edit, reload; or dump a frame) |
| `cantrip toggle` / `start` / `stop` / `cancel` | Dictation transitions |
| `cantrip status` / `ping` | Daemon state |
| `cantrip transcribe [--local] <wav>` | One-shot file transcription; transcript-only stdout, diagnostics on stderr |
| `cantrip models pull` / `status` | Manage local STT models |
| `cantrip config show` / `edit` / `init` / `path` | Inspect and edit configuration |
| `cantrip key set` / `rm` / `status <id>` | Store and manage keyring credential ids |
| `cantrip doctor` | Environment report |
| `cantrip last` | Re-deliver the last saved transcript (which may belong to an older dictation) |
| `cantrip recover [--local] [--clipboard]` | Retry retained failed/partial audio; optionally use local STT or copy without sending keys |
| `cantrip reload` | Re-read configuration in the running daemon |

Two hotkeys, one with cleanup and one without: `toggle` and `start` take
`--postproc clean|raw` to force transcript cleanup on or off for that
dictation, overriding `[postproc].enabled`. Bind one key to
`cantrip toggle --postproc clean` and the other to
`cantrip toggle --postproc raw`; each key starts and stops its own dictation
mode (cleanup runs only when the capture was started with `clean`). Without
the flag, `[postproc].enabled` decides.

## Recovery

Run `cantrip status` to see the last result and any retained audio, even after
restarting the daemon. Retry with the configured provider but avoid an
unexpected paste into the focused window:

```sh
cantrip recover --clipboard
```

For local recovery without changing your usual configuration:

```sh
cantrip models pull                 # once, if Parakeet is not installed
cantrip recover --local --clipboard
```

Paste with Ctrl+V in a GUI or Ctrl+Shift+V in a terminal. `--local` uses installed
Parakeet and disables cleanup for that operation; it does not change separately
opted-in count-only telemetry. A partial result is visibly marked incomplete
and leaves the audio available. Retrying processes the whole recording again.

For an audio file outside the recovery slot:

```sh
(umask 077; cantrip transcribe --local recording.wav > recovered.txt)
```

The CLI exits unsuccessfully if only a partial transcript was produced, while
still printing the available text. Keep the original file until satisfied.

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
  `0700`) and are deleted after processing. If STT fails or produces only
  partial text, Cantrip may retain one owner-only copy at
  `~/.local/state/cantrip/last-failed.wav`. An unrelated successful dictation
  does not erase it. A new failed/partial take replaces it; complete nonempty
  recovery consumes it only after recovered text has been saved.
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
`cargo clippy --all-targets -- -D warnings`, and `cargo test`, stopping at the
first red gate.

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
