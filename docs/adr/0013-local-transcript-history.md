# ADR 0013: Owner-private local transcript history

Date: 2026-08-13. Status: accepted.

## Problem

Cantrip keeps only `last-transcript.txt`. That supports immediate recovery but
cannot answer longitudinal questions about transcription quality, post-processing
behavior, or day-to-day failure modes. The evaluation corpus therefore depends on
manually remembered examples instead of representative production usage.

Transcript history is also sensitive. Persisting it invisibly in ordinary logs,
cloud observability, or the repository would violate Cantrip's local-first privacy
posture.

## Decision

Every successful STT result, including empty and partial results, is saved as one
immutable JSON record under:

```text
$XDG_STATE_HOME/cantrip/transcripts/
```

The default is `~/.local/state/cantrip/transcripts/`. XDG defines state home as the
location for persistent action history and logs. Audio remains ephemeral except for
the existing single failed-WAV recovery copy.

Each record contains:

- a schema version, session id, completion timestamp, and source (`dictation`,
  `recover`, or `transcribe`);
- audio duration and total STT-plus-cleanup pipeline latency;
- STT model, local/cloud backend class, latency, partial-result flag, and API
  cost (`0` for local inference; omitted when a cloud backend does not report it);
- post-processing status, model, latency, pass count, prompt version, custom
  instructions, token usage, and provider-reported API cost when available;
- the raw and post-processed transcript together in the same record. The latter
  is present only after successful cleanup.

Records use sortable unique filenames. Writes go to a new owner-only temporary file,
are synced, and are atomically renamed. The archive directory is verified as a
non-symlink directory owned by the current user and forced to mode `0700`; records
use mode `0600`.

Schema 2 adds audio duration, pipeline latency, token usage, and cost. Cost is
never reconstructed from mutable pricing tables: `reported_cost_usd` is present
only when the compatible provider returns an authoritative per-request charge.
Token counts may be available without cost. Multi-pass cleanup aggregates usage
and reports a cost only when every pass supplies one.

An archive failure never discards or blocks a valid dictation. The daemon reports the
failure without transcript content and continues delivery. Full STT failures have no
raw transcript to archive and retain the existing `last-failed.wav` behavior.

History is retained until the operator deletes it. Cantrip does not upload, summarize,
index, redact, or commit these records automatically. The directory may contain
passwords, private names, dictated messages, and other secrets; backup and sync tools
must treat it accordingly.

## Evaluation workflow

Production history is evidence, not automatically ground truth. Operators may inspect
or aggregate it locally. A production record becomes a repository eval only after
explicit selection, correction, redaction or synthetic replacement, category labeling,
and review. Private history files never enter version control directly.

Decision-grade eval runs keep the corpus version, prompt version, model and reasoning
configuration, per-case outputs, scorers, latency/cost metrics, and immutable run
metadata. Typical cases, known failures, adversarial cases, and a held-out set remain
separate enough to avoid optimizing only for previously observed examples.

## Consequences

Cantrip gains a durable local evidence trail suitable for pairwise raw-versus-cleaned
review and future local analytics. Disk usage grows with dictation volume, and the
operator assumes custody of highly sensitive text. Retention controls, search UI, and
automatic eval promotion are separate decisions; this ADR adds no database, cloud
service, background indexer, or second transcript log.
