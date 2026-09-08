# ADR 0021: Signed pixel waveform from independent PCM measurements

Date: 2026-09-07. Status: accepted.

## Problem

Increasing the listening HUD from 22 bars to a denser grid did not produce more
waveform detail. Eleven coarse logarithmic measurements were interpolated across
the display, and each bucket's largest absolute peak was mirrored around the
center. That produces a smooth envelope rather than independent audio columns.
The operator selected the signed pixel waveform in a public-audio design study.
Quieter speech needs a more sensitive response and a natural release to silence;
routine processing needs visible activity without inventing progress or words.

## Decision

At 100 ms intervals, capture 60 chronological raw signed PCM min/max pairs from
newly available audio, capped at the newest 100 ms (1,600 samples at 16 kHz).
Each column owns one bucket; there is no spatial interpolation. Shorter fresh
reads cover less than 100 ms.
Keep the existing cursor, split-sample handling, silence timing and logarithmic
health level. Do not replay old PCM as fresh activity or normalize each frame.
Empty buckets contain zeroes; both signed 16-bit extremes remain valid samples.

Carry the fixed 60-pair array through the existing private status IPC. Serialize
and deserialize its exact shape without an additional heap array or dependency.
The raw waveform and the logarithmic health level have different purposes; do not
reuse the health-level conversion for visual shape.
Allocate the recording state's larger signal cache once per take, not once per
measurement, so the rest of the daemon state machine stays compact.

Draw 60 columns of 3-by-3 logical-pixel cells on a 5-pixel pitch inside the existing
336-by-44 HUD. Positive peaks control the upper edge and negative peaks the lower
edge. For each side, magnitude at or below 32 stays zero. Otherwise apply a fixed
1.6× gain before square-root scaling: `sqrt(min(1, 1.6 * magnitude / 32767))`.
Apply the input floor before gain; never normalize per frame. Each extent is 1
plus 15.5 times that amplitude: a 2-pixel quiet baseline and a 33-pixel full-scale
waveform.
Align cell edges to physical pixels and fade boundary cells by fractional coverage,
rather than switching complete rows on and off.

Smooth each column's positive and negative extent independently over time: close
95% of a rising gap in about 100 ms and of a falling gap in about 420 ms. Snap
negligible residuals to finish settling exactly, including at silence. Keep
compositor frame-callback pacing, responsive input polling and unchanged-frame
render caching. Temporal smoothing never blends neighboring buckets or adds
random audio.

Transcription uses two broad, counter-moving lobes with quiet edges, on 2.6 s and
3.9 s cycles. Finishing, finalizing and delivery instead use equal-height groups
of pixels, brightening in mirrored pairs toward the center on a repeating 1.8 s
cycle. The groups never accumulate or grow a filled region. These are explicitly
indeterminate activity, not completion estimates. Interpolate phase changes from
the last presented frame over 280 ms. Determinate center-row fill advances only
from measured chunk reports, never elapsed time; entering finishing removes it.

A complete typed, pasted or copied result settles into a full-width band of three
pixel rows in the current accent color. The bright, steady band stays within the
waveform's geometry, without an icon, a new color or a progress animation. A
routine typed or pasted acknowledgement gets its full 700 ms settled hold
**after** the 280 ms transition, then a 140 ms fade. Reduced motion shows the
settled band immediately, holds it for 700 ms and cuts to idle.
Copied and cleanup-failure feedback retain their explicit captions and four-second
notice window; partial, uncertain and deferred delivery never receive the resolved
band. A helper acknowledgement is not proof that the target application received
the text.

This is presentation time only: there is no minimum processing-stage dwell and
no delay to output. New recording interrupts a transition, settled hold or fade
immediately. Repeated status snapshots cannot renew a result; cached successes
on attach or daemon restart are still not replayed.

Routine stages remain wordless by default: no delayed reveal, long-recording
label or caption latch. Explicit `hud.labels = true` enables continuous
accessibility text. Keep labels for actionable exceptions, including unavailable
input, cancellation, failed or partial outcomes, uncertain or deferred delivery,
and manual-paste feedback.

Reduced motion presents a static transcription silhouette and evenly lit finishing
groups, and presents measured data directly. Stale or disconnected status freezes
or replaces activity rather than implying live work or Ready. Preserve the palette,
typography, footprint and noninteractive desktop surface.

The 2026-09-08 presentation refinement replaces the original single transcription
ripple, centered finishing breath and successful flat-baseline collapse. The breath
gave cleanup the same waveform vocabulary as transcription, while the old 700 ms
result window included its 280 ms transition and left little time at rest. Distinct
ordered groups and a settled accent band make the post-recording phases legible without
adding default words, guessed progress or another delivery state. Listening PCM,
signed sample scaling, interpolation and attack/release remain unchanged.
The operator rejected the initial pixel checkmark; completion now uses the same
track and palette rather than introducing a separate success symbol.

## Alternatives

- More interpolated bars only refine the same envelope; they cannot recover detail.
- Random offsets or decorative oscillation while listening would invent microphone
  activity; explicitly indeterminate processing motion has a different meaning.
- Hard pixel-row steps would reintroduce the just-resolved motion regression.
- Per-frame normalization could make quiet noise appear loud.
- A frequency spectrum would give columns a different meaning and is not the
  waveform the operator selected.

## Compatibility and limits

This replaces the waveform representation described by ADR 0014 and refines the
passive track in ADR 0019. Recording, transcription, retention and delivery semantics
are unchanged, as are themes and configuration keys. The status waveform changes from 11
logarithmic signed-byte pairs to 60 raw signed-16-bit pairs. The daemon and typed
clients must run the same revision; activate at an idle boundary and restart the
HUD with the daemon. There is no legacy decoder or alternate visualization mode.

The HUD is a bounded visualization, not an audio analyzer. Bucket peaks do not
retain every sample or every frequency component. The retained WAV remains the
source of truth and is not changed by this rendering decision.
