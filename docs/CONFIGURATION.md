# Configuration guide

Cantrip reads one TOML file. Examples use `"$HOME/.local/bin/cantrip"`; substitute
your installed path if different. In command descriptions, `cantrip` is shorthand
for that executable, not an assumption that installation changed `PATH`.

`cantrip config path` prints the file's location (normally
`~/.config/cantrip/config.toml`). `config show` loads the file, applies defaults,
validates it, and serializes the resulting effective configuration; it does
**not** print the original file/comments or query the running daemon's current
snapshot. `config init` creates the annotated defaults only when the file does
not exist. `config edit` opens the file in `$EDITOR`. Do not reinitialize an
existing configuration during updates.

`cantrip settings` opens a window for common settings. Save writes the file back
with comments preserved and reloads the running daemon.

Settings can repair a file that parses as TOML but fails Cantrip validation:
it loads the actual values, explains the validation error, and enables Save
after correction. Malformed TOML keeps structured saving disabled; the explicit
Repair flow edits the original text and keeps a backup. External file edits
are not silently overwritten, and a pending reload disables overlapping saves.

Use Save or `cantrip reload` after changing configuration. Accepted processing
jobs keep their STT, cleanup, and delivery snapshot; reload affects subsequent
operations, not in-flight inference. `audio_source` applies to the next capture.
`keep_warm` governs model preload at startup and needs a daemon restart.

