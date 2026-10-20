# Dictate

The core journey: a user presses the dictation shortcut, speaks, presses it
again, watches the HUD, and pastes the transcript. With clipboard delivery the
text lands on the Wayland clipboard and the take is archived locally (US-001,
US-002, US-005).

## Sub-features

- `dictate-start` enters `recording`; the HUD shows "Recording · mm:ss" and live microphone levels.
- `dictate-stop` moves through `processing` stages to an idle terminal outcome.
- `dictate-deliver` reports `completeness: complete`, `delivery: copied`; the clipboard holds the sentence.
- `dictate-archive` writes `transcripts/<take>.json` with mode 600 and lists the take in `recordings --json`.

## How to get to it (user POV)

- Compositor shortcut bound to `cantrip toggle` (docs/DESKTOP.md "Dictation shortcut").
- `cantrip start` / `cantrip stop` in a terminal (docs/USAGE.md 4B, clipboard path).

## Driving it with verify-cantrip

Preconditions:

- Session is up with the daemon; config has `injection = "clipboard"` (the `up` default).

- **Journey.** `$V journey --run "$RUN" dictate`. Checks: speech played,
  outcome complete/copied, clipboard ≥ 0.75 word-similar to the JFK sentence,
  archive file owner-only, take listed by ID.
- **By hand.**
  1. `$V cantrip --run "$RUN" -- status --json` and note `outcome.event_id` (N).
  2. `$V cantrip --run "$RUN" -- start` prints `state: recording`.
  3. `sleep 1; $V speak --run "$RUN"` (blocks ~11 s while the clip plays).
     `$V shot --run "$RUN" hud-live` mid-speech shows the HUD with signal.
  4. `$V cantrip --run "$RUN" -- stop` prints `state: processing`.
  5. `$V wait --run "$RUN" --after-event N` prints the outcome:
     `"completeness": "complete"`, `"delivery": "copied"`.
  6. `$V clipboard --run "$RUN"` prints the sentence.
  7. `$V cantrip --run "$RUN" -- recordings --json` lists the take ID from step 5.
- **Proof.** `evidence/dictate-1-recording.png` (HUD while speaking),
  `dictate-2-settled.png`, the journey JSON, and `transcript.jsonl`.

## Gotchas

- `stop` only acknowledges processing; always wait for a newer `event_id`.
- Speaking in the same instant as `start` can clip the first words before
  `pw-record` links; wait ~1 s.
- Recognition varies by a word or two per take; do not assert exact text.
- `delivery: copied` is the mechanism's acknowledgement; the clipboard read is the receipt.
- Automatic paste/type is guarded to verified Hyprland/logind; this headless
  session cannot prove it (see failure-states `auto` check).
