# 0020: Retain every stopped take; only confirmed Forget deletes audio

- **Status:** accepted
- **Date:** 2026-09-07
- **Primitives:** history, stt, capture
- **Powder:** —

## Context

Cloud failure left locally recoverable speech waiting for manual action, while
cancellation and empty recognition had implicit deletion paths. The accepted
2026-10-12 amendment replaces the original whole-take-only fallback with the
[current bounded suffix contract](../CONFIGURATION.md#stt--transcription).

## Decision

Retain every stopped take in the existing private archive before inference;
only explicit confirmed Forget deletes retained audio. Keep configured cloud
STT preferred, but on failure complete using installed default local Parakeet,
without implicit model downloads, cloud-provider switching, or a retry ledger.

## Consequences

Cloud chunks share five seconds. Preserve completed native chunks and recognize
only untouched source frames locally; without a native suffix, attempt the
whole take once. Never concatenate overlapping alternate passes. Archive actual
local/cloud/mixed provenance. Cancellation stops later work, not retention.
Success, partial/empty recognition, cancellation, and graceful shutdown retain
independent takes. Trusted finalized runtime leftovers import idempotently;
consume originals only after matching durability, never automatically replay.
Forget removes selected audio and incomplete text; complete text stays.
Runtime-only audio is not guaranteed to survive reboot.

Revisit if: the operator explicitly changes retention/deletion policy or a new
fallback design can preserve exact nonoverlapping source coverage and provenance.
