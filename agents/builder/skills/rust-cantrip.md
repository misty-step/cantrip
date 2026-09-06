# Cantrip implementation notes

- Keep behavior in its owning module: daemon owns state, inject owns delivery, and HUD is read-only.
- Keep one std-thread + mpsc worker; do not add an async runtime or a second work ledger.
- Injection modes stay distinct: Auto may fall back; Paste uses `wl-copy` then one `Ctrl+Shift+V`; Type uses `wtype`/`ydotool` and never touches the clipboard; clipboard mode only copies.
- HUD phases stay honest. Draw determinate multi-chunk fill only from measured `N/M`; do not fabricate progress.
- Keep secrets in the OS keyring via `cantrip key`; never put them in files or logs.
- Use `anyhow::Context` at fallible boundaries and reserve `unwrap()` for tests.
