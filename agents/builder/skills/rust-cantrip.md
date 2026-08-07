# Rust / Cantrip style

- `cargo fmt` on every changed Rust file before finishing.
- `clippy --all-targets -- -D warnings` must stay clean.
- Prefer existing modules: daemon owns state; inject owns delivery; hud is read-only.
- Do not add an async runtime or a second work ledger.
- Injection modes: Auto may fall back; Type and Paste must not touch clipboard on failure once that policy is fixed.
- HUD: no fake progress; multi-chunk fill only from measured `N/M`.
- Secrets stay in the OS keyring via `cantrip key`, never in files or logs.
