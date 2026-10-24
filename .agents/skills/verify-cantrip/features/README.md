# Cantrip verification map

This directory is the maintained source for verifying Cantrip's user-facing
behavior on Linux. Read this index, then use the matching feature file as the
recipe. `V` is `.agents/skills/verify-cantrip/bin/verify-cantrip`; `RUN` is a
fresh, short directory such as `~/.cache/tmp/vc/$(date +%m%d%H%M%S)`.

## Baseline preconditions

- `$V doctor` exits 0.
- A binary under test: `$V install --out "$RUN"` (published release) or
  `target/release/cantrip` (this checkout).
- `$V up --run "$RUN" --binary <binary>` printed its JSON and
  `$V doctor --run "$RUN"` shows the session daemon idle.
- Never drive the operator's own daemon (`~/.local/bin/cantrip`, default XDG
  dirs); not even `ping`, which writes to its log. Every command goes through
  `$V cantrip --run "$RUN" -- ...` or a journey.

## Driving conventions

- A shortcut press is `cantrip toggle` / `start` / `stop`; the compositor only
  runs that command.
- Speech is `samples/jfk.wav` played into the private sink with `$V speak`.
  Wait ~1 s after `recording` before speaking, as a person would.
- Wait on `status --json` for `idle` plus an outcome with a newer `event_id`,
  never on fixed sleeps (`$V wait --after-event N`).
- Restore anything a recipe changes in the session config (`reload` after).

## Proof and skip reporting

- Pair each action with the resulting state: outcome (`completeness`,
  `delivery`), clipboard bytes, `recordings --json` row, archive file.
- UI proof is a screenshot plus its OCR text from the headless output.
- CLI proof is command, exit code, stdout, stderr (`evidence/transcript.jsonl`).
- Report an unreachable path with the attempted command and the unmet
  precondition. Do not report a skipped entry point as verified through another.

## Account and sign-in: n/a

Cantrip has no accounts or sign-in anywhere: the desktop app is local and the
website is static. There is no `sign-in` feature. Cloud STT/cleanup credentials
are API keys in the OS keyring (`cantrip key set|status|rm ID`); the paid cloud
lanes are not driven here because they spend money with the owner's provider
keys. Verify them only on an explicit request, with the owner's chosen provider.

## Features

- [Install a published release](./install.md) covers download, checksum and
  attestation verification, private-prefix install, and the repeat-install refusal.
- [Transcribe a file](./transcribe.md) covers one-shot local transcription,
  `doctor`, `models status`, and the missing-model refusal.
- [Dictate](./dictate.md) covers start/speak/stop with the live HUD and
  clipboard delivery, plus history and `recordings`.
- [Recover takes](./recover.md) covers copy by ID, cancel, local recovery, and
  confirmed Forget.
- [Native windows](./native-windows.md) covers Settings (past transcripts),
  Actions (recordings and recovery), and HUD states.
- [Website](./website.md) covers cantrip.mistystep.io pages, deploy identity,
  downloads, 404, and desktop/mobile rendering.
- [Failure states](./failure-states.md) covers silence, absent microphone,
  second daemon, daemon down, missing model, and automatic delivery on an
  unverified desktop.

## Not mapped yet

- Automatic paste/type on the supported Hyprland/logind desktop (US-001,
  US-003, US-004): needs an attended, focused live session. Headless Sway is an
  unverified desktop, so `auto` there proves only the guard.
- Opt-in cleanup and cloud STT (US-007 to US-010, US-014): need a configured
  endpoint; local cleanup through an OpenAI-compatible localhost server is the
  free route.
- Handoff targets (US-011) and the Omarchy bar (US-012,
  `integrations/omarchy/`).
- macOS app (US-013): `scripts/verify-macos` on a Mac.
