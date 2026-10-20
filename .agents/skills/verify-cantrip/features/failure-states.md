# Failure states

The states that break dictation must fail honestly: no invented text, no
silent delivery, no second owner, no implicit downloads, and no keystrokes
where the destination cannot be verified (US-003, US-004, US-005).

## Sub-features

- `fail-silence` a silent take settles `empty` with audio retained, not invented text.
- `fail-absent-mic` an `audio_source` that does not exist never yields a complete take.
- `fail-second-daemon` a second `daemon` on the same runtime exits non-zero; the first keeps answering.
- `fail-no-model` `transcribe --local` without a model exits non-zero and downloads nothing.
- `fail-unverified-desktop` `injection = "auto"` outside verified Hyprland/logind copies or defers, never pastes or types.
- `fail-daemon-down` `ping` with no daemon fails within seconds with a "start it with: cantrip daemon" hint.

## How to get to it (user POV)

- Speaking nothing; a microphone that was unplugged after configuring it;
  starting Cantrip twice; skipping `models pull`; using a non-Hyprland desktop;
  running commands while the daemon is stopped.

## Driving it with verify-cantrip

Preconditions:

- Session is up with the daemon. The journey restores the session config after each change.

- **Journey.** `$V journey --run "$RUN" breaks`. Screenshots:
  `breaks-1-silence.png`, `breaks-2-absent-mic.png`,
  `breaks-3-auto-unverified-desktop.png`.
- **By hand (absent mic).** Edit `audio_source` in the session config
  (`$V cantrip --run "$RUN" -- config path`), `reload`, `start`, wait 3 s,
  `stop`, `$V wait`; restore and `reload`.

## Gotchas

- Absent microphone (Linux, v0.1.8): `doctor` still prints `capture: ready
  (pw-record; configured source)` and the take settles `empty` ("No text
  returned. Saved audio is available.") with a silent WAV retained. PipeWire
  leaves `pw-record` unlinked, so no other microphone is recorded, but the user
  is not told the source is missing. Tracked as a product gap; the check
  asserts only "not complete".
- Silence may settle `empty` or `failed`; both are honest.
- The daemon-down check uses a private empty runtime dir, so it never touches
  the operator's socket.
