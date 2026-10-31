# Microphone access through the capture owner — 2026-10-30

Sanitized evidence for `fix/microphone-capture-owner` (#198). Build: ad-hoc
`Cantrip.app` packaged from `3556070` (`source_tree_dirty: false`), installed in
a disposable macOS 27 `macvm` guest with BlackHole 2ch as the default input and
no `audio_source`. Config: `[stt] endpoint = "http://127.0.0.1:9/v1"`,
`injection = "clipboard"`. No transcript text or audio is recorded here.

## Settings first, then `cantrip start` without the menu

1. Cantrip.app launched and owns the engine. The CLI process (spawned by the
   guest agent) is itself authorized, yet status reports the app's answer:

   ```text
   $ cantrip doctor            # no engine yet: labeled, not the app's answer
   microphone: authorized — this process only; Cantrip is not running and may differ
   $ cantrip status --json     # app running
   … "handoff":null,"microphone":"not-determined"}
   ```

2. `1-menu-before-not-determined.jpg`: the menu offers Allow Microphone Access….
   `2-settings-before-not-requested.jpg`: Settings, opened from that menu, shows
   Not requested yet and its Allow button.
3. Clicking Allow in Settings: `3-settings-allow-prompt-requesting.jpg` shows the
   macOS prompt "Allow "Cantrip" to access your microphone?" and Settings
   reading Waiting for your decision…; `status --json` reported
   `"microphone":"requesting"`.
4. After Allow in the prompt, without touching the menu:

   ```text
   "microphone":"authorized"
   $ cantrip start   → state: recording / message: recording (exit 0)
   $ cantrip cancel  → state: processing / message: cancelling (exit 0)
   ```

   `daemon.log` (capture owner):

   ```text
   [Daemon] command=allow-microphone sender=pid=1747      # the Settings process
   [Capture] microphone access requested
   [Capture] microphone access answered state=Authorized
   [Capture] microphone recording started
   ```

5. `4-settings-and-menu-allowed.jpg`: Settings shows Allowed and the menu no
   longer offers Allow Microphone Access….

## Not run

The reverse order (grant from the menu, then Settings) and an app attached to
an external `cantrip daemon` were not exercised in this VM run.
