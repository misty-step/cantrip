# Failure states

The states that break dictation must fail honestly: no invented text, no
silent delivery, no second owner, no implicit downloads, and no keystrokes
where the destination cannot be verified (US-003, US-004, US-005).

## Sub-features

- `fail-silence` initial near-digital silence shows non-blocking Settings guidance after about three seconds; a silent take settles `empty` with its WAV retained, not invented text.
- `fail-no-frames` a finalized stopped zero-frame take settles `no-audio`, not a generic capture/storage failure; cancellation keeps its separate outcome.
- `fail-absent-mic` an `audio_source` that does not exist is refused as `capture-source-missing`, never substituted; `doctor` identifies the missing source. On Mac the menu offers explicit Settings/default-input actions.
- `fail-second-daemon` a second `daemon` on the same runtime exits non-zero; the first keeps answering.
- `fail-no-model` `transcribe --local` without a model exits non-zero and downloads nothing.
- `fail-unverified-desktop` `injection = "auto"` outside verified Hyprland/logind copies or defers, never pastes or types; the clipboard agrees with the reported delivery.
- `fail-daemon-down` `ping` with no daemon fails within seconds with a "start it with: cantrip daemon" hint.

## How to get to it (user POV)

- Speaking nothing; a microphone that was unplugged after configuring it;
  starting Cantrip twice; skipping `models pull`; using a non-Hyprland desktop;
  running commands while the daemon is stopped.

## Driving it with verify-cantrip

Preconditions:

- Session is up with the daemon. The journey restores the session config after each change.

- **Journey.** `$V journey --run "$RUN" breaks`. It restores the session config
  in a `finally`, even when a check aborts. Screenshots:
  `breaks-1-silence.png`, `breaks-2-absent-mic.png`,
  `breaks-3-auto-unverified-desktop.png`.
- **By hand (absent mic).** Edit `audio_source` in the session config
  (`$V cantrip --run "$RUN" -- config path`), `reload`, then `doctor` and `start`. Expect `capture: blocked` and a refused
  start naming the missing microphone. Restore and `reload`.
- **macOS.** Use the candidate native app in a disposable `macvm` guest with
  BlackHole and `scripts/verify-capture-setup-macos`; follow the
  [attended VM recipe](../../../../docs/USAGE.md#disposable-vm-proof-no-physical-headset).
  Inspect the actual HUD/menu/focus states as well as the driver's metadata checks.

## Gotchas

- The session keeps `node.dont-fallback = true` as a safety net when probing
  absent microphones, including older binaries. The journey requires refusal
  before launching `pw-record`, a clear error in `doctor` and `start`, and
  byte-for-byte unchanged accepted takes. Never remove that safety net when
  checking older binaries (https://github.com/misty-step/cantrip/issues/162).
- The daemon-down check uses a private empty runtime dir, so it never touches
  the operator's socket.
