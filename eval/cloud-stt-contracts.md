# Cloud STT contracts and pricing (verified 2026-08-03)

Contracts below were verified against official provider docs by a fresh-context
research lane (retrieval date 2026-08-03); citations per claim. "Reachability"
state is for this machine through the Mint broker at delivery time.

## ElevenLabs — Scribe v2 (REACHABLE — verified live)
- Contract: `POST https://api.elevenlabs.io/v1/speech-to-text`, multipart form
  data, `xi-api-key` header.
- Required fields: `file`, `model_id` (`scribe_v2` | `scribe_v1`). Useful:
  `language_code`, `tag_audio_events`, `num_speakers`,
  `timestamps_granularity`, `diarize`, `diarization_threshold`,
  `additional_formats`, `webhook_metadata`, `no_verbatim`,
  `use_speaker_library`, `detect_speaker_roles`, `entity_detection`,
  `entity_redaction`, `keyterms`, `use_multi_channel`,
  `multichannel_output_style`.
- Single-channel response: required `language_code`, `language_probability`,
  `text`, `words[]`; word keys `text`, `start`, `end`, `type`, `speaker_id`,
  `logprob`, `characters`, `channel_index`. Multichannel root uses `transcripts`.
- Pricing: Scribe v2 **$0.22/hour (~$0.003667/min)**; Scribe v2 Realtime
  $0.39/hour; entity detection +$0.070/hour; keyterm prompting +$0.050/hour.
- Files over 8 min are internally chunked (concurrency
  `min(4, floor(duration_s/480))`). Input file < 5 GB.
- Sources: `https://elevenlabs.io/docs/api-reference/speech-to-text/convert`,
  `https://elevenlabs.io/docs/overview/capabilities/speech-to-text`,
  `https://elevenlabs.io/pricing/api/`.

## Deepgram — Nova-3 (REACHABLE — verified live)
- Contract: `POST https://api.deepgram.com/v1/listen`, `Authorization: Token
  <key>`, `Content-Type: audio/wav` (or `application/json` for remote URL).
- Query params: `model=nova-3`, `smart_format=true`, `detect_language=true`.
- Transcript: `results.channels[].alternatives[].transcript`; `smart_format`
  adds `punctuated_word` in word objects.
- Pricing (PAYG, pre-recorded): Nova-3 Monolingual **$0.0048/min**,
  Multilingual $0.0058/min. New accounts get $200 credit.
- Limits: PAYG pre-recorded up to 50 concurrent requests (NA/EU/AU); requests
  over 10 min can return 504.
- Sources: `https://developers.deepgram.com/docs/pre-recorded-audio`,
  `https://developers.deepgram.com/reference/speech-to-text/listen-pre-recorded`,
  `https://developers.deepgram.com/reference/api-rate-limits`,
  `https://deepgram.com/pricing`.

## OpenAI — Whisper family (REACHABLE — verified live)
- Contract: `POST https://api.openai.com/v1/audio/transcriptions`,
  `Authorization: Bearer <key>`, multipart `file` + `model`. Models:
  `whisper-1`, `gpt-4o-mini-transcribe`, `gpt-4o-transcribe`.
- Default JSON `{text}`; Whisper supports `verbose_json` with timestamps;
  GPT-4o transcription models support json/text only.
- Pricing: `whisper-1` **$0.006/min** (duration-billed); `gpt-4o-mini-transcribe`
  **$1.25/M in, $5/M out** (≈$0.003/min); `gpt-4o-transcribe` $2.50/M in,
  $10/M out (≈$0.006/min). GPT-4o variants are token-billed.
- Sources: `https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create`,
  `https://developers.openai.com/api/docs/pricing`.

## Mistral — Voxtral Mini Transcribe (UNAVAILABLE: no Mint credential)
- Contract: `POST https://api.mistral.ai/v1/audio/transcriptions`, header
  `x-api-key: <key>` (files uploads may use Bearer; do not conflate).
  Multipart `file` (or `file_url`), `model` (`voxtral-mini-2602`,
  alias `voxtral-mini-latest`), optional `language`, `diarize`,
  `timestamp_granularities`, `context_bias`. Response: `model`, `text`,
  `language`, optional `segments`.
- Pricing: **$0.003/min**.
- Sources: `https://docs.mistral.ai/models/model-cards/voxtral-mini-transcribe-26-02`,
  `https://docs.mistral.ai/studio-api/audio/speech_to_text/offline_transcription`,
  `https://mistral.ai/pricing/api/`.

## xAI — Grok STT (UNAVAILABLE: account credits exhausted upstream)
- Contract: `POST https://api.x.ai/v1/stt`, `Authorization: Bearer <key>`,
  multipart `file` (last; max 500 MB) or `url`; params `audio_format`,
  `sample_rate`, `language`, `format`, `multichannel`, `channels`, `diarize`,
  `keyterm`, `filler_words`, `vad_threshold`. Response `text`, `language`,
  `duration`, optional `words[]` / `channels[]`.
