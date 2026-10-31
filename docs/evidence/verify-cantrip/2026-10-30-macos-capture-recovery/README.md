# macOS capture start, empty takes and failure HUD — 2026-10-30

Sanitized evidence for `fix/macos-capture-start-recovery`. No transcript text or
audio is recorded here. VM runs used a disposable macOS 27 `macvm` guest with
BlackHole virtual inputs; nothing ran against the operator's live install.

## 1. Live failure (operator Mac, Cantrip.app 0.1.11, read-only observation)

`cantrip start` → `Starting recording failed` / `capture-failed`. `daemon.log`
held only `WARN [Capture] starting recording failed class=capture-failed`.
The macOS unified log for the app process at the same instant:

```text
AVAudioEngineGraph.mm:504  Error, formats don't match! Input HW format: <1 ch, 48000 Hz, Float32>, tap format: <1 ch, 16000 Hz, Float32>
AVAEInternal.h:109         [AVAudioEngineGraph.mm:1412:Initialize: (err = AUGraphParser::InitializeActiveNodesInInputChain(ThisGraph, *GetInputNode())): error -10868
AVAudioEngine.mm:242       Engine@…: could not initialize, error = -10868
```

Configured `audio_source = "BuiltInMicrophoneDevice"` (48 kHz); the system
default input was a Bluetooth headset at 16 kHz. AVAudioEngine binds its input
node to the default device; after `kAudioOutputUnitProperty_CurrentDevice`
selects the configured device, the node's output format still describes the
default device, and the nil-format tap inherits it.

## 2. VM reproduction and fix

Topology: default input/output `BlackHole 2ch` at 16 kHz, configured
`audio_source = "BlackHole16ch_UID"` at 48 kHz.

| Build | `cantrip start` | Unified log |
|---|---|---|
| master `4944dc5` (baseline) | `capture-failed` | `formats don't match! Input HW format: 16 ch, 48000 Hz … tap format: 2 ch, 16000 Hz` → `-10868` |
| `592f00f` (hardware tap format) | 7/7 starts recorded; WAVs 48 044–80 044 bytes (1.5–2.5 s of PCM) | no `-10868`; configuration change posted before start |
| `592f00f`, no `audio_source` (default input) | recorded, 48 044-byte WAV | — |

## 3. Empty take → storage warning → startup notice

Baseline, start then cancel within 20 ms (before the first 100 ms tap buffer):

```text
[Capture] microphone recording started
[Daemon] state recording -> processing reason=cancel
WARN [Daemon] artifact persistence failed class=storage-failed
[Daemon] operation settled completeness=Failed delivery=None cleanup=Off
```

A 44-byte `rec-<id>.wav` stayed in the runtime directory beside an identical
durable copy. Restarting baseline: `WARN [Daemon] recovery discovery incomplete
class=storage-failed` and the notice "Saved recordings could not all be loaded".

Installing the integrated build over that exact state and starting it: no
discovery warning, runtime leftover consumed, durable take still listed
(`19 ms  no text  text=false audio=false pending=true`). A new start-then-cancel
settles `completeness=Cancelled delivery=Cancelled` with no storage warning and
no runtime leftover.

## 4. Failure HUD lifetime

- `before-failure-hud-38s-after-settle.jpg`: baseline keeps the failure panel
  on screen until Dismiss.
- `after-failure-hud-2s.jpg` / `after-failure-hud-11s.jpg`: integrated build shows
  the failure, then clears it after the 8 s hold; `cantrip status` still reports
  `dismissed: false`.
- `after-menu-keeps-outcome-and-recovery.jpg`: the menu still carries the
  outcome, its recovery actions and Dismiss Outcome after the panel clears.
- A capture-start setup failure (unknown `audio_source`) behaves the same:
  visible at ~1 s, gone at ~10 s, `dismissed: false`, and `daemon.log` now names
  the cause: `WARN [Capture] microphone unavailable configured=true`.

## 5. Microphone authorization split (not fixed here)

`permission-split-settings-vs-capture-owner.jpg`: after granting access through
the Settings window (its own process), Settings shows `Allowed` while the running
app — the capture owner — still offers `Allow Microphone Access…` and refuses to
record: `WARN [Capture] microphone permission not authorized state=NotDetermined`.
The reverse also reproduces (grant from the app menu; an open Settings window
keeps showing `Not requested yet`; its Allow button then flips to `Allowed`
without a prompt). Each ad-hoc rebuild is a new code identity, so macOS asks for
microphone access again after installing a new development build.
