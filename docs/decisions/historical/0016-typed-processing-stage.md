# ADR 0016: Processing stage remains typed across the IPC boundary

Date: 2026-08-15. Status: accepted.

## Problem

The processing stage starts as `pipeline::Stage`, travels through the worker
channel, and remains typed in daemon state. The daemon then formats it as a
string for `WireReply`. Both public IPC views expose that string, and the HUD
reconstructs cleaning state and measured chunk progress with equality checks,
prefix checks, splitting, and integer parsing.

This gives the processing stage multiple owners. The pipeline defines and
formats it, IPC supplies legacy defaults, and the HUD parses and validates the
`transcribing N/M` grammar. Adding or changing a stage can therefore require
coordinated edits across the pipeline, daemon, IPC, and presentation code.

## Decision

`pipeline::Stage` is the single in-process representation of processing state:

- `Transcribing { chunk, total }` carries 1-based measured chunk progress;
- `CleaningUp` carries cleanup work;
- `Unknown(String)` retains an unrecognized stage from a newer daemon.

`Stage` implements the stable text conversion used by the existing flat socket
field. Parsing is total: known valid forms become typed variants, while
malformed and future values become `Unknown` instead of rejecting the complete
status reply. A missing legacy processing stage defaults to single-chunk
transcribing.

`CommandReply` and `StatusSnapshot::Processing` expose `Stage`, not `String`.
The daemon passes its existing typed stage into `WireReply`; only the private
wire representation stores text. The CLI formats `Stage` through its stable
wire display. The HUD matches typed variants and computes determinate fill only
for `Transcribing` values satisfying `1 <= chunk <= total` and `total > 1`.
`Unknown` retains the current safe presentation: indeterminate transcribing.

The optional command-reply stage remains supported. In particular, `recover`
continues to return `stage: "transcribing"`. Removing that field is a separate
protocol decision and is not part of this change.

## Why not alternatives

- **Delete stage telemetry:** violates ADRs 0010 and 0011. The HUD may show
  determinate progress only from measured multi-chunk STT, and status must
  expose `transcribing N/M`.
- **Add an IPC-specific processing-stage enum:** creates a second owner and a
  conversion seam without an independent concept.
- **Keep strings and add HUD parsing helpers:** moves the existing grammar but
  leaves caller knowledge intact.
- **Render `StatusSnapshot` directly and delete the HUD view state:** moves HUD
  animation history and conservative unknown-state behavior into rendering
  code. It does not remove those responsibilities.
- **Reject malformed or unknown stages:** breaks rolling daemon/client
  replacement and is stricter than the current safe indeterminate fallback.

## Preserved invariants and non-goals

The Unix-socket request strings, flat JSON field names, and existing stage values
remain unchanged. Legacy missing fields remain readable. Unknown stages do not
create false measured progress. Local-first behavior, transcript privacy,
capture, injection, recovery, daemon state ownership, the one warm worker, std
threads plus mpsc, and shutdown behavior remain unchanged.

This decision adds no protocol version, configuration, dependency, adapter,
thread, runtime component, or UI composition. It does not change settings
status display or define new processing stages.

## Verification

The implementation is accepted when:

- stage fixtures cover the three existing wire forms, missing legacy stage,
  invalid chunk bounds, malformed values, and an unknown future value;
- command fixtures prove `recover` retains its stage output;
- HUD tests prove multi-chunk progress is measured, single-chunk and unknown
  stages remain indeterminate, and cleaning completes an armed meter;
- the complete Rust test suite, formatting check, and Clippy with warnings
  denied pass;
- an isolated daemon answers command and status requests through their distinct
  client paths without changing their output shape.
