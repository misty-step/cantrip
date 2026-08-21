# Rust / Cantrip style

Project commands and invariants live in [`AGENTS.md`](../../../AGENTS.md) — read
that first. This file only keeps forest-builder style that is not already
stated there.

- Prefer existing modules: `daemon` owns state, `inject` owns delivery, `hud` is read-only.
- HUD: no fake progress; multi-chunk fill only from measured `N/M`.