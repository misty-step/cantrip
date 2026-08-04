# Gauntlet clip provenance (retrieved 2026-08-03)

Each clip beyond `jfk` was fetched from its authoritative dataset, verified for
16 kHz mono 16-bit WAV (5-20 s), and its reference transcript was matched
exactly (canonical fingerprint: uppercase, non-alphanumerics removed) against
the dataset row before being recorded in `manifest.json`.

| id | file | source | stable source URL | license |
|---|---|---|---|---|
| jfk | samples/jfk.wav | JFK public-domain speech (local fixture; ships with cantrip) | local fixture | public-domain |
| librispeech-clean | samples/eval/librispeech-clean.wav | LibriSpeech openSLR12 dev-clean utterance 2277-149896-0000 | https://www.openslr.org/12 ; https://huggingface.co/datasets/openslr/librispeech_asr | CC BY 4.0 |
| librispeech-spelling | samples/eval/librispeech-spelling.wav | LibriSpeech openSLR12 dev-clean utterance 2035-147960-0002 (proper-noun stress clip) | https://www.openslr.org/12 ; https://huggingface.co/datasets/openslr/librispeech_asr | CC BY 4.0 |
| librispeech-other | samples/eval/librispeech-other.wav | LibriSpeech openSLR12 test-other utterance 7902-96591-0008 | https://www.openslr.org/12 ; https://huggingface.co/datasets/openslr/librispeech_asr | CC BY 4.0 |
| commonvoice-37021060 | samples/eval/commonvoice-37021060.wav | Common Voice 22.0 en/test clip common_voice_en_37021060.mp3 (India/South-Asia accent) | https://huggingface.co/datasets/fsicoli/common_voice_22_0 ; metadata https://huggingface.co/datasets/fsicoli/common_voice_22_0/resolve/main/transcript/en/test.tsv ; audio tar https://huggingface.co/datasets/fsicoli/common_voice_22_0/resolve/main/audio/en/test/en_test_0.tar (member payload bytes 481307648-481352620) | CC0-1.0 |

## Verification

- WAVE properties checked with the stdlib `wave` module for every file (rate
  16000, channels 1, width 16-bit, duration 5-20 s).
- LibriSpeech references matched exactly against the HF datasets-server row
  (`openslr/librispeech_asr`) and the official OpenSLR `.trans.txt` files.
- Common Voice sentence matched exactly against the `fsicoli/common_voice_22_0`
  `en/test.tsv` row. Common Voice 22.0 metadata is CC0-1.0 (v26 card confirms CC0);
  raw v22 files are gated on HuggingFace, so the row was fetched before gating.
- No transcript content appears outside `manifest.json` in this repo.
- `jfk` is a repository fixture (ships with cantrip). For auditability its
  SHA-256 is `59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`;
  the recorded reference is the canonical public-domain passage it contains.
