# 0004: One cleanup wire protocol, no provider SDKs

- **Status:** accepted
- **Date:** 2026-08-03
- **Primitives:** postproc, config
- **Powder:** —

## Context

Raw Parakeet text needs optional punctuation and vocabulary cleanup. Local and
cloud models expose the same OpenAI-compatible chat schema; provider-specific
clients would duplicate the endpoint/model configuration in
[postproc](../../crates/engine/src/postproc.rs).

## Decision

Use the existing blocking HTTP client and configured endpoint, model, and
optional keyring label. Do not add per-provider structs, SDKs, or an async
runtime; local versus cloud is the configured endpoint.

## Consequences

Cleanup stays opt-in, with vocabulary shared across dictation lanes. Protocol,
HTTP, timeout, and empty-output failures return raw text with visible failure
feedback. Logs contain counts and latency, never transcript or response bodies.
Default cleanup is one pass; short-text skipping avoids unnecessary requests.
See [configuration](../CONFIGURATION.md#postproc--cleanup) for current settings.

Revisit if: a required cleanup capability cannot use the shared wire protocol.
