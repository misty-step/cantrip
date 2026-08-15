# ADR 0014: The recording HUD shows measured input signal

Date: 2026-08-14. Status: accepted.

## Problem

The existing recording capsule proves that `pw-record` started and shows elapsed
time, but its pulsing dot does not prove that captured PCM contains a microphone
signal. An operator can therefore dictate for many minutes into a muted, dead,
or disconnected capture path and discover the failure only after stopping.

The status surface must distinguish process activity from measured audio without
claiming speech detection or destroying a recoverable take.

## Decision

The daemon measures the signed 16-bit PCM that the existing `pw-record` process
appends to its live WAV. It finds the RIFF `data` chunk and reads only newly
appended samples on a fixed 200 ms daemon-loop cadence. The daemon caches that
window's result; status clients only read the cache, so the HUD's 200 ms polling
and the settings window's 1 s polling cannot consume or shorten each other's
measurement windows. The status reply reports three optional fields:

- `audio_level`: the newest peak, logarithmically mapped to `0..=100` from a
  floor of approximately -60 dBFS;
- `audio_silent`: true after every measured peak has remained at or below that
  floor for three seconds;
- `audio_waveform`: eleven chronological `[minimum, maximum]` PCM envelope bins
  downsampled from the newest 200 ms window. Both edges use the same signed
  logarithmic `-100..=100` scale as `audio_level`.

The monitor reuses the recording stream. It does not start a second PipeWire
client, duplicate audio, change the completed WAV, or add an audio framework.
Monitoring failure is non-fatal and leaves all three cached fields absent.

The HUD remains a read-only state mirror. While recording, its left glyph is a
compact waveform driven by the latest `audio_waveform` min/max envelope, not an
equalizer or synthetic oscillation. A faint centerline marks zero input. Each
new measured frame eases from the prior measured frame over 140 ms; reduced
motion snaps directly to the data. When `audio_silent` is true, the trace is
flat and the fixed capsule changes to an amber `No mic signal` composition
while keeping the elapsed timer.
Capture continues, and the warning clears on the first sample above the floor.

The threshold intentionally detects near-digital silence, not the absence of
speech. Quiet-room microphone noise can keep the warning clear; the measured
envelope still exposes that input. The design adds no voice activity detector,
adaptive calibration, configuration knob, notification, sound, or automatic
cancel.

`cantrip status` prints the measured level, silence state, and envelope bins.
The IPC fields use Serde defaults so older daemon replies remain readable. The
deterministic HUD screenshot hook includes `--state no-signal`.

## Why not alternatives

- **Passive live meter only:** still requires the operator to notice that bars
  never move during a long take.
- **Stream-health indicator:** proves bytes are arriving but cannot distinguish
  a muted source that emits zero-valued PCM.
- **Speech-energy threshold:** catches more quiet or misrouted inputs but warns
  during normal thinking pauses and against distant microphones.
- **Startup-only check:** cannot catch a source that fails mid-dictation.
- **Automatic cancel:** a threshold error or intentional silence would destroy
  the take. The operator owns stop and cancel.
- **Second capture process or PipeWire binding:** duplicates the source or adds
  lifecycle and dependency burden when the authoritative WAV already contains
  the required samples.

## Consequences

The recording composition now proves measured input activity rather than only a
live child process. A muted or dead path becomes visible within three seconds,
while normal recording, stop, cancellation, STT, injection, recovery, privacy,
and `SIGINT` shutdown behavior remain unchanged.

This supersedes ADR 0010 only where it specifies a pulsing recording dot and the
old screenshot-state list. Its fixed geometry, honest-progress rule, processing
spinner, determinate multi-chunk fill, reduced-motion behavior, and outcome
flashes remain in force.
