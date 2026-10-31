# Native macOS Settings window — 2026-10-31

Sanitized evidence for `feat/macos-native-windows` (#211, #198), run in a
disposable macOS 27.0 `macvm` guest (light appearance, no audio input device)
with a synthetic commented `config.toml` and seven synthetic public
transcripts. No real speech, audio or personal text is recorded here.

| Build | `source_revision` | `source_tree_dirty` | `Contents/MacOS/cantrip` SHA-256 |
| --- | --- | --- | --- |
| A | `f9ce9727689689fdf269f841bf5c3d451d97921c` | false | `45eb30dcf4bc22229d0c716f2eee93ff7c3608e8c9219926c973a886d77acf97` |
| B | `a6cc820a5d8087dd915b181df6c789b168316071` | false | `b2a1f4a8a8821bb8673a94974a3d4deaf593929602fbdf192a3709907a041a54` |

Both are ad-hoc `Cantrip.app` packages from `scripts/package-macos --target
aarch64-apple-darwin --ad-hoc`; the guest's `shasum` matched the hash above and
`build.json` reported the revision. B adds only the access-row layout fix.

## Build A: the Settings journey

1. `open -a Cantrip`, then the menu's **Settings…** opened the AppKit window
   inside the app on Transcripts (`1-settings-from-menu.jpg`); `pgrep -fl
   cantrip` listed one process.
2. Copy on the newest row showed `copied` and the result line
   (`2-transcripts-copied.jpg`); the guest's `pbpaste` held that transcript's
   full saved text.
3. General → **Allow Microphone Access…** raised macOS's prompt for "Cantrip"
   (`3-general-allow-prompt.jpg`). `cantrip status --json` reported
   `{"authorization": "requesting", "grantee": "cantrip"}`, then
   `"authorized"` after Allow.
4. Vocabulary edited to add `Tahoe`, then HUD → Always show state labels, then
   **Save & Apply** read "Saved and applied" (`4-hud-saved-and-applied.jpg`).
   The file kept its comment line and gained `vocabulary = ["Cantrip",
   "Parakeet", "Tahoe"]` and `labels = true`; status reported
   `"hud": {"labels": true, …}`.
5. ⌘W closed the window and `cantrip settings` (exit 0) opened a new window on
   Transcripts with one app process (`5-closed-then-cli-reopened.jpg`).
6. **Quit Cantrip** from the menu left no `cantrip` process (`pgrep` exit 1).

## Build B: an outside daemon's grantee

`cantrip daemon` was started by the guest agent, so no app bundle launched it:
status reported `"grantee": {"launcher": null}`. After `tccutil reset
Microphone`, status reported `"authorization": "not-determined"`. Cantrip.app
attached to that daemon, and `cantrip settings` → General showed "Not
requested yet", **Allow Microphone Access for the app that started this
daemon…** on its own row, and the warning "The app that started this daemon,
and everything it runs, share this microphone access."
(`6-outside-daemon-grantee-row.jpg`). Build A showed the same text but its
long button widened the form past the window's edge; build B fixes that.

## Not verified here

- Dark appearance in the live guest (offscreen renders only, on the build host).
- A named launcher such as Terminal, the denied and restricted rows, and
  answering Allow for an outside daemon.
- That the ⌘W and the request in step 5 fell within one app tick.
- A repaired configuration file and real microphone devices.
