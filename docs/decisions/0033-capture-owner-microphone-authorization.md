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

The accepted 2026-11-02 amendment follows the operator's “Prompt, then record”
choice after [#208](https://github.com/misty-step/cantrip/issues/208): a ready
first dictation press requests undetermined access and continues after Allow.
It replaces the original start-never-asks rule while preserving capture-owner
attribution and disclosure before the operator chooses Allow.

## Decision

Every surface reads microphone authorization from the capture owner's status
snapshot. Explicit Allow and an admitted first dictation press invoke the same
capture-owner request function through the
[Platform port](../../crates/engine/src/ports.rs). `allow-microphone` is the
explicit Allow IPC command; a start or toggle calls the shared function rather
than sending that command. No client queries or requests in its own process,
and there is no client-side fallback: without a reachable
owner its access is unknown. The owner's request never blocks its engine thread
and reports `requesting` until the operator answers. Platforms without per-app
authorization (Linux) report none and refuse the command as
`microphone-unsupported`.

macOS attributes a request to the app responsible for the process. Cantrip.app,
started by launchd, answers for itself. An outside `cantrip daemon` answers
through the app that started it, such as Terminal: the prompt, the Privacy &
Security row and the grant belong to that app and cover everything it runs.
The owner finds that app once at start by walking its parent processes to the
first app bundle, because macOS has no public call for the responsible app,
and reports it in status. Every surface names it before an Allow it would
answer and in its not-requested, denied and restricted guidance. Such a daemon
keeps its request, because macOS offers no other way to grant a process
started from a terminal.

## Consequences

Settings, the menu and capture agree, including when the app is attached to an
external daemon, whose grant goes to the app that started it. No surface can
show or change access while no engine runs, and `doctor` then labels its own
answer as this process only.

After configuration and model admission, a macOS start or toggle with
undetermined access requests it without blocking the engine. A missing local
model fails before any prompt. The pending press reports microphone `requesting`;
the requesting HUD, menu, and accepted start reply name any launcher grantee and
show the existing shared-grant disclosure before the operator chooses Allow.
Authorized continues that same press into recording; Cancel, Stop,
or the matching toggle consumes the pending intent, so a later Allow cannot
start it. Denied and Restricted end the press with their distinct failure
classes and the named app's System Settings › Privacy & Security › Microphone
guidance. The engine's `microphone_transition` table owns these edges; Recorder
start retains its own authorization check.

Status, polling, `doctor`, and opening Settings never request access. The
explicit Allow controls remain available without starting a recording.
Platforms without per-app authorization keep their existing start behavior.
Owners that predate the status field omit it and read as unreported.

Revisit if: macOS shares microphone authorization across a bundle's processes,
or Cantrip records in more than one process.
