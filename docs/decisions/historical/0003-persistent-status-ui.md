# ADR 0003: Persistent status indicator via long-lived notification

Date: 2026-08-03. Status: accepted.

## Problem

`notify()` fires fire-and-forget banners. The "Listening…" banner expires
after a few seconds, so during a long dictation nothing on screen says
cantrip is still recording. Users lose track of daemon state — the exact
failure dictation tools cannot afford.

## Decision

The daemon owns one long-lived notification handle (`StatusUi` in
`daemon.rs`) for the whole Recording → Processing lifetime:

- Recording start: show "Listening…" with `Timeout::Never` and keep the
  `NotificationHandle`.
- While recording: once per second, update the body in place with the
  elapsed time (`replaces_id` update — same banner, no new pop-ups). The
  serve loop already ticks at 50 ms, so this costs one DBus call per second.
- Stop: update the same banner to "Transcribing…".
- Terminal events (injected, cancelled, error, heard nothing): close the
  persistent banner, then show a normal transient result notification.
- Daemon shutdown closes the banner (Drop), so no zombie "Listening…"
  outlives the daemon.

`notify()` is replaced by `StatusUi`; one notification path only.

## Why not a layer-shell HUD now

ADR 0002 stands: GTK/layer-shell dev packages are not installable on the
target machine this milestone. Notifications ride the existing notify-rust
DBus dependency, work on COSMIC/GNOME/KDE/mako unchanged, and the HUD
slots in later as a socket client without daemon changes. Cost: servers
render persistent notifications differently (GNOME keeps a tray entry,
mako keeps the banner). Acceptable; the state is visible either way.
