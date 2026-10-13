# 0030: Local-only policy belongs to the handoff target

- **Status:** accepted
- **Date:** 2026-09-28
- **Primitives:** handoff, stt
- **Powder:** —

## Context

The operator's private pile requires on-device speech recognition while ordinary
desktop takes may use configured cloud STT and cleanup. A per-invocation flag
would protect privacy only while every caller remembered it; the destination
owns this rule.

## Decision

Snapshot local_only on the named target: use installed default local STT and
disable cleanup before capture, with no cloud attempt or fallback. Refuse a
missing local model and explicit --postproc clean before recording rather than
silently dropping the requested option.

## Consequences

Local-only commands receive CANTRIP_LOCAL_ONLY=1; other commands have it removed.
Handoff commands also receive CANTRIP_TAKE_AUDIO when retained audio is available.
The pile keeps its own audio copy and transcribes in a networkless sandbox;
Cantrip's text drives its own history/HUD, not the pile's transcript. Other
routes keep configured lanes. Recovery remains caller-selected, so use --local
for these takes. See [configuration](../CONFIGURATION.md).

Revisit if: destination policy requires isolation beyond the target's lane
snapshot and the receiver's own networkless transcription boundary.
