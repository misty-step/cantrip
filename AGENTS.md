# Cantrip

Cantrip v2 is a macOS + Linux tray app for dictation and meeting notes. Its
present-tense intent is [PRODUCT.md](PRODUCT.md) (what and why) and
[DESIGN.md](DESIGN.md) (observable contracts, UI, checklist); never edit them
to fit code. How it is built is [ARCHITECTURE.md](ARCHITECTURE.md). SPEC.md,
VISION.md, `docs/` and `docs/decisions/` are v1 history: context, not
constraints.

Design principles (UX/DX/AX) live in [mistystack design](https://github.com/misty-step/mistystack/blob/master/skills/design/SKILL.md#principles); point there, don't copy them.

## Layout

- `crates/core` (`cantrip-core`): every rule a user can observe. No OS calls,
  no UI, no async runtime. OS boundaries are the traits in `ports.rs`.
- `crates/app` (`cantrip`): the egui UI (`ui/`), wiring (`main.rs`), HTTP
  (`net.rs`), single-instance IPC (`ipc.rs`), and per-OS adapters in
  `platform/{linux,macos}`. `cfg(target_os)` stays inside `platform/`.

## Contracts that are easy to break

- **Closed control surface.** The popover has 7 standing controls, plus 1
  contextual recovery row, and Settings has 6 controls in one pane. Add
  nothing. Labels come from `cantrip_core::surface`.
- **Delivery.** Every write or paste is addressed to the target captured at
  start (250 ms deadline, else `uncertain` forever), and focus is verified
  again just before dispatch.
  - Never send a keystroke to whatever holds focus.
  - Never retry a possibly completed handoff through the other tier.
  - `Inserted` needs a read-back edit delta.
  - Restore the clipboard only on a confirmed edit with the clipboard
    unchanged by identity (not text).
- **Speech.** Every OpenRouter request carries
  `provider.data_collection: "deny"`. A rewrite error, empty reply, truncation
  or 4 s timeout gives Raw. A transcription error keeps the audio staged and
  never falls back to local recognition. Local recognition only with no key or
  no connection, and only when guaranteed on-device.
- **Meeting folder is append-only.** Exclusive create only: no temp files,
  overwrites, renames, deletes or write-opens of existing files. Disjointness
  from staging is judged by filesystem identity. Staging is deleted only after
  a complete note is verified and flushed.
- **Privacy.** Logs hold outcomes, durations, counts, model names and error
  classes, never transcript text, audio, clipboard or field contents, or note
  file names. Keys live only in the OS secret store.

## Proof

`./scripts/check` is the local gate: fmt, clippy on Linux, a clippy
type-check of the macOS module (`aarch64-apple-darwin`), and every test. Live
desktop tests are `#[ignore]`; run them on a real session. Real-path evidence
means the compiled binary on a real session (`cantrip e2e …`, or the app driven
by its shortcut), not only unit tests. Never claim macOS runtime behavior
without a Mac.

## Secrets

Never commit secrets or print `OPENROUTER_API_KEY`.
