# Cantrip

Dictation and meeting notes for **macOS and Linux**. Press one shortcut and
speak; the text lands in the app you were using. Or start a meeting, and get a
Markdown note with a summary, decisions, actions and a timestamped transcript.
One shortcut, three modes, one vocabulary list, one API key.

[Website](https://cantrip.mistystep.io) · [Product](PRODUCT.md) ·
[Design](DESIGN.md) · [Architecture](ARCHITECTURE.md)

> **v2 status:** implemented from the Active Folio and verified on Linux X11
> (see [ARCHITECTURE.md › Verification boundaries](ARCHITECTURE.md#verification-boundaries)).
> The macOS build type-checks and its helpers are tested, but it has not yet run
> on a Mac. Published releases and the website's install guide still describe v1.

## Use

- **Dictate.** Hold `Ctrl+Alt+Space` (`⌃⌥Space` on macOS) to talk and let go,
  or tap it to start and tap again to stop. Escape discards the take.
- **Modes.** `Raw` (exactly what was heard), `Clean` (fixes punctuation,
  fillers, stutters, misheard vocabulary and self-corrections) and `Elevated`
  (crisp prose). Turn **Post-Processing** off and no text reaches a language
  model.
- **Delivery.** Text goes into the field you started in, or is copied, with
  the reason shown, when focus moved or the field can't be reached. Cantrip
  never types into whatever happens to be focused.
- **Meetings.** `Start meeting` records your microphone (`You`) and the
  system's audio (`Others`). The note is written under
  `cantrip-meeting/YYYY/MM/` in your Meeting folder, which Cantrip only ever
  adds files to.
- **Recovery.** A failed or undelivered take keeps its audio until you choose
  Retry or Discard in the menu.

Speech goes to OpenRouter (`microsoft/mai-transcribe-2`, `openai/gpt-6-luna`)
with `provider.data_collection: "deny"`. Without a key or network, Cantrip uses
on-device recognition only where it's guaranteed. Otherwise it refuses and
keeps the recording.

## Build and run from source

Linux (Debian/Ubuntu package names):

```sh
sudo apt install build-essential pkg-config libdbus-1-dev pipewire-bin
cargo build --release
./target/release/cantrip            # tray app; `cantrip popover` opens the menu
cp contrib/linux/cantrip.desktop ~/.local/share/applications/   # launcher entry
```

On sessions without a tray (stock GNOME), the launcher entry opens the menu.
On native Wayland you can bind `cantrip toggle` as a compositor shortcut.

macOS 15+: `cargo build --release` on a Mac. App bundling, signing and
notarization for v2 are not done yet.

## Check

```sh
./scripts/check            # fmt, clippy (Linux + macOS type-check), all tests
DISPLAY=:0 cargo test -p cantrip -- --ignored --test-threads=1   # live desktop tests
cantrip e2e dictate <wav> --mode clean    # one real take over a recorded clip
```

## History

v1 (the local-first Parakeet dictation app, through 0.1.15) is preserved in
git history. [SPEC.md](SPEC.md), [VISION.md](VISION.md), `docs/` and
`docs/decisions/` describe v1. Where they disagree with v2,
[PRODUCT.md](PRODUCT.md) wins.

MIT licensed.
