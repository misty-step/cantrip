---
version: 1
name: Cantrip Lantern
colors:
  # Roles come from the active Omarchy theme (src/theme.rs). Values here are the
  # Tokyo Night fallback; every other tone is derived by mixing roles.
  background: "#13141c"
  surface: "#1a1b26"
  border: "#414868"
  foreground: "#a9b1d6"
  accent: "#7aa2f7"
  attention: "#e0af68"
  derived:
    hairline: "mix({colors.surface}, {colors.border}, 0.35)"
    raised: "mix({colors.surface}, {colors.foreground}, 0.05)"
    well: "mix({colors.surface}, {colors.background}, 0.6)"
    text-muted: "mix({colors.foreground}, {colors.surface}, 0.32)"
    text-faint: "mix({colors.foreground}, {colors.surface}, 0.52)"
    accent-soft: "mix({colors.surface}, {colors.accent}, 0.2)"
    attention-soft: "mix({colors.surface}, {colors.attention}, 0.14)"
typography:
  wordmark:
    family: LED matrix (5x9 cells, src/ui.rs)
    size: 9 cells tall; 2 px cells, 1 px gaps at 1x
  hero:
    family: Geist SemiBold
    size: 19px
  heading:
    family: Geist SemiBold
    size: 15px
  body:
    family: Geist
    size: 13.5px
  small:
    family: Geist
    size: 12px
  data:
    family: Geist Mono
    size: 12.5px
  hud-title:
    family: Hack
    size: 12px
  hud-detail:
    family: Hack
    size: 11px
  hud-action:
    family: Hack
    size: 10.5px
spacing:
  scale: [4, 8, 12, 16, 20, 24, 32]
radii:
  hud: 10px
  card: 10px
  control: 7px
  cell: 0px
---

# Cantrip Lantern

## Overview

Cantrip is an instrument, not an app you visit. Its passive HUD is a 60×7
grid of square LEDs behind glass; the deliberate windows (Actions, Settings,
the HUD gallery) are quiet panels built from the same material. The design
serves three jobs: glanceable trust while speaking (HUD), deliberate and
explained recovery (Actions), and a configuration whose saved state is never
in doubt (Settings). Lineage, rejected concepts and the decision record live
in [ADR 0028](adr/0028-lantern-instrument.md).

Scope: HUD painter and captions, Actions, Settings, HUD gallery, shared
tokens and type. Not changed: the Omarchy bar badge, CLI text, the website.

## Colors

Six roles are read from the desktop theme (`background`, `surface`, `border`
← `muted`, `foreground`, `accent`, `attention` ← `yellow`); Tokyo Night is the
fallback. There is no theme engine: every other tone is a mix of two roles,
so any Omarchy theme, dark or light, stays coherent. A theme is treated as
light when its background luminance exceeds 0.5 and egui starts from its light
visuals.

- Accent means live or done: listening light, measured progress, the settled
  success grid, primary buttons, selection.
- Attention means "needs you": exception rims, attention rows, destructive
  actions. It never marks success.
- Foreground at the rest floor means neutral or unknown.
- No success green, no status reds: the HUD's success colour is the accent.

## Typography

The deliberate windows use the Geist family (Vercel, SIL OFL 1.1), bundled
in the binary from `assets/fonts/`, so rendering never depends on installed
fonts. Release archives carry its licence as `FONTS-LICENSE.txt`.

- **Geist** (Regular, Medium, SemiBold) — every sentence, label and button.
- **Geist Mono** — data whose columns should align: capture times, durations,
  recording IDs, paths, key IDs, endpoints, diagnosis output.
- **The HUD keeps Hack** (ADR 0021 preserves HUD typography). Captions gain
  hierarchy through colour and spacing only: title in foreground, detail in
  `text-muted`, action lines in attention or `text-muted`, each line centred
  under the field.

Sentence case everywhere. No all-caps labels, no tracked eyebrows.

## Mark

The wordmark is not a typeface: it is lit on the HUD's own LED matrix
(`ui::wordmark`). Lowercase 5×9 glyphs — x-height 5 cells, two-row ascenders
and descenders — sit in one continuous panel whose unlit cells show at 10 %
ink and whose lit cells take the accent. Words in use: "cantrip",
"settings", "hud gallery".

- Designed 16-first: the compact optical variant (1 px cells, no gaps, no
  rest matrix, 9 px tall) reads "cantrip" inside a 16 px line on dark
  (operator and Tokyo Night), light, and light/dark browser-tab backgrounds;
  proof sheet `design/mark/mark-proof-zoom.png` in the review run.
- The display variant (2 px cells and 1 px gaps at 1×, whole-pixel cells at
  every scale) is the window header. `ui::wordmark` falls back to the compact
  variant whenever the display variant does not fit the available width.
- The mark is always the accent on the theme's own ground; never outlined,
  shadowed, gradient-filled or recoloured to a success colour.

## Layout

- Spacing uses the 4 px scale; windows pad 20, cards pad 14×16, rows 8 apart.
- Windows centre a readable column (Actions ≤ 720, Settings ≤ 640) and
  reflow to any tiled width down to 420 logical px.
- The HUD keeps its footprint: a 420-wide surface, a 336-wide housing, 44 px
  field band, bottom-anchored 36 px above the output edge.

## Elevation & Depth

- HUD: a soft black shadow uses the 6 px transparent margin (contact alpha
  0.3, one pixel lower). The housing is a vertical gradient over the field
  band only, a 1 px rim, and a 1 px top highlight. Captions never change the
  pixels of the field band.
