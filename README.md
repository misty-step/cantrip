# Cantrip

Local-first dictation for Linux. Press a key, speak, let go — your words appear
where your cursor is. A cantrip is a small spell you can always cast; this is
that, for text.

## Features

- **Local transcription by default.** Speech never leaves the machine: an
  int8 ONNX [Parakeet TDT 0.6B v3](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx)
  model via [transcribe-rs](https://github.com/cjpais/transcribe-rs) runs on
  CPU in ~250 ms for short dictations.
- **Local post-processing.** The raw transcript passes through a small
  language model (default `qwen3:8b` on local Ollama) that removes spoken
  disfluencies — "um", "uh", false starts, repeated words — and adds
  punctuation and capitalization, then the corrected text is typed at your
  cursor.
- **Optional cloud lanes.** Any OpenAI-compatible STT or chat endpoint can
  replace the local models (see `docs/CONFIGURATION.md`). API keys live in
  the OS keyring, never in files.
- **Layer-shell status HUD.** A small always-on-top capsule shows live state:
  listening (with a breathing pulse and timer), transcribing, cleaning up,
  and a gentle success check. Pure Rust, no GTK.
- **Injection without touching the clipboard.** Type mode uses `wtype`
  (wlroots/COSMIC) or `ydotool`; clipboard mode is a documented fallback.

```
trigger ──> capture (pw-record) ──> STT (local, default) ──> postproc (Ollama) ──> inject ──> notification
             16 kHz mono s16          parakeet | canary | cloud    qwen3:8b | ...      wtype | clip
```

## Requirements

- Linux with PipeWire (`pw-record`) and a Wayland compositor.
- `wl-clipboard` for the clipboard fallback (usually preinstalled).
- For direct typing: `wtype` (wlroots/COSMIC) or `ydotool` (needs `ydotoold`).
- The HUD needs a compositor with the Wayland layer-shell protocol (COSMIC,
  Sway, Hyprland, wlroots-based).

```sh
sudo apt install wtype        # or: sudo apt install ydotool
```

## Quickstart

```sh
cargo build --release

# 1. First-time model download (~460 MB) and environment check
./target/release/cantrip models pull
./target/release/cantrip doctor

# 2. Initialize config at ~/.config/cantrip/config.toml
./target/release/cantrip config init

# 3. Bring up the daemon (it keeps the STT model warm and runs the HUD itself)
./target/release/cantrip daemon &

# 4. Dictate — bind this to a key in your compositor
./target/release/cantrip toggle       # press once to start, once to stop
```

If the postprocessor is enabled (set `model = "qwen3:8b"` and ensure Ollama is
running: `ollama pull qwen3:8b`), the corrected text lands at your cursor.
`cantrip cancel` discards a recording without injecting.

## CLI

| Command | Purpose |
|---|---|
| `cantrip daemon [--preload]` | Run the dictation daemon |
| `cantrip hud [--screenshot PATH]` | Run the layer-shell status HUD (or dump one frame to a PNG and exit). The daemon spawns and watches it; run manually only to override |
| `cantrip settings [--screenshot PATH]` | Open the configuration window (view, edit, reload; or dump a frame) |
| `cantrip toggle` / `start` / `stop` / `cancel` | Dictation transitions |
| `cantrip status` / `ping` | Daemon state |
| `cantrip transcribe <wav>` | One-shot file transcription (debug; prints to stdout) |
| `cantrip models pull` / `status` | Manage local STT models |
| `cantrip config show` / `edit` / `init` / `path` | Inspect and edit configuration |
| `cantrip key set` / `rm` / `status <id>` | Store and manage keyring credential ids |
| `cantrip doctor` | Environment report |

Two hotkeys, one with cleanup and one without: `toggle` and `start` take
`--postproc clean|raw` to force transcript cleanup on or off for that
dictation, overriding `[postproc].enabled`. Bind one key to
`cantrip toggle --postproc clean` and the other to
`cantrip toggle --postproc raw`; each key starts and stops its own dictation
mode (cleanup runs only when the capture was started with `clean`). Without
the flag, `[postproc].enabled` decides.

## Configuration

Everything lives in `~/.config/cantrip/config.toml` (or `cantrip config path`);
`cantrip config show` prints the active file, `config edit` opens it, and
`cantrip settings` opens a window you can keep open to view and adjust it
(Save reloads the daemon). The gauntlet-informed recommended setup and every
knob (STT model, cloud STT, postproc model + instructions, cloud postproc) are
documented in [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md).

## Privacy

- Audio and transcripts never leave the machine in the default local lanes.
- Recordings live in `$XDG_RUNTIME_DIR/cantrip` (tmpfs, per-user 0700) and are
  deleted after processing, success or failure.
- Logs contain character counts only — never transcript content. The single
  exemption is `cantrip transcribe` stdout.
- Clipboard mode overwrites the clipboard and does not restore the previous
  contents (restoring is racy on Wayland).

## Evaluation gauntlet

`examples/eval` is a reproducible harness that scores any configured STT and
post-proc lane over a 5-clip reference set (WER/CER, latency, cost) and ranks
arrangements. Findings and reproduction steps: [`docs/EVALUATION.md`](docs/EVALUATION.md).

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

- **Local git hooks** (format + clippy on commit, tests + secret scan on push):
  `.githooks/install.sh`. Requires `gitleaks` (staged scan) and `trufflehog`
  (working-tree scan) on `PATH`.
- **CI** (`.github/workflows/ci.yml`): fmt, clippy `-D warnings`, tests, and a
  TruffleHog + Gitleaks secret scan on every push/PR.
- Architecture decisions: [`docs/adr/`](docs/adr/). Log tags: `[Daemon]`
  `[Capture]` `[STT]` `[Postproc]` `[Inject]` `[Models]` `[HUD]`.

## Prior art & credits

Design informed by [Handy](https://github.com/cjpais/Handy) (and its
`transcribe-rs` engine, which Cantrip uses directly) and by
[Vox](https://github.com/misty-step/vox), our macOS predecessor whose pipeline
architecture and privacy rules carry over.
