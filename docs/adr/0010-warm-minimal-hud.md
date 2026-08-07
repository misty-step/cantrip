# ADR 0010: The HUD is a Warm Minimal capsule (static phases, no fake progress)

Date: 2026-08-05. Status: accepted.

## Problem

The HUD pill (ADR 0006) carried a kinetic design: the pill width followed
its content, recording showed an expanding ping ring, and processing showed a
rotating comet arc. All of those signal *progress* — "how much is done" —
but most phases had no defined 100% or duration estimate. The old animation
was therefore fake progress, and
an interrupted transition left partial text on screen while the pill kept
"filling". The operator locked a replacement direction ("Warm Minimal",
prototype catalog `catalog.html` section G, 2026-08-05) with two rules:
each phase is a fixed composition (motion stays localized and never
measures a trajectory), and no surface may imply a trajectory that is not
measurably real.

## Decision

The HUD renders one fixed 320×40 borderless capsule, top-centre, on the
420×56 layer-shell canvas:

- **Fill**: an opaque blend of the near-black floor `#0e0e11` with the state
  accent at a per-state ratio — recording 0.15, transcribing/cleaning 0.13,
  sent 0.45 (full-lit), notice 0.06 (drained). No border, rim light,
  underglow ribbon, or drop shadow.
- **Composition**: a static stage word centered as the visual anchor, a
  state glyph in a 28px zone at the left, and a monospace mm:ss timer
  right-aligned while recording. The capsule never resizes with content;
  overlong words truncate with an ellipsis.
- **Glyphs**: listening is a pulsing dot. Transcribing and cleaning use the
  indeterminate spinner (a rotating open arc that never fills). Multi-chunk
  transcription (`transcribing N/M`, M>1) keeps that spinner and adds a
  timed left-to-right capsule fill from empty toward each measured fraction.
  The per-state accent color differentiates the stage. The spinner's turn is
  a pure function of the render phase, so a frozen phase (reduced motion,
  screenshot) draws a calm full ring rather than a static open arc that
  could read as an unmeasured partial meter.
- **Motion**: each working phase is a fixed composition. Continuous motion
  is the localized breathing pulse on the working glyphs (alpha 0.8–1.0,
  scale 0.96–1.0), the spinner's turn when indeterminate, the ticking timer,
  and — only when the daemon reports multi-chunk STT — a timed left-to-right
  capsule fill that always starts empty and eases toward each measured `N/M`
  fraction (~480 ms). No unmeasured decorative fill. State changes ease over ~260 ms: pill pop-in (scale + alpha), accent
  and base-fill crossfade, the fresh glyph scales in (ease-out-back), and the
  stage word drifts up ~3 px. Outcome flashes are static compositions for
  ~2.5 s.
- **Outcomes**: Sent = full-lit green capsule + check pop ("Success", no
  timer); Notice = drained warm capsule + slashed ring ("Heard nothing" or
  the short operator reason). Distinguishable by luminance and glyph shape
  without color. Detailed delivery text stays on `cantrip status` / logs.
- **Reduced motion**: read once at startup from
  `gsettings get org.gnome.desktop.interface enable-animations`. When
  disabled, the pulse freezes, the spinner draws as a static
  full ring, the recording glyph is a plain static dot, and state changes
  swap instantly.
- **Screenshot hook**: `cantrip hud --screenshot <path> --state
  recording|transcribing|cleaning|sent|notice` renders any state's settled
  frame (progress 1.0, phase 0.0, no flash fade window) and exits, so every
  composition verifies offline with byte-identical output.
- **Determinate progress**: multi-chunk local STT exposes a real fraction
  via the existing status `stage` field (`transcribing N/M`). The HUD eases
  a left-to-right capsule fill to `N/M` while that stage is active. Single-
  chunk and cleaning stay on the indeterminate spinner — no decorative
  meter without a measurement. A finer decoder-token cursor remains optional
  future work if `transcribe-rs` exposes one.

## Why not alternatives

- Width-following fill / completing arcs without a measurement (ADR 0006
  style): fake progress. Rejected. A fill is allowed only for multi-chunk
  STT where `stage` carries a real `N/M`. The single-chunk / cleaning spinner
  stays indeterminate by construction: it rotates a fixed open arc that never
  completes, so it measures nothing — it only signals "busy".
- Determinate-only HUD without a measurement: would show nothing during
  long unmeasured phases; the breathing pulse and spinner carry "busy,
  unmeasured" honestly until a fraction exists.
- GTK or notify-based status (ADR 0002/0003): superseded by the layer-shell
  HUD; not revisited here.

## Consequences

- The pill reads the mode by composition and tint; trajectory appears only
  as a measured multi-chunk fill. A glance-stable fixed capsule is still
  cheaper than the old kinetic width-following pill.
- Reduced-motion detection depends on gsettings being present; on desktops
  without it (or non-GNOME key names) animations default to on.
- Long stage words (localized) truncate; the centered word cannot collide
  with the timer cluster.
- This ADR supersedes the visual description in ADR 0006; the layer-shell
  mechanics and supervision (ADR 0008) are unchanged.
- Open risk: the notice drain is low-luminance by design and could be
  missed if outcome timing ever shortens — the 2.5 s dwell is part of the
  contract.
