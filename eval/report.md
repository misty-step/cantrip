# Cantrip transcription + post-processing gauntlet — findings (2026-08-03)

## Setup

- 5 clips (16 kHz mono WAV, 5-20 s): `jfk` (public domain, loud), LibriSpeech
  `dev-clean` and `test-other` ("spelling" = proper-noun stress), Common Voice
  22.0 India/South-Asia accent. Reference transcripts are verbatim from the
  datasets (see `samples/eval/PROVENANCE.md`).
- Units: WER/CER after lowercase + punctuation stripping; latency = wall clock
  per call; cost drawn from verified provider pricing
  (`eval/cloud-stt-contracts.md`), live OpenRouter /models prices for cloud
  postproc. Local inference runs CPU-only (no CUDA toolkit / `nvcc` on this
  host); whisper.cpp is the CPU build.
- Full matrix: 9 STT models × 5 clips = 45 transcripts; 6 postproc models × 45
  = 270 post-proc passes. `./target/release/eval run` reproduces it.

## Transcription — who is best

Ranked by mean WER over the 5 clips (repr. in `eval/results/boards.md`):

| model | WER | CER | warm ms | RTF | cost (5 clips) |
|---|---|---|---|---|---|
| ElevenLabs Scribe v2 (cloud) | **0.061** | 0.034 | 945 | 0.11 | $0.0030 |
| OpenAI gpt-4o-mini-transcribe (cloud) | 0.065 | 0.046 | 922 | 0.10 | **$0.0015** |
| Whisper large-v3 (local CPU) | 0.077 | 0.029 | 51 700 | 5.9 | $0 |
| Canary 1B (local) | 0.083 | 0.053 | 898 | 0.09 | $0 |
| OpenAI whisper-1 (cloud) | 0.085 | 0.032 | 1881 | 0.22 | $0.0049 |
| Parakeet TDT v3 int8 (local, cantrip default) | 0.123 | 0.051 | **252** | **0.03** | $0 |
| Deepgram Nova-3 (cloud) | 0.123 | 0.048 | 1864 | 0.27 | $0.0039 |
| Whisper large-v3-turbo (local CPU) | 0.127 | 0.053 | 39 600 | 4.6 | $0 |
| Moonshine 245M/Base (local) | 0.186 | 0.122 | 487 | 0.05 | $0 |

- **Most accurate:** ElevenLabs Scribe v2 (0.061), then gpt-4o-mini-transcribe.
- **Best free, fast local:** Canary 1B (0.083 at 0.9 s, $0) — clearly beats
  parakeet/whisper-turbo on the accent + proper-noun clips.
- **Fastest:** parakeet (252 ms, RTF 0.03); Moonshine is a strong RTF (0.05)
  but weakest WER here.
- **Cheapest cloud per clip:** gpt-4o-mini-transcribe ($0.0003) — and it is
  simultaneously near-best accuracy and fast, the best all-round cloud pick.
- **Local Whisper (CPU) is impractical:** RTF ≈ 5-6. A CUDA-enabled build would
  materially change this lane.
- Clip discrimination: `jfk` + `librispeech-clean` are trivially 0.0 WER for
  every model; the *only* discriminating clip is the Common Voice accent clip,
  where cloud models score 0.29 vs. local 0.36-0.71. The set favors
  clean-speech quality and software robustness, not noise robustness.

## Post-processing — who does (not) help

| model | input WER | final WER | delta | cost (matrix) |
|---|---|---|---|---|
| qwen3-8b (local) | 0.1034 | 0.1034 | +0.0000 | $0 |
| qwen3-14b (local) | 0.1034 | 0.1029 | -0.0004 | $0 |
| phi-4 14B (local) | 0.1034 | 0.1047 | +0.0013 | $0 |
| gemma3-12b (local) | 0.1034 | 0.1312 | **+0.0278** | $0 |
| qwen3-14b (OpenRouter) | 0.1034 | 0.1082 | +0.0048 | $0.027 |
| qwen3-30b-a3b (OpenRouter) | 0.1034 | 0.1020 | -0.0013 | $0.0005 |

- **Post-proc is near-neutral on this corpus** (deltas within ±0.03). The
  transcripts are already clean and punctuated; a cleanup pass neither helps
  nor hurts for all Qwen/Phi models. The single real effect is **gemma3-12b
  actively degrades** (+0.028); **qwen3-30b-a3b** is the only model that
  improved mean WER.
- This is a dataset artifact, not a verdict on postproc value: the earlier
  live demo (messy real dictation) showed qwen3-8b inserting punctuation and
  exact-spellings for PipeWire/Parakeet. Diacritics-space postproc shows up on
  messy dictation, which this clean clip set does not stress.
- Cloud qwen3-14b through OpenRouter shows 1-2 degenerate passes per 5 and
  ~10 s warm latency; the MoE 30B ("a3b") is ~7x cheaper and faster with the
  best delta.

## Arrangements (STT × postproc, top)

- Best final WER: **ElevenLabs × (qwen3-8b/14b/phi4/30b-a3b) = 0.061**;
  gpt-4o-mini-transcribe × same = 0.065; all free/cheap postproc.
- Best cost-constrained: **parakeet/canary (local, $0) + qwen3-8b ($0)** —
  $0 total at 0.083-0.123 WER; or **gpt-4o-mini-transcribe + qwen3-8b** at
  ~$0.0015 total for the 0.065 bucket.
- No arrangement benefits from paying for postproc on this corpus (equal
  final WER to the free local pass).

## Blocked / not-run lanes (documented in `eval/cloud-stt-contracts.md`)

- **Grok STT** — xAI account credits exhausted (upstream `permission-denied`).
- **Mistral Voxtral Mini, Groq Whisper turbo, NVIDIA Parakeet V3 cloud,
  Microsoft MAI-Transcribe 1.5** — no Mint credential/alias (or V3 not hosted).
  Contracts + pricing verified and documented for a future lane.
- **OpenRouter gpt-4o-mini / gemini-2.5-flash-lite** — OpenRouter account
  privacy/guardrail setting returns "No endpoints available matching your
  data policy" (404); enable at https://openrouter.ai/settings/privacy or use
  direct provider keys. Qwen routes (DeepInfra/Nebius) pass that policy.

## Caveats

- 5 clips is small and 2 are trivial; treat ranks as indicative, not
  statistically robust. The accent clip is the sole strong discriminator.
- Note the "noisy" label: LibriSpeech test-other here is acoustically quiet;
  the loudest clip is `jfk`. Noise robustness is not well tested.
- Local STT numbers are CPU; whisper GPU would shift those lanes.
- Cost figures are from verified 2026-08-03 pricing; verify before budgeting.
