# ADR 0019: Per-take recovery and verified delivery

Date: 2026-09-06. Status: accepted.

## Problem

A single failed-audio slot can replace an older unresolved recording. A generic
success state cannot distinguish complete transcription from partial text,
clipboard handoff, uncertain keyboard input, or failed persistence. A focused
window query alone does not establish keyboard-surface focus or prove that a
session stayed unlocked during inference.

The operator needs independent recovery, truthful passive status, and deliberate
actions without introducing a second database, work ledger, or async runtime.

## Decision

### One canonical recording identity

Keep the existing owner-private transcript history as the authority. Each take
uses one immutable ID and a matching WAV sidecar in the same private directory.
Availability is derived from trusted files, not a persisted promise. Publish and
sync artifacts before removing an original; existing audio must match the exact
source before it can certify that source's durability.

Retain stopped captures before blocking inference. Failures, partial results,
processing cancellation, meaningful empty results, and unsuccessful delivery
leave independently recoverable takes. A complete durable transcript plus
successful delivery permits that take's audio to be removed. Persistence errors
are explicit and preserve runtime audio when possible; tmpfs is not durable
across reboot. Abandoning a worker must not discard a requested stopped capture.

Legacy audio and replay text import independently and idempotently. Their
originals are consumed only after durable publication. A retry does not replace
usable prior text with an incomplete attempt. Dismissal acknowledges a notice;
Copy never deletes audio; confirmed Forget removes only the selected take's audio
and incomplete text, retaining a complete archived transcript.

### Daemon-owned outcomes and cancellation

Keep std threads and mpsc with one processing worker. Operation identity, daemon
epoch, terminal outcome, notice identity, artifact availability, and capabilities
are distinct fields. Command acknowledgement means acceptance, not delivery.
Status and recording metadata remain transcript-free. Clients must not turn a
failed or stale observation into Idle/Ready or silently retarget a selected take.

Cancellation is cooperative and identity-scoped. A blocked provider call may
finish, but cancellation prevents later chunks and delivery. The final delivery
boundary is sealed against late cancellation; uncertain external effects are
reported rather than retried or described as cancelled-before-delivery.

### Verified desktop permits and bounded delivery

Snapshot the intended destination at stop and require uninterrupted compositor
and session history. Observe keyboard-interactive layers as well as windows,
logind activity/locks, native compositor lock state, suspend, and reconnection.
Reconnection creates a new epoch and never revalidates an old permit. Unknown
state fails closed.

Automatic keyboard delivery currently supports a direct Hyprland desktop with
the Lua focus/layer interfaces, verified against 0.56.2, and an authenticated
logind session. Direct PID-to-session association is preferred. For UWSM-managed
compositors outside session scope, compositor-owned metadata is only a locator;
controller ownership and its authenticated D-Bus PID must establish the exact
compositor/session relationship. Unknown formats or ambiguous relationships
defer delivery. This narrow fallback depends on desktop-private metadata and
must remain fail-closed as that metadata evolves. Nested compositors are not
sufficient evidence of parent focus/lock history.

Use native Wayland virtual-keyboard delivery, not `wtype` or `ydotool` children.
Clipboard setup, discovery, typing, and helper teardown have bounded deadlines.
Strict Type never touches the clipboard. Auto prefers paste and only changes
backend after a proven pre-handoff failure. Once keys or clipboard handoff may
have occurred, report uncertainty and never retry through another backend.
Explicit Clipboard/Copy does not require a destination permit. A compositor or
helper acknowledgement is not proof of application receipt.

### Passive status, deliberate actions

The bottom-anchored HUD remains pointer-transparent and keyboard-noninteractive.
Only measured microphone activity and chunk progress animate; stale observations
stop presenting live state. Recovery, cancellation, setup, and confirmed deletion
belong in the native `cantrip actions` window. Actions and Settings share the
installed desktop palette; Settings rejects concurrent disk edits and overlapping
reloads. Reduced motion and persistent labels are configurable.

The Omarchy badge delegates protocol framing and deadlines to the installed CLI.
Lost status means unknown state; the last pending count remains explicitly last
confirmed. The installer preserves unrelated layout/menu content and publishes
complete plugin replacements with private rollback backups. No uninstall ledger
or separate durable state is introduced.

## Compatibility and consequences

This supersedes the single-slot recovery behavior in ADR 0018, the delivery
backend/atomicity claims in ADR 0009, and the visual composition in ADR 0010.
The privacy/history principles of ADR 0013 and command/status separation of ADR
0015 remain. All clients ship with the new identity-aware protocol; there is no
parallel legacy mutation path.

Unsupported desktops must use clipboard mode or deliberate Copy/recovery rather
than automatic keyboard delivery. This sacrifices silent compatibility to avoid
sending private text into an unverified destination. Independent pending takes
consume disk until successfully resolved or explicitly forgotten; there is no
automatic expiry policy.

## Verification

Repository checks cover artifact durability/identity, legacy migration,
partial/cancelled work, private diagnostics, stale observations, concurrent
settings edits, bounded helpers, and installer preservation. Real isolated
Wayland/PipeWire smoke exercised local dictation, exact selected-recording Copy,
local recovery, partial results, blocked-request cancellation, daemon restart,
dismissal, confirmed deletion, and native HUD/Actions/Settings rendering. A
shared-memory PNG padding failure found during rendering has regression coverage.

The read-only guard refused the current native-locked desktop without keyboard
or clipboard operations. Positive automatic keyboard delivery on an unlocked
native desktop was not exercised in this shipping session; the safety checks
were not bypassed to manufacture that evidence.
