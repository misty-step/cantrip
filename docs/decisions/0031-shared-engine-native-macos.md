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
Do not replace unknown lock/session history with private screen-lock APIs or
permission-based assumptions.

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

### Capture setup guidance (2026-11-03)

A configured microphone that is absent is a typed `MissingMicrophone` failure
and ends the press as `capture-source-missing`, including across the native
capture-thread boundary. Never silently substitute the default microphone.
The shared status snapshot carries local-only capture guidance; operational
logs keep failure classes, not device UIDs/names or content. macOS presents
explicit **Open Settings** and **Use <default input name> instead** actions
prominently in its menu, because its HUD remains nonactivating and click-through.
Only the explicit Use action persists the named UID, with stale selection/device
checks; opening the HUD/menu or a new press does not change the configuration.

Reuse the existing PCM signal monitor and its three-second, approximately
−60 dBFS threshold. Initial silence guidance is non-blocking and clears with
signal; later thinking pauses do not warn.
No PCM frames is distinct from measured silence; stopping a finalized zero-frame
take ends `no-audio`, without changing cancellation or durable-retention policy.
The operator still owns Stop/Cancel; no silence-driven automatic stop is added.
Available input names are resolved at the press for local feedback; an absent
UID gets a generic configured-microphone label rather than a guessed name.

Revisit if: a supported uninterrupted history/effect contract permits guarded
Mac keyboard delivery, through an explicit product decision.
