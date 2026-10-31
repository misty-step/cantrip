# Outside daemon: Cantrip names the app macOS grants — 2026-10-31

Fix for QA finding F1 on PR #203. Build: ad-hoc `Cantrip.app` packaged from
`c399bd2` (`source_tree_dirty: false`, binary sha256 `ab1606426b59…`) in a
disposable macOS 27 `macvm` guest. Config: `[stt] endpoint =
"http://127.0.0.1:9/v1"`, `injection = "clipboard"`. No transcript or audio.

Steps (QA's F1 steps): in the guest's Terminal.app, run
`/Applications/Cantrip.app/Contents/MacOS/cantrip daemon` (pid 864, parent 800,
the Terminal shell); then `open -n /Applications/Cantrip.app` (pid 934, parent
launchd), which attaches.

Before any prompt (measured):

```text
status --json → "microphone": {"authorization": "not-determined", "grantee": {"launcher": "Terminal"}}
status        → microphone-grantee: Terminal and everything it runs share this microphone access; Terminal started this daemon.
doctor        → microphone: not requested; choose Allow Microphone Access in the Cantrip menu or Settings. Terminal and everything it runs share this microphone access; Terminal started this daemon. — reported by the running Cantrip
daemon.log    → [Capture] microphone grantee=launcher
```

- `1-menu-allow-for-terminal.jpg`: the menu offers "Allow Microphone Access for
  Terminal…" with the disclosure line beneath it.
- `2-settings-note-before-allow.jpg`: Settings shows the same disclosure below
  the microphone rows.

`cantrip microphone allow` (`f1-allow-reply.txt`) replied "Waiting for your
answer to the microphone prompt. Terminal and everything it runs share this
microphone access; Terminal started this daemon." `3-prompt-names-terminal.jpg`:
the prompt is *Allow "Terminal" to access your microphone?*, as disclosed.

After **Don't Allow** (`f1-denied.txt`): status `denied`; doctor and the
`microphone-denied` reply say "enable Terminal in System Settings > Privacy &
Security > Microphone"; `4-menu-denied-for-terminal.jpg` shows the HUD notice
and "Microphone Access Denied for Terminal — Open Privacy Settings…".
`5-privacy-microphone-pane.jpg` is the pane that guidance points to.

Not run here: the Allow answer in this mode and the app-owned journeys at this
build (QA recheck), and Linux, which has no grantee.
