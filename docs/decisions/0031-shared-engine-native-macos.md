# 0031: One shared engine; macOS manual paste without silent downgrade

- **Status:** accepted
- **Date:** 2026-10-05
- **Primitives:** engine, platform, capture, delivery
- **Powder:** —

## Context

macOS needs AppKit main-thread ownership, native capture, credentials, and app
packaging, but those mechanisms do not justify a second workflow/history store.
The operator chose explicit manual paste; frontmost-app or permission snapshots
cannot prove uninterrupted history for automatic keys.

## Decision

Keep one cantrip-engine workflow and durable archive behind
[Platform, Recorder, and DeliveryPermit](../../crates/engine/src/ports.rs), with
native mechanisms in the host, std threads/mpsc, and one workspace version.
macOS supports explicit clipboard/manual paste only: existing Auto/Paste/Type
choices defer before clipboard/key effects and are never silently rewritten.

## Consequences

Linux keeps supervised Wayland feedback and guarded delivery. The Mac app owns
AppKit on the main thread and an engine std thread, or attaches without owning
an external daemon; app quit never kills that external owner. Mac daemon is
engine-only. Capture failure quiesces producers and finalizes accepted WAV
prefixes for shared durable retention. Mac uses private Application Support
state and login Keychain; permissions/ACL stripping are not encryption.
Microphone permission and Open At Login are explicit; supported behavior needs
no Accessibility, Input Monitoring, or Screen Recording grants. Clipboard copy
never reads/restores prior contents. Separate native packaging fails closed for
production signing/notarization; offscreen pixels are not live panel/focus proof.
See [native installation and verification](../INSTALLATION.md#macos-native-app-from-source).

Revisit if: a supported uninterrupted history/effect contract permits guarded
Mac keyboard delivery, through an explicit product decision.
