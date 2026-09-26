# ADR 0029: A quiet Cantrip mark in the Omarchy bar, without a backlog count

Date: 2026-09-26. Status: accepted. Story: US-012.

## Context

The Omarchy bar item was a microphone glyph with a superscript count of
unresolved saved recordings. That count only fell when each take was recovered
or its audio forgotten; it had no timeout or dismissal. On the operator's
desktop it read 13, twelve of them more than a day old. The operator rarely
revisits those takes and did not want the number. The glyph was also generic.

A design round compared three directions on Omarchy's real widget kit, across
installed themes: a quiet mark in one fixed slot; a mark with words and elapsed
time (like the neighbouring Steno item); and a mark shown only while needed.
The operator chose the quiet mark, drawn as the solid pixel C.

## Decision

- The item is one fixed `Style.bar.iconSlot` holding Cantrip's pixel C (the
  favicon's eight cells, drawn touching so the stroke matches neighbouring
  Nerd Font glyphs). It never shows a count or text and never changes width.
- Rest: bar foreground at 55% opacity. Unknown status: 30%.
- Recording and processing: the route color from ADR 0028, the theme accent or
  the handoff target's color. Processing moves three lit cells around the C
  every 140 ms unless the shell disables foreground animation.
- Attention: the theme's urgent color while the status snapshot's `attention`
  flag is set. The daemon owns that rule (`TerminalOutcome::needs_attention`
  on the current outcome), so the widget never re-derives it. It clears when
  the next take starts (the daemon replaces the outcome) or when middle-click
  runs `cantrip dismiss`. Dismissal never deletes recordings.
- The tooltip carries state, target and the latest outcome. Unresolved takes
  remain listed in Actions (right-click); the bar no longer counts them.

## Consequences

The backlog of unresolved takes is no longer visible from the bar; it is only
discoverable in Actions and `cantrip status`. On monochrome themes the default
flow's recording color differs from rest mainly by brightness. The status
snapshot keeps `pending_recordings` for other clients.
