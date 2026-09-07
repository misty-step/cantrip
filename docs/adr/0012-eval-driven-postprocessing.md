# ADR 0012: Eval-driven transcript post-processing

Date: 2026-08-13. Status: accepted.

## Problem

Post-processing can produce text that answers the transcript instead of cleaning it. The existing prompt uses many negative instructions. The response path also uses content-length and token-preservation heuristics. Those checks add policy to production code, reject valid edits, and cannot prove that a response is faithful.

The existing evaluation uses clean read-speech clips and WER. It does not measure punctuation, formatting, or whether a model keeps questions and commands as transcript text.

## Decision

Use model and prompt quality as the main post-processing control.

- Write the fixed prompt with positive commands and ASD-STE100 writing rules.
- Keep one instruction in each sentence.
- Use active voice and short sentences.
- Tell the model to keep questions, requests, and commands as the speaker's words.
- Wrap each transcript in matching `Source` and `Clean transcript` labels.
- Test post-processing with a text corpus that has exact accepted outputs.
- Score cleanup, role fidelity, content preservation, and formatting separately.
- Measure latency and cost for each model.
- Keep the OpenAI-compatible endpoint and model configuration.

Production accepts a non-empty text response after protocol cleanup. It does not use content-length, token-ratio, or Markdown heuristics. Request, HTTP, response-shape, and empty-output failures still return the raw transcript.

## Model selection

The evaluation matrix includes current low-latency cloud models and a local reference. A model must pass the behavior corpus before latency and cost decide the recommendation. Model names and prices change, so the evaluation result is evidence for the recommendation, not a permanent provider abstraction.

## Consequences

The production path becomes smaller and has no approximate content policy. A bad model response can pass through, so the behavior corpus and model review become release criteria for prompt or model changes. Exact accepted outputs can be reviewed without adding the same rules to the daemon.
The corpus uses synthetic text. Evaluation output files can contain model responses and stay in the existing evaluation-results location. Runtime logs continue to contain character counts only.


## Evaluation design amendment (2026-09-03)

The first baseline exposed two limits. The audio corpus is five clean/public
clips, with two near-trivial clips (`eval/report.md:45-48`), and the behavior
runner reduces every case to one exact-accepted boolean
(`examples/eval/main.rs:1340-1367`). WER cannot detect an answer to a dictated
question, and an aggregate exact count cannot explain a dropped negation,
changed quantity, or malformed list.

The evaluation contract is therefore deterministic-first:

1. Keep `accepted` strings as the strict reviewed oracle.
2. Add case-local contracts for required spans, forbidden additions,
   speech-act/role, and formatting. A pure grader reports each dimension and
   a reason code; it does not add policy to `src/postproc.rs`.
3. Add a model judge only as an explicit, additive signal. It returns
   structured pass/fail/uncertain decisions, uses a fixed model distinct from
   the candidate, and is calibrated against human labels. A judge pass never
   overrides a deterministic role or protocol failure.
4. Split the public behavior corpus into regression, decision, and held-out
   cases. Keep the current 24 cases as regression cases and add cases for
   speech acts, negation/quantities/names/paths, corrections, paragraphs,
   lists, refusals, empty output, and answer-to-question failures.
5. Keep the current five audio clips for regression, then add balanced
   decision and held-out strata for accents, noise, short commands, technical
   vocabulary, and spontaneous dictation. Record source, license, WAV
   properties, reference, and SHA-256 in `samples/eval/PROVENANCE.md`.

STT promotion reports macro and micro WER/CER, warm p50/p95, cold load-plus-
decode latency, RTF, failure rate, and cost status. Post-processing promotion
requires protocol and role fidelity before latency or cost is compared. A
missing live price is `unknown`, not zero.

The canonical external dataset is `cantrip-evals`. Langfuse publishing stays
explicit and receives only public/synthetic dataset items plus metadata-only
traces. Local JSON remains the source of truth.

Do not introduce Harbor for this matrix. Harbor `0.21.0` is the Iron Forest
agent/task-sandbox precedent (`iron-forest/evals/pyproject.toml:1-7`,
`iron-forest/evals/run-fast.sh:10-16`), while this evaluation is a
deterministic Rust pipeline. Extend `examples/eval` with pure graders. Add a
separate Harbor package only if a later requirement evaluates interactive
daemon behavior in an isolated keyboard/clipboard/PipeWire environment.

The amendment records an accepted evaluation contract, not an active execution
checklist or proof that every grader and report field has shipped.
[The evaluation guide](../EVALUATION.md) retains the proposed corpus/reporting
design, reproducibility procedures, and baseline protections. Linear owns any
selected implementation work. Grader changes remain outside production
post-processing heuristics, and baseline promotion requires review of the
existing regression cases under the new scorer.
