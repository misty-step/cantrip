# Configuration guide

Cantrip reads one TOML file. `cantrip config path` prints its location
(default `~/.config/cantrip/config.toml`), `config show` prints it,
`config init` creates it with defaults, and `config edit` opens it in
`$EDITOR`. `cantrip settings` opens a window you can keep open to view
and adjust the common settings; its Save button writes the file back
(with comments preserved) and reloads the running daemon.

Settings can repair a file that parses as TOML but fails Cantrip validation:
it loads the actual values, explains the validation error, and enables Save
after correction. If the TOML syntax is malformed or the file cannot be read,
Settings never replaces it with defaults; it directs you to `cantrip config
edit` and keeps structured saving disabled until the file parses.

Changes apply immediately, no restart needed. The Save button (or `cantrip
reload`, for edits made directly to the file with `config edit`) makes the
daemon re-read the config in place. Each stage picks up the newest values at
its own boundary: `[stt]`, `vocabulary`, and `[postproc]` when a recording
stops, `injection` when the corrected text comes back, and `audio_source` on
the next recording. The one setting that genuinely needs a daemon restart is
`keep_warm`, which only governs model preload at startup.

This example opts into local cleanup. Fresh defaults use
`[postproc].enabled = false` and an empty cleanup model.

```toml
injection = "auto"        # auto | paste | type | clipboard — how corrected text is delivered
keep_warm = true          # keep the STT model resident between dictations (faster)
# audio_source = "…"      # optional PipeWire node; omit for the default input
vocabulary = ["PipeWire", "Parakeet"]   # exact-spelling terms fed to postproc + cloud STT

[stt]
model = "parakeet-tdt-0.6b-v3-int8"   # local registry name (see below)
# endpoint = "https://api.openai.com/v1"  # cloud API base; cantrip appends /audio/transcriptions
# model    = "gpt-4o-mini-transcribe"
# api_key_id = "openai"

[postproc]
enabled = true            # false = pass the raw transcript straight through
endpoint = "http://localhost:11434/v1"    # OpenAI-compatible endpoint
model = "qwen3:8b"        # any model your endpoint serves
timeout_ms = 30000
passes = 1                # cleanup rounds; 2 adds a proofread pass (slower)
min_chars = 40            # skip cleanup under this length; 0 = never skip
instructions = ""         # optional extra style guidance
```

## `[stt]` — transcription

**Local (default, offline, $0).** `model` is a registry name; install the
weights with `cantrip models pull`:

| Model | Notes | Gauntlet WER |
|---|---|---|
| `parakeet-tdt-0.6b-v3-int8` | Fastest local; ships by default (only local model in the registry today) | 0.123 |

**Cloud.** Set `endpoint` to an OpenAI-compatible API **base** URL (for example
`https://api.openai.com/v1`). Cantrip posts to `{endpoint}/audio/transcriptions`.
Also set `model` and `api_key_id`. Store the credential id ahead of time:

```sh
cantrip key set openai    # prompts for the key; stored in the OS keyring
```

From the gauntlet, `gpt-4o-mini-transcribe` is the best accuracy-per-dollar
cloud model (~WER 0.065 at ~$0.0003/clip).

## `[postproc]` — cleanup

The built-in prompt defines conservative transcript cleanup. Use
`instructions` only for extra style guidance. It is appended to the fixed
contract, so keep it short and avoid redefining the task.

- **Disfluency removal.** The built-in prompt removes filler sounds, false
  starts, and repeated words.
- **`passes`.** The cleanup runs `passes` rounds in a chain (default 1). Each
  later round is a focused proofread for residual speech-recognition errors,
  such as a truncated acronym. One pass avoids compounded drift and latency.
  Use 2 only after evaluating it on your own dictation corpus.
- **`min_chars`.** Skip cleanup when the raw transcript has fewer than this
  many characters (default 40). Short commands skip the cloud round-trip.
  Set `0` to always run cleanup when enabled.
- **Local.** Default endpoint is Ollama at `localhost:11434`.
  `qwen3:8b` is the free local recommendation; any `ollama list` model works.
- **Cloud.** Point the endpoint at any OpenAI-compatible provider and set
  `api_key_id`. The primary operator uses `google/gemini-3.7-flash` through
  OpenRouter with `passes = 1`. The last completed behavior matrix selected
  Gemini 3.6 Flash: it kept all 21 role-sensitive cases as transcript text,
  averaged 2.9 seconds, and cost about $0.003 per cleanup.
- A postproc failure never drops a dictation: the raw transcript is used.

## Transcript history

Every successful STT result is archived locally, including empty and partial
results and results from `cantrip transcribe` or `cantrip recover`. The default
directory is:

```text
~/.local/state/cantrip/transcripts/
```

`$XDG_STATE_HOME` replaces `~/.local/state` when set. Each immutable JSON file
links the raw and post-processed transcript under one session id. It also records
the completion timestamp, source, audio duration, total pipeline latency, STT
model/backend/latency, cleanup model/status/latency/prompt version, and available
token and billing usage. Local STT has zero API cost. Cloud STT cost is omitted
until its compatible response reports it. Post-processing
`reported_cost_usd` is stored only when the provider returns the charge; Cantrip
does not estimate cost from prices that can change later.

The directory is mode `0700`; files are mode `0600` and published atomically.
An archive write failure is reported but never drops a valid dictation.

This is sensitive plaintext history, retained until you delete it. It is not
written to operational logs, uploaded, indexed, summarized, or committed by
Cantrip. Review backup and home-directory sync policies before relying on it.

For example, inspect raw and cleaned pairs locally with `jq`:

```sh
history=${XDG_STATE_HOME:-$HOME/.local/state}/cantrip/transcripts
jq -s 'map(select(.postproc.status == "applied") |
  {session_id, raw_transcript, postprocessed_transcript, postproc})' \
  \"$history\"/*.json
```

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
