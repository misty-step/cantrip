# OpenRouter STT contenders

as_of: 2026-09-11 (catalog snapshot `2026-09-11T22:12:40Z`; eval run started `2026-09-11T22:16:09Z`).

Current production cloud STT: `microsoft/mai-transcribe-2` via OpenRouter (`https://openrouter.ai/api/v1`).
Local baseline: Parakeet `parakeet-v3-int8` (`transcribe_rs`, `~/.local/share/cantrip/models/parakeet-tdt-0.6b-v3-int8`).

This file is catalog facts plus one 5-clip measurement.

## Scope

- Corpus: `samples/eval/manifest.json` five public 16 kHz mono WAV clips (jfk, librispeech-clean, librispeech-spelling, librispeech-other, commonvoice-37021060). Audio sum 48.65 s.
- Config: `eval/config-openrouter-stt.json`. `postproc: []`. Out: `eval/results/openrouter-stt-contenders`.
- Adapter: existing eval `kind: openrouter` JSON (`POST /api/v1/audio/transcriptions`, `input_audio` raw base64 WAV). Endpoint `https://openrouter.ai`, marker `__mint.openrouter.default__`, scheme Bearer. Auth: OS keyring id `openrouter`. `CANTRIP_PROXY` unset.
- Catalog snapshot: `eval/results/openrouter-stt-contenders/models-transcription.json`.
- Eval `per_min` rate `0.001` is a validate placeholder only. Recorded `cost_usd` prefers response `usage.cost` when finite and `>= 0`.
- OpenRouter `/models` `pricing.prompt` units are mixed. Catalog unit is `unknown` unless an observed `usage.cost` implies a rate on this corpus.
- Boards and this file: ids and metrics only. Transcripts stay in `transcripts.json`.

## Catalog

Live `GET /api/v1/models?output_modalities=transcription`, HTTP 200, `total_count` 21 (unchanged from the 2026-09-11 contract list).

pricing_source: `https://openrouter.ai/api/v1/models?output_modalities=transcription`
as_of: `2026-09-11T22:12:40Z`

| id | pricing.prompt | pricing.completion | pricing_source | unit |
|---|---|---|---|---|
| `meta/muse-voice-transcribe-1.0` | 0.18 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `microsoft/mai-transcribe-2` | 0.1 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `nvidia/nemotron-3.5-asr-streaming-multilingual-0.6b` | 0.00000333 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `mistralai/voxtral-small-24b-2507-stt` | 0.00005 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `mistralai/voxtral-mini-3b-2507` | 0.0000166667 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `qwen/qwen3-asr-1.7b` | 0.0000075 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `qwen/qwen3-asr-0.6b` | 0.00000333 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `openai/gpt-transcribe` | 0.0045 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `fish-audio/transcribe-1` | 0.0001 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `x-ai/grok-stt-1.0` | 0.1 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `deepgram/nova-3` | 0.0043 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `microsoft/mai-transcribe-1.5` | 0.36 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `nvidia/parakeet-tdt-0.6b-v3` | 0.0015 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `mistralai/voxtral-mini-transcribe` | 0.003 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `qwen/qwen3-asr-flash-2026-02-10` | 0.000035 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `google/chirp-3` | 0.016 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `openai/gpt-4o-mini-transcribe` | 0.00000125 | 0.000005 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `openai/whisper-large-v3` | 0.0000075 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `openai/whisper-large-v3-turbo` | 0.00000333 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `openai/whisper-1` | 0.006 | 0 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |
| `openai/gpt-4o-transcribe` | 0.0000025 | 0.00001 | https://openrouter.ai/api/v1/models?output_modalities=transcription | unknown |

## Observed (this run)

HTTP class 2xx, 5/5 clips unless noted. Latency list is per-call `latency_ms` in clip order: jfk (cold), librispeech-clean, librispeech-spelling, librispeech-other, commonvoice-37021060. Warm mean and RTF exclude the cold call (eval board convention). `usage.cost` sum is the harness `cost_usd` total when `usage.cost` was present (finite `>= 0`); local Parakeet has no `usage.cost` and `pricing.unit` `zero`.

Implied `$/min` = `usage.cost` sum / (audio_secs/60) on this 48.65 s set. That is an observation, not a catalog unit.

