# Configuration guide

Cantrip reads one TOML file. `cantrip config path` prints its location
(default `~/.config/cantrip/config.toml`), `config show` prints it,
`config init` creates it with defaults, and `config edit` opens it in
`$EDITOR`. `cantrip settings` opens a window you can keep open to view
and adjust the common settings; its Save button writes the file back
(with comments preserved) and reloads the running daemon.

Changes take effect on daemon start. Use `cantrip start` / `stop`, or restart
the daemon process, after editing.

```toml
injection = "auto"        # auto | type | clipboard — how corrected text is delivered
keep_warm = true          # keep the STT model resident between dictations (faster)
# audio_source = "…"      # optional PipeWire node; omit for the default input
vocabulary = ["PipeWire", "Parakeet"]   # exact-spelling terms fed to postproc + cloud STT

[stt]
model = "parakeet-tdt-0.6b-v3-int8"   # local registry name (see below)
# endpoint = "https://api.openai.com/v1/audio/transcriptions"  # cloud override
# model    = "gpt-4o-mini-transcribe"
# api_key_id = "openai"

[postproc]
enabled = true            # false = pass the raw transcript straight through
endpoint = "http://localhost:11434/v1"    # OpenAI-compatible endpoint
model = "qwen3:8b"        # any model your endpoint serves
timeout_ms = 30000
instructions = """You are cleaning up a dictated transcript. Remove filler
words and false starts (such as um, uh, like, you know) and repeated words.
Add correct punctuation, capitalization, and spelling. Keep the speaker's
meaning and all meaningful words. Output only the corrected text, with no
preamble."""
```

## `[stt]` — transcription

**Local (default, offline, $0).** `model` is a registry name; install the
weights with `cantrip models pull`:

| Model | Notes | Gauntlet WER |
|---|---|---|
| `parakeet-tdt-0.6b-v3-int8` | Fastest local; ships by default | 0.123 |
| `canary-1b` | Most accurate free local (0.9 s); weights are gated on Hugging Face — place the four ONNX/tokenizer files in `~/.local/share/cantrip/models/canary-1b` to use | 0.083 |

**Cloud.** Set `endpoint` (an OpenAI-compatible `/v1/audio/transcriptions`
URL), `model`, and `api_key_id`. Store the credential id ahead of time:

```sh
cantrip key set openai    # prompts for the key; stored in the OS keyring
```

From the gauntlet, `gpt-4o-mini-transcribe` is the best accuracy-per-dollar
cloud model (~WER 0.065 at ~$0.0003/clip).

## `[postproc]` — cleanup

Gives you the biggest perceived-quality lever. The whole cleanup behavior is
the `instructions` text, so it is safe to experiment from the config alone:

- **Disfluency removal.** The default instructions permit deleting filler
  words, false starts, and repetitions. Earlier wording that said "never
  remove any word" is what let "um"/"uh"/repeats survive.
- **Local.** Default endpoint is Ollama at `localhost:11434`.
  `qwen3:8b` is the gauntlet recommendation (free, fast, neutral on accuracy);
  any `ollama list` model works.
- **Cloud.** Point the endpoint at any OpenAI-compatible provider and set
  `api_key_id`. A precedent from the gauntlet: OpenRouter routing of
  `qwen/qwen3-14b` / `qwen/qwen3-30b-a3b-instruct-2507` (the latter is the
  only postproc model in the matrix that improved mean WER).
- A postproc failure never drops a dictation: the raw transcript is used.

## `vocabulary`

Exact-spelling terms injected into the postproc system prompt (and the cloud
STT prompt) so technical names like `PipeWire` survive cleanup. Add jargon
you dictate often.

## `injection`

- `auto` – use `wtype` when a compatible compositor is detected, else
  `ydotool`, else the clipboard.
- `type` – type directly (never touches the clipboard).
- `clipboard` – put the text on the Wayland clipboard for you to paste.
