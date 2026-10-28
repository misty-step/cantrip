---
name: verify-cantrip
description: "Drive Cantrip, the local-first Linux dictation app, the way a user does and capture proof: install the published release, dictate real speech through a private PipeWire microphone into an isolated daemon, read the result off a real Wayland clipboard, recover/cancel/forget takes, open the native Settings/Actions windows, and check the public website. Use to prove a Cantrip change works in the real app, before shipping, or when anything about dictation, delivery, recovery, release install, or cantrip.mistystep.io looks off."
---

# Verify Cantrip

Cantrip's user surface is a native Linux CLI/daemon with a Wayland HUD and
native windows, plus a static website at <https://cantrip.mistystep.io>.
The helper `bin/verify-cantrip` (Python 3, stdlib only) drives all of it from a
shell. It works the same for Claude Code and Codex.

Every Cantrip process it starts runs with **private HOME/XDG directories**, a
**headless Sway** compositor (own clipboard, own layer-shell HUD), and a
**private PipeWire microphone**: a null sink `cantrip_verify_<pid>` that speech
is played into, remapped as the source node `cantrip_verify_<pid>_mic`, which
the session config selects as `audio_source`. Inherited audio routing
(`PIPEWIRE_NODE`, `PIPEWIRE_PROPS`, `PULSE_SINK`, `PULSE_SOURCE`, …, which agent
sandboxes set) is dropped from the session and from every PipeWire command the
helper runs, and `node.dont-fallback = true` is set so no stream can fall back
to the operator's default microphone or speakers. Every journey take checks
with `pw-dump` that the daemon's recorder is linked **only** to its own `_mic`,
and that the `_mic` is fed **only** by its own sink.
The session's `PATH` starts with a transparent `pw-record` shim that appends
the daemon's exact arguments and `PIPEWIRE_PROPS` to `session/pw-record.jsonl`
and then execs the real `pw-record`, so how Cantrip launches capture is
observable without changing it.
The session has no D-Bus session bus, so the OS keyring is unreachable from it.
The operator's daemon, clipboard, configuration, keyring, model directory
(read-only reflink copy), and transcript history are never touched. Two runs
with different `--run` directories can coexist.

Paths below are relative to the repository root. `V` is the helper:

```sh
V=.agents/skills/verify-cantrip/bin/verify-cantrip
RUN=~/.cache/tmp/vc/$(date +%m%d%H%M%S)   # new per run; short (socket paths fit 107 bytes); never /tmp (RAM)
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
   It creates the private sink and `_mic` source, starts headless Sway,
   writes a fresh config (`config init`, then `injection = "clipboard"`,
   `audio_source = <sink>_mic`, `[hud] labels = true`), reflink-copies the
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
still the same process (PID + start time), the private `_mic` source exists, and the session daemon answers `ping`.
It also reports host load, and whether the **operator's own daemon** socket
exists: that daemon is observed only, never contacted or driven. Run doctor first, after any failed drive, and
whenever something looks off. A non-zero exit means do not drive.

## Drive

Scripted journeys (each prints `[ok]/[FAIL]/[KNOWN]` lines and writes
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
$V cantrip --run "$RUN" -- status --json | jq .  # any cantrip subcommand, session env; raw stdout/stderr/exit
$V speak --run "$RUN" [file.wav]                # play into the private mic (default samples/jfk.wav); blocks ~11 s, add & to act mid-speech
$V links --run "$RUN"                           # nodes the daemon is recording from now (expect only the _mic)
$V wait --run "$RUN" --after-event N            # block until idle with an outcome newer than N
                                                # (N = .outcome.event_id from status --json before acting; 0 if none)
$V clipboard --run "$RUN" [--set TEXT]          # read (or overwrite, then read) the session clipboard
$V shot --run "$RUN" NAME                       # screenshot + OCR text of the headless output
$V window --run "$RUN" settings|actions [--doctor] [--name NAME]   # fails if the window exits early
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
- Recognition varies by a word or two between takes; journeys require at
  least 80% of the JFK sentence's words, in order, not exact strings.
- A check that fails because of a tracked product bug is reported as `KNOWN`
  with its issue link under `known_failures`; it does not fail the journey,
  and it is never weakened to pass. Any other failed check exits 1.
- To publish proof, copy `evidence/` into
  `docs/evidence/verify-cantrip/<YYYY-MM-DD>/` on the PR branch.

## Cleanup

`$V down --run "$RUN"` stops only the processes `up` recorded (PID plus kernel
start time, so a reused PID is never signalled), sends the daemon SIGTERM and
waits, unloads only PipeWire modules whose arguments still name this run's sink
(module IDs are reused), and deletes `$RUN/session/` (private
history, audio, config, model copy). `$RUN/evidence/` survives. Run it after
every failed iteration too. `up` refuses a run directory that still has a
session. Never `pkill cantrip`: the operator's daemon and other agents'
sessions share the machine.

## Helpers

`bin/verify-cantrip` is the only helper; `$V --help` and `$V <command> --help`
document every flag. Keep its invocations in this file in sync when it changes.
For macOS, use `scripts/verify-macos` on a Mac (see `docs/INSTALLATION.md`);
this skill is Linux-only. Keep the map current with `/maintain-verification-skill`.
