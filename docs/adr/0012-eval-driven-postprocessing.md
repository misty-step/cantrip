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