- Model name: `grok-stt`.
- Pricing: **$0.10/hr REST**, $0.20/hr streaming.
- Live probe 2026-08-03: upstream `permission-denied` ("team has used all
  available credits or reached its monthly spending limit"); lane unavailable
  until xAI credits are restored.
- Sources: `https://docs.x.ai/developers/model-capabilities/audio/speech-to-text`,
  `https://docs.x.ai/developers/pricing`, `https://docs.x.ai/developers/models/grok-stt`.

## NVIDIA — Parakeet V3 cloud (UNAVAILABLE: not hosted, no Mint credential)
- `build.nvidia.com/nvidia/parakeet-tdt-0_6b-v3` returns 404; only V2 is
  listed/hosted. HuggingFace hosts V3 as weights only
  (`nvidia/parakeet-tdt-0.6b-v3`). No hosted V3 API contract to cite.
- The 25-language V3 weights are exactly what runs locally here
  (parakeet-tdt-0.6b-v3-int8).
- Sources: `https://build.nvidia.com/explore/discover`,
  `https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3`.

## Groq — Whisper large-v3-turbo (UNAVAILABLE: no Mint credential)
- Contract: `POST https://api.groq.com/openai/v1/audio/transcriptions`, OpenAI
  compatible, model `whisper-large-v3-turbo` (or `whisper-large-v3`); params
  file/url/model/language/prompt/response_format/timestamp_granularities.
  25 MB free-tier / 100 MB dev-tier uploads, minimum billed 10 s/request.
- Pricing: **$0.04/hour**. Limits vary by tier (e.g. 20 RPM, 2,000 RPD base).
- Sources: `https://console.groq.com/docs/speech-to-text`,
  `https://console.groq.com/docs/rate-limits`, `https://groq.com/pricing`.

## Microsoft — MAI-Transcribe 1.5 (UNAVAILABLE: needs Azure resource/key, no Mint credential)
- Hosted via Azure Speech LLM Speech API (public preview, no SLA):
  `POST https://<resource>.cognitiveservices.azure.com/speechtotext/transcriptions:transcribe?api-version=2025-10-15`,
  header `Ocp-Apim-Subscription-Key`, multipart `audio` +
  `definition.enhancedMode.model=mai-transcribe-1.5`,
  `enhancedMode.enabled=true`. Audio < 300 MB WAV/MP3/FLAC.
- Diarization and prompt tuning unsupported; `phraseList`/`transcribeStyle`
  only on 1.5.
- Pricing: Microsoft Foundry blog states $0.36/hour; Azure Speech pricing page
  renders MAI-transcribe as "$/hour" regionally quoted (1-second billing
  increments). Cited as rough, not contractual.
- Sources: `https://learn.microsoft.com/en-us/azure/ai-services/speech-service/mai-transcribe`,
  `https://techcommunity.microsoft.com/blog/azure-ai-foundry-blog/new-mai-models-in-microsoft-foundry-across-text-image-voice-and-speech/4524632`,
  `https://azure.microsoft.com/en-us/pricing/details/speech/`.

## Living speech runner — OpenRouter transcription route

The existing `eval` example includes a separate `living-speech` command. It
does not change Cantrip's selected/default model, read transcript history,
download models, or use the application's credential store. It reads only the
configured corpus and installed baseline directory, and uses an injected
`OPENROUTER_API_KEY` for explicitly authorized paid invocations.

```sh
cargo run --release --example eval -- living-speech \
  --config /private/eval/config.json --out /private/eval/preflight-unique \
  --run-id preflight-unique --source-revision FULL_GIT_SHA --dry-run

cargo run --release --example eval -- living-speech \
  --config /private/eval/config.json --out /private/eval/run-unique \
  --run-id run-unique --source-revision FULL_GIT_SHA --allow-paid
```

`--out` must not exist; its parent must already exist. Each invocation writes
owner-only receipts to a fresh directory. `--run-id` is a fresh public-safe
identifier; the durable commission ledger rejects reuse of a spent run ID.
`--source-revision` is a 7–64-character hexadecimal Git revision supplied by
the operator. Dry-run validates the corpus, current public transcription
catalog, installed local model, and commission budget without checking
credentials or making paid calls. It writes/prints `preflight.json` with
request count, audio seconds, conservative reservations and cumulative
remaining budget; it never generates measured `results.json`.

Configuration is JSON with `schema_version: 1` and these fields; unknown fields
are rejected:

| Object | Required fields |
| --- | --- |
| Root | `corpus`, `models`, `budget`; optional `local_parakeet` |
| `corpus` | `id`, `version`, `description`, `clips` |
| Each clip | `id`, `file`, `ref`, `category`, `source`, `license`, `reference_reviewed` |
| Each cloud model | `id`, `name`, `ceiling_usd_per_audio_hour`, `pricing_source` |
| Optional `local_parakeet` | `id`, `name`, `model_version`, `dir`, `quant` |
| `budget` | `commission_id`, `ledger_path`, `commission_cap_usd`, `per_run_cap_usd` |

- Corpus and clip IDs, local model IDs, commission IDs and run IDs start with
  an ASCII alphanumeric and contain only ASCII alphanumerics, `-` or `_`
  (maximum 80 characters). Clip IDs must be opaque public labels, never
  original personal take IDs. Human-readable public metadata is explicitly
  configured; do not put private material in corpus descriptions or names.
- `file`, local `dir` and `ledger_path` are absolute paths or paths relative to
  the configuration file. Paths are not globbed or discovered. Each selected
  WAV must contain nonempty 16 kHz mono signed PCM16 audio and fit the
  transcription endpoint's 25 MB multipart limit. Bytes are read/validated
  once and the same reviewed bytes are sent to every model.
- `ref` is the exact approved reference text. Every clip needs nonempty
  `source` and `license`, `reference_reviewed: true`, and a reference that
  remains nonempty under the existing ASCII scorer. `category` is exactly
  `dictation` or `public-speech`; at least one real, reviewed dictation clip
  is required. References and provenance fields never enter public receipts.
- `models` contains distinct, explicitly chosen `provider/model` IDs present
  in the current OpenRouter transcription catalog. There must be at least
  five total models, counting at most one optional installed local Parakeet
  baseline. Its `quant` is `int8`, `int4`, `fp16` or `fp32`; missing or
  unloadable selected local assets fail preflight, never trigger a download.
- Every paid lane requires a positive finite reviewed ceiling in **USD per
  audio hour** and its HTTPS `pricing_source`. The catalog records canonical
  versions, but its `pricing.prompt` is deliberately not used for billing:
  duration-price units differ, including MAI's per-hour rate. Reservations
  round each clip up to a whole second and each amount up to a nanodollar.
- `commission_cap_usd` must be exactly `4.5`; `per_run_cap_usd` is positive and
  at most `4.5`. The stable `ledger_path` must be outside public output, in an
  existing owner-only directory. Existing ledgers must be owner-only regular
  files with no additional hard links. Do not delete, reset or replace a
  commission ledger between runs. An exclusive nonblocking lock prevents
  concurrent spending, and fsynced append-only reservations survive crashes.
  Missing usage, errors, timeouts and empty output retain the full reservation;
  successful responses may settle to authoritative `usage.cost`. Any observed
  cost above its ceiling permanently halts further paid commission calls.
  A partial/corrupt journal fails closed and requires operator reconciliation,
  not an automatic reset.
  Only exclusive creation of a new journal permits initialization. An existing
  empty file is corrupt, never a new allowance; metadata and replay are checked
  after the lock is acquired. Impossible over-cap/halted reservations and
  overflowing accounting also fail closed.

The paid route is direct
`POST https://openrouter.ai/api/v1/audio/transcriptions`, Bearer authentication,
multipart WAV with an anonymous `audio.wav` filename, `language=en` and
`response_format=json`. The response's `text` is scored in memory and discarded;
valid numeric `usage.cost` is authoritative independently of HTTP status or
recognition text validity. Failed recognition retains max(reservation, reported cost);
absent/invalid usage is
unknown, never zero. No paid retries or redirect following are performed.
Provider-key limits and any authorization for recurring paid calls are
separate operator responsibilities; `--allow-paid` authorizes only this
invocation, not a schedule.

Observed runs write sanitized `results.json`, append-only JSON-line `run.log`
and immutable, create-new per-call receipts under `calls/`. The log is synced
before a reserved request and after its outcome, so interrupted runs preserve
receipts without pretending they completed. Validated plans live separately
in `preflight.json`. The runner never writes audio, references, transcripts,
private paths, original take IDs, provider error bodies or credentials to
these files. The corpus SHA-256 includes ordered, length-prefixed corpus
identity, opaque clip IDs, categories, exact references, source/license and
WAV bytes, but not filesystem paths.

JSON receipts are streamed into a private create-new temporary file, fsynced,
and atomically linked to the final name without clobbering prior evidence;
the parent directory is synced. An incomplete JSON write has no final filename
and cannot be proposed as a public receipt.

Every configured model/clip pair is accounted for after execution begins:
errors, empty output and budget-blocked pairs are full deletion (WER/CER 1.0),
not omitted from macro means. Latency median/p95 use successful calls only;
local model loading is excluded and disclosed separately. WER can exceed 1.0.
Host/load limitations, unknown charges and partial failures are explicit.
`budget.reserved_usd` is this run's accounted liability after permitted
settlements, `reported_usd` is the actual reported paid usage sum, and
`unknown_cost_calls` counts submitted requests without valid cost (not
unsubmitted blocked pairs). The private ledger applies the cumulative
commission cap independently of earlier unrelated API-key usage. Any failed
or empty call, unknown submitted cost, halted spending or receipt failure
produces a nonzero exit **after** writing available results; operators must not
mark that run all-successful.

Only reviewed observed `results.json` and its text-free `run.log` should be
published. Public benchmark corpora/normalization and vendor-reported rankings
remain separate from Cantrip measurements; these scores do not imply direct
comparability.

Primary API references:
- [OpenRouter speech-to-text guide](https://openrouter.ai/docs/guides/overview/multimodal/stt)
- [Create transcription API](https://openrouter.ai/docs/api/api-reference/stt/create-transcription)
- [Current transcription model catalog](https://openrouter.ai/api/v1/models?output_modalities=transcription)
