# Configuration guide

Cantrip reads one TOML file. `cantrip config path` prints its location
(default `~/.config/cantrip/config.toml`), `config show` prints it,
`config init` creates it with defaults, and `config edit` opens it in
`$EDITOR`. `cantrip settings` opens a window you can keep open to view
and adjust the common settings; its Save button writes the file back
(with comments preserved) and reloads the running daemon.

Settings can repair a file that parses as TOML but fails Cantrip validation:
it loads the actual values, explains the validation error, and enables Save
after correction. Malformed TOML keeps structured saving disabled; the explicit
Repair flow edits the original text and keeps a backup. External file edits
are not silently overwritten, and a pending reload disables overlapping saves.

Use Save or `cantrip reload` after changing configuration. Accepted processing
jobs keep their STT, cleanup, and delivery snapshot; reload affects subsequent
operations, not in-flight inference. `audio_source` applies to the next capture.
`keep_warm` governs model preload at startup and needs a daemon restart.

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
# reasoning_effort = "low" # optional: low | medium | high | none (for providers supporting reasoning.effort)
instructions = ""         # optional extra style guidance

[hud]
labels = false             # show stage labels continuously
# reduced_motion = true    # true/false override; omit to follow desktop preference
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

**Long recordings.** Remote PCM/IEEE-float WAVs are split before upload, keeping
sample bytes, rate, channels, and format intact. Native Cantrip capture uses
low-energy splits around 30 seconds (up to 33 seconds); other PCM formats split
on frame boundaries at most 30 seconds apart. Every multipart request, including
its headers and vocabulary, is capped at 24,000,000 bytes. Short bounded files
are sent unchanged. Results are joined in order, with one cleanup/delivery step.

This avoids OpenRouter's documented 25 MB multipart cliff without a recording
cutoff or codec process. Providers can still impose smaller limits or time out.
Chunk seams can affect recognition. The remote file reader supports
little-endian RIFF/WAVE PCM and IEEE float, including extensible variants.
Compressed WAV encodings, RF64/RIFX, and multiple data chunks must be converted
to standard PCM WAV before `cantrip transcribe`. Local Parakeet still requires
16 kHz mono PCM16.

**Automatic local fallback.** A cloud error (including unavailable credentials),
partial transcript, or empty recognition of nonempty audio triggers one whole-take
attempt with installed default Parakeet. A complete local transcript replaces
the cloud prefix; alternate passes are never concatenated. If local fallback
cannot complete, useful cloud text is preserved. Configured cleanup runs once on
the selected transcript, not on each attempt. Cancellation prevents starting a
fallback or subsequent cleanup request; already-running requests may finish.
There are no automatic downloads or switches to another cloud provider.
Install the safety-net model with `cantrip models pull`; `cantrip doctor` reports
readiness. With `keep_warm = true`, cloud-configured daemons preload local Parakeet
too. External files outside Parakeet's native audio format can still fail locally;
retain or convert the original file.

**Recovery.** `cantrip recover --id ID --clipboard` retries the selected retained
recording with configured STT while only copying the result. Omit `--id` to select
the newest unresolved take with retained audio. `cantrip recover --id ID --local --clipboard` uses
installed default Parakeet and skips cleanup for that job, without rewriting
this file or changing subsequent dictations. Install the model explicitly with
`cantrip models pull` if needed. `cantrip transcribe --local <wav>` provides the
same local recognition/cleanup override for files. Separately opted-in telemetry
remains count-only and enabled; `--local` is not an all-network-off switch.

`cantrip recordings` lists capture times, recording IDs, durations, and artifact
availability after daemon restart. `cantrip actions` exposes the same metadata,
explicit copy/recovery, and confirmed Forget without showing transcript text.
Operational details remain in `~/.local/state/cantrip/daemon.log`; transcripts
never appear there. Every stopped take has independent retained audio; unrelated
successes or later failures cannot erase it. Successful recovery marks it resolved
without deleting audio. Cancellation retains capture before skipping inference.
Graceful shutdown retains live capture; startup imports trusted finalized runtime
leftovers. Dismissal and Copy never delete artifacts. Only confirmed Forget removes
retained audio and incomplete text; complete archived text remains.

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
  `api_key_id`. The committed behavior matrix recommends
  `google/gemini-3.6-flash` with default reasoning: it kept all 21
  role-sensitive cases as transcript text, averaged 2.9 seconds, and cost
  about $0.003 per cleanup. The primary operator currently runs
  `google/gemini-3.7-flash` through OpenRouter with `passes = 1` as an
  operator override, not the gauntlet winner.
