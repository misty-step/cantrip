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
capture-thread boundary. Never silently substitute another microphone.
The shared status snapshot carries local-only capture guidance; operational
logs keep failure classes, not device UIDs/names or content.

On macOS, open the Cantrip menu for **Open Settings** (the General/input pane)
or **Use <default input name> instead**. The nonactivating, click-through HUD
names that default input and directs the operator to the menu rather than
taking focus. Only clicking Use persists that named concrete UID; it does not
start a take or turn the selection into a future system-default rule. Recheck
the saved source and named device before saving; refuse stale actions rather
than overwrite a newer choice or retarget another input. Keep the failed
outcome visible when no current offer can be shown. Reconnect the original input
or make an explicit selection, then press again. Linux uses the shared HUD and
`cantrip settings` for repair.

The existing PCM signal monitor owns the approximately −60 dBFS floor and
three-second initial grace. Its cases stay distinct:

- Measured initial PCM at or below the floor shows **No sound from <device> —
  check the input**, plus the Settings route. It does not stop or block capture.
  Signal above the floor clears it; later thinking pauses do not warn.
- Missing frames or an unreadable/unavailable monitor cannot establish silence.
  Show **Input status unavailable**, clear stale level/waveform observations,
  and preserve Stop/Cancel.
- An otherwise clean stop that reports typed `EmptyRecording` ends `no-audio`
  with the Settings guidance. A storage, finalization or other stop error keeps
  its original failure class, even when a zero-frame header remains on disk.
  Cancellation and durable take retention keep their existing contracts.

Use actual input names when known: native CoreAudio names on Mac and the
selected PipeWire node description on Linux, resolved on the capture path
without another recorder. An absent UID gets a generic configured-microphone
label rather than a guessed name. There is no silence-driven automatic stop,
speech detector, threshold setting, passive microphone switch or audio deletion.

Revisit if: a supported uninterrupted history/effect contract permits guarded
Mac keyboard delivery, through an explicit product decision.
