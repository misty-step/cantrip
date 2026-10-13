# 0027: Named local handoffs never fall back to desktop injection

- **Status:** accepted
- **Date:** 2026-09-24
- **Primitives:** handoff, delivery
- **Powder:** —

## Context

Desktop focus verification does not describe a configured local process receiving
transcript bytes on stdin. Sending agent-directed text through keyboard or
clipboard injection is inappropriate; [configuration](../CONFIGURATION.md)
already names the trusted destination.

## Decision

Explicit start/toggle --handoff NAME selects a validated local target before
capture and snapshots its name, argv, and timeout. After existing STT/cleanup,
send only complete nonempty text to a directly spawned executable on stdin;
never fall back to clipboard/keyboard or retry automatically.

## Consequences

Reload cannot retarget an in-flight take. The process receives CANTRIP_TAKE_ID,
is bounded to 1–120 seconds (default 15), and has discarded stdout/stderr.
Exit zero records handed-off; failure/timeout remains recoverable. Cancellation
before spawn prevents execution. The operator explicitly trusts the executable
with text. Existing per-take history, content-free logging, std threads, and
single-ledger ownership remain.

Revisit if: a destination needs a different explicit receipt/cancellation
contract that cannot be supplied by bounded local stdin handoff.
