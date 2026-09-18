# ADR 0026: TypeSafe System One decision models in Cantrip

Date: 2026-09-18. Status: accepted.

## Context

Cantrip uses post-processing to turn raw speech-to-text transcripts into clean
prose. ADR 0012 established an evaluation-driven post-processing contract,
noting two fundamental challenges:

1. **Answer-to-question failure mode:** Generative LLMs (System Two) frequently
   attempt to converse with or execute dictated requests rather than cleaning
   them (e.g. answering "Where is the config?" instead of transcribing it).
2. **Evaluation brittleness:** Exact-accepted string matching in `examples/eval`
   cannot distinguish harmless punctuation differences from fatal role failures,
   dropped negations, or invented facts. ADR 0012 proposed an explicit, additive
   model judge returning structured pass/fail/uncertain decisions.

Generative chat models are poorly suited for gating and verification: they are
slow (800ms–2,500ms autoregressive token streaming), expensive on output tokens,
and introduce new hallucination vectors.

TypeSafe's Jev 1.13 introduces a "System One" decision model that outputs
calibrated probabilities and discrete choices (`noul`, `choice`, `score`) across
typed questions with zero text generation and zero output token cost ($0.00 / 1M).

## Decision

Adopt TypeSafe System One structured decision models in Cantrip across two
strictly bounded tiers:

### 1. Evaluation Judge (ADR 0012 implementation)

Add an explicit System One model judge to `examples/eval`. The judge evaluates
synthetic and public behavior cases across atomic dimensions:
- **Role fidelity:** Did the model keep text as the speaker's words without
  answering or executing commands?
- **Negation preservation:** Did the cleanup preserve all negations (not, no, never)?
- **Content fidelity:** Were quantities, proper nouns, and key facts preserved?
- **Formatting & Disfluency:** Were stutters and false starts removed?

The judge runs against public/synthetic data only, never private operator audio.
A judge pass never overrides a deterministic protocol failure or reviewed exact
oracle.

### 2. Runtime Gating and Watchdog (Opt-in)

Permit System One decision models in the daemon runtime under an explicit,
opt-in configuration (`[postproc].decision_model`, default unset/disabled):

- **Pre-cleanup triage:** When enabled, a fast forward-pass decision checks if
  the raw transcript requires cleanup. Clean transcripts bypass the generative
  LLM and deliver immediately, saving 1–2 seconds of latency and token cost.
- **Post-cleanup watchdog:** When a generative LLM is used, a decision checks
  whether the output answered the prompt or dropped critical facts. If an
  answer-to-question failure is detected with high probability, the output is
  rejected and Cantrip delivers the raw STT transcript.

### 3. Boundaries and Invariants

- **Non-goal relaxation:** `VISION.md` specifies "OpenAI-compatible HTTP only".
  This ADR records an explicit, narrow exception for the System One decisions
  endpoint (`/v1/systemone` and OpenRouter `/api/alpha/decisions`). No heavy
  SDK is introduced: Cantrip uses a lightweight internal client over existing
  `ureq` and `serde_json`.
- **Privacy mandate:** Runtime decision calls transmit transcript text to a
  configured cloud provider. It remains strictly opt-in and disabled by default.
  Credentials live in the OS keyring (`cantrip key`), never in files or logs.
- **Dictation posture:** Cantrip remains an input utility for existing apps,
  not a voice assistant. System One decisions are used exclusively for quality,
  triage, and safety gating. Dictated commands are never executed as system or
  agent actions.
- **Fail-open safety:** Because Jev 1.13 has documented calibration variances
  ("jagged edges"), all runtime decision gates fail open. An HTTP error,
  timeout, or ambiguous probability preserves the speaker's words and standard
  delivery pipeline.

## Consequences

- `examples/eval` gains an objective, multi-dimensional grader that satisfies
  the ADR 0012 model judge specification.
- Operators opting into cloud cleanup can enable the decision gatekeeper to cut
  latency on clean takes and guard against conversational hallucinations.
- The process model remains one Rust binary with `std::thread` and `mpsc`; no
  async runtime or third-party client library is added.
