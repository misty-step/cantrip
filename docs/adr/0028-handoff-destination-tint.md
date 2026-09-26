# ADR 0028: Handoff takes are tinted and labelled; the default flow is not

Date: 2026-09-25. Status: accepted. Story: US-011.

## Context

The desktop shortcut (typed or pasted at the cursor) and a handoff shortcut
(`toggle --handoff kaylee`) produced identical HUD, bar and actions-window
states while recording and processing. The operator could not tell at a glance
where a take's words would go until a handoff succeeded.

A design round explored route treatments for both flows: a tint-only variant, a
destination plate with pixel icons, and a silhouette/caret variant. The operator
declined all changes to the default flow: it must stay exactly as it was, and a
non-default target needs only its own color plus a small label.

## Decision

- The daemon fixes a take's `Handoff {name, label, color}` when capture starts
  and publishes it on `StatusSnapshot.handoff` while the take is active and on
  every `TerminalOutcome.handoff` from that take. Default, recovery, replay and
  forget operations carry none. The color is a `#rrggbb` string on the wire.
- Colors come from the active Omarchy `colors.toml`. Targets, in config name
  order, each take the theme's `magenta`, `blue`, `green` or `cyan` farthest in
  hue from `accent` (the default flow), `yellow` (attention) and earlier targets,
  while one stays 30° clear; after that, a 15° hue ring at those colors' average
  saturation and lightness. Missing optional hues fall back only for tints; they
  never replace the theme's base palette.
- The HUD adds a 15 px upper-left "to LABEL" row and paints the target color on
  the border, a 12% surface wash and the active field. Failures keep the yellow
  rail and field; cancellation keeps the neutral field. The route follows the
  active take, else the presented outcome, so a persistent handoff failure never
  tints the next default take.
- While a handoff take is active, the Omarchy bar keeps its glyph and uses the
  target color as its active color, and its tooltip says "Recording to LABEL";
  the actions window appends "· to LABEL" in that color; `cantrip status` prints
  `handoff: LABEL (NAME)`. For that take's outcome, the tooltip adds "(to LABEL)",
  the actions window shows the outcome message with "· to LABEL" in the target
  color, and `cantrip status` prints `last-handoff: LABEL (NAME)`. The idle bar
  glyph stays uncolored, as it is for default outcomes.
- The route is reconciled on every HUD refresh, so an expiring handoff result
  cannot leave its tint on later idle feedback. Monochrome themes (gray named
  colors) get distinct chromatic target hues at the theme's lightness.

## Consequences

Default takes are pixel-identical to the previous HUD. A theme change applies
from the next handoff take. No icons, shape changes, motion or width changes
indicate destination. The design gallery (before/after of every state) lives
outside the repository with the operator's design artifacts.
