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

## OpenRouter — Meta Muse Voice Transcribe 1.0 (eval-only)

as_of: 2026-09-11. Eval adapter only; not wired into production daemon STT.

- Contract (Muse model page, JSON): `POST https://openrouter.ai/api/v1/audio/transcriptions`,
  `Authorization: Bearer`, `Content-Type: application/json`,
  body `{ "model": "meta/muse-voice-transcribe-1.0", "input_audio": { "data": "<raw base64, not data URI>", "format": "wav" } }`.
  Response `{ "text", "usage": { seconds, tokens, cost } }`. This host's 2026-09-11
  JSON smoke returned `usage` keys `cost` and `seconds` (no tokens).
- OpenRouter STT API also documents OpenAI-style multipart `file` + `model`
  (25 MB cap) at the same path. Muse's model page documents JSON only.
- Live probe 2026-09-11, `samples/jfk.wav`, direct OpenRouter (keyring
  `openrouter`; `CANTRIP_PROXY` unset): JSON HTTP 2xx; multipart HTTP 2xx.
  Production Cantrip (`POST {base}/audio/transcriptions` file+model) is
  therefore a drop-in at HTTP-class 2xx on this host. The eval lane still
  uses JSON because that is Muse's advertised contract.
- Auth: mint marker `__mint.openrouter.default__` only when `CANTRIP_PROXY`
  is set; otherwise `cantrip key` id `openrouter`, else `OPENROUTER_API_KEY`.
  Markers are never sent as credentials on the direct path.
- Pricing: Meta list price **$0.18/hour = $0.003/min**. OpenRouter
  `/models?output_modalities=transcription` lists `pricing.prompt: "0.18"`.
  Eval `per_min` rate is 0.003; recorded `cost_usd` prefers response
  `usage.cost` when it is finite and `>= 0`, else falls back to per_min.
  pricing_source: Meta Model API pricing page; OpenRouter model/STT docs.
- Sources: `https://dev.meta.ai/docs/pricing-rate-limits.md`,
  `https://openrouter.ai/meta/muse-voice-transcribe-1.0`,
  `https://openrouter.ai/docs/api/api-reference/stt/create-transcription`.

## OpenRouter catalog equivalents (eval-only)

as_of: 2026-09-11. Same JSON `kind: openrouter` adapter as Muse (keyring
`openrouter`; `CANTRIP_PROXY` unset). Not wired into production daemon STT.
Results: `/tmp/cantrip-eval-stt-muse-cloud` (does not replace
`eval/results/stt-muse`).

- `openai/whisper-1`: JSON HTTP 401 on first clip. Not scored. OpenAI
  whisper-1 **$0.006/min**; OpenRouter `pricing.prompt` 0.006.
- `openai/gpt-4o-mini-transcribe`: JSON HTTP 401 on first clip. Not scored.
  Historical ~$0.003/min; token-billed. Recorded cost would prefer
  `usage.cost`.
- `deepgram/nova-3`: JSON HTTP 2xx, 5/5 clips. Deepgram PAYG monolingual
  **$0.0048/min**; OpenRouter `pricing.prompt` **0.0043**. Recorded `cost_usd`
  is summed `usage.cost` (0.003486 / 48.65 s ≈ $0.0043/min).
- ElevenLabs Scribe v2 is not on OpenRouter. Mint probe: DNS NXDOMAIN for the
  broker host (HTTP 000 / curl 6). Not scored.

Other OpenRouter STT models (MAI-Transcribe 2, Voxtral, Qwen3 ASR, Nemotron,
grok-stt) were not run.
