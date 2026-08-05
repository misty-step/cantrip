# ADR 0010: The HUD is a Warm Minimal capsule (static phases, no fake progress)

Date: 2026-08-05. Status: accepted.

## Problem

The HUD pill (ADR 0006) carried a kinetic design: the pill width followed
its content, recording showed an expanding ping ring, and processing showed a
rotating comet arc. All of those signal *progress* — "how much is done" —
but the daemon cannot measure a fraction for any phase: there is no defined
100% and no duration estimate. The animation was therefore fake progress, and
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
- **Glyphs**: each working state carries an honest, distinct shape —
  recording is a plain dot, transcribing is an indeterminate spinner (a
  rotating open arc that never fills), cleaning is an eight-ray sparkle
  (cardinal + short diagonal rays; never the crossed lines of an X). The
  spinner's turn is the only added continuous motion; it is a pure function
  of the render phase, so a frozen phase (reduced motion, screenshot) draws
  a calm full ring rather than a static open arc that could read as a
  partially-filled meter.
- **Motion**: each working phase is a fixed composition; the only continuous
  motion is the localized breathing pulse on the working glyphs (alpha
  0.8–1.0, scale 0.96–1.0), the spinner's turn, and the ticking timer —
  never a length change or a determinate meter.
  State changes ease over ~260 ms: pill pop-in (scale + alpha), accent and
  fill crossfade, the fresh glyph scales in (ease-out-back), and the stage
  word drifts up ~3 px. Outcome flashes are static compositions for ~2.5 s.
- **Outcomes**: Sent = full-lit green capsule + check pop ("Pasted N
  chars", no timer); Notice = drained warm capsule + slashed ring ("Heard
  nothing"). Distinguishable by luminance and glyph shape without color.
- **Reduced motion**: read once at startup from
  `gsettings get org.gnome.desktop.interface enable-animations`. When
  disabled, the pulse freezes, the transcribing spinner draws as a static
  full ring, the recording glyph is a plain static dot, and state changes
  swap instantly.
- **Screenshot hook**: `cantrip hud --screenshot <path> --state
  recording|transcribing|cleaning|sent|notice` renders any state's settled
  frame (progress 1.0, phase 0.0, no flash fade window) and exits, so every
  composition verifies offline with byte-identical output.
- **Determinate progress**: the "Warm Minimal" determinate cluster (five
  coarse segments + exact %) is designed but NOT implemented. It ships only
  when the daemon can reliably measure a fraction of a phase (candidate:
  Parakeet decoder token position vs total audio frames — requires
  `transcribe-rs` to expose a cursor; chunk counts are a coarser fallback).
  A future `progress { stage, fraction }` daemon event gates it. The
  transcribing spinner is indeterminate, not determinate progress: it
  rotates without completing, so it never claims a fraction.

## Why not alternatives

- Width-following fill / completing arcs (ADR 0006 style): fake progress;
  the daemon has no fraction to display. The transcribing spinner is
  different by construction: it rotates a fixed open arc that never
  completes, so it measures nothing — it only signals "busy".
- Determinate-only HUD without a measurement: would show nothing during
  long phases; the breathing pulse carries "busy, unmeasured" honestly.
- GTK or notify-based status (ADR 0002/0003): superseded by the layer-shell
  HUD; not revisited here.

## Consequences

- The pill reads the mode by composition and tint, never by trajectory; a
  glance-stable surface is cheaper to render (one fixed raster path, no
  width easing) than the kinetic pill.
- Reduced-motion detection depends on gsettings being present; on desktops
  without it (or non-GNOME key names) animations default to on.
- Long stage words (localized) truncate; the centered word cannot collide
  with the timer cluster.
- This ADR supersedes the visual description in ADR 0006; the layer-shell
  mechanics and supervision (ADR 0008) are unchanged.
- Open risk: the notice drain is low-luminance by design and could be
  missed if outcome timing ever shortens — the 2.5 s dwell is part of the
  contract.
