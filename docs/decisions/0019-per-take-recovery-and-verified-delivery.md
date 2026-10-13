# 0019: One per-take archive and verified delivery, no uncertain retries

- **Status:** accepted
- **Date:** 2026-09-06
- **Primitives:** history, delivery, hud
- **Powder:** —

## Context

A single failed-audio slot could overwrite another unresolved recording. Focus
snapshots could not prove uninterrupted destination/session history, and generic
success conflated recognition, persistence, and external receipt. The existing
[history store](../../crates/engine/src/archive.rs) must remain the authority.

## Decision

Use one immutable take ID and the owner-private transcript archive for independent
recovery, with no second database, durable ledger, or async runtime. Automatic
keyboard delivery requires uninterrupted verified destination/session history;
unknown history defers, and potentially completed handoffs are uncertain and
never retried through another backend.

## Consequences

Publish matching durable artifacts before consuming originals; report storage
failures and preserve runtime audio when durability is uncertain. Cancellation
is identity-scoped and cooperative. Strict Type never touches the clipboard;
explicit Copy is separate from automatic delivery. Helper acknowledgement does
not prove application receipt. The passive HUD mirrors daemon outcomes, uses
measured progress only, and leaves recovery/deletion to deliberate actions.
[0020](0020-local-completion-and-explicit-audio-deletion.md) supersedes deletion
after successful delivery; [historical 0021](historical/0021-signed-pixel-waveform.md)
retains the later waveform rationale.

Revisit if: a new desktop supplies a supported, uninterrupted history and
effect-aware delivery contract sufficient to extend automatic delivery.