- Housing light: the housing is lit by the field's own presented cells —
  elliptical falloff centred on the field, alpha `0.22 × lit`, where `lit` is
  mean cell light normalised above the rest floor. Rest light is exactly zero.
  It is derived, never animated on its own, so it cannot invent activity.
- Each cell is an LED: its body is the flat surface colour, lit by the accent
  at the cell's light. The housing light never tints a cell.
- Windows: cards on `surface` over `background`, 1 px hairline, radius 10.
  Dialogs float over a 45 % background scrim with a soft shadow.

## Shapes

HUD housing radius 10; cards 10; buttons, inputs and segmented controls 7;
chips are pills; cells and stamps are square — the pixel is the identity.

## Components

**HUD field** — unchanged geometry and light rules (ADR 0021, ADR 0023).
States: rest, listening, working packet, measured front with frontier,
cleanup, settled success, attention row, neutral row.

**HUD housing** — default rim `mix(surface, border, 0.32)`; attention rim
`mix(surface, attention, 0.75)`; neutral and resolved use the default rim.

**HUD caption** — title, detail and action stack centred below the field.
Title in foreground; detail in `text-muted`; action lines in attention when
the outcome needs the operator, otherwise in `text-muted`. Busy notices sit
under a short centred hairline.

**Take stamp** — a 12×3 cell glyph drawn with the HUD's 3/5 px geometry:
lit full grid = complete text; left half lit = partial text; attention centre
row = waiting with no transcript; rest floor = nothing saved. Colour follows
the same roles as the HUD.

**Status hero** (Actions) — stamp, state title, one sentence, and only the
controls that apply now. Idle never shows disabled Stop/Cancel.

**Take row**: stamp, capture time with its relative day ("Today 10:42:07",
"Yesterday 17:31:12", or the date; mono), facts sentence, duration (mono,
right). Selected: `accent-soft` fill with an accent edge.

**Segmented control** — Waiting/All in Actions; On this computer/Cloud
provider, Delivery modes and Motion in Settings. Selected segment:
`accent-soft` fill, foreground text.

**Buttons** — primary (accent fill, text on accent), secondary (raised fill,
hairline), quiet (text only), destructive (attention text and hairline;
attention fill only inside the Forget confirmation). Disabled buttons are
hidden when the action cannot apply to the current state, and dimmed only
while an action lane is busy.

**Save footer** (Settings) — pinned to the window bottom: state text on the
left ("Unsaved changes", "Saved and applied", errors), Revert and Save on the
right. Save is enabled only with unsaved, loadable changes.

**Forget confirmation** — modal card; names the take, lists what is deleted
and what is kept, focuses "Keep recording"; the destructive button reads
"Forget recording".

## Motion

| Moment | Duration / easing | Purpose | Reduced motion |
| --- | --- | --- | --- |
| HUD appears | 120 ms ease-out alpha | soften the pop without delaying feedback | instant |
| State colour and rim change | the field's own settle (400 ms; 280 ms onset while listening), smoothstep | one continuous instrument | instant |
| Caption change | immediate | words are feedback; exception guidance is legible at its first frame | immediate |
| Success settle | same 400 ms; columns start up to 45 % later by distance from centre | arrival radiates from the middle, never left-to-right | instant full grid |
| Housing light | follows presented cells | light, not decoration | follows static frames |
| Result fade | 140 ms (unchanged) | leave quietly | cut |
| Window widgets | 80 ms egui hover/toggle | feedback | — |

Existing HUD timing contracts (listening attack/release, 600 ms measured
reveal, 180 ms dwell, 1200 ms success hold, 4 s notices, presentation lag)
are unchanged.

## Copy

Plain verbs, sentence case, one name per action through the whole flow: the
HUD says "Copy transcript in Cantrip Actions" and the button says "Copy
transcript". Errors state what happened and what is kept. Examples:

- Empty history: "No saved recordings yet. Every stopped take appears here."
- Nothing waiting: "Nothing is waiting. Choose All to browse saved takes."
- Unreachable: "Cantrip isn't running. Saved recordings below are read from
  disk; actions need Cantrip running."
- Placeholders describe the empty meaning ("Default microphone"), never a
  plausible value.

## Accessibility

- Text contrast targets 4.5:1 for body and 3:1 for faint hints on every
  bundled palette mix; exceptions are labelled in words, not colour alone.
- HUD states stay distinguishable by light distribution without colour.
- Keyboard: Tab order follows reading order; arrows, Page Up/Down, Home/End
  move through takes; Enter or Space activates; Escape closes a dialog, then
  the window. Focus rings use the accent at 2 px.
- `hud.labels = true` keeps continuous words on the HUD; reduced motion is
  honoured as tabled above.

## Implementation map

- `src/fonts.rs` — bundled Geist bytes and egui font families (windows only).
- `src/theme.rs` — palette roles plus `mix`, `Tones` and `is_light`.
- `src/ui.rs` — shared egui style, buttons, segmented control, cards, stamp.
- `src/hud.rs` — `Canvas::paint_hud` housing, light, LED bodies, caption
  layout; `Model` colour crossfade, appear fade, centre-out settle.
- `src/actions.rs`, `src/settings.rs`, `src/hud/gallery.rs` — surfaces.

## Validation

`cantrip hud-gallery --export DIR` writes every catalogue state (1×/2×,
labels on/off) and every journey. Window states are captured under Xvfb with
an isolated XDG profile and a fixture status socket. Each named state is
reviewed before and after in dark, fallback and light palettes.
