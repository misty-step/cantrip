# ADR 0020: Automatic local completion and explicit audio deletion

Date: 2026-09-07. Status: accepted.

## Problem

Cloud STT returned HTTP 429 on chunk 2 of a six-chunk, 162-second recording.
Cantrip preserved the recording but required a manual local recovery operation.
Installed Parakeet completed the full take in about four seconds. Capture
cancellation, abandoned recorder cleanup, and short empty recognition also had
implicit deletion paths. Recording trust should not depend on those distinctions.

## Decision

Keep cloud STT as the configured preferred backend. On failure, partial output,
or empty recognition for nonempty audio, attempt the whole take once with the
installed default local Parakeet model. Do not retry a rate-limited provider,
contact a second cloud provider, or download a model implicitly. Keep the local
model warm when `keep_warm` is enabled, including with cloud STT configured.

Use a whole local pass rather than mixing independently recognized prefixes and
suffixes. This spends some repeated inference on the exceptional path but keeps
one coherent transcript and no chunk checkpoint ledger. Select one result,
then run configured cleanup once and deliver once. Cancellation prevents starting
fallback and later cleanup requests; an in-flight request may finish. Preserve
usable text when both attempts fail. Record the actual selected backend/model;
cloud attempts make total STT API cost unknown even if local fallback succeeds.
Reset measured progress for the local pass rather than pretending cloud progress
also measures local work. Report local fallback in the terminal outcome.

Only confirmed Forget deletes retained audio. Resolving successful delivery marks
a take resolved without unlinking its WAV. Cancellation stops transcription and
delivery, not retention. Empty recognition, including very short recordings, is
not proof that the operator said nothing. Preserve every stopped take before
inference using the existing owner-private archive. Runtime copies may be removed
only after confirming matching durable audio, never merely because text exists.

Use one take ID from capture start through recovery. A graceful shutdown stops
and retains live capture. Dropping a recorder stops it without deleting its WAV.
At startup import trusted finalized `rec-<id>.wav` runtime leftovers into the same
history, independently and idempotently; bad or conflicting files remain intact
and produce a storage warning. Do not automatically replay old takes into the
current desktop. Existing focus/session permits and uncertain-delivery rules
remain unchanged: safety takes precedence over pretending delivery succeeded.

## Alternatives

- **Bounded cloud retries/backoff:** useful for transient errors but adds latency
  and keeps completion dependent on the failed service. Local completion is
  already available and independent.
- **Per-chunk backend switching:** avoids repeated inference but introduces mixed
  recognition, prefix/suffix coverage bookkeeping, and more cancellation states.
- **Always prefer local STT:** maximizes availability but overrides the operator's
  chosen recognition quality even when the cloud works.
- **Durable queue of retry jobs:** adds another work ledger, replay policy, and
  restart scheduling. Per-take artifacts already represent recoverable work.

## Limits and operating cost

This supersedes ADR 0018's rejection of automatic local fallback and ADR 0019's
permission to remove audio after successful delivery. It does not promise success
through missing/broken models, invalid audio, disk failure, or unavailable capture
hardware. Active recordings remain in the runtime directory; power loss or reboot
before durable retention can lose that live audio. An unfinalized WAV is retained,
not falsely advertised as recoverable. No software can guarantee verbatim
recognition or prove an external application received injected text.

Retained 16 kHz mono PCM16 costs about 1.92 MB per recorded minute (115 MB/hour),
including successful takes. There is no silent expiry. Use selected-recording
Forget to reclaim audio; complete archived text remains. `doctor` reports whether
the installed local fallback is available.

## Verification

A 66-second public speech fixture reproduced the old behavior against a local
HTTP endpoint: first chunk accepted, second chunk returned 429, CLI exited 1 with
only the cloud prefix. The changed CLI sent the same two requests, ran all three
chunks through real installed Parakeet, and exited 0. All six occurrences of the
fixture's closing phrase survived, without the cloud prefix. The archive reported
local STT, the configured cloud model as fallback provenance, and unknown API cost.

An isolated PipeWire server and policy-only session manager exercised real
`pw-record` cancellation and graceful daemon shutdown. Both retained finalized
owner-only WAVs without transcription. A half-second silent capture remained
available after empty local recognition; it did not inherit the cloud error as
its terminal outcome. Restart imported a finalized orphan byte-for-byte under its
original take ID. Explicit Forget removed only the selected audio, preserving
complete text and unrelated takes. The isolated daemon had no Wayland compositor:
its clipboard handoff was reported uncertain and never retried; native successful
paste was not exercised or assumed.

Repository checks cover successful resolution retaining audio until Forget,
failed-fallback text preservation, cancellation between cleanup passes, empty
results, recorder drop, and trusted/idempotent restart imports. Smoke fixtures
used isolated XDG storage, never the operator's clipboard or recording history.
