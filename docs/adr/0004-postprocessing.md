# ADR 0004: Transcript post-processing through one OpenAI-compatible client

Date: 2026-08-03. Status: accepted.

## Problem

Raw Parakeet output has weak punctuation, casing drift, and misspells
domain terms. Users want an optional cleanup pass with their own
vocabulary, using either a local model or a cloud model.

## Decision: one wire protocol, zero provider abstractions

Post-processing is a blocking POST to `{endpoint}/chat/completions`
(OpenAI chat schema) via the existing `ureq` dependency. "Local vs cloud"
is purely which `endpoint` the user configures:

- local: `http://localhost:11434/v1` (Ollama), llama-server, vLLM, …
- cloud: OpenAI, Groq, OpenRouter, …

No per-provider structs, no SDKs, no async runtime. `endpoint` + `model`
+ optional keyring key id is the entire abstraction (`src/postproc.rs`).

## Prompt contract

System prompt = fixed instruction ("rewrite the dictated transcript with
correct punctuation, capitalization, spelling; preserve wording; output
only the corrected text") + the user's `vocabulary` terms (exact-spelling
bias) + optional free-form `instructions` from config. User message = the
transcript. Temperature 0. `<think>…</think>` blocks are stripped from
the reply — local reasoning models otherwise inject chain-of-thought into
the user's document.

## Failure policy: raw text wins

Dictated words are irreplaceable; a cleanup pass is not. On any postproc
error (timeout, HTTP error, empty reply) the daemon injects the raw
transcript and the result notification says cleanup failed. This is
deliberate, user-visible product behavior, not a silent fallback.
Timeout defaults to 10 s (`postproc.timeout_ms`).

## Privacy

Transcript content never reaches logs. Postproc logs char counts, latency,
and HTTP status codes only — never response bodies, which can echo the
request. Enabling a non-localhost endpoint is the user explicitly choosing
to send dictation off-device; the default config keeps postproc disabled
and pointed at localhost.

## Vocabulary lives at the top level

`vocabulary` is a dictation-wide concept, not a postproc setting: the same
list feeds the postproc prompt and the cloud-STT `prompt` field (ADR 0005).
transcribe-rs exposes no Parakeet hotword biasing, so local STT gets
vocabulary correction only via postproc.

## Latency controls (2026-08-07)

Default cleanup is one pass. A second pass doubles cloud round-trips and did
not improve the messy-dictation bench over one pass of a strong model.

`postproc.min_chars` (default 40) skips cleanup for short transcripts so brief
commands avoid a network hop. `0` disables the skip. The skip is logged as
char counts only (`skipped_short`), never transcript text.
