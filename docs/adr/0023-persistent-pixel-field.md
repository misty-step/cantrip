# ADR 0023: One persistent pixel field, animated by active cells

Date: 2026-09-15. Status: accepted.

## Problem

The listening, transcribing, cleanup and success compositions used different
track geometries. Listening drew only the vertical extent each signed PCM
column reached, transcribing drew a centered three-row band, and cleanup/success
drew a seven-row, full-width grid. The container therefore appeared to change
size and shape between phases: most of the pixel field existed only in the
later states. The distinction made the instrument feel like separate widgets
and made transitions read as geometry morphs rather than one continuous
surface.

## Decision

The HUD visualization is one fixed field of `CELLS × ROWS` (60 × 7) logical
3×3 cells on a 5-pixel pitch, centered in the container at its full width. The
field is painted for every visible composition. Each cell always has at least
`FIELD_REST` (0.10) opacity in the current accent/attention/foreground color;
work is expressed by raising specific fixed cells above that floor, never by
adding, removing or resizing cells.

- **Listening.** The signed PCM min/max still selects which rows a column
  reaches above and below the center. A row fully outside the extent stays at
  the rest floor; a row the extent only partly covers fades between the floor
  and the column's active opacity by its fractional coverage. The column's
  active opacity remains `0.3 + 0.7 × amplitude`, and amplitude at or below the
  existing input floor stays entirely at rest.
- **Transcribing.** Only the centered three-row band is raised: the measured
  fractional front, or the bounded indeterminate packet, brightens those fixed
  cells. Every other cell of the field stays at rest. Pending cells are the
  resting field, not a second geometry.
- **Cleanup and delivery.** Every row is raised with the existing independent
  per-cell brightness noise; all 420 cells remain active.
- **Success.** Every row is raised to full accent brightness.
- **Attention and neutral.** The whole field stays visible with the existing
  center-row emphasis.

Cell state is the only per-frame render input: `TrackColumn` is now just the
seven per-row opacities, and `TrackFrame` is just the columns. The `upper`,
`lower` and `expansion` geometry and the render-key height/expansion fields are
removed. The track always occupies the full container width; the old resting
336-pixel-inset width and its 400 ms expansion morph are gone.

Temporal behavior is unchanged in meaning and timing: transitions still ease
over `SETTLE`/`LISTENING_ONSET`, measured progress still reveals over
`PROGRESS_REVEAL`, waveform attack/release still use the 100 ms/420 ms
constants, and reduced motion still presents the target state directly. With a
constant field, those transitions are opacity crossfades over a stable surface
rather than shape morphs. Honest-progress rules, the success hold/fade,
persistent outcomes and the passive, noninteractive layer shell are unchanged.

## Why not alternatives

- **A separate field backdrop behind the existing geometry.** The active layer
  and the backdrop could disagree about width and alignment, and it leaves two
  competing models of the same pixels. A single per-cell state is simpler.
- **Keeping per-state geometry and only dimming it.** The phase would still
  appear to add and remove pixels; the point is that the surface never changes
  size or shape.
- **Hiding the field behind quiet audio.** That is the current behavior and the
  problem this decision removes. The floor is deliberately faint so activity,
  not the field itself, carries meaning.
- **Per-frame normalization or a new activity scale.** Would change measured
  waveform meaning and is not required by a uniform field.

## Consequences

The instrument reads as one fixed surface whose lit cells move with the work.
Listening, transcription, cleanup and success are now distinguishable by
*brightness distribution only*, so the faint rest floor must stay subtle enough
not to compete with active cells; `FIELD_REST` is the single tuning point.
Geometry-related tests are replaced by field-occupancy tests: every visible
composition must keep all seven rows at or above the floor, and only the
expected band may rise above it.

This supersedes the track geometry described by
[ADR 0021](0021-signed-pixel-waveform.md) — the signed PCM mapping, honest
front/packet, cleanup noise, success grid, timing and reduced-motion contracts
still apply. It replaces the earlier geometry description in ADR 0010 and
ADR 0014 as well.
