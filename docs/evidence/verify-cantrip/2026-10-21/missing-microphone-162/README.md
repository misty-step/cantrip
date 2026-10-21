# Missing selected microphone: issue 162

Built from `eae6bc6447404050d198a70397a3fbea67993219` plus this PR.
No installation, desktop service restart, paid provider, keyring access, physical
microphone capture, or operator clipboard/history access was used. Owner sign-in
is n/a: Cantrip has no accounts. All sessions were torn down.

## Before and after

The tightened `verify-cantrip journey breaks` failed on the installed v0.1.8
baseline: doctor reported ready, start accepted the missing source, and a new
history artifact appeared. The inherited no-fallback safety net stayed enabled.
[Before report](before-breaks.json).

The release build passed all five verification-skill journeys: transcribe,
dictate, recover, breaks, and windows (50 checks, no known failures).
[After report and binary checksum](after-all.json).
[Actual doctor/start command output](missing-source-commands.json).
The missing-source attempt launched no recorder and left accepted history
byte-for-byte unchanged. Normal public-speech capture reached the real isolated
Wayland clipboard and durable WAV; local recovery, cancellation and explicit
Forget also passed.

```sh
cargo build --release --locked
V=.agents/skills/verify-cantrip/bin/verify-cantrip
# Use a new short private RUN under ~/.cache/tmp/vc.
$V up --run "$RUN" --binary target/release/cantrip
$V doctor --run "$RUN"
$V journey --run "$RUN" all
$V down --run "$RUN"
```

## Inherited PipeWire properties

[Live stream observations](inherited-properties.json) cover both braced and bare
SPA properties, including a quoted description containing braces and a nested
object. Each private daemon inherited `node.dont-fallback = false`,
`node.dont-reconnect = false`, `target.object` and `PIPEWIRE_NODE` pointing at
another private microphone. Config selected its own private microphone.

`pw-dump` showed the configured source's object serial, both safeguards true,
and the unrelated description, resampling quality and nested value intact.
Graph links contained only the configured private microphone. Both variants
transcribed the public speech into the isolated clipboard and retained audio.
Inherited properties are passed to pw-record's native parser before Cantrip's
safeguards; PIPEWIRE_PROPS is removed only from that selected-source child.

## Checks and limits

`RUST_TEST_THREADS=4 TMPDIR=~/.cache/tmp/cantrip-162-tests ./scripts/check` passed
the complete repository gate, including the accepted-PCM startup regression.
An initial unrestricted parallel run timed out in six existing handoff tests;
the isolated handoff rerun and complete four-thread run passed.

The read-only harness verification-policy probe reported pre-existing baseline
failures: the tracked legacy instruction alias and missing account-policy words
in the existing verification skill. This PR adds neither an instruction alias
nor a new verification skill. The application has no account/login surface.

The separate T3 Live microphone function also lacks no-fallback safeguards and
needs recorder-EOF handling to avoid leaving a paid session waiting. That
integration follow-up is outside this Linux dictation PR; no live paid session
was opened. macOS runtime proof is outside this Linux-only change.
