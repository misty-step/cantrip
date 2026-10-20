---
name: verify-cantrip
description: "Drive Cantrip, the local-first Linux dictation app, the way a user does and capture proof: install the published release, dictate real speech through a private PipeWire microphone into an isolated daemon, read the result off a real Wayland clipboard, recover/cancel/forget takes, open the native Settings/Actions windows, and check the public website. Use to prove a Cantrip change works in the real app, before shipping, or when anything about dictation, delivery, recovery, release install, or cantrip.mistystep.io looks off."
---

# Verify Cantrip

Cantrip's user surface is a native Linux CLI/daemon with a Wayland HUD and
native windows, plus a static website at <https://cantrip.mistystep.io>.
The helper `bin/verify-cantrip` (Python 3, stdlib only) drives all of it from a
shell. It works the same for Claude Code and Codex; T3 preview tools are never
required.

Every Cantrip process it starts runs with **private HOME/XDG directories**, a
**headless Sway** compositor (own clipboard, own layer-shell HUD), and a
**private PipeWire null sink as the microphone**. The operator's daemon,
clipboard, configuration, keyring, model directory (read-only reflink copy),
and transcript history are never touched. Two runs with different `--run`
directories can coexist.

Paths below are relative to the repository root. `V` is the helper:

```sh
V=.agents/skills/verify-cantrip/bin/verify-cantrip
RUN=~/.cache/tmp/verify-cantrip/$(date -u +%Y%m%dT%H%M%SZ)   # scratch + evidence; never /tmp (RAM)
```

## Account and sign-in

None exists. Cantrip has no accounts: the desktop app is local-only and the
website is static with no account (`site/src/lib/sentry.ts`). Owner sign-in is
**n/a**. Cloud STT/cleanup API keys live in the OS keyring via `cantrip key`;
they are provider credentials, not a Cantrip sign-in, and this skill never
reads them or calls paid providers (see [features/README.md](features/README.md)).

## Launch

Prerequisites (Arch names; `doctor` lists what is missing): `sway grim wtype
wl-clipboard tesseract pipewire pipewire-pulse (pactl, pw-play, pw-record) gh
jq curl` and the `playwright` CLI with Chromium. `sudo pacman -S --needed sway
grim wtype wl-clipboard tesseract tesseract-data-eng` covers the session tools.

1. **Pick the binary under test.**
   - Published release (what users get):
     `$V install --out "$RUN"` downloads the latest tag (or `--version X.Y.Z`),
     checks `SHA256SUMS`, `release.json` identity, the GitHub attestation of both
     files, the inner `checksums.txt`, installs into `$RUN/prefix` with private
     XDG dirs, and checks `--version` and the binary SHA-256 against
     `release.json`. Binary: `$RUN/prefix/bin/cantrip`.
   - This checkout: `cargo build --release --locked`, binary
     `target/release/cantrip`.
2. **Start the session:** `$V up --run "$RUN" --binary "$RUN/prefix/bin/cantrip"`.
   It creates the null sink `cantrip_verify_<pid>`, starts headless Sway,
   writes a fresh config (`config init`, then `injection = "clipboard"`,
   `audio_source = <sink>`, `[hud] labels = true`), reflink-copies the
   operator's Parakeet model if present (`--pull-model` downloads ~640 MB
   instead, which is the real first-run step), and starts `cantrip daemon`.
   **Ready** = `up` printed its JSON and `doctor --run` shows `session daemon
   ping: state: idle`. `--no-daemon` stops before the daemon for one-shot CLI
   work.
3. **Teardown:** `$V down --run "$RUN"`. See Cleanup.

## Doctor

`$V doctor [--binary PATH] [--run "$RUN"]` is read-only. It checks tools, the
speech fixture, the reusable model, the binary's `--version`, the public
website's `/health.json`, and with `--run` that every process `up` started is
still the same process, the sink exists, and the session daemon answers `ping`.
It also reports whether the **operator's own daemon** is running: that daemon
is observed only, never driven. Run doctor first, after any failed drive, and
whenever something looks off. A non-zero exit means do not drive.

## Drive

Scripted journeys (each prints `[ok]/[FAIL]` lines and writes
`evidence/journey-<name>-<UTC>.json`; exit 0 only if all checks pass):

```sh
$V journey --run "$RUN" transcribe   # one-shot file transcription + doctor
$V journey --run "$RUN" dictate      # start → speak → stop → clipboard
$V journey --run "$RUN" recover      # copy by id, cancel, local recover, forget
$V journey --run "$RUN" breaks       # silence, 2nd daemon, no model, absent mic, auto on unverified desktop, daemon down
$V journey --run "$RUN" windows      # native Settings, Actions, HUD render
$V journey --run "$RUN" all
$V site --out "$RUN"                 # public website, read-only
```

Primitives for exploring beyond the journeys (all logged to evidence):

```sh
$V cantrip --run "$RUN" -- status --json        # any cantrip subcommand, session env
$V speak --run "$RUN" [file.wav]                # play into the private mic (default samples/jfk.wav)
$V wait --run "$RUN" --after-event N            # block until idle with an outcome newer than N
$V clipboard --run "$RUN"                       # wl-paste from the session clipboard
$V shot --run "$RUN" NAME                       # screenshot + OCR text of the headless output
$V window --run "$RUN" settings|actions [--doctor] [--name NAME]
```

A user's "press the shortcut" is `cantrip toggle` (or `start`/`stop`); the
compositor binding only runs that command. Feature recipes live in
[features/README.md](features/README.md).

## Evidence

Everything lands in `$RUN/evidence/`: `transcript.jsonl` (every command, exit
code, stdout, stderr), `journey-*.json` / `site.json` / `install.json` reports,
PNG screenshots with `.ocr.txt` siblings, and `daemon.log` / `compositor.log`.

Proof standards:

- Drive the user path: CLI commands, real PipeWire audio, real Wayland
  clipboard, real native windows. No test-only endpoints or internal setters.
- Capture the action and the resulting state: status outcome **and** clipboard
  bytes **and** the archived history file / `recordings --json` row.
- The speech fixture is public (`samples/jfk.wav`). Never feed private audio,
  never commit anything from `session/` (history, WAVs), never paste transcript
  text from the operator's own history into evidence.
- Recognition varies by a word or two between takes; journeys compare word
  sequences (≥ 0.75 similarity), not exact strings.
- To publish proof, copy `evidence/` into
  `docs/evidence/verify-cantrip/<YYYY-MM-DD>/` on the PR branch.

## Cleanup

`$V down --run "$RUN"` stops only the PIDs `up` recorded (checked against their
command lines, so a reused PID is never signalled), sends the daemon SIGTERM and
waits, unloads only its own sink module, and deletes `$RUN/session/` (private
history, audio, config, model copy). `$RUN/evidence/` survives. Run it after
every failed iteration too. `up` refuses a run directory that still has a
session. Never `pkill cantrip`: the operator's daemon and other agents'
sessions share the machine.

## Helpers

`bin/verify-cantrip` is the only helper; `$V --help` and `$V <command> --help`
document every flag. Keep its invocations in this file in sync when it changes.
For macOS, use `scripts/verify-macos` on a Mac (see `docs/INSTALLATION.md`);
this skill is Linux-only. Keep the map current with `/maintain-verification-skill`.