- **`reasoning_effort`.** Optional reasoning effort level (e.g. `low`, `medium`,
  `high`, `none`) for OpenAI-compatible providers that support
  `reasoning.effort` (such as OpenRouter). Omitted from the request when
  unset; local endpoints ignore unknown fields.
- A postproc failure never drops a dictation: the raw transcript is used.

## Transcript history

Every successful STT result is archived locally, including empty and partial
results and results from `cantrip transcribe` or `cantrip recover`. The default
directory is:

```text
~/.local/state/cantrip/transcripts/
```

`$XDG_STATE_HOME` replaces `~/.local/state` when set. Each JSON record
links raw and post-processed text under one immutable take ID. It also records
the completion timestamp, source, audio duration, total pipeline latency, STT
model/backend/latency, cleanup model/status/latency/prompt version, and available
token and billing usage. The selected STT backend/model is recorded, with
`stt.fallback_from_model` identifying the cloud model when local fallback is used.
Pure local STT has zero API cost; any configured-cloud attempt leaves STT cost
unknown, even when local fallback succeeds. Post-processing
`reported_cost_usd` is stored only when the provider returns the charge; Cantrip
does not estimate cost from prices that can change later.

The directory is mode `0700`; files are mode `0600` and published atomically.
An archive write failure is reported but never drops a valid dictation.

This is sensitive plaintext history, retained until you delete it. It is not
written to operational logs, uploaded, indexed, summarized, or committed by
Cantrip. Review backup and home-directory sync policies before relying on it.
Audio remains even after successful delivery: about 1.92 MB/minute (115 MB/hour)
for native capture. Use confirmed Forget to reclaim selected audio. Disk failures
are explicit; active/runtime-only recordings do not survive reboot or power loss.

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

- `auto` – paste first with `wl-copy` and a native Wayland `Ctrl+Shift+V` chord.
  If the keyboard backend is unavailable before any input, copy-only is possible
  after a fresh safety check. If clipboard setup fails before handoff, native
  typing is possible. No fallback follows a potentially completed handoff or keys.
- `paste` – clipboard plus `Ctrl+Shift+V` only, with no typing fallback.
- `type` – native virtual-keyboard typing only; never reads or writes the
  clipboard. Newlines become spaces so transcript content cannot press Return.
- `clipboard` – explicitly copy for manual paste; no destination-focus permit
  or keyboard input. Clipboard contents are not restored afterward.

Automatic keyboard delivery requires verified focus, layer-surface and session
history on a direct Hyprland desktop (verified on 0.56.2) with logind. The guard
tracks focus changes, session locks, suspend, and compositor/session reconnection.
Unknown or changed state defers delivery, even in `auto`; choose explicit Copy
or clipboard recovery instead. `doctor` reports backend availability, not a
promise that the current destination is safe.

Text is fully composed before delivery. The paste chord preserves paragraph
breaks and uses `Ctrl+Shift+V` for terminal compatibility. Compositor/helper
acknowledgement does not prove the destination application accepted the text.
An uncertain outcome may have changed the clipboard or sent some keys; inspect
the destination before retrying. Cantrip never retries an uncertain handoff.

## `[hud]` — passive status

`labels = true` keeps stage text visible. `reduced_motion = true` or `false`
overrides the desktop preference; omit it to follow the desktop. The HUD never
takes focus or accepts pointer input. Waveform and chunk progress come from the
daemon's measurements; a stale connection is shown as unknown, not Ready.
HUD, Actions, and Settings use the active Omarchy palette when available.

## Opt-in telemetry (`[telemetry]`)

Cantrip can export one Langfuse trace per dictation for latency and quality
analysis. The repo rule has no exception here: traces carry character counts,
durations, model names, backend names, and error classifications only — never
transcript text, never audio. Tracing is off by default and adds no network
traffic until you enable it.

```toml
[telemetry]
enabled    = true
endpoint   = "https://us.cloud.langfuse.com/api/public/otel/v1/traces"
public_key = "pk-lf-..."            # project public key (not a secret)
api_key_id = "langfuse"             # keyring entry holding the secret key
```

Store the secret key with `cantrip key set langfuse` (paste the `sk-lf-...`
value when prompted). Keys never live in files or git. When enabled, the
daemon exports from a background thread after each job settles; a full queue
or an export failure only logs a warning and never affects dictation.
`cantrip doctor` reports the telemetry state honestly, including whether the
keyring entry is present.

The same `[telemetry]` block gates `eval`'s optional Langfuse dataset and
score publishing (`cargo run --example eval -- langfuse`); that path uses the
eval harness's own public corpus and never operator dictation. See
`docs/EVALUATION.md`.
