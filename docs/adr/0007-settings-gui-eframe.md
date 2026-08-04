# ADR 0007: Configuration via an egui settings window

Date: 2026-08-04. Status: accepted.

## Problem

Editing cantrip means hand-editing `~/.config/cantrip/config.toml` in a
text editor, then restarting the daemon. Users want a window they can
keep open to view and adjust the current configuration: transcription
model, cloud endpoints, post-processing model and instructions, and
injection mode, with a single click to apply and reload the daemon.

## Decision

Add a `cantrip settings` subcommand that opens a native window built on
`eframe` (egui, `glow` renderer) via `winit`/Wayland. The window reads
the real config (`Config::load`), shows the editable fields in grouped
sections, and offers "Reload from disk" and "Save & reload daemon".

The config window is a stateful editor, not a raw TOML editor. It
presents a curated set of the common knobs (injection, keep-warm,
vocabulary, STT model/endpoint/key, postproc enabled/endpoint/model/key/
timeout/instructions) and validates before writing, so the daemon never
receives a config that `Config::validate()` would reject at load.

### Comment-preserving writes

Saving uses `toml_edit` on the existing file instead of a
`toml::to_string_pretty` round-trip. Users keep heavily annotated config
templates; a naive rewrite would destroy every `#` comment and reorder
their file. `toml_edit` preserves comments and ordering for keys the
window did not touch, and only writes the values actually edited.
Optional keys the user clears get removed, not written as empty strings.

### Why egui instead of hand-rolled smithay

The HUD (ADR 0006) renders on `smithay-client-toolkit` + `ab_glyph`,
which is right for a read-only, always-on-top chip. A *mutable* config
form needs editable text fields, a combo box, checkboxes, a multi-line
text area, buttons, and scrolling. Reimplementing those widgets on raw
Wayland would be a mini widget toolkit with high bug surface for no user
benefit. egui is pure Rust (no GTK, no C headers, no sudo — satisfying
ADR 0002), and is the standard, boring choice for this class of Rust
tool window. The HUD stays on the layer-shell stack; it is a separate,
independently-motivated surface.

### Verification without a screen-capture tool

This machine has no screenshot utility, so the window cannot be
inspected by shelling out to `grim`/`wayshot`. `cantrip settings
--screenshot <path>` renders the window and ejects one frame to a PNG
(via a tiny encoder that reuses `flate2`, already a dependency) so the
UI can be checked visually during development and regression testing.
The daemon badge in the header polls `cantrip status` once per second,
so the window also shows live daemon state.

## Accepted tradeoffs

- A large new dependency tree (`eframe`, `winit`, `glow`/`glutin`) for
  one window. Justified by the widget-rewrite cost it avoids; it is a
  standard, ubiquitous set.
- GL via Wayland is required; the HUD's CPU-rendered path is untouched.
- A failing post-processing or transcription endpoint is never a reason
  to drop a dictation, and neither is this window: the daemon is a
  separate process and ignores the UI entirely.
