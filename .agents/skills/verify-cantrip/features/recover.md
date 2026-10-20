# Recover takes

A user gets a past transcript back, cancels a take without losing its audio,
retries it locally, and deliberately forgets its retained audio while complete
archived text stays (US-005, US-006).

## Sub-features

- `recover-copy` copies an exact take's saved text with `copy ID`, without sending keys.
- `recover-cancel` stops a recording with nothing delivered and audio retained.
- `recover-local` re-transcribes a selected take with `recover --id ID --local --clipboard`.
- `recover-forget` refuses without `--yes`; with it, removes audio and keeps complete archived text.

## How to get to it (user POV)

- CLI: `cantrip recordings`, `copy ID`, `cancel`, `recover --id ID --local --clipboard`, `forget ID --yes`.
- Native: `cantrip actions` ("Recover locally to clipboard", "Copy this transcript", Forget) and
  `cantrip settings` "Past transcripts" Copy (see native-windows).

## Driving it with verify-cantrip

Preconditions:

- Session is up with the daemon. The journey dictates its own take first.

- **Journey.** `$V journey --run "$RUN" recover`. Checks: copy ID restores the
  sentence after the clipboard was overwritten; `cancel` settles `cancelled`
  with the placeholder clipboard untouched and `audio: true`; local recover
  completes and copies; `forget` without `--yes` exits non-zero; with `--yes`
  the row shows `audio_available: false` and the archive JSON still exists.
- **By hand.** Each step goes through `$V cantrip --run "$RUN" -- ...`:
  1. `copy <ID>`, `$V wait --after-event N`, then `$V clipboard --run "$RUN"` shows the sentence.
  2. `start`, `sleep 1`, `$V speak --run "$RUN"`, `cancel`, `$V wait` shows `"completeness": "cancelled"`.
  3. `recover --id <cancelled ID> --local --clipboard`, `$V wait` shows complete/copied.
  4. `forget <ID>` exits 1 asking for `--yes`; `forget <ID> --yes`, then
     `recordings --json` shows the row with `"audio_available": false`.
- **Proof.** `evidence/recover-1-cancelled.png`, `recover-2-recovered.png`,
  the journey JSON.

## Gotchas

- `recover` without `--id` picks the newest unresolved take; always pass `--id`.
- `--clipboard` matters: without it recovery uses the configured injection.
- Forget never removes complete archived text by design; asserting the archive
  is gone would be a wrong test.
