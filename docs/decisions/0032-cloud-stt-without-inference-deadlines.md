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
app-imposed total, per-chunk, per-request, upload, or response deadline. Optional
cleanup separately shares its explicit `postproc.timeout_ms` across triage,
generative passes, and watchdog; there is no hidden five-second clamp.

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
  cannot override that policy. Ureq's ordinary connection-establishment timeout
  remains a network-connect guard, not an inference deadline. Connection
  establishment failure is a real transport failure; elapsed time after a
  successful connection is not by itself a reason to abandon recognition.
- Check cancellation before and after requests and between chunks. An already
  running synchronous request may finish and can keep the worker occupied until
  it returns; cancellation does not promise to interrupt that socket call.
  Afterwards, no later chunk, local fallback, cleanup, or delivery begins for
  the cancelled take. Retained audio remains available.
- Cleanup's explicit shared budget includes connections and response bodies; no
  later cleanup request starts after it expires. An unfinished generative chain
  preserves raw STT text. A watchdog failure or timeout after completed passes
  preserves the candidate under the existing fail-open contract. Cancelling the
  take prevents delivery of either result.

Acceptance examples:

- A valid cloud response arriving after five seconds or after sixty seconds is
  selected, without time-based local fallback or provenance downgrade.
- A twenty-minute take remains valid: upload byte caps can split it, but neither
  its duration nor elapsed recognition time supplies a rejection deadline.
- A genuine provider error, broken connection, or empty result still recovers
  locally where possible, preserving completed text and audio on partial failure.
- Cancel during a blocked request: it may return later, but its result is never
  delivered and it starts no subsequent fallback or cleanup.
- With `postproc.timeout_ms = 30000`, cleanup is not cut off at five seconds;
  triage, passes, and watchdog share the configured thirty seconds, not thirty
  seconds per request. Raw/watchdog preservation remains unchanged.

Revisit if: the operator explicitly authorizes a different waiting policy after
reviewing its effect on long dictation and valid slow cloud responses.
