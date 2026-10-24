# Native windows

A user opens the native Settings window to edit configuration and copy past
transcripts, the Actions window to review recordings and recover them, and sees
the passive HUD's states (US-002, US-005).

## Sub-features

- `settings-open` shows dictation state, config path, and "Past transcripts" newest first with Copy.
- `actions-open` shows the current outcome, Recordings with retained-audio size, and per-take recovery buttons.
- `actions-doctor` opens the read-only setup diagnosis (`actions --doctor`).
- `hud-states` renders HUD scenarios (`hud --screenshot PATH --state NAME`).

## How to get to it (user POV)

- `cantrip settings`, `cantrip actions`, `cantrip actions --doctor`.
- The HUD appears during dictation (see dictate); `cantrip hud-gallery` is a hidden developer gallery.

## Driving it with verify-cantrip

Preconditions:

- Session is up. Run dictate first so the lists have rows.

- **Journey.** `$V journey --run "$RUN" windows` opens Settings and Actions in
  headless Sway, screenshots each with OCR, and renders the HUD `copied` state.
- **By hand.** `$V window --run "$RUN" settings`, `$V window --run "$RUN" actions`,
  `$V window --run "$RUN" actions --doctor --name actions-doctor`.
  OCR shows "Past transcripts" and "N saved transcripts" (Settings) and
  "Recordings", "Recover locally to clipboard" (Actions). The helper exits 1 if
  the window process exits before the screenshot; the journey re-shoots for up
  to 20 s until the expected text appears (OCR of one frame can be empty).
- **Interact.** Keyboard input goes through `wtype` with the session env, as
  `scripts/verify-settings-history.py` does for the US-005 copy journey
  (`BROWSER=none CI=1 python3 scripts/verify-settings-history.py --binary
  <binary> --out <empty dir>`, which builds its own headless session).
- **Proof.** `evidence/windows-settings.png`, `windows-actions.png`,
  `windows-hud-copied.png` and the `.ocr.txt` files.

## Gotchas

- OCR is approximate; assert a few stable words, not full lines.
- `--doctor` exists only for `actions`; `settings` rejects it.
- `actions --doctor` runs Check setup on open, but its monospace result renders
  below the Recordings list, outside the window's default height, so the
  screenshot shows only the normal Actions view. Prove setup with `cantrip
  doctor` (transcribe journey); treat the `--doctor` screenshot as "the window
  opens", not as diagnosis proof.
- The Settings check needs a numbered "N saved transcripts" line: the word
  "Copy" alone also appears in the help text with zero rows.
- HUD PNGs have no useful OCR text; inspect the image.
- Windows render in a floating Sway window; nothing else is on screen.
- `--screenshot` flags render one frame and exit; `$V window` shows the real
  running window instead.
