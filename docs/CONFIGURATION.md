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
injection = "auto"        # auto | paste | type | clipboard — how corrected text is delivered
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
passes = 2              # cleanup rounds: a second pass re-reads and fixes residuals
instructions = """Fix speech recognition errors, such as dropped letters,
missing spaces between words, and truncated acronyms. Remove filler words,
false starts, and repeated words. Add correct punctuation, capitalization,
and spelling. Keep the speaker's exact meaning. Output only the corrected
text."""
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
- **`passes`.** The cleanup runs `passes` rounds in a chain (default 2). The
  first round uses `instructions`; every later round is a focused proofread
  pass that re-reads the output and fixes residual speech-recognition errors
  the earlier round left (e.g. a truncated acronym like `AP` for `API`).
  A single pass of a small model reliably misses a few errors on longer
  dictations, so 2 is the quality default; set 1 for minimum latency.
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

- `auto` – the default: copy to the Wayland clipboard and send one `Ctrl+V`
  (paragraph breaks are preserved), falling back to `wtype` typing, then
  `ydotool`, then clipboard-only if any backend or shortcut is unavailable.
- `paste` – copy and `Ctrl+V` only, no typing fallback; paragraph breaks are
  preserved.
- `type` – type directly (never touches the clipboard; newlines are flattened
  to spaces because typing them would send a Return key, which submits in
  chat apps).
- `clipboard` – put the text on the Wayland clipboard for you to paste.

Pasted or copied text stays on the clipboard, so you can paste it again by
hand. Injection is atomic: nothing is typed into a live window until the whole
text is ready, and the single paste keypress cannot be interrupted by losing
focus mid-composition like long typing streams can. Dictating into a terminal
is the one case for `type` (`Ctrl+V` is not paste there).
