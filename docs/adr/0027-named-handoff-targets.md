# ADR 0027: Named local handoff targets

Date: 2026-09-24. Status: accepted. Story: US-011.

## Context

Desktop keyboard and clipboard injection are inappropriate when a transcript is
intended for a local agent. Focus verification is meaningful for desktop input,
not for a configured process receiving bytes on standard input.

## Decision

An explicit `start --handoff NAME` or `toggle --handoff NAME` selects a validated
`[handoff.NAME]` configuration entry before capture. The daemon snapshots the
name and argv/timeout into the recording's config; reload cannot retarget an
in-flight take. After the existing STT and optional cleanup pipeline, only a
complete nonempty transcript is sent to a directly spawned executable on stdin,
with `CANTRIP_TAKE_ID` in its environment. The process is bounded to 1–120 seconds
(default 15); stdout/stderr are discarded. Exit zero records `handed-off`, and
failure or timeout records a recoverable failed delivery. A handoff never falls
back to clipboard or keyboard and never retries automatically. Cancellation
before spawn prevents the command from running.

## Consequences

Stopped audio and text retain their existing per-take history and recovery
semantics. The local executable is explicitly trusted with the transcript; the
operator controls its arguments and should use a private, executable file.
Operational logs and telemetry never contain transcript bytes or child output.
No second ledger or async runtime is introduced; ordinary desktop delivery and
`last`/`recover` remain unchanged.
