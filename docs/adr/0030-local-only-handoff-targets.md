# ADR 0030: Local-only handoff targets

Date: 2026-09-28. Status: accepted. Story: US-011.

## Context

The operator asked for a Cantrip shortcut that hands a spoken take straight to
his pile, a private capture inbox on this computer, the way `[handoff.kaylee]`
hands one to Kaylee. The pile's own rule is that speech to text runs on this
computer. On his desktop, `[stt]` names a cloud model and cleanup can be turned
on per take, so a plain handoff target would send pile audio to the cloud lane,
and a failed cloud pass would only then fall back to local Parakeet.

A per-invocation flag (for example `--stt local` on the shortcut) would keep
the rule only while every caller remembered it. The destination owns the rule,
so the target carries it.

## Decision

`[handoff.NAME]` gains `local_only` (default `false`). When a take starts for a
local-only target, its configuration snapshot (the same snapshot that fixes the
target for the take) replaces `[stt]` with the installed default local model
and turns cleanup off, as `cantrip recover --local` already does. With no
endpoint there is no cloud attempt and no fallback path. The model check runs
on that snapshot, so a missing local model refuses the take before recording,
and `--postproc clean` on a local-only target is refused before recording as
`handoff-local-only` instead of being silently dropped.

The target's command runs with `CANTRIP_LOCAL_ONLY=1`, and every other target's
command runs with that variable removed, so a receiver whose destination
requires local speech to text can refuse a take from a misconfigured target
instead of silently accepting a cloud transcript.

Every handoff command also gets `CANTRIP_TAKE_AUDIO`, the path of the take's
retained recording when it is available. The pile keeps its own copy of the
recording and makes its own transcript from it in a sandbox with no network,
because its rule is stricter than "no cloud lane": nothing that transcribes a
pile capture may be able to reach the network. Cantrip's local transcript still
drives the HUD and Cantrip's history; the pile does not use it.

## Consequences

- Other targets and desktop takes keep the configured lanes; changing `[stt]`
  or `[postproc]` never changes where a local-only take goes.
- A local-only take is only as fast and accurate as the local model.
- Recovery is unchanged: `cantrip recover` of a retained take uses the lane its
  caller chooses, so recover such a take with `--local`.
