# Cantrip evaluation

Status: versioned evaluation procedures, accepted contracts, and retained design
context. [ADR 0012](adr/0012-eval-driven-postprocessing.md) records the accepted
deterministic-first evaluation direction; [ADR 0013](adr/0013-local-transcript-history.md)
owns the private-history-to-reviewed-fixture boundary. Detailed corpus sizes,
schema examples, and calibration targets below are design proposals, not a claim
that every target is implemented.

The implementation audit and 2026-08-13 baseline are historical evidence, not a
live gap register or a new cloud run. Current work and selected unresolved
proposals belong in Linear; this guide does not prescribe the next execution
slice. [README.md](../README.md#work-and-documentation-ownership) explains ownership.

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
- `behavior --postproc-manifest PATH` overrides the text-case manifest.
- `--out DIR` prevents a partial run from replacing canonical results.
- Cloud lanes that use `__mint.*` markers route through `CANTRIP_PROXY` when
  set: the broker rewrites HTTPS provider URLs to `<prefix>/<host>/<path>`
  and substitutes the markers. When `CANTRIP_PROXY` is unset, OpenRouter
  lanes use direct Bearer authentication, first reading the OS keyring
  (`cantrip key`), then falling back to `OPENROUTER_API_KEY`. Missing or
  malformed credentials fail without printing keys or headers. Other
  marker-based cloud lanes still require the broker. Its endpoint is
  deliberately not committed; obtain it from the repo owner.

The vocabulary-system comparison uses `eval/config-vocab-systems.json`.
Each lane's `vocabulary_mode` is `global` (the default), `candidate-filtered`,
`none`, or `deterministic-aliases`. Filtered mode builds the production prompt
for each source from matching token sequences, one-edit spelling candidates,
and the baseline's spoken aliases; it does not rewrite the source. Common-word
collisions remain candidates, so this is not semantic disambiguation. No-vocab
mode keeps the cleanup prompt but omits vocabulary. Deterministic aliases
perform only the documented token replacements, without an LLM or formatting.
Custom full-prompt `instructions` cannot be combined with a non-global mode.

Cloud cleanup requests match production: configured effort sends only
`reasoning: {"effort": "..."}`; unset effort omits `reasoning`. No evaluator-only
reasoning flags or generation cap are added. Completed behavior records include
`elapsed_us` (including retries) and explicit request errors; `pricing.json`
records the observed USD-per-token prices. The comparison's detailed board
reports strict exact-accepted scores, nearest-rank percentiles, variability,
and separately identified vocabulary corruptions without relaxing the oracle.

## Langfuse publish

`eval` can mirror the already-written result JSONs into a Langfuse dataset
without changing local scoring or reproducibility. This is the separate
metadata/data path for traces and datasets: local JSON output remains the
source of truth.

```sh
cargo run --release --example eval -- langfuse --out eval/results --dataset cantrip-evals
```

- Reuses the daemon's `[telemetry]` config: `enabled`, OTLP `endpoint`,
  `public_key`, and the `langfuse` OS-keyring secret. It refuses to run when
  telemetry is disabled.
- Reuses or creates the named Langfuse dataset, uploads the public/synthetic
  corpus (clip references and synthetic behavior cases), then posts metadata-only
  experiment traces and numeric scores for results in the selected directory.
- `--dataset` is optional; the default is the canonical `cantrip-evals` dataset.
  New item IDs include the remote dataset ID, logical clip/case key, and public
  content. Changed references or case content create separate items rather than
  overwriting the old corpus. Matching existing items are reused; exact legacy
  items with server-generated IDs are adopted in place. This does not delete
  pre-existing duplicates or rewrite old traces and scores.
- Completed `run`, post-processing-only, and `behavior` commands write immutable
  `run.json` metadata containing the command's `started_at_unix_ms`. Publishing
  uses that recorded start, not the upload clock or file modification time:
  Langfuse's observation storage key includes the start time. Spans use that
  run anchor plus the measured latency, not reconstructed per-call execution
  times. Legacy baselines may instead record `started_at`
  (`YYYY-MM-DDTHH:MM:SSZ`) or `date` (`YYYY-MM-DD`, UTC). Legacy output without
  a recorded timestamp must be rerun before publishing; no timestamp is invented.
- Run identity hashes the parsed JSON content of `run.json`, the selected
  manifests, and all applicable result files. Dataset, experiment, item, trace,
  span, and score identities remain stable on repeated publication of the same
  bundle, including after moving it. Result rows remain distinct, including
  repeated measurements. Changing the bundle creates a separate experiment.
  Treat these inputs as immutable; retries must use the same bundle and config.
- To distinguish intentionally independent runs with otherwise identical
  bundles, record distinct run IDs in `run.json`, or supply `--run-id <label>`.
  Reuse that label when retrying; changing it intentionally publishes another
  experiment. Directory names and publication timestamps are not run identity.
- Requests retry HTTP 408, 429, 500, 502, 503, and 504, connection/DNS failures,
  and interrupted response reads, with at most three attempts. Backoff is
  250 ms then 500 ms; `Retry-After` delta-seconds or GMT HTTP-date hints can
  extend either delay only up to five seconds. Each attempt retains the
  60-second request timeout. Other HTTP failures stop immediately, and OTLP
  partial rejection fails the publish. Errors report the operation and
  status/failure category, never response bodies or credentials.

Privacy boundary: dataset inputs and expected outputs are the public clips
and synthetic behavior cases. Experiment spans carry ids, counts, latency,
cost, and pass/error flags only — never transcript text, never audio. Daily
operator dictations never reach this path.


Cantrip has two related but different evaluation surfaces:

* The **audio matrix** runs speech-to-text (STT), then optionally runs every
  post-processing lane over each cached transcript. It currently reports WER,
  CER, wall-clock latency, real-time factor (RTF), and estimated cost.
* The **behavior matrix** sends synthetic raw transcripts directly to the
  post-processor and compares the response with reviewed accepted outputs. It
  currently groups cases as cleanup, role, preservation, and formatting.

The audio matrix is useful for transcription and systems trade-offs. It is
not a sufficient post-processing quality test: the current audio transcripts
are mostly clean, so a harmful answer can retain a good WER. The behavior
matrix is the decision surface for transcript fidelity.

## Historical implementation audit

This snapshot retains the evidence and limits that motivated the design. Source
line numbers and implementation gaps below are historical, not current bug
claims or work state. In particular, its timestamped Langfuse default and absent
manifest split metadata predate the documented publish/corpus updates above and
below. Do not re-open those findings from this snapshot alone.

### Surfaces observed in the audit

| Surface | Evidence | Behavior at the time |
|---|---|---|
| Configuration | `eval/config.json:1-6`, `eval/config.json:7-112`, `eval/config.json:114-186` | One JSON file names the two manifests, `eval/results` as the default output, six vocabulary spellings, nine STT lanes, and seven post-processing lanes. Local paths are home-relative. |
| Audio manifest | `samples/eval/manifest.json:2-46` | Five public or public-domain 16 kHz mono WAV clips with verbatim references and source/license text. |
| Audio execution | `examples/eval/main.rs:1106-1229` | Reads each clip once, loads each local model once, executes selected STT lanes, records one first-call/cold marker, writes `transcripts.json`, then invokes post-processing. |
| STT scoring | `examples/eval/main.rs:1634-1730`; `examples/eval/wer.rs:3-25`, `44-61` | Reports macro-mean normalized WER/CER per lane, warm mean latency, one cold value, RTF, and lane cost. Normalization keeps ASCII alphanumerics plus apostrophe/hyphen. |
| Post-processing execution | `examples/eval/main.rs:1480-1620` | Reuses cached STT output, calls each selected lane, records raw/final text, token counts, latency, cost, and a size-based degenerate flag. |
| Behavior manifest | `samples/eval/postproc-behavior.json:2-212` | Twenty-four synthetic cases with one category and one or more reviewed accepted strings per case. |
| Behavior scoring | `examples/eval/main.rs:1242-1444` | Repeats cases when requested, normalizes line endings/trailing whitespace, and marks a case passed if it exactly matches any accepted string. The board reports category counts, mean/p95 latency, and cost. |
| Cloud routing | `examples/eval/main.rs:40-63`, `457-555`, `602-648`, `660-690`, `747-799` | `CANTRIP_PROXY` rewrites HTTPS URLs to a broker prefix. Marker values such as `__mint.openrouter.default__` are sent only when the proxy is set. |
| Langfuse path | `examples/eval/langfuse.rs:40-117`, `124-303`, `305-315` | An explicit command uploads public/synthetic dataset items and metadata-only traces. Without `--dataset`, it creates a timestamped dataset name. |
| Versioned behavior evidence | `eval/baselines/2026-08-13-postproc-behavior/run.json:1-43`; `eval/baselines/2026-08-13-postproc-behavior/board.md:1-8` | The first immutable baseline records corpus/config/prompt/result hashes, three repetitions, four post-processing lanes, and complete behavior results. |

### Findings and gaps at the time

1. **The audio corpus cannot support a general STT ranking.** The manifest has
   only five clips (`samples/eval/manifest.json:2-46`). The report says two
   clips are near-trivial and the Common Voice accent clip is the main
   discriminator (`eval/report.md:45-48`, `130-135`). There is no balanced
   noise, spontaneous-dictation, short-command, technical-vocabulary, or
   long-form slice, and no held-out audio split.
2. **WER/CER are the only quality metrics.** The normalizer in
   `examples/eval/wer.rs:3-25` is deliberately ASCII-oriented, and
   `build_boards` computes macro means over clips
   (`examples/eval/main.rs:1647-1681`). Punctuation, formatting, named-entity
   preservation, speech-act fidelity, and failure rate are invisible. A
   future Unicode policy must be an explicit scorer version, not a silent
   change to the historical score.
3. **Latency sampling is too weak for a release decision.** STT marks only
   `index == 0` as cold (`examples/eval/main.rs:1161-1184`), reports a warm
   mean rather than p50/p95, and does not repeat a clip. Post-processing has
   a p95 for behavior calls, but failed calls are recorded with zero latency
   (`examples/eval/main.rs:1318-1337`) and are therefore not a trustworthy
   tail metric.
4. **Cost provenance is incomplete.** Static prices are lane configuration
   (`examples/eval/main.rs:943-975`); OpenRouter prices are fetched live
   (`examples/eval/main.rs:861-897`) but the snapshot is not written into the
   result. A missing OpenRouter model silently produces zero cost after a
   warning (`examples/eval/main.rs:1514-1520`). The result needs a
   `known/estimated/unknown` cost status and the exact price-source timestamp.
5. **The behavior matrix has one strict grader, not four dimension graders.**
   `BehaviorCase` carries only `category`, `input`, and `accepted`
   (`examples/eval/main.rs:177-188`), and the board derives each category by
   filtering one boolean (`examples/eval/main.rs:1399-1407`). Exact accepted
   strings are valuable regression oracles, but they cannot distinguish a
   harmless punctuation variant from an answer, refusal, omitted fact, or
   invented content. There is no additive model judge or calibration record.
6. **There is no held-out split or case-level promotion gate.** Every current
   behavior case is in one unlabelled set
   (`samples/eval/postproc-behavior.json:2-212`). Aggregate means can hide one
   severe role failure. A candidate must be judged per case and per dimension
   before latency or cost is considered.
7. **Run metadata is incomplete.** The normal run writes transcripts,
   post-processing results, and boards
   (`examples/eval/main.rs:1218-1229`, `1604-1619`) but not a self-contained
   run manifest containing corpus/config/prompt/grader/model hashes, host
   conditions, retries, split, and price snapshots. `--out` protects the
   configured directory only by convention (`examples/eval/main.rs:1097-1104`);
   callers can still point two runs at the same directory.
8. **The cloud contract is broker-only when a lane uses a marker.**
   `lane_available` checks endpoint/path/model but not marker or proxy
   (`examples/eval/main.rs:1037-1056`), while the adapters require a marker
   and add its header only if `CANTRIP_PROXY` is set
   (`examples/eval/main.rs:539-555`). The old run instructions described
   marker lanes as attempting direct connections; that is unsafe because the
   marker is not a credential. The audit proposed failing early with an
   actionable broker prerequisite instead of making an unauthenticated direct
   request.
9. **The Langfuse dataset name was not stable.** The audit recorded a
   `cantrip-eval-<timestamp>` default in
   `examples/eval/langfuse.rs:305-315`. The accepted dataset identity is now
   `cantrip-evals`, as documented under [Langfuse publish](#langfuse-publish).
   This old naming gap is not an unresolved execution item.
10. **The existing post-processing baseline is not an STT baseline.** The
    immutable directory is
    `eval/baselines/2026-08-13-postproc-behavior/`, not a complete audio and
    arrangement baseline. Its `run.json` records a post-processing corpus and
    four lanes (`run.json:4-32`). A future audio run must be promoted into a
    separate dated directory; do not rewrite this historical baseline or its
    hashes.

The audit also recorded an operator-local prerequisite: an empty daemon
`[postproc].model` and a stopped Ollama backend caused `--postproc clean` to be
rejected. That was a backend prerequisite, not an evaluation dispatch failure.
It is not a statement about the operator's current configuration or runtime.

## Corpus design: `cantrip-evals`

The current manifests now carry non-semantic identity metadata:

* `samples/eval/manifest.json:2-5` identifies
  `cantrip.eval.audio.v1`, dataset `cantrip-evals`, corpus version
  `2026-08-13`, and the current default `regression` split. Each current clip
  is explicitly tagged at `samples/eval/manifest.json:8-41`.
* `samples/eval/postproc-behavior.json:2-10` identifies
  `cantrip.eval.behavior.v1`, dataset `cantrip-evals`, corpus version
  `2026-08-13`, and the deterministic/model-judge grader names.

The current historical baseline predates those metadata fields; its recorded
SHA-256 values remain valid for the exact files used on 2026-08-13. Any new
run must hash the post-metadata manifests in its own `run.json`.

### Audio set

Retain the five public clips as a `regression` slice. The proposed expansion
targets at least 24 decision clips and 12 held-out clips, balanced across the
following strata rather than selected for one model. These sizes describe a
design target, not scheduled work:

| Stratum | Decision target | What it tests |
|---|---:|---|
| Clean read speech | 4 | Baseline recognition and long-form stability |
| Accents and speaking rates | 4 | Robustness beyond the one current accent clip |
| Background/noise and room reverberation | 4 | Real capture conditions; measure SNR in provenance |
| Short questions and commands | 4 | Clipboard-bound dictation shape and punctuation cues |
| Technical vocabulary and proper names | 4 | Cantrip, PipeWire, APIs, paths, acronyms, names |
| Spontaneous dictation and corrections | 4 | Fillers, false starts, repetitions, and self-corrections |

Every new WAV must be 16 kHz, mono, 16-bit PCM, have a stable source and
license, and be listed with exact reference text and SHA-256 in
`samples/eval/PROVENANCE.md`. Do not add private operator audio to the
repository or to Langfuse. If an owner-recorded clip is needed, commit only
an anonymized, consented fixture and its provenance.


### Behavior set

Retain the 24 reviewed public `regression` cases. The proposed expansion targets
at least 16 decision cases and 16 held-out cases, covering:

* speech acts: questions, commands, requests, refusals, and quoted
  instructions that must remain transcript text;
* fidelity hazards: negation, quantities, dates, names, paths, acronyms,
  homophones, uncertainty, and literal quoted text;
* structure: explicit paragraph breaks, ordered lists, punctuation around
  clauses, and prose that only resembles Markdown;
* failures: empty output, preamble/refusal, answer-to-question, invented
  facts, dropped clause, changed number, and changed pronoun.

Each behavior case should gain explicit deterministic fields, for example:

```json
{
  "id": "question-must-remain-text",
  "split": "decision",
  "category": "role",
  "input": "what time is the review",
  "accepted": ["What time is the review?"],
  "must_contain": ["review"],
  "must_preserve": ["what time"],
  "must_not_contain": ["The review is"],
  "format": {"terminal_punctuation": "question", "max_paragraphs": 1}
}
```

The strings in this example are a schema example, not a new reviewed case.
`accepted` remains the strict oracle and every accepted alternative must be
reviewed. The additional fields make failure reasons observable without
putting approximate content rules into production `src/postproc.rs`.

The fixture contract should identify each case's split and stratum; audio
metadata also needs duration, SHA-256, and SNR when applicable. Corpus identity,
provenance, and split semantics belong with the reviewed manifests, not solely
in an implementation ticket.

## Metrics and graders

These are the accepted evaluation goals and their proposed reporting design.
They are not an inventory of fields or graders already emitted by every runner.

### STT metrics

For each lane, split, clip, and repetition, persist:

* `wer_macro`: mean of per-clip WER, retained for comparability with the
  current board;
* `wer_micro`: total word edits divided by total reference words;
* `cer_macro` and `cer_micro` with the scorer version recorded;
* `latency_ms`: elapsed request/decode time, including retries;
* `cold_latency_ms`: model load plus first decode when applicable;
* `warm_p50_ms`, `warm_p95_ms`, and `rtf_p50`/`rtf_p95`;
* `failure_rate` and an explicit failure code; failed calls must not become
  zero-latency successes;
* `cost_usd` plus `cost_status` (`measured`, `estimated`, or `unknown`),
  token/audio usage, and a price snapshot hash.

Report macro and micro quality together. Use the clip as the sampling unit
for quality and the call as the sampling unit for latency/cost. Do not rank a
lane on a five-clip mean without showing per-stratum rows.

### Post-processing deterministic graders

The first grader is pure, versioned code and runs before any model judge:

1. **Protocol**: non-empty output, normalized line endings, no transport
   error, and no wrapper/preamble markers after the production normalization
   boundary.
2. **Exact accepted**: normalized output equals one reviewed `accepted`
   alternative. This is the hard regression oracle.
3. **Cleanup**: for a cleanup case, all required edits are present and no
   `must_preserve` span is missing. Filler/repetition removal is case-specific,
   not a global token heuristic.
4. **Role fidelity**: required speech-act spans remain, `must_not_contain`
   answer/refusal markers are absent, and the output is not an answer to the
   dictated question or command.
5. **Content preservation**: every required fact/span, negation, quantity,
   proper name, path, and pronoun survives; forbidden additions are absent.
6. **Formatting**: paragraph/list/punctuation constraints match the reviewed
   case. Whitespace normalization is limited to line endings and trailing
   whitespace; it must not erase meaningful structure.

Each grader returns `pass`, `fail`, and a machine-readable reason. Aggregate
by case and category, not only one global boolean. A candidate cannot pass the
behavior gate if protocol or role fidelity fails, even when WER is low.

### Additive model judge

A model judge is optional and never replaces deterministic graders. It receives
the source, candidate output, case contract, and deterministic findings, then
returns strict JSON:

```json
{
  "semantic_fidelity": "pass|fail|uncertain",
  "role_fidelity": "pass|fail|uncertain",
  "formatting": "pass|fail|uncertain",
  "confidence": 0.0,
  "reason_code": "short-code"
}
```

Use a fixed judge model/version and a prompt hash. The judge must be distinct
from the candidate lane. Store its structured decision and model metadata in
the local result; never let free-form judge prose decide promotion. Calibrate
it against human labels on at least 40 mixed pass/fail trials before using it
as an additive signal. A deterministic fail plus judge pass remains a
human-review item, not an automatic promotion.

### Latency and cost policy

Run behavior cases at least three times, as the historical baseline did
(`eval/baselines/2026-08-13-postproc-behavior/run.json:9-11`). Run audio clips
with one warm-up and at least three measured repetitions per lane/clip. Keep
lane order, concurrency, CPU/GPU mode, and model-load policy in `run.json`.

Use provider-reported usage when available. For OpenRouter, save the `/models`
response (or its content hash and retrieval time) with the run. A missing
price is `unknown`, never zero. Static provider prices must carry a source
URL and `as_of` date; do not infer pricing or capabilities from a model name.

## Lane catalog and cloud prerequisites

The existing lane IDs and adapters remain the compatibility catalog:

* local STT: `parakeet-v3-int8`, `canary-1b-alt`, `moonshine-base`,
  `whisper-large-v3-turbo`, and `whisper-large-v3`
  (`eval/config.json:7-56`);
* cloud STT: OpenAI, Deepgram, and ElevenLabs lanes with provider-specific
  paths, schemes, and rates (`eval/config.json:58-112`);
* post-processing: one local Ollama lane and OpenRouter Gemini/GPT lanes
  (`eval/config.json:114-186`).

The proposed catalog schema adds `provider`, `modality`, `availability`,
`requires_proxy`, `pricing_source`, and `pricing_as_of` to each lane. A lane
with a `__mint.*` marker is broker-only: require `CANTRIP_PROXY`, send the
marker only through that proxy, and print a prerequisite failure if the
variable is absent. Never commit a credential or substitute a guessed model
price. Gemini claims must cite the official model documentation or model card;
OpenRouter slug/context/pricing claims must cite the model listing metadata;
Mercury claims must cite Inception Labs release posts. Anything else is
`[INFERENCE]`.

If `[postproc].model` is empty or its backend is stopped, distinguish that daemon
prerequisite from availability of an independently configured eval lane. Do not
infer either state from this document.

## Output, privacy, and Langfuse

Use `eval/results` only for an intentional canonical local run. Use
`--out /tmp/cantrip-eval-<run-id>` for experiments and never mix files from
different corpus/config/prompt hashes. The target output contract is:

* `run.json`: schema, run id/date, git revision, split, lane IDs, hashes,
  repetitions, environment, retry policy, and price status/snapshot;
* `transcripts.json`, `postproc.json`, and `behavior.json` as applicable;
* `boards.md`, `behavior.md`, and machine-readable grader results.

Raw model responses may be written only to local result directories for public
or synthetic corpora. Do not commit private dictation, clipboard text,
credentials, or machine-local audio. Langfuse is an explicit publish step, not
the local source of truth. Use one stable dataset named `cantrip-evals`;
experiment names carry the immutable bundle identity described under
[Langfuse publish](#langfuse-publish). Upload public clip references and synthetic
behavior cases as dataset items, and send only IDs, grader scores, latency, cost,
and error flags in experiment traces. Audio and private operator transcripts
never go to Langfuse.

Keep curated public/synthetic inputs, provenance, and selected reproducible
baselines versioned. Retain growing or complete raw run bundles in approved
artifact storage when they need to survive the local experiment, with immutable
identity and the existing access restrictions. Linear owns the experiment
question, selected work, and safe comparison summary with proof links—not a copy
of private recordings or raw output. This policy moves no existing artifacts.

## Baselines and promotion

Keep the existing immutable
`eval/baselines/2026-08-13-postproc-behavior/` unchanged. Future baselines use
`eval/baselines/YYYY-MM-DD-<experiment>/` and contain `run.json`, all boards,
all machine-readable results, and the hashes needed to reproduce them.

Promotion is a two-stage gate:

1. **Correctness gate:** no protocol failures; all high-risk role cases pass
   deterministic role fidelity; no regression on any previously passing
   preservation case; decision and held-out exact/dimension scores are at
   least the incumbent within the pre-declared tolerance. Review every
   deterministic fail and every judge `uncertain`.
2. **Efficiency gate:** among correctness-qualified candidates, prefer the
   Pareto frontier on WER/CER (STT), deterministic behavior score, warm/p95
   latency, and cost. A candidate that spends more or is slower needs a
   pre-declared quality improvement and an explicit operator decision; cost
   savings never excuse a role or preservation regression.

Record the selected incumbent, tolerances, judge model, and reviewer in
`run.json`. A new corpus or scorer version starts a new baseline series; it
does not overwrite or silently re-score an old baseline.

A grader change must review the existing regression cases under the new scorer
before promoting a baseline. Historical scores and hashes remain evidence for
their original corpus and scorer, not scores silently updated by a code change.

## Harbor decision

Do **not** add Harbor for this evaluation surface. The Iron Forest precedent
uses `harbor==0.21.0` in `iron-forest/evals/pyproject.toml:1-7`, starts
containerized task jobs in `iron-forest/evals/run-fast.sh:10-16`, and runs an
agent inside a task sandbox (`iron-forest/evals/iron_forest_eval/agent.py:29-116`).
That is appropriate for agent/tool/repository scenarios. Cantrip's current
evaluation is a deterministic Rust binary that already owns WAV decoding,
local model loading, HTTP adapters, scoring, and privacy boundaries. Harbor
would add a Python/Docker control plane without isolating a capability the
current benchmark needs.

Extend `examples/eval` and keep pure graders in a Rust module. Reconsider a
separate `cantrip/eval/harbor/` package only if a future requirement evaluates
interactive daemon behavior (keyboard, clipboard, PipeWire, or recovery) in
an isolated environment. Do not mix that task runner into the STT/postproc
matrix.

