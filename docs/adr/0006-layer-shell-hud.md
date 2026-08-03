# ADR 0006: Status via a layer-shell HUD overlay

Date: 2026-08-03. Status: accepted.

## Problem

Dictation needs an always-visible presence: the user must see, at a
glance and without searching the screen, that cantrip is recording, is
transcribing, is cleaning up the text, and that the text arrived. A
notification banner can be missed, dismissed, or scrolled away. ADR 0003
started with a persistent notification; the operator chose a real
overlay as the primary surface.

## Decision

New `cantrip hud` client renders a small always-on-top chip, anchored
top-center, using the `zwlr_layer_shell_v1` Wayland protocol through
pure-Rust clients (`wayland-client`, `smithay-client-toolkit` with
`sctk::shm::slot::SlotPool` over `wl_shm`, and `ab_glyph` text). No GTK,
no C headers, no sudo — buildable on this machine, where ADR 0002 said
the GTK dev packages are unavailable. COSMIC (this desktop) implements
`zwlr_layer_shell_v1`; GNOME does not, so the HUD falls back to nothing
there while the ADR 0003 notification remains the portable status line.

The HUD is a state mirror, never a state owner. It polls
`cantrip status` over the daemon socket (~5/s) and renders whatever the
daemon reports:

```
● REC 00:12   Listening…          red pulsing dot while recording
    ⟳        Transcribing…        spinner while STT runs
    ⟳        Cleaning up…         spinner while postproc runs
✓           Typed 108 chars       daemon's terminal message, then auto-hide
```

The result flash is not a fake success. The `status` reply also carries
`last`, the daemon's terminal message (`Typed N chars`, `Heard nothing`,
`Cancelled`, ...), cleared when a new recording starts; the HUD shows
that text for ~2.5 s and then hides.

## IPC additions

The `status` reply gains optional `elapsed` (recording seconds),
`stage` (`transcribing` | `cleaning`), `last` (most recent terminal
message), and `last_ok` (whether that message was a delivered dictation)
fields, all `#[serde(default)]` so old clients still parse.
Processing stage is not inferred: the transcription worker pushes
`Stage::CleaningUp` over a small stage channel the moment post-processing
starts, so a multi-second cleanup is visible, not just the trailing toast.
The daemon drains stage events before results and applies only the most
recent, so a fast job cannot relabel the next one.

## Behavior

- Auto-hides after ~3 s of idle; reappears on the next recording.
- Polling keeps the HUD trivially crash-safe: the daemon never acts on
  HUD state; a dead or restarted HUD changes nothing.
- The daemon service keeps running without Wayland (it runs in a herdr
  pane with no display) — the HUD is a separate invocation from the
  user's session.

## Why not keep upgrading notifications

Notifications are desktop-server-dependent for lifetime and styling;
a HUD owns its whole aesthetic and is genuinely unmissable. The ADR 0003
staged banner stays as-is for non-layer-shell fallback and the result
summary; no further notification work is planned.
