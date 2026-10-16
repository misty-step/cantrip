# 0032: Wait for cloud STT; elapsed recognition time is not failure

- **Status:** accepted
- **Date:** 2026-10-16
- **Primitives:** stt, postproc, config
- **Powder:** —

## Context

The operator explicitly reversed the five-second cloud policy: “it takes as long
as it takes”; for a twenty-minute dictation, five seconds is an unreasonable
recognition cutoff. The shared [STT client](../../crates/engine/src/stt.rs) must
prefer the configured cloud result, not turn a valid slow response into local
fallback. The local Parakeet encoder's 30-second input chunks are model limits,
not cloud inference budgets. This supersedes the elapsed-time advice in
[0018](historical/0018-bounded-remote-stt-and-recovery.md) and
[0020](0020-local-completion-and-explicit-audio-deletion.md), without replacing
their upload, recovery, or retention contracts.

## Decision

Cloud STT waits for a response, a real failure, or user cancellation, with no
app-imposed total, per-chunk, per-request, upload, response, read, or write
deadline. Native credential reads are separately bounded and cancellable before
recognition. Optional cleanup shares its explicit `postproc.timeout_ms` across
credential waits, DNS, triage, generative passes, and watchdog; there is no hidden
five-second cleanup inference clamp. Native credential operations retain their
independent lookup guard within that shared maximum.

## Consequences

- Raising five seconds to another fixed number, duration-scaled cutoffs, and
  latency-driven fallback are rejected. A valid response remains selected
  regardless of elapsed recognition time; there is no STT timeout setting.
- Keep the 24,000,000-byte complete multipart cap and existing over-cap chunk
  planning, exact source coverage, and ordered results. Neither a byte cap nor
  local model chunking defines a cloud inference deadline or take-length limit.
- Actual HTTP/provider errors, transport failures, unavailable credentials, and
  empty recognition of nonempty audio still use installed default local Parakeet
  recovery. Preserve completed native cloud chunks and recognize only untouched
  source frames locally; without a usable native suffix, attempt the whole take
  once. Preserve useful partial text and retained audio, never concatenate
  overlapping passes, and archive honest local/cloud/mixed provenance. No
  implicit model download, cloud-provider switch, or new worker/runtime is added.
- A provider may still enforce its own timeout and return a real error; waiting
  cannot override that policy. The native ureq 3 transport explicitly configures
  separate 30-second DNS-resolution and connection-establishment guards, not a
  total request budget or inference deadline. Its global, per-call, DNS, connect,
  request-send, upload, response-header, and response-body timeout defaults are
  unset; Cantrip supplies only those network-establishment guards for STT rather
  than relying on ureq 2 defaults. Network establishment failure is a real
  transport failure; elapsed time after a successful connection is not by
  itself a reason to abandon recognition. See the pinned dependency's
  [timeout configuration](https://docs.rs/ureq/3.3.0/ureq/config/struct.ConfigBuilder.html#method.timeout_global).
- Native credential reads keep their separate five-second maximum before STT
  and honor cancellation promptly while waiting. Native keyring APIs cannot be
  interrupted: leave at most one read outstanding, discard late results, and
  allow subsequent takes to recover when the store does. Do not cache secrets
  or expose credentials/provider content in errors or operational logs.
- Check cancellation before and after requests and between chunks. An already
  running synchronous HTTP request or DNS lookup may finish later and keep the
  worker occupied until it returns; cancellation does not promise to interrupt
  either. Afterwards, no later chunk, local fallback, cleanup, or delivery
  begins for the cancelled take. Retained audio remains available.
- Cleanup's explicit shared budget includes native credential waits, DNS,
  connections, optional decisions, and response bodies; no later cleanup request
  starts after cancellation or budget expiry. Credential reads also respect
  their independent five-second maximum, not a hidden inference clamp on the
  overall cleanup chain. An unfinished generative chain preserves raw STT text.
  A watchdog failure or timeout after completed passes preserves the candidate
  under the existing fail-open contract. Cancelling the take prevents delivery
  of either result.

Acceptance examples:

- A valid cloud response arriving after five seconds or after sixty seconds is
  selected, without time-based local fallback or provenance downgrade.
- A twenty-minute take remains valid: upload byte caps can split it, but neither
  its duration nor elapsed recognition time supplies a rejection deadline.
- A genuine provider error, broken connection, or empty result still recovers
  locally where possible, preserving completed text and audio on partial failure.
- Cancel during blocked HTTP or DNS: it may return later, but its result is never
  delivered and it starts no subsequent chunk, fallback, or cleanup.
- Cancel during native credential waiting: it settles promptly, launches no
  recognition or delivery, and leaves at most one native read outstanding.
- With `postproc.timeout_ms = 30000`, cleanup inference is not cut off at five
  seconds; credentials, DNS, triage, passes, and watchdog share the configured
  thirty seconds, not thirty seconds per request. Individual credential reads
  remain independently bounded. Raw/watchdog preservation remains unchanged.

Revisit if: the operator explicitly authorizes a different waiting policy after
reviewing its effect on long dictation and valid slow cloud responses.
