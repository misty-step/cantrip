# Native macOS Settings window — 2026-10-30

Sanitized evidence for `feat/macos-native-windows` (#198). Build: ad-hoc
`Cantrip.app` packaged with `scripts/package-macos --target aarch64-apple-darwin
--ad-hoc` from the branch's working tree, installed in a disposable macOS 27.0
`macvm` guest (light appearance, no audio input device). The guest held a
synthetic commented `config.toml` and seven synthetic public transcripts; no
real speech, audio or personal text is recorded here.

## Observed

1. `open -a Cantrip`, then the menu's **Settings…** (`1-menu-settings-item.jpg`)
   opened the AppKit Settings window inside the app: `pgrep -fl cantrip`
   listed one process before and after.
2. **Transcripts** pane: seven saved transcripts, newest first. Copy on the
   newest row showed `copied` and "Copied transcript from … Paste when ready."
   (`2-transcripts-copied.jpg`); `pbpaste` in the guest returned that
   transcript's full saved text, byte for byte.
3. **General** pane: Microphone access read "Not requested yet". **Allow
   Microphone Access…** raised macOS's prompt for "Cantrip" while
   `cantrip status` reported `microphone: requesting`
   (`3-general-allow-prompt.jpg`). After Allow: `microphone: authorized`.
4. Vocabulary edited to add `Tahoe`, then **HUD** → Always show state labels,
   then **Save & Apply**: the window read "Saved and applied"
   (`4-hud-saved-and-applied.jpg`). The file kept its comment line and gained
   `vocabulary = ["Cantrip", "Parakeet", "Tahoe"]` and `[hud] labels = true`;
   `cantrip status --json` reported `"hud": {"labels": true, …}`.
5. With the window open, `cantrip settings` exited 0 and the same window stayed
   the only one; `pgrep` still listed one process.
6. **Quit Cantrip** from the menu closed the window and the app:
   `pgrep -fl cantrip` found nothing (exit 1).
7. With no app running, `cantrip settings` exited 0 after opening Cantrip
   through LaunchServices (new pid), which showed Settings on Transcripts
   (`5-cli-opened-app-on-transcripts.jpg`).

## Not verified here

- Dark appearance in the live guest. Every pane was rendered offscreen in a
  dark-appearance session on the build host and inspected there.
- A missing or repaired configuration file, the microphone list with real
  devices, and an app attached to an external `cantrip daemon`.
- Keyboard focus after `cantrip settings`: on macOS 27 the window came forward
  but the requesting process did not hand Cantrip keyboard focus.