| lane | model | mean WER | mean CER | latency_ms | warm ms | cold ms | RTF | usage.cost sum | implied $/min | calls | HTTP class |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `openrouter-microsoft-mai-transcribe-2` | `microsoft/mai-transcribe-2` | 0.0260 | 0.0053 | 726, 286, 273, 403, 232 | 298 | 726 | 0.033 | 0.001389 | 0.001713 | 5 | 2xx |
| `openrouter-google-chirp-3` | `google/chirp-3` | 0.0403 | 0.0375 | 1215, 1167, 1194, 1703, 1058 | 1280 | 1215 | 0.142 | 0.013333 | 0.016445 | 5 | 2xx |
| `openrouter-qwen-qwen3-asr-0.6b` | `qwen/qwen3-asr-0.6b` | 0.0440 | 0.0324 | 622, 487, 487, 649, 489 | 528 | 622 | 0.060 | 0.000162 | 0.000200 | 5 | 2xx |
| `openrouter-microsoft-mai-transcribe-1.5` | `microsoft/mai-transcribe-1.5` | 0.0469 | 0.0161 | 1074, 805, 995, 1048, 822 | 918 | 1074 | 0.104 | 0.005000 | 0.006167 | 5 | 2xx |
| `openrouter-qwen-qwen3-asr-flash-2026-02-10` | `qwen/qwen3-asr-flash-2026-02-10` | 0.0469 | 0.0313 | 2218, 1376, 993, 2254, 828 | 1363 | 2218 | 0.146 | 0.001610 | 0.001986 | 5 | 2xx |
| `openrouter-openai-whisper-large-v3` | `openai/whisper-large-v3` | 0.0845 | 0.0351 | 954, 1330, 1726, 1948, 2967 | 1993 | 954 | 0.231 | 0.000365 | 0.000450 | 5 | 2xx |
| `openrouter-openai-whisper-large-v3-turbo` | `openai/whisper-large-v3-turbo` | 0.0845 | 0.0294 | 1306, 1175, 1488, 1899, 1988 | 1638 | 1306 | 0.185 | 0.000162 | 0.000200 | 5 | 2xx |
| `openrouter-mistralai-voxtral-small-24b-2507-stt` | `mistralai/voxtral-small-24b-2507-stt` | 0.0885 | 0.0427 | 765, 753, 964, 1368, 862 | 987 | 765 | 0.108 | 0.002432 | 0.003000 | 5 | 2xx |
| `openrouter-fish-audio-transcribe-1` | `fish-audio/transcribe-1` | 0.0897 | 0.0389 | 1789, 826, 612, 703, 379 | 630 | 1789 | 0.073 | 0.005000 | 0.006167 | 5 | 2xx |
| `openrouter-mistralai-voxtral-mini-3b-2507` | `mistralai/voxtral-mini-3b-2507` | 0.0908 | 0.0614 | 352, 675, 518, 567, 358 | 530 | 352 | 0.062 | 0.000811 | 0.001000 | 5 | 2xx |
| `openrouter-nvidia-parakeet-tdt-0.6b-v3` | `nvidia/parakeet-tdt-0.6b-v3` | 0.0908 | 0.0495 | 352, 524, 219, 819, 215 | 444 | 352 | 0.047 | 0.001216 | 0.001500 | 5 | 2xx |
| `openrouter-qwen-qwen3-asr-1.7b` | `qwen/qwen3-asr-1.7b` | 0.0977 | 0.0399 | 636, 459, 537, 736, 457 | 547 | 636 | 0.060 | 0.000365 | 0.000450 | 5 | 2xx |
| `openrouter-mistralai-voxtral-mini-transcribe` | `mistralai/voxtral-mini-transcribe` | 0.0988 | 0.0419 | 564, 725, 632, 663, 507 | 632 | 564 | 0.074 | 0.002300 | 0.002837 | 5 | 2xx |
| `openrouter-x-ai-grok-stt-1.0` | `x-ai/grok-stt-1.0` | 0.1120 | 0.0551 | 409, 333, 371, 489, 322 | 379 | 409 | 0.042 | 0.000000 | 0.000000 | 5 | 2xx |
| `parakeet-v3-int8` | local `parakeet-tdt-0.6b-v3-int8` | 0.1234 | 0.0511 | 276, 188, 230, 338, 191 | 237 | 859 | 0.026 | n/a (local; recorded $0) | n/a | 5 | n/a (local) |
| `openrouter-deepgram-nova-3` | `deepgram/nova-3` | 0.1234 | 0.0479 | 568, 188, 453, 337, 661 | 410 | 568 | 0.048 | 0.003486 | 0.004300 | 5 | 2xx |
| `openrouter-nvidia-nemotron-3.5-asr-streaming-multilingual-0.6b` | `nvidia/nemotron-3.5-asr-streaming-multilingual-0.6b` | 0.1354 | 0.0630 | 1464, 1300, 1041, 1523, 986 | 1212 | 1464 | 0.138 | 0.000162 | 0.000200 | 5 | 2xx |
| `openrouter-meta-muse-voice-transcribe-1.0` | `meta/muse-voice-transcribe-1.0` | 0.1377 | 0.0775 | 3067, 2247, 2910, 3420, 2657 | 2808 | 3067 | 0.314 | 0.002444 | 0.003014 | 5 | 2xx |

Per-clip WER (same clip order as the latency list):

