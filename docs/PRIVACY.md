# Privacy and retained data

Cantrip transcribes on your CPU by default. Cleanup and telemetry are disabled
by default; model weights require a separate, deliberate download. **Local does
not mean ephemeral:** stopped microphone recordings and transcript history stay
on your machine until you deliberately remove them.

Before your [first dictation](USAGE.md#first-dictation), decide whether this
retention is appropriate for the material you will speak. Avoid sensitive text
in a trial, and review home-directory backup and sync policies.

## What leaves the machine

| Choice | Network and content boundary |
|---|---|
| Default local transcription | Installed Parakeet runs on CPU. Dictation audio and text are not sent to a provider. |
| Model download | `models pull` downloads the registered model from `blob.handy.computer` and verifies its pinned checksum. It does not upload recordings. There is no automatic model download during dictation or fallback. |
| Cloud transcription | Setting `[stt].endpoint` sends the selected recording's audio, model selection, and configured vocabulary to that endpoint. |
| Local cleanup | An explicitly configured local endpoint receives transcript text, cleanup instructions, and vocabulary. Its own logging and networking policies are separate from Cantrip's. |
| Cloud cleanup | Enabling a remote `[postproc].endpoint` sends transcript text, instructions, and vocabulary to that provider, even when speech recognition was local. |
| Opt-in telemetry | Enabling `[telemetry]` sends operational metadata to the configured Langfuse endpoint, never audio or transcript text. |

Cloud providers have their own retention and account policies. Cantrip does not
make those policies local by using an OpenAI-compatible API. Review the endpoint
and model before [enabling a cloud feature](CONFIGURATION.md).

If configured-cloud transcription fails, returns partial text, or recognizes
nothing from nonempty audio, Cantrip makes one whole-take attempt with the
**already installed** default local model. It never silently downloads weights
or switches to another cloud provider. That fallback cannot undo audio already
sent to the configured provider, and it keeps your configured cleanup policy.

An explicit `recover --local` or `transcribe --local` uses installed Parakeet
and disables cleanup **for that operation**. It does not change your saved
configuration or disable separately opted-in telemetry. `--local` is therefore
not an all-network-off switch. To keep content local, use local STT and leave
remote cleanup off; leave telemetry disabled as well when you want no telemetry
traffic. Downloads and software updates are separate network actions.

## Storage locations

Run all commands as the same ordinary user with the same XDG environment.
Changing these directories for one terminal or service can make existing data
appear missing; it does not migrate or delete it.

| Data | Default location | Override |
|---|---|---|
| Configuration | `~/.config/cantrip/config.toml` | `$XDG_CONFIG_HOME/cantrip/config.toml` |
| Downloaded models | `~/.local/share/cantrip/models/` | `$XDG_DATA_HOME/cantrip/models/` |
| Transcript JSON and retained WAV audio | `~/.local/state/cantrip/transcripts/` | `$XDG_STATE_HOME/cantrip/transcripts/` |
| Operational log | `~/.local/state/cantrip/daemon.log` | `$XDG_STATE_HOME/cantrip/daemon.log` |
| Control socket and in-flight audio | `$XDG_RUNTIME_DIR/cantrip/` | Falls back to `/tmp/cantrip-$UID/cantrip/` when the user runtime directory is unavailable |
| API credentials | OS Secret Service keyring | Referenced by credential id, not stored in the TOML file |

Runtime storage is normally a per-user tmpfs directory. Do not assume the `/tmp`
fallback or a customized runtime directory is memory-only. The history directory
is owner-only (`0700`), and retained files are owner-only (`0600`). These are
access permissions, **not encryption**. Plaintext audio and text may also be
captured by your backups, snapshots, filesystem, clipboard manager, or other
software running as you.

## Recording lifecycle

Before transcription, every stopped microphone take is retained under its own
recording ID with recovery audio. This includes successful, cancelled, empty,
partial, failed, and undelivered takes. A later failure or an unrelated success
cannot replace another recording. Successful delivery or recovery marks a take
resolved without deleting its audio. There is no silent expiry.

Native 16 kHz mono PCM16 audio uses about **1.92 MB per minute (115 MB per hour)**.
The Actions window shows retained recordings; use its **Include completed
history** option to find resolved takes as well. [Copy and recovery](USAGE.md#recovery)
always operate on a selected recording, not an interchangeable failure slot.

Graceful shutdown stops and retains live capture. On startup, Cantrip imports
trusted, finalized runtime leftovers under their original IDs. Runtime originals
are consumed only after matching durable audio is confirmed. Legacy
`last-failed.wav` and `last-transcript.txt` migrate independently and
idempotently, with originals consumed only after durable publication.

Storage failures are reported. Active or runtime-only audio can be lost after a
reboot, power failure, or forced termination; neither successful recognition nor
an idle status proves a durable save. An archive write failure is surfaced but
does not discard otherwise valid text intended for delivery. Inspect storage
warnings before recording more or relying on recovery.

## Transcript history

Every successful STT result is archived locally, including empty and partial
results and results from `transcribe` or `recover`. A JSON record links raw and
post-processed text under one stable take ID. It also records:

- completion time, source, audio duration, and pipeline latency;
- selected STT model, local/cloud backend, latency, and completeness;
- the original cloud model in `stt.fallback_from_model` when local fallback wins;
- cleanup model, status, latency, passes, prompt version, custom instructions,
  and available token and provider-reported billing usage.

Pure local STT has zero API cost. A configured-cloud attempt leaves STT cost
unknown even when local fallback succeeds. Cleanup `reported_cost_usd` is stored
only when the provider reports the charge; Cantrip does not estimate billing
from mutable price lists.

This is sensitive plaintext history. Cantrip does not automatically upload,
index, summarize, commit, or turn it into evaluation fixtures. A retry preserves
usable prior text until a complete replacement is available; the recording ID
remains the same. History is not an immutable record of every retry.

To inspect raw/cleaned pairs locally with `jq`:

```sh
history="${XDG_STATE_HOME:-$HOME/.local/state}/cantrip/transcripts"
jq -s 'map(select(.postproc.status == "applied") |
  {session_id, raw_transcript, postprocessed_transcript, postproc})' \
  "$history"/*.json
```

This command deliberately prints private text into your terminal. Do not paste
its output into an issue, public log, or repository without reviewing and
redacting it.

## What Forget deletes

**Forget retained recording** in Actions requires confirmation. The equivalent
CLI operation is:

```sh
"$HOME/.local/bin/cantrip" forget RECORDING_ID --yes
```

Choose the exact ID from `recordings`; wait for the operation's outcome rather
than treating command acceptance as completed deletion. Forget:

- removes that take's retained WAV audio;
- removes incomplete transcript text and clears its pending recovery marker;
- removes quarantined corrupt-record copies for that take;
- **keeps complete archived transcript text** and marks it resolved.

Dismiss only acknowledges a notice. Copy, successful delivery, recovery, and
cancellation do not delete retained artifacts. Forget is not “erase everything
I dictated,” does not empty the clipboard, and cannot retract content already
pasted into an application or sent to a provider.

There is no native command to erase complete archived transcript text. If you
also want that text removed, stop the daemon through its
[existing startup owner](DESKTOP.md#stop-update-and-remove), identify the
selected take's JSON file in your actual history directory, and remove only
that reviewed file yourself. Account separately for backups, exported text,
clipboard history, and provider retention. File deletion is not a secure-erasure
guarantee. Do not remove whole configuration, model, or state directories as a
troubleshooting shortcut.

## Logs, credentials, and telemetry

Operational logs and normal IPC/status output do not include transcript text.
They report counts, timing, models, outcomes, and error classifications. The
explicit text-output command is `cantrip transcribe`; its stdout is the
transcript, with diagnostics on stderr. Actions and `recordings` show metadata
rather than transcript content.

Store credentials interactively with `key set ID`; only the credential id
belongs in configuration. Cloud features require an unlocked Secret Service
keyring and the correct user-session D-Bus connection. Do not put keys in unit
files, command lines, source control, or screenshots. Binary installation,
update, rollback, and uninstall do not remove keyring entries; `key rm ID` is a
separate deliberate action.

Opt-in telemetry carries character and token counts, durations, model/backend
names, completeness/delivery metadata, and coarse error classifications. It
never carries audio, transcript text, or provider error bodies containing text.
Export failures or a full queue warn without blocking dictation. See
[telemetry configuration](CONFIGURATION.md#opt-in-telemetry-telemetry) for the
explicit opt-in. The evaluation harness's optional publishing uses its own
public corpus, not private dictation history.

## Clipboard and destination applications

Paste delivery and explicit clipboard actions replace the clipboard without
restoring its previous contents; restoration would race other Wayland users of
the clipboard. Clipboard managers may retain a separate copy. Strict `type`
mode never reads or writes the clipboard and changes newlines to spaces.

Cantrip guards automatic keyboard delivery against changed or unverifiable
focus/session history. A compositor or helper acknowledgement is not proof that
the destination application accepted the text. Inspect the destination before
retrying an uncertain handoff to avoid duplicate text. See
[delivery outcomes](USAGE.md#understand-the-outcome) and
[desktop support](DESKTOP.md#supported-desktops).
