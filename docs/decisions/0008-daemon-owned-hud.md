# 0008: The Linux daemon owns HUD lifecycle

- **Status:** accepted
- **Date:** 2026-08-05
- **Primitives:** daemon, hud
- **Powder:** —

## Context

After notifications were removed, the HUD became the dictation feedback surface.
A manually launched process could be forgotten or die with the session. The
[Linux host](../../src/daemon.rs) must supervise it without requiring a separate
service manager.

## Decision

The daemon supervises the Linux HUD; a kernel-owned exclusive nonblocking flock
on the runtime hud.lock is the shared single-instance protocol. Do not replace
this with PID files, HUD-to-daemon heartbeats, or mandatory systemd activation.

## Consequences

The kernel releases ownership on process death. Manual and supervised HUDs
cooperate through the same lock; screenshot invocations skip it. Supervision
checks every five seconds and throttles spawn attempts to one per thirty
seconds. Missing Wayland access affects feedback, not dictation service.
The [native macOS host](0031-shared-engine-native-macos.md) owns its own surface.

Revisit if: the Linux HUD can no longer be supervised through the daemon's
plain process model and shared kernel lease.
