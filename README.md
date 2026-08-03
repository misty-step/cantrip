# Cantrip

Local-first dictation for Linux. Press a key, speak, press it again — your words
appear where your cursor is. A cantrip is a small spell you can always cast; this
is that, for text.

Speech never leaves the machine: transcription runs locally on
[Parakeet TDT 0.6B v3](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx)
(int8 ONNX) via [transcribe-rs](https://github.com/cjpais/transcribe-rs).

## Status

Milestone 1: working headless dictation pipeline (daemon + CLI).

```
trigger ──> capture (pw-record) ──> STT (Parakeet, local) ──> inject ──> notification
             16 kHz mono s16          transcribe-rs/ONNX        wtype | ydotool | clipboard
```

Planned next: GTK4/libadwaita settings + layer-shell HUD, GlobalShortcuts portal,
streaming partials, LLM post-processing (local via OpenAI-compatible endpoint, BYOK cloud).

## Requirements

- Linux with PipeWire (`pw-record`) and Wayland.
- `wl-clipboard` (clipboard fallback — usually preinstalled).
- Optional, for direct typing instead of clipboard: `wtype` (wlroots/COSMIC
  compositors) or `ydotool` (works everywhere, needs the `ydotoold` daemon).

```sh
sudo apt install wtype        # or: sudo apt install ydotool
```

## Quickstart

```sh
cargo build --release

# 1. Fetch the model (~460 MB, one time)
./target/release/cantrip models pull

# 2. Check your environment
./target/release/cantrip doctor

# 3. Run the daemon (keeps the model warm)
./target/release/cantrip daemon --preload

# 4. From another terminal — or a keybinding:
./target/release/cantrip toggle   # start recording
./target/release/cantrip toggle   # stop, transcribe, inject
```

### Bind a key (COSMIC)

Settings → Input Devices → Keyboard → Keyboard Shortcuts → Custom Shortcuts:

- Command: `/path/to/cantrip toggle`
- Shortcut: e.g. `Super+D`

Any compositor works the same way — the daemon listens on a Unix socket, so the
trigger is just a CLI call. `cantrip cancel` discards an in-flight recording.

## CLI

| Command | Purpose |
|---|---|
| `cantrip daemon [--preload]` | Run the dictation daemon (foreground) |
| `cantrip toggle` | Start/stop dictation |
| `cantrip start` / `stop` / `cancel` | Explicit transitions |
| `cantrip status` / `ping` | Daemon state |
| `cantrip transcribe <wav>` | One-shot file transcription (debug) |
| `cantrip models pull` / `status` | Manage the local model |
| `cantrip doctor` | Environment report |

## Configuration

`~/.config/cantrip/config.toml`:

```toml
model = "parakeet-tdt-0.6b-v3-int8"
injection = "auto"        # auto | type | clipboard
keep_warm = true           # keep model loaded in the daemon
# language = "en"          # Parakeet v3 auto-detects when unset
# audio_source = "..."     # PipeWire target (default: system default mic)
```

## Privacy

- Audio and transcripts never leave the machine.
- Recordings live in `$XDG_RUNTIME_DIR` (tmpfs, per-user 0700) and are deleted
  after transcription, success or failure.
- Logs contain character counts, never transcript content.
- Clipboard mode overwrites the clipboard and does not restore the previous
  contents (restoring is racy on Wayland).

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

See `docs/adr/` for architecture decisions. Log lines use bracket tags:
`[Daemon]`, `[Capture]`, `[STT]`, `[Inject]`, `[Models]`.

## Prior art & credits

Design informed by [Handy](https://github.com/cjpais/Handy) (and its
`transcribe-rs` engine, which Cantrip uses directly), and by
[Vox](https://github.com/misty-step/vox), our macOS predecessor whose pipeline
architecture and privacy rules carry over.
