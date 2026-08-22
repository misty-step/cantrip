# Evaluation gauntlet

`examples/eval` is a reproducible benchmark harness for Cantrip's
transcription and post-processing choices. The audio matrix scores WER, CER,
latency, and cost. The behavior matrix compares model output with reviewed
accepted transcripts for cleanup, role fidelity, preservation, and formatting.
No transcript text is printed to stdout or logs.

The harness, manifests, corpora, lane catalog, reviewed reports, and
decision-grade baselines are version controlled. `eval/config.json` uses
home-relative local model paths and credential markers, not secrets. Scratch
runs stay under ignored `eval/results*` directories.

## Run

```sh
cargo run --release --example eval -- run
# Post-proc only, reusing cached STT transcripts:
cargo run --release --example eval -- run --ppr-only
# Text-only post-proc behavior matrix:
cargo run --release --example eval -- behavior
```

- `--config PATH` selects a lane definition JSON file.
- `--stt a,b`, `--postproc c,d`, `--clips a,b`, and `--cases a,b` narrow a run.
- `behavior --repeat N` repeats each selected case.
- `--out DIR` prevents a partial run from replacing canonical results.
- Cloud lanes route through a credential broker via the `CANTRIP_PROXY`
  environment variable (the broker rewrites `https://` provider URLs to
  `<prefix>/<host>/<path>` and substitutes key markers). Without it, cloud
  lanes attempt direct connections. The broker endpoint is deliberately not
  committed; ask the repo owner for the value.

## Post-processing behavior set

`samples/eval/postproc-behavior.json` contains synthetic raw transcripts and
reviewed accepted outputs. The cases cover cleanup, questions and commands,
content preservation, and formatting. Exact accepted outputs make model
changes reviewable without adding approximate content rules to the daemon.

`behavior` writes full responses to `behavior.json`. Its board reports total
and category pass counts, mean and p95 latency, and live OpenRouter cost.

## Langfuse publish

`eval` can mirror the already-written result JSONs into a Langfuse dataset
without changing local scoring or reproducibility. This is the separate
metadata/data path for traces and datasets: local JSON output remains the
source of truth.

```sh
cargo run --release --example eval -- langfuse --out eval/results --dataset my-run
```

- Reuses the daemon's `[telemetry]` config: `enabled`, OTLP `endpoint`,
  `public_key`, and the `langfuse` OS-keyring secret. It refuses to run when
  telemetry is disabled.
- Creates a Langfuse dataset, uploads the public/synthetic corpus (clip
  references and synthetic behavior cases), then posts metadata-only
  experiment traces and numeric scores for every result already on disk in
  the selected output directory.
- `--dataset` is optional; without it the command uses a unique
  `cantrip-eval-<timestamp>` name. When `--dataset` names an existing dataset,
  the publish reuses that dataset and upserts items and scores by deterministic
  id, so a re-run does not create duplicates.
- Dataset, dataset-item, score, and experiment-trace calls retry HTTP 429 and
  5xx responses with exponential backoff (three attempts) before the publish
  fails.

Privacy boundary: dataset inputs and expected outputs are the public clips
and synthetic behavior cases. Experiment spans carry ids, counts, latency,
cost, and pass/error flags only — never transcript text, never audio. Daily
operator dictations never reach this path.

## Versioned experiments

Evaluation is a first-class product subsystem:

- Commit evaluator code, fixed corpora, provenance, lane definitions, and
  pricing assumptions with the production change they evaluate.
- Run behavioral finalists at least three times. Review every exact miss and
  every role-sensitive response before selecting a model or prompt.
- Keep exploratory output in `eval/results*`. Do not treat one scratch run as
  a baseline.
- Promote decision-grade runs to
  `eval/baselines/YYYY-MM-DD-<experiment>/`. Keep `run.json`, `board.md`, and
  the complete result JSON together. Never overwrite an earlier baseline.
