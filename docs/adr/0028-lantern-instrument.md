# ADR 0028: Lantern — the instrument's material, light and windows

Date: 2026-09-25. Status: accepted. Refines the presentation of
[ADR 0023](0023-persistent-pixel-field.md) and the windows of
[ADR 0007](0007-settings-gui-eframe.md) and
[ADR 0019](0019-per-take-recovery-and-verified-delivery.md). The field
geometry, light rules, timing and honesty contracts of
[ADR 0021](0021-signed-pixel-waveform.md) and ADR 0023 are unchanged.

## Problem

The field was right; everything around it read as a developer tool. The HUD
sat in a flat rectangle with a full-contrast border, so on light themes it was
a white form field and on busy wallpaper it had no edge. Attention states
looked like log lines, and success had no arrival. The Actions and Settings
windows were monospace stacks with one visual weight: disabled controls in
idle, semicolon-joined take rows, placeholders that looked like configured
values, and Save at the end of a long scroll.

## Decision

Treat Cantrip as one instrument. `docs/DESIGN.md` is the spec; the decisions
future work must not quietly undo are:

- **Housing light is derived, never animated.** The HUD housing is a rounded,
  shadowed glass whose light is the presented cells' own light — per column,
  spread by a Gaussian, zero at the rest floor. It follows every existing
  transition and reduced-motion rule for free and can never show activity the
  cells do not. Each cell is an LED with a flat body in the surface colour, so
  housing light never masquerades as cell light.
- **Presentation shares the field's clock; words do not wait.** Colour and rim
  crossfade over the field's own settle (400 ms, or the 280 ms listening
  onset); success settles centre-out inside that settle (never left to right,
  which would read as progress); the capsule fades in over 120 ms only when it
  first appears, never when work resets while it is visible. Captions change
  at once, so an exception's guidance is readable at its first frame. Reduced
  motion is instant. Lost status freezes presentation exactly as it freezes
  cells.
- **HUD typography is preserved.** Captions stay in Hack (ADR 0021); they gain
  hierarchy through colour and centred lines. Action lines name the button
  they point to ("Copy transcript in Cantrip Actions"), one name per action.
- **Windows share one material** built only from palette mixes (`Tones`), with
  egui's light visuals for light themes. Geist and Geist Mono (SIL OFL 1.1)
  are bundled; release archives carry `FONTS-LICENSE.txt`, leaving `LICENSE`
  a pure MIT text.
- **The mark is the matrix.** The wordmark is drawn with the HUD's LED cells,
  designed 16-first with a compact optical variant.
- **Actions leads with status and only offers what applies**; takes are rows
  with cell stamps and relative days; Forget lives in a danger zone behind a
  confirmation that defaults to keeping the recording. It stays
  metadata-only.
- **Settings makes modes explicit** (on this computer or cloud provider,
  delivery mode, motion) and keeps a pinned footer that says whether changes
  are unsaved, saved, or failed. Writes still preserve comments, validate
  first and refuse concurrent edits.
- **Every state is exportable.** `cantrip hud-gallery --export DIR` writes
  each catalogue still (1×/2×, labels on/off) and journey through the
  production replay, model and painter, with no window or compositor, so a
  review never has to map a layer on a live desktop (a mapped layer would
  invalidate an in-flight delivery permit).

## Alternatives

Six concepts were explored and critiqued against the jobs; lineage is kept
with the review evidence. A command-palette window merging Actions and
Settings was rejected: speed would hide the consequences of clipboard
replacement and deletion. A live HUD mirror inside Actions was rejected:
Actions polls every 2 s, so it would present stale activity as live. A
"spellbook" brand overriding the desktop palette and adding success sparkles
was rejected against the palette and no-success-icon contracts. A settings
sidebar was rejected as navigation overhead for about fifteen fields.

## Consequences

The painter does more per pixel but is faster than before at 2× (about
0.7 ms against 1.1 ms per listening frame) because the housing interior takes
a fast path and opaque writes skip blending. The pixel test for "captions
never touch the field band" compares the field span rather than full rows,
because a taller housing legitimately rounds its corners lower. The binary
grows by the bundled fonts (about 530 KB).
