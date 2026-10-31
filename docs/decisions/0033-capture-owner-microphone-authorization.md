# 0033: Microphone authorization belongs to the capture owner

- **Status:** accepted
- **Date:** 2026-10-30
- **Primitives:** engine, platform, capture, ipc
- **Powder:** —

## Context

macOS answers microphone authorization per process and caches the answer there:
an already-running Cantrip process does not see a grant made in another one until
it requests access itself or restarts. Settings, the menu-bar app and `doctor`
each queried and requested in their own process. In a 2026-10-30 VM run, Settings
showed Allowed while the app that records still reported not determined and
refused capture; granting from the menu left Settings behind instead
([#198](https://github.com/misty-step/cantrip/issues/198)). Only the process that
runs the engine's Recorder — Cantrip.app when it owns the engine, or an external
`cantrip daemon` — has an answer that matters.

## Decision

Every surface reads microphone authorization from the capture owner's status
snapshot and asks for it with the owner's `allow-microphone` command, through the
[Platform port](../../crates/engine/src/ports.rs). No client queries or requests
in its own process, and there is no client-side fallback: without a reachable
owner its access is unknown. The owner's request never blocks its engine thread
and reports `requesting` until the operator answers. Platforms without per-app
authorization (Linux) report none and refuse the command as
`microphone-unsupported`.

## Consequences

Settings, the menu and capture agree, including when the app is attached to an
external daemon; `cantrip microphone allow` covers terminal setup. No surface can
show or change access while no engine runs, and `doctor` then labels its own
answer as this process only. The request stays an explicit operator action:
start, status and polling never ask, and Recorder start keeps its own check.
Owners that predate the field omit it and read as unreported.

Revisit if: macOS shares microphone authorization across a bundle's processes,
or Cantrip records in more than one process.
