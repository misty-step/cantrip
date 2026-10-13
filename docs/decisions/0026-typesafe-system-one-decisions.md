# 0026: Structured decisions are opt-in gates, never dictated actions

- **Status:** accepted
- **Date:** 2026-09-18
- **Primitives:** postproc, eval
- **Powder:** —

## Context

Generative cleanup can answer questions or lose critical facts, while exact
matching cannot distinguish every harmless edit from a role failure.
[0012](0012-eval-driven-postprocessing.md) permits an explicit additive judge;
structured System One decisions provide bounded triage and verification.

## Decision

Allow structured System One judgments for public/synthetic evaluation and
explicit opt-in runtime cleanup triage/watchdog only. This is a narrow HTTP
exception for /v1/systemone and OpenRouter /api/alpha/decisions, using existing
ureq/serde_json rather than SDKs or an async runtime. Never execute dictated
commands as system or agent actions.

## Consequences

Runtime decisions are disabled by default and send text only to the configured
provider when opted in. Keys stay in the OS keyring; logs exclude content.
Errors, timeouts, and ambiguous probabilities fail open, preserving the standard
pipeline and speaker's words. A high-confidence role/fact failure returns raw
STT text. Evaluation uses public/synthetic data only; judge passes never override
deterministic protocol failures or the reviewed exact oracle.

Revisit if: evidence shows these bounded decisions cannot preserve the fail-open
contract or the explicit quality/triage-only dictation posture.
