# ADR 0011: Chunk long local STT audio before Parakeet inference

Date: 2026-08-07. Status: accepted.

## Problem

A five-minute live dictation ended with `Transcription failed` and no
clipboard content. The daemon log showed Parakeet ONNX failing inside the
encoder self-attention path:

```text
Attempting to broadcast an axis by a dimension other than 1. 77 by 5077
```

Shorter takes (up to ~180s) had succeeded earlier in the same session. The
local path fed the entire WAV to one `ParakeetModel::transcribe_with` call,
so a long monologue hit a hard model/graph limit and the worker returned a
generic failure. The HUD flashed "Transcription failed" with no cause, and
the daemon log lived only on the hub process's PTY — easy to lose after a
detach or reboot.

## Decision

1. **Chunk local STT.** `Transcriber::transcribe_wav` splits long audio
   with a local energy-adaptive chunker (30s target, 3s low-energy search,
   0.5s minimum residual) and runs each chunk through Parakeet. The
   chunker lives in cantrip so dependency log lines cannot print
   transcript text. Short audio still becomes one chunk. Remote STT is
   unchanged (the endpoint owns its own limits).
2. **Classify failures for the operator.** `stt::classify_failure` maps
   structural error text (broadcast/axis, HTTP status, timeout, unreadable
   WAV) to a short notice. The daemon writes that notice into `last` and
   the HUD flash; full error detail stays in the log (no transcript body).
3. **Durable daemon log.** When `cantrip daemon` starts, tracing tees into
   `~/.local/state/cantrip/daemon.log` as well as stderr, so a detached
   hub session still leaves an auditable trail that survives reboot.

## Why not alternatives

- Cap recording length: hides the product limit behind a silent cutoff and
  throws away spoken content the operator already produced.
- Use `transcribe-rs` EnergyAdaptiveChunked as-is: it `log::info`s each
  chunk's text, which violates the no-transcript-logging rule whenever a
  log bridge is active. A local chunker keeps the same energy split idea
  without that side channel.
- Keep the generic "Transcription failed" notice: forces the operator to
  dig logs for every length cliff.

## Consequences

- Five-minute (and longer) local dictations should produce text again,
  with a small join seam risk at chunk boundaries.
- Operators can run `cantrip status` and read `last:` for a cause, and
  inspect `~/.local/state/cantrip/daemon.log` for the full structural error.
- The 30s target is conservative relative to the longest known-good
  single-pass (~180s). Raise it only with a measured encoder pass that
  stays under the ONNX cliff.