- Update `eval/report.md` when a baseline changes the current recommendation.
- Commit full outputs only for synthetic or public corpora. Private dictation,
  user clipboard text, credentials, and machine-local audio remain untracked.

### Production-to-eval flywheel

Cantrip keeps owner-private production transcript records under
`$XDG_STATE_HOME/cantrip/transcripts`; see
[ADR 0013](adr/0013-local-transcript-history.md). These records close the gap
between synthetic cases and daily use, but they are not automatically eval
inputs or ground truth.

1. Define the capability or failure category before selecting examples.
2. Sample representative successes and failures locally. Compare raw and
   cleaned text pairwise; do not rely on one aggregate score.
3. Manually label the expected result. Redact or replace identifying content,
   then review the transformed case before committing it.
4. Add recurring failures to a regression set. Keep a separate held-out set so
   prompt work is not optimized only for known cases.
5. Version the corpus, accepted outputs, prompt and scorer code, model/reasoning
   configuration, repetitions, per-case responses, latency, cost, and run
   metadata.
6. Run the same corpus against each candidate. Inspect category scores and
   individual regressions, calibrate automated graders against human review,
   and promote the complete immutable run only when it supports a decision.
7. Repeat after production failures and model, prompt, or pipeline changes.

This follows the common lifecycle in
[OpenAI's evaluation guidance](https://developers.openai.com/api/docs/guides/evaluation-best-practices),
[Anthropic's eval-driven development guidance](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents),
[MLflow's production-trace dataset workflow](https://mlflow.org/docs/latest/genai/datasets/),
and
[LangSmith's offline/online evaluation loop](https://docs.langchain.com/langsmith/evaluation).
Cantrip keeps the implementation as versioned JSON and Markdown rather than
adding a hosted evaluation service.

The first versioned behavioral baseline is
[`eval/baselines/2026-08-13-postproc-behavior`](../eval/baselines/2026-08-13-postproc-behavior/).

## Clip set

5 clips (16 kHz mono WAV, 5–20 s, in `samples/eval/`): the classic public
domain `jfk`, two LibriSpeech slices (`dev-clean`, `test-other`), and a
Common Voice accent clip. Reference transcripts are verbatim from the
datasets (`samples/eval/PROVENANCE.md`). Two clips are near-trivial (all
models score WER 0); the Common Voice accent clip is the real discriminator.

## Matrix headline (2026-08)

STT (mean WER over 5 clips):

| Lane | WER | Latency |
|---|---|---|
| ElevenLabs Scribe v2 (cloud) | 0.061 | 0.95 s |
| OpenAI gpt-4o-mini-transcribe (cloud) | 0.065 | 0.92 s |
| Whisper large-v3 (local CPU) | 0.077 | 52 s (CPU) |
| Canary 1B (local) | 0.083 | 0.90 s |
| Parakeet TDT v3 int8 (local, default) | 0.123 | 0.25 s |

The audio-derived postproc matrix remains near-neutral because those
transcripts are already clean. The separate 24-case behavior matrix selects
`google/gemini-3.6-flash` with default reasoning: 63/72 exact passes across
three runs per case, including 21/21 role-sensitive passes. Mean latency was
2.93 seconds, p95 was 6.68 seconds, and measured cost was about $0.003 per
cleanup. See [`eval/report.md`](../eval/report.md) for the lower-cost lanes and
failure analysis.

## Caveats

- Small, skewed set (5 clips; 2 trivial). Ranks are indicative, not
  statistically robust.
- Local STT numbers are CPU-only; a CUDA-enabled build would materially
  change the Whisper lanes.
- Cloud lanes not yet exercised: Grok STT (now reachable via OpenRouter),
  Mistral/Groq/NVIDIA-cloud/MAI (no broker credential yet). Contract notes
  and verified pricing: [`eval/cloud-stt-contracts.md`](../eval/cloud-stt-contracts.md).
- STT costs use verified 2026-08-03 prices; postproc uses live 2026-08-13 OpenRouter prices.
