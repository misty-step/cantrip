# Transcribe a file

A user transcribes an existing WAV with the local Parakeet model and no daemon,
gets the text on stdout, and finds it in private history. `doctor` and
`models status` report setup without claiming dictation works.

## Sub-features

- `transcribe-local` prints the recognized text on stdout; diagnostics go to stderr.
- `transcribe-history` saves the successful result to owner-only history with `source: transcribe`.
- `doctor-report` lists config, capture, STT, cleanup, telemetry, injection, HUD, and daemon lines.
- `models-status` shows the installed model path.

## How to get to it (user POV)

- `cantrip transcribe --local recording.wav` (docs/USAGE.md "Transcribe an existing file").
- `cantrip doctor`, `cantrip models pull`, `cantrip models status` (first dictation, step 2).

## Driving it with verify-cantrip

Preconditions:

- Session is up (`--no-daemon` is enough for transcription alone).

- **Journey.** Run `$V journey --run "$RUN" transcribe`. Checks: stdout is the
  JFK sentence, a history entry with `source: transcribe` exists, and `doctor`
  exits 0 with `capture: ready (pw-record; configured source)`.
- **By hand.** `$V cantrip --run "$RUN" -- transcribe --local samples/jfk.wav`
  prints "And so, my fellow Americans, ask not what your country can do for
  you…" with exit 0.
- **Model.** `$V cantrip --run "$RUN" -- models status` prints `installed:`
  with a path inside `$RUN/session`.
- **First-run download.** `$V up --pull-model ...` runs the real `models pull`
  (~640 MB network download) instead of copying the operator's model.

## Gotchas

- Never run the operator's `cantrip transcribe` to "just check": it writes the
  operator's real history. Always go through the session.
- `doctor` exit 0 is not readiness; it is a report.
- A partial transcript exits non-zero while still printing text.
