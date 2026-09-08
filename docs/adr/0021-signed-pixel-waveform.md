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

Multi-chunk transcription fills the full seven-row pixel grid from left to right,
using only the reported completed-chunk fraction. Filled cells may shimmer, but
the bright/dim boundary cannot creep ahead of the measured target. Phase and
reported-progress changes interpolate from the last presented frame over 280 ms.
Single-chunk or unknown progress uses a short, repeating left-to-right pixel
packet, never an accumulating percentage. Existing approximately 30-second STT
chunk boundaries remain unchanged; do not split speech merely to animate the HUD.

Finishing, finalizing and delivery light every cell with independent, smoothly
varying brightness. Deterministic per-cell noise gives cleanup its processing
texture without synchronized flashing, gaps, or a filled-region estimate.
Each cell remains active; reduced motion holds a static grid.

A complete typed, pasted or copied result settles into the entire seven-row grid
at full brightness in the current accent color. There is no icon or new success
color. A routine typed or pasted acknowledgement gets its full 1200 ms settled
hold **after** the 280 ms transition, then a 140 ms fade. Reduced motion shows the
settled grid immediately, holds it for 1200 ms and cuts to idle.
Copied and cleanup-failure feedback retain their explicit captions and four-second
notice window; partial, uncertain and deferred delivery never receive the resolved
grid. A helper acknowledgement is not proof that the target application received
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

Reduced motion presents measured waveform and progress data directly and freezes
indeterminate pixel activity. Stale or disconnected status freezes or replaces
activity rather than implying live work or Ready. Preserve the palette,
typography, footprint and noninteractive desktop surface.

The 2026-09-08 operator reviews replaced the initial counter-moving transcription
lobes, grouped cleanup packets and success checkmark. The measured progress front
now owns transcription's geometry; independent brightness across the full grid
distinguishes cleanup from the steady full-brightness acknowledgement. The hold
excludes settling and fading so completion has a visible moment at rest.
Listening PCM, signed sample scaling, interpolation and attack/release remain
unchanged.

### Local state review

`cantrip hud-gallery` is an offline native review window using the already-present
egui toolkit. Fixture status events drive the same HUD model, temporal
interpolation, layout, pixel painter and premultiplied frame fade as the passive
Wayland surface. The gallery displays that native pixel buffer as a texture; it
does not reimplement the visual states as egui widgets or web animations.

Keep the state catalog tied to the production screenshot scenarios. Replay and
seeking rebuild the same fixture history through the model, including presented
frames, so a transition can be inspected without microphone input or real
transcription. Playback, zoom, labels and reduced motion are local controls,
never saved configuration. The gallery does not contact the daemon, providers or
keyring, and does not inspect recordings. It remains separate from the public
website and needs no server, Storybook stack or second application runtime.

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
