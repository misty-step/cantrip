# ADR 0008: The daemon owns the HUD lifecycle

Date: 2026-08-05. Status: accepted.

## Problem

The HUD pill (ADR 0006) is the only feedback surface for dictation
since the native notifications were removed (commit `d286298`). The
pill must therefore always be running, but a manually started HUD dies
with the session, crashes, or is simply forgotten. Requiring the user
to keep it alive is not acceptable for a dictation tool. The daemon
must guarantee the pill is up.

## Decision

The daemon supervises the HUD and the HUD self-limits to one instance,
using a single `flock` (exclusive, non-blocking) on
`$XDG_RUNTIME_DIR/cantrip/hud.lock` as the shared protocol:

- The HUD opens the lock file and takes `LOCK_EX | LOCK_NB` at startup.
  On contention it logs one line and exits 0. It holds the file for its
  whole lifetime; the kernel releases the lock when the process exits,
  however it died — no stale-lock cleanup is ever needed.
- A daemon supervisor thread checks the lock every 5 s. If the lock is
  free (no HUD alive), it releases it and spawns a detached `cantrip hud`
  child (its own session via `setsid`, output to `runtime/hud.log`).
  The child takes the lock itself; a race at worst produces a second
  HUD process that immediately exits.
- Spawn attempts are throttled to one per 30 s. A HUD that cannot start
  (e.g. no Wayland in a headless session) therefore logs at most one
  warning per 30 s instead of respawn-looping.
- `hud --screenshot` is a test hook and deliberately skips the lock.

The daemon already validates the configuration at startup
(`Config::load` runs `validate()` and refuses to start with a clear
error), which completes the "everything is valid and quietly managed"
requirement: one command (`cantrip daemon`) brings up the validated
service and its always-on status surface, and the pill heals itself
after a crash within one supervision interval.

## Why flock and not alternatives

- **PID file + `kill(pid, 0)`**: stale-pid races, PID reuse, and a
  check that can only probe "some process lives", not "the HUD lives".
- **The HUD polling the daemon / heartbeat**: only proves the daemon is
  up, not that a HUD is; the daemon cannot detect a missing HUD.
- **systemd user units / D-Bus activation**: no rootless systemd
  guarantee on every target (this project installs without sudo), and
  the supervisor must work with the plain `cantrip daemon` process
  model.

`flock` is a kernel-owned lease on an open file description: exactly
one holder at a time, released atomically on process death, and the
whole protocol is two syscalls in the stdlib-free `libc` crate already
in the dependency tree.

## Consequences

- The user runs `cantrip daemon` and nothing else; the pill appears,
  survives daemon restarts, and respawns if killed.
- A user who wants a custom HUD invocation still can: starting one by
  hand is exactly equivalent to the daemon's spawn; both cooperate
  through the lock instead of fighting.
- The daemon's child inherits the daemon's environment, so the HUD
  only works when the daemon itself has Wayland access. A headless
  daemon logs the HUD failure and retries at the 30 s cadence — the
  dictation service itself is unaffected (per ADR 0006, HUD failures
  are deliberately non-fatal).
- This supersedes the tail of ADR 0006 ("the ADR 0003 staged banner
  stays as-is"): notifications are deleted and the supervised pill is
  the sole status surface.
