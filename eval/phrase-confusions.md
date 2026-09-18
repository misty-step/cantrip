# Phrase confusions (live STT misses)

Work record 2026-09-18. GitHub issues are disabled on this repo; Linear
was unavailable from the filing session. This file + the eval case is
the ticket.

## no-excuse → "note: use"

- **Spoken:** "no excuse"
- **Heard:** "note: use" (operator had to edit the transcript by hand)
- **Backend:** configured cloud STT (`microsoft/mai-transcribe-2` via
  OpenRouter). Workstation `[postproc] enabled = false`, so cleanup did
  not have a chance to repair it.
- **Why it matters:** the phrase is short, stressed, and used as a
  factory-law closer. A wrong reading changes the instruction.

### Required eval (not yet recorded)

Add a wav clip of the operator saying "No excuse. Automated testing
should just be comprehensive." to `samples/eval/manifest.json`.

Fail closed if the STT transcript contains `note: use` / `note use` for
that clip. Do not "fix" this only in postproc — postproc is opt-in.

### Postproc safety net

`samples/eval/vocab-behavior.json` case `no-excuse-not-note-use` is the
cleanup grader if postproc is on. It is not a substitute for the wav.

### Architecture notes (do not boil the ocean)

- Vocabulary list cannot encode this: it is a phrase, not a product name.
- Enabling postproc for every take is a product decision (US-008+), not
  this ticket.
- Prefer a short-phrase / factory-idiom eval split over growing the
  LibriSpeech regression set.