| lane | jfk | librispeech-clean | librispeech-spelling | librispeech-other | commonvoice-37021060 |
|---|---|---|---|---|---|
| `openrouter-microsoft-mai-transcribe-2` | 0.000 | 0.000 | 0.038 | 0.020 | 0.071 |
| `openrouter-google-chirp-3` | 0.000 | 0.000 | 0.038 | 0.020 | 0.143 |
| `openrouter-qwen-qwen3-asr-0.6b` | 0.000 | 0.000 | 0.077 | 0.000 | 0.143 |
| `openrouter-microsoft-mai-transcribe-1.5` | 0.000 | 0.000 | 0.000 | 0.020 | 0.214 |
| `openrouter-qwen-qwen3-asr-flash-2026-02-10` | 0.000 | 0.000 | 0.000 | 0.020 | 0.214 |
| `openrouter-openai-whisper-large-v3` | 0.000 | 0.000 | 0.077 | 0.060 | 0.286 |
| `openrouter-openai-whisper-large-v3-turbo` | 0.000 | 0.000 | 0.077 | 0.060 | 0.286 |
| `openrouter-mistralai-voxtral-small-24b-2507-stt` | 0.000 | 0.000 | 0.077 | 0.080 | 0.286 |
| `openrouter-fish-audio-transcribe-1` | 0.000 | 0.000 | 0.000 | 0.020 | 0.429 |
| `openrouter-mistralai-voxtral-mini-3b-2507` | 0.000 | 0.000 | 0.077 | 0.020 | 0.357 |
| `openrouter-nvidia-parakeet-tdt-0.6b-v3` | 0.000 | 0.000 | 0.077 | 0.020 | 0.357 |
| `openrouter-qwen-qwen3-asr-1.7b` | 0.000 | 0.000 | 0.000 | 0.060 | 0.429 |
| `openrouter-mistralai-voxtral-mini-transcribe` | 0.000 | 0.000 | 0.077 | 0.060 | 0.357 |
| `openrouter-x-ai-grok-stt-1.0` | 0.000 | 0.000 | 0.000 | 0.060 | 0.500 |
| `parakeet-v3-int8` | 0.000 | 0.000 | 0.077 | 0.040 | 0.500 |
| `openrouter-deepgram-nova-3` | 0.000 | 0.000 | 0.077 | 0.040 | 0.500 |
| `openrouter-nvidia-nemotron-3.5-asr-streaming-multilingual-0.6b` | 0.000 | 0.000 | 0.077 | 0.100 | 0.500 |
| `openrouter-meta-muse-voice-transcribe-1.0` | 0.000 | 0.000 | 0.077 | 0.040 | 0.571 |

### Production model (this run)

`microsoft/mai-transcribe-2`: mean WER 0.0260, mean CER 0.0053, warm 298 ms, cold 726 ms, RTF 0.033, `usage.cost` sum $0.001389, 5/5, HTTP 2xx.

### Muse re-run

`meta/muse-voice-transcribe-1.0` on this same 5-clip corpus:

- Prior observed (not this out_dir): mean WER 0.105 and 0.119 across two runs (accent clip 0.429 vs 0.500); warm ~2.4–2.8 s; `usage.cost` sum $0.002444.
- This run: mean WER 0.1377, mean CER 0.0775, accent clip WER 0.571, warm mean 2808 ms, cold 3067 ms, `usage.cost` sum $0.002444.
- Observed WER range across those three measurements: 0.105–0.1377.

### Other prior 5-clip notes (not this out_dir)

- Local Parakeet: WER 0.1234 CER 0.0511 this run; matches the prior 0.1234 / 0.0511. Warm ~237 ms this run vs ~230 ms prior. Recorded cost $0.
- OpenRouter `deepgram/nova-3`: WER 0.1234 CER 0.0479 this run; matches prior 0.1234 / 0.0479. Warm mean 410 ms this run (per-call 568, 188, 453, 337, 661) vs prior warm mean 230 ms (per-call 174–353, cold 809). `usage.cost` sum $0.003486 both.
- `openai/whisper-1` and `openai/gpt-4o-mini-transcribe`: HTTP 401 on this keyring, same class as prior.

## Failed / unrun

All 21 catalog ids were attempted. Four returned HTTP 401 on the first clip (`jfk`); remaining clips for that lane were skipped. The matrix continued.

| id | class | reason |
|---|---|---|
| `openai/gpt-transcribe` | 4xx (401) | HTTP 401 on clip `jfk`; lane skipped |
| `openai/gpt-4o-mini-transcribe` | 4xx (401) | HTTP 401 on clip `jfk`; lane skipped |
| `openai/whisper-1` | 4xx (401) | HTTP 401 on clip `jfk`; lane skipped |
| `openai/gpt-4o-transcribe` | 4xx (401) | HTTP 401 on clip `jfk`; lane skipped |

No catalog id was left unrun. No HTTP 402/403. Paid lanes were not spend-stopped.

## Harness

`examples/eval/main.rs`: on cloud STT 4xx/5xx, skip the rest of that lane, record HTTP class in `stt-skips.json`, continue the matrix. HTTP 402/403 also stops remaining paid STT kinds (`openrouter`, `openai`, `deepgram`, `elevenlabs`). Daemon config was not changed.
