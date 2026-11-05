# Gauntlet clip provenance (retrieved 2026-08-03)

Each clip beyond `jfk` was fetched from its authoritative dataset, verified for
16 kHz mono 16-bit WAV (5-20 s), and its reference transcript was matched
exactly (canonical fingerprint: uppercase, non-alphanumerics removed) against
the dataset row before being recorded in `manifest.json`.

The manifest records the current corpus as schema `cantrip.eval.audio.v1`,
dataset `cantrip-evals`, corpus version `2026-08-13`, and split `regression`.
These fields describe the corpus; they do not change the historical
2026-08-13 result hashes. New public clips must be added with a stable source
URL, license, exact reference match, WAV properties, and a SHA-256 before they
enter a decision-grade run. A future held-out split must not be copied from
these regression clips or used to tune the prompt.

| id | file | source | stable source URL | license |
|---|---|---|---|---|
| jfk | samples/jfk.wav | JFK public-domain speech (local fixture; ships with cantrip) | local fixture | public-domain |
| librispeech-clean | samples/eval/librispeech-clean.wav | LibriSpeech openSLR12 dev-clean utterance 2277-149896-0000 | https://www.openslr.org/12 ; https://huggingface.co/datasets/openslr/librispeech_asr | CC BY 4.0 |
| librispeech-spelling | samples/eval/librispeech-spelling.wav | LibriSpeech openSLR12 dev-clean utterance 2035-147960-0002 (proper-noun stress clip) | https://www.openslr.org/12 ; https://huggingface.co/datasets/openslr/librispeech_asr | CC BY 4.0 |
| librispeech-other | samples/eval/librispeech-other.wav | LibriSpeech openSLR12 test-other utterance 7902-96591-0008 | https://www.openslr.org/12 ; https://huggingface.co/datasets/openslr/librispeech_asr | CC BY 4.0 |
| commonvoice-37021060 | samples/eval/commonvoice-37021060.wav | Common Voice 22.0 en/test clip common_voice_en_37021060.mp3 (India/South-Asia accent) | https://huggingface.co/datasets/fsicoli/common_voice_22_0 ; metadata https://huggingface.co/datasets/fsicoli/common_voice_22_0/resolve/main/transcript/en/test.tsv ; audio tar https://huggingface.co/datasets/fsicoli/common_voice_22_0/resolve/main/audio/en/test/en_test_0.tar (member payload bytes 481307648-481352620) | CC0-1.0 |

## Verification

- WAVE properties checked with the stdlib `wave` module for every gauntlet file (rate
  16000, channels 1, width 16-bit, duration 5-20 s).
- LibriSpeech references matched exactly against the HF datasets-server row
  (`openslr/librispeech_asr`) and the official OpenSLR `.trans.txt` files.
- Common Voice sentence matched exactly against the `fsicoli/common_voice_22_0`
  `en/test.tsv` row. Common Voice 22.0 metadata is CC0-1.0 (v26 card confirms CC0);
  raw v22 files are gated on HuggingFace, so the row was fetched before gating.
- Gauntlet reference transcripts live in `manifest.json`.
- `jfk` is a repository fixture (ships with cantrip). For auditability its
  SHA-256 is `59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`;
  the recorded reference is the canonical public-domain passage it contains.

## Short-final-chunk regression

[`samples/stt-short-tail.wav`](../stt-short-tail.wav) is original synthetic speech,
not an operator recording. It contains 35.78 seconds of 16 kHz mono PCM16 audio.
Its SHA-256 is `e0668d08b6b757a778e987e0b8e7a16cd17f160a0319e1b16670a7ee04d127f2`.

[`scripts/generate-stt-short-tail`](../../scripts/generate-stt-short-tail) owns the
text, pinned voice/configuration checksums, and construction. The fixture was
generated on Boat with Piper 1.8.0, FFmpeg 6.1.1, and the
[`en_US-ljspeech-medium` voice](https://huggingface.co/rhasspy/piper-voices/blob/main/en/en_US/ljspeech/medium/MODEL_CARD).
That voice's model card identifies its training data as the
[public-domain LJ Speech dataset](https://keithito.com/LJ-Speech-Dataset/).
The synthetic text is part of this repository's MIT-licensed generator.

The prefix is fitted to 31.7 seconds; the final phrase begins at 32.41 seconds.
A unique 30 ms quiet frame ends at 32.01 seconds, so the old planner chooses
32.01 + 3.77 seconds. Other zero samples become one PCM16 unit, an inaudible
floor that keeps this split deterministic. Seeded Gaussian noise at -5.5 dB SNR
in the final fragment reproduces omitted/corrupted final words without using
private audio. The repaired planner instead chooses 27.24 + 8.54 seconds.

To regenerate on a Boat worker with Piper 1.8.0 and FFmpeg 6.1.1 installed, fetch
the voice and configuration from the linked model directory, then run:

```sh
python3 scripts/generate-stt-short-tail \
  --voice /absolute/path/to/en_US-ljspeech-medium.onnx \
  --piper /absolute/path/to/piper \
  --output /absolute/path/to/new-stt-short-tail.wav
cmp samples/stt-short-tail.wav /absolute/path/to/new-stt-short-tail.wav
```

The generator refuses an existing output or a different voice/configuration.
The [real Parakeet regression](../../crates/engine/tests/stt_short_tail.rs) checks
the complete reference ending once, independent of punctuation and case.
