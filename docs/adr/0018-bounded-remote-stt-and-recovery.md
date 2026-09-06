# ADR 0018: Bound remote audio requests and preserve recoverable failures

Date: 2026-09-06. Status: accepted.

## Problem

An 888-second dictation produced a 28.4 MB WAV. The configured OpenRouter
transcription endpoint rejected the single multipart upload with HTTP 413.
The brief amber HUD notice left the operator unsure whether their speech was
lost. Retrying the same recording used the same failing cloud configuration.

OpenRouter documents a 25 MB multipart limit and 60-second upstream request
timeouts in its [speech-to-text guide](https://openrouter.ai/docs/guides/overview/multimodal/stt).
These are request limits, not acceptable limits on dictation duration.

## Decision

- Bound remote WAV requests before uploading. Native 16 kHz mono PCM16 uses
  the existing low-energy planner around 30 seconds (up to 33). Other PCM and
  IEEE-float WAVs split on source frame boundaries at most 30 seconds apart.
  Every serialized multipart request, including headers and vocabulary, is
  capped at 24,000,000 bytes. Preserve source samples and format headers;
  safe short files remain byte-identical. Rebuilt chunks omit unrelated
  metadata and include required float `fact` counts. RF64/RIFX, compressed
  WAV encodings, and multiple data chunks are rejected rather than split
  incorrectly; convert these external files to ordinary PCM first.
  Native planning still uses one f32 per source frame, released before upload
  buffers are allocated. Other formats need only metadata, ranges, and a
  bounded request buffer. `hound` is a dev-dependency for independent decoding
  tests, not a production codec.
- Reuse the existing complete/partial transcription outcome and measured
  `transcribing N/M` progress for both backends. Join results in order and
  perform cleanup and injection once, after transcription settles. No
  per-chunk paste, speculative progress, automatic provider switch, or blind
  retry of rejected requests.
- Preserve the full WAV on partial as well as complete STT failure. Deliver
  available partial text with an explicit warning, never a success flash.
  An unrelated successful dictation must not erase the recovery slot. Only
  complete nonempty recovery consumes it after history or replay-file
  persistence succeeds. The next failed dictation may replace it, retaining
  the existing one-slot privacy contract. Atomic private staging files protect
  the previous audio/replay file from interrupted replacement.
- Add `recover --local --clipboard`: explicitly use the installed local model
  without cloud cleanup or persistent configuration changes, and copy rather
  than typing into whichever application has focus. Add `transcribe --local`
  for the same local file workflow. Missing local models produce an actionable
  error rather than an unannounced download. Opted-in metadata telemetry is
  unchanged; local STT does not mean all network activity is disabled.
- Keep actionable error notices visible while idle until another operation
  replaces them. The live smoke test exposed that the existing HUD logged
  labels but drew only colored bars. Notices now use a readable 420×88 text
  panel, with the original 420×56 track preserved for normal states. Text uses
  the already-transitive `ab_glyph` renderer and bundled Hack font from
  `epaint_default_fonts`, now explicit dependencies; no host font installation
  or UI framework is added. Ordinary success, cancellation, and no-speech
  notices remain transient. Status lists retained audio and recovery commands,
  including after daemon restart.

## Alternatives considered

- **Limit recording duration:** destroys the long-form dictation workflow and
  risks discarding speech. Rejected.
- **Compress the whole recording:** adds codec/runtime dependencies and only
  moves the size threshold; it does not solve upstream inference timeouts.
- **OpenRouter base64 JSON offload:** provider-specific, adds encoding overhead,
  and still leaves other compatible endpoints and request timeouts unsolved.
- **Automatic local/provider fallback:** changes recognition quality, latency,
  and potentially privacy or cost without an explicit operator choice. Keep
  normal provider selection; make offline recovery a single command.
- **A durable queue of chunk jobs:** adds a second work ledger and restart
  semantics unnecessarily. The existing worker, transcript archive, and single
  failed-audio slot remain the owners.

## Tradeoffs and verification

Chunk boundaries can affect recognition despite low-energy splits; requests
add round-trip overhead. In exchange, upload size and inference work no longer
scale with total recording length. Other provider failures remain possible
and must preserve both existing text and recoverable audio.

Verification covers WAV sample coverage and bounded request sizes, ordered
results and progress, later-chunk failure, retained audio across unrelated
success, offline clipboard recovery without configuration changes, and the
actual CLI/HUD surfaces. Operational output contains counts and error classes,
never transcript or audio content.
