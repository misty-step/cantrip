# Evaluation gauntlet

`examples/eval` is a reproducible benchmark harness for cantrip's
transcription and post-processing choices. It runs configured STT and
postproc lanes over a fixed clip set, scores each call (WER/CER, wall-clock
latency, cost from verified provider prices), and emits ranking boards. No
transcript text is printed to stdout or logs — frames carry ids, counts, and
metrics only; raw transcripts live in the gitignored output directory.

Results of the full matrix and the analysis are committed in
[`eval/report.md`](../eval/report.md); machine-specific reproductions
(absolute model paths, cloud credentials) live in the gitignored
`eval/config.json` on the author machine.

## Run

```sh
cargo run --release --example eval -- run
# Full matrix: 9 STT lanes × 5 clips, then 6 postproc lanes × 45 transcripts.
# Post-proc only, reusing cached transcripts (keeps STT out of the rerun):
cargo run --release --example eval -- run --ppr-only
```

- `--config PATH` point at your own lane definition JSON.
- `--stt a,b` / `--postproc c,d` / `--clips x,y` narrow a run; `--out DIR`
  prevents clobbering previous results.
- Cloud lanes route through a credential broker via the `CANTRIP_PROXY`
  environment variable (the broker rewrites `https://` provider URLs to
  `<prefix>/<host>/<path>` and substitutes key markers). Without it, cloud
  lanes attempt direct connections. The broker endpoint is deliberately not
  committed; ask the repo owner for the value.

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

Post-proc (final WER vs input 0.1034): qwen3-8b/14b and phi-4 are
within ±0.0014 (neutral); **gemma3-12b hurts (+0.028)**; only
`qwen/qwen3-30b-a3b` improved (−0.0013, at ~$0.00001/call).

Key reading: the best all-round cloud STT is `gpt-4o-mini-transcribe`; the
best free fast local is **Canary 1B**; Parakeet is fine for clean dictation
and wins on latency; postproc with qwen3-family on already-clean
transcripts is neutral, so the free local lane is the right default.

## Caveats

- Small, skewed set (5 clips; 2 trivial). Ranks are indicative, not
  statistically robust.
- Local STT numbers are CPU-only; a CUDA-enabled build would materially
  change the Whisper lanes.
- Cloud lanes not yet exercised: Grok STT (now reachable via OpenRouter),
  Mistral/Groq/NVIDIA-cloud/MAI (no broker credential yet). Contract notes
  and verified pricing: [`eval/cloud-stt-contracts.md`](../eval/cloud-stt-contracts.md).
- Costs are from verified 2026-08-03 prices.
