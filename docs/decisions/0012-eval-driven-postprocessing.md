# 0012: Evaluate cleanup fidelity instead of production text heuristics

- **Status:** accepted
- **Date:** 2026-08-13
- **Primitives:** postproc, eval
- **Powder:** —

## Context

Cleanup models can answer dictated questions instead of preserving the speaker's
words. Length, token-ratio, and Markdown heuristics reject valid edits without
proving fidelity. Clean read-speech WER alone cannot measure role or formatting
failures in [the evaluation runner](../../examples/eval/).

## Decision

Use prompt/model quality and deterministic-first evaluation rather than
approximate content policy in production. Extend the existing Rust evaluation
pipeline, not Harbor; model judges are explicit additive signals and never
override deterministic role/protocol failures or the reviewed exact oracle.

## Consequences

Production accepts nonempty text after protocol cleanup; request and response
failures still return raw text. Synthetic behavior cases distinguish cleanup,
role, content, and formatting, with latency/cost compared only after fidelity.
Public/synthetic data may be explicitly published; local JSON stays authoritative.
[Opt-in structured runtime decisions](0026-typesafe-system-one-decisions.md)
record the later narrow gating exception. See [evaluation](../EVALUATION.md).

Revisit if: evaluation requires interactive daemon behavior in an isolated
keyboard/clipboard/PipeWire environment; that is the stated condition for a
separate Harbor package.