This example keeps the local-first defaults: Parakeet is selected but its
weights must be [downloaded deliberately](USAGE.md#2-download-the-local-model-deliberately);
cleanup remains disabled with no cleanup model selected. Do not replace an
existing file wholesale with the example. Enable cleanup only after the
[endpoint, model, and any credential are ready](#postproc--cleanup).

```toml
injection = "auto"        # auto | paste | type | clipboard — how finished text is delivered
keep_warm = true          # preload the local STT model at startup; restart to apply
# audio_source = "…"      # optional PipeWire node; omit for the default input
vocabulary = ["PipeWire", "Parakeet"]   # exact-spelling terms fed to postproc + cloud STT

[stt]
model = "parakeet-tdt-0.6b-v3-int8"   # local registry name (see below)
# endpoint = "https://api.openai.com/v1"  # cloud API base; cantrip appends /audio/transcriptions
# model    = "gpt-4o-mini-transcribe"
# api_key_id = "openai"

[postproc]
enabled = false           # deliver raw recognition; cleanup is an explicit opt-in
endpoint = "http://localhost:11434/v1"    # does not install or start an endpoint
model = ""               # choose a model served by your endpoint before enabling
timeout_ms = 30000
passes = 1                # cleanup rounds; 2 adds a proofread pass (slower)
min_chars = 40            # skip cleanup under this length; 0 = never skip
# reasoning_effort = "low" # optional: low | medium | high | none (for providers supporting reasoning.effort)
instructions = ""         # optional extra style guidance

[hud]
labels = false             # true = continuous accessibility stage labels
# reduced_motion = true    # true/false override; omit to follow desktop preference
```

## Capture and model preload

Omit `audio_source` to use the default PipeWire input. Set it to a specific
PipeWire node only when you intend to select that microphone; it applies on the
next capture. A `doctor` report that finds `pw-record` is not proof that the
selected input records sound. Make an [attended trial](USAGE.md#first-dictation)
after changing it.

`keep_warm` controls local-model preload when the daemon starts. Changing it
requires a restart through the [existing startup owner](DESKTOP.md#keep-one-startup-owner),
not a second daemon. Models remain in the same data directory across
configuration changes; see [storage locations](PRIVACY.md#storage-locations).

## `[stt]` — transcription

**Local (default).** `model` is a registry name. The model is not bundled with
the binary; install its weights once with
`"$HOME/.local/bin/cantrip" models pull`. Installed local inference needs no
network and has no per-request API charge.

| Model | Availability |
|---|---|
| `parakeet-tdt-0.6b-v3-int8` | Default selection; download weights separately. The only current local registry model. |

**Cloud.** Set `endpoint` to an OpenAI-compatible API **base** URL (for example
`https://api.openai.com/v1`). Cantrip posts to `{endpoint}/audio/transcriptions`.
Also set `model` and `api_key_id`. Store the credential id ahead of time:

```sh
"$HOME/.local/bin/cantrip" key set openai   # prompts for the key; stored in the OS keyring
```

An unlocked Secret Service keyring and the same user-session D-Bus connection
must be available when the daemon uses that id. A configured id is not proof
that a key exists or the provider accepts it. Cloud recognition sends audio to
the chosen endpoint; review the [network/content boundary](PRIVACY.md#what-leaves-the-machine)
before opting in. Evaluated provider comparisons live in the
[evaluation guide](https://github.com/misty-step/cantrip/blob/master/docs/EVALUATION.md),
not in the default configuration.

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
Install the safety-net model deliberately with `models pull`; `doctor` reports
whether its installed files are available, not a successful inference. With
`keep_warm = true`, cloud-configured daemons preload local Parakeet too.
External files outside Parakeet's native audio format can still fail locally;
retain or convert the original file.

For selected-recording retries and the per-operation `--local` override, see
[recovery](USAGE.md#recovery). Explicit local recovery/file transcription skips
cleanup without rewriting this file, but does not disable independently opted-in
metadata telemetry. [Retained audio and plaintext history](PRIVACY.md) remain
local until deliberately removed.

## `[postproc]` — cleanup

Cleanup is **off by default**. Configure it only after ordinary local dictation
works:

1. Choose a local or remote OpenAI-compatible chat endpoint and a model it
   actually serves. Cantrip does not install or launch Ollama or download its
   models. For local Ollama, start that service and deliberately install the
   chosen model there before using Cantrip cleanup.
2. Set `[postproc].endpoint` and `model` in the existing configuration. If the
   provider requires a credential, store it with `key set ID` and set the
   matching `api_key_id`; never paste the key into the file.
3. Only then set `enabled = true`, save/reload, and make a harmless attended
   trial. Read `cleanup` in the settled outcome. `doctor` describes configuration
   and available prerequisites; it does not prove that the endpoint is running,
   the credential works, or the selected model is usable.

For example, an explicitly prepared local Ollama service could use:

```toml
[postproc]
enabled = true
endpoint = "http://localhost:11434/v1"
model = "qwen3:8b"
```

Apply these fields to your existing `[postproc]` table; do not add a duplicate
table or replace unrelated settings. A `--postproc clean` capture also needs
this usable endpoint/model, even when the saved default is disabled. The
[per-take controls](USAGE.md#everyday-controls) explain clean/raw shortcuts.

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
- **Local.** The default endpoint address is Ollama at `localhost:11434`, but
  its presence in the file does not mean a service is installed or running.
  `qwen3:8b` is a local option; use a model your endpoint actually serves.
- **Cloud.** Point the endpoint at an OpenAI-compatible chat provider and set
  its `api_key_id`. This sends transcript text to that provider even if STT is
  local. Choose deliberately using the provider's policies and your own
  [evaluation](https://github.com/misty-step/cantrip/blob/master/docs/EVALUATION.md).
- **`reasoning_effort`.** Optional reasoning effort level (e.g. `low`, `medium`,
  `high`, `none`) for OpenAI-compatible providers that support
  `reasoning.effort` (such as OpenRouter). Omitted from the request when
  unset; local endpoints ignore unknown fields.
- A postproc failure never drops a dictation: the raw transcript is used.

## Transcript history

Cantrip retains stopped microphone audio and sensitive plaintext transcript
history, including successful takes. The canonical
[privacy and history guide](PRIVACY.md#transcript-history) explains file locations,
record fields, local inspection, durability limits, and exact
[Forget semantics](PRIVACY.md#what-forget-deletes). This is retained data, not an
ephemeral cache; changing configuration does not erase it.

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

Default words are reserved for actionable exceptions: unavailable input,
cancellation, failed or partial outcomes, uncertain or deferred delivery, and
manual-paste feedback. There is no timed reveal, long-recording label or latched
caption. `labels = true` keeps stage text visible as an accessibility override.

Listening shows 60 independent signed PCM pixel columns, sampled every 100 ms.
A fixed 1.6× gain before square-root scaling makes quiet speech more visible
without changing the silence floor. Each side attacks quickly and releases more
slowly to silence; neighboring columns are never blended.

Transcription and finishing/cleanup use distinct pixel activity patterns. They
indicate indeterminate work, not a timer or completion estimate. Phase changes
transition from the last presented frame. Only measured multi-chunk reports
advance the center-row progress fill. A settled completion mark acknowledges
the delivery mechanism, not receipt by the destination application.

`reduced_motion = true` or `false` overrides the desktop preference; omit it to
follow the desktop. Reduced motion freezes indeterminate activity and presents
measurements directly. Stale or disconnected status stops live animation and
shows unknown, not Ready. The HUD never takes focus or accepts pointer input.
HUD, Actions, and Settings use the active Omarchy palette when available.
See [ADR 0021](https://github.com/misty-step/cantrip/blob/master/docs/adr/0021-signed-pixel-waveform.md)
for the rendering contract.

## Opt-in telemetry (`[telemetry]`)

Telemetry is off by default. Opting in can export one Langfuse trace per
dictation for operational analysis. It carries metadata, never audio or
transcript text; see the [privacy boundary](PRIVACY.md#logs-credentials-and-telemetry).
Enabling it is a separate network choice from local/cloud STT and cleanup.

```toml
[telemetry]
enabled    = true
endpoint   = "https://us.cloud.langfuse.com/api/public/otel/v1/traces"
public_key = "pk-lf-..."            # project public key (not a secret)
api_key_id = "langfuse"             # keyring entry holding the secret key
```

Store the secret key with `"$HOME/.local/bin/cantrip" key set langfuse` when
prompted. The public key is a project identifier; only the secret key belongs
in the OS keyring, never files or git. Export failures and full queues warn
without affecting dictation. Read the telemetry findings in `doctor`; a
configured exporter is not proof of a successful remote export.

The same `[telemetry]` block gates `eval`'s optional Langfuse dataset and
score publishing (`cargo run --example eval -- langfuse`); that path uses the
eval harness's own public corpus and never operator dictation. See
the [evaluation guide](https://github.com/misty-step/cantrip/blob/master/docs/EVALUATION.md).
