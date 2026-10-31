# Native macOS Recordings and Recovery window — 2026-10-31

Sanitized evidence for `feat/macos-native-recordings` (#198). Build: ad-hoc
`Cantrip.app` packaged with `scripts/package-macos --target aarch64-apple-darwin
--ad-hoc` from `aca2d84` (`source_tree_dirty: false`), installed in a disposable
macOS 27 `macvm` guest in the light appearance. Config:
`injection = "clipboard"`, `[stt] endpoint = "http://127.0.0.1:9/v1"`.

The guest image had no audio input device (its only device was the output-only
Apple Virtual Sound Device), so no take was captured with BlackHole. Instead,
two synthetic tone WAVs were left as finalized runtime recordings before launch,
and the engine imported them as unresolved takes with retained audio. One
synthetic complete take (public sentence, with audio) sat in transcript
history. No speech, private audio or personal text is recorded here.

## Observed

1. With Cantrip running, `cantrip actions` exited 0 twice and showed one
   window; `pgrep -fl cantrip` listed one process (pid 761). Initial selection
   was the newest waiting take. Recover Locally was disabled with "Needs the
   local model. Install it from Setup."
2. **Recover with Provider** on the 5 s take: the outcome "Could not connect
   to cloud transcription… No text delivered. Saved audio is available…"
   (`completeness failed`, `error stt-failed`, bound to that take ID) appeared
   with **Dismiss Outcome** (`2-recover-failed-outcome.jpg`). `pbpaste` was
   empty. **Dismiss Outcome**: `status --json` reported event 1
   `dismissed: true`.
3. **Forget…** opened the alert sheet naming the take's time, duration and ID
   (`3-forget-confirmation.jpg`). Return left the sheet open and the WAV in
   place; Escape closed it and the WAV stayed. **Forget…** then **Forget
   Recording**: only that take's `.wav` and its text-less `.json` were removed;
   the other imported take and the complete take kept all their files
   (`ls -l` of `transcripts/` before and after). The window then read "This
   recording is no longer available. Choose another recording explicitly."
   instead of selecting another take.
4. **All Recordings**, the complete take, **Copy Transcript**: `pbpaste`
   returned its saved text exactly; the window showed "Copied. Paste when
   ready." (`4-copy-transcript.jpg`).
5. Menu **Check Setup…** switched the same window to Setup and showed the
   check's report (`5-check-setup-from-menu.jpg`); still one process.
6. Menu **Quit Cantrip**: `pgrep -fl cantrip` found nothing (exit 1).
7. With no app running, `cantrip actions` exited 0 after opening Cantrip
   through LaunchServices (new pid 993), which showed the window
   (`1-cli-opened-window.jpg`).

## Re-run at `5cdd005` (rebased onto #211's `f9ce972`)

A fresh guest with the same fixtures and a packaged build of `5cdd005`.

8. With Terminal in front (`lsappinfo front` named Terminal), typing
   `cantrip actions` in it brought the window forward as the key window:
   `lsappinfo front` then named Cantrip, and the selected row showed the
   active highlight. One `cantrip` process.
9. **All Recordings** showed all five columns at the default window size,
   with complete capture times and **Waiting** in Recovery for the two
   unresolved takes (`6-terminal-request-focused-all-recordings.jpg`).

## Not verified here

- Dark appearance in the live guest. Both views were rendered offscreen in a
  dark-appearance session on the build host and inspected there.
- BlackHole capture, Install Local Model's download, closing during a
  download, the API-key sheet, Reload Configuration and Start Cantrip.
- The toolbar's Recordings/Setup switch: a system notification covered it in
  every guest capture; offscreen renders draw its selected segment without a
  label, which needs a live look.
- Keyboard focus in the first run: the window came forward inactive. Fixed
  in `5cdd005`; see step 8.
- In the first run, **All Recordings** pushed the Recovery column past the
  table's visible width. Fixed in `5cdd005`; see step 9.
- Dark appearance in a live guest: `defaults write -g AppleInterfaceStyle
  Dark` and relaunching Cantrip left the window light, so no live dark
  capture exists.
