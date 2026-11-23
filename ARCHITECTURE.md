# Cantrip v2 — Architecture

How v2 is built. What it must do is fixed by [PRODUCT.md](PRODUCT.md) and
[DESIGN.md](DESIGN.md); this file records the implementation choices DESIGN.md
leaves open, and exactly what has and has not been verified.

## Chosen stack

**One Rust workspace, two crates, one UI codebase, thin per-OS adapters.**

| Layer | Choice |
| --- | --- |
| Language | Rust (stable 1.98.1, pinned) |
| Policy | `crates/core` (`cantrip-core`): pure Rust, no OS calls, no UI, no async runtime |
| UI | egui / eframe 0.37 (glow), one implementation of the popover, Settings pane and HUD pill for both OSes |
| HTTP | `ureq` 3: rustls on Linux, the system TLS stack (native-tls → Security.framework) on macOS |
| Concurrency | `std::thread` + channels; the UI owns the main thread |
| macOS | `objc2` 0.6 / `objc2-*` 0.3 bindings (AppKit, Accessibility, Core Graphics, AVFoundation, Core Audio, Speech, ServiceManagement), `security-framework`, `global-hotkey` |
| Linux | `x11rb` (X11 / XWayland), `dbus` (AT-SPI, Secret Service, portals, StatusNotifierItem via `ksni`), PipeWire `pw-record` |

### Why this beats the alternatives

- **Native UI per OS (AppKit + GTK4/Qt) over a shared core** gives the best
  platform fidelity, but doubles every UI change, splits screenshots and tests,
  and GTK4 cannot make a non-activating overlay on Wayland anyway. The closed
  surface (7 + 1 + 6 controls and one pill) is small enough that one renderer
  drawn with system fonts and OS-matched palettes reads as native. If a
  platform later demands it, the UI is a pure view over `app::Control`, so a
  native client can replace it without touching policy.
- **Tauri / Electron** add a web runtime, a second language and a
  focus-stealing window model to an app whose core promise is never taking
  focus. Startup and memory are worse for a menu-bar utility.
- **Swift app + separate Linux app** (v1's direction) duplicates the delivery
  and archive invariants, the two hardest parts to get right, in two languages.
  Here they exist once, in `cantrip-core`, tested once, used by both OSes.
- **Async runtime** (tokio): every OS call is blocking anyway (AX, AT-SPI, X11,
  D-Bus); a few dedicated threads with deadlines are simpler to reason about
  and bound (e.g. the 250 ms capture and 4 s rewrite deadlines are worker
  threads whose late answers are dropped).

## Module boundaries

```
crates/core/src            (no cfg(target_os) except default data paths)
  ports.rs        every OS boundary as a small trait: Desktop, Clipboard,
                  Recorder, MeetingCapture, LocalRecognizer, SecretStore,
                  LoginItem, Transport
  shortcut.rs     chord model, hold (≥300 ms) vs tap vs Escape state machine
  speech.rs       OpenRouter contracts: payloads, exact prompts, parsing,
                  rewrite watchdog and Raw fallback
  offline.rs      the fail-closed no-key / no-network gate
  delivery.rs     capture with deadline, the gate, tier 1 / tier 2, edit-delta
                  confirmation, clipboard snapshot/restore, sticky claim
  dictation.rs    one take press → outcome, retention, Retry / Discard
  meeting/        parts (split at silences, ≤5 min), transcript merge, note
                  rendering, crash-safe store, archive invariants 1–4, process
  staging.rs      owner-only durable private storage, dictation items
  surface.rs      the closed control surface and every HUD text
  settings.rs     the six Settings values + Mode + Post-Processing
  app.rs          AppCore: commands in, UiSnapshot out (implements Control)

crates/app/src
  main.rs         CLI entry, single instance, wiring, hotkey thread
  ui/             egui popover, Settings, HUD, theme (contrast-tested), gallery
  net.rs          ureq Transport (Offline vs Failed vs Timeout mapping)
  ipc.rs          Unix-socket single-instance channel (popover/settings/toggle)
  cli.rs          `cantrip e2e …` headless verification over recorded audio
  platform/mod.rs Platform bundle + Hotkeys/Tray/UiPlatform traits
  platform/linux/ X11/XWayland, AT-SPI, PipeWire, Secret Service, SNI, portals
  platform/macos/ AppKit, AX, CGEvent, NSPasteboard, AVFoundation, Core Audio
  platform/macos_logic.rs  OS-free macOS helpers, unit-tested on Linux
```

Rules: the core owns every decision a user can observe; adapters only answer
questions and perform one mechanism each. The UI never holds policy — it
renders a `UiSnapshot` and emits `Command`s. `cfg(target_os)` appears only in
`platform/`, `main.rs`'s module list and default data paths.

## Backend mapping

| Concern | macOS 15+ | Linux X11 (and XWayland apps) | Linux native Wayland |
| --- | --- | --- | --- |
| Global shortcut | Carbon hot key (`global-hotkey`): press + release | `XGrabKey` + XKB detectable auto-repeat: press + release | xdg-desktop-portal GlobalShortcuts: press + release where the portal sends `Deactivated`, else tap-to-toggle |
| Escape during a take | Carbon hot key while active | `XGrabKey` of plain Escape while active, under the exact lock masks only (none, CapsLock, NumLock, both; never `AnyModifier`, which collides with window-manager chords such as stock Openbox's `Alt+Escape`) | not grabbable via the portal (caveat) |
| Focus capture | NSWorkspace frontmost app + AX focused window/element | `_NET_ACTIVE_WINDOW` + `_NET_WM_PID` + AT-SPI focused editable in that window's frame | AT-SPI active frame + focused editable; unidentified → `uncertain` |
| Tier 1 insert | AX `kAXSelectedTextAttribute` set | AT-SPI EditableText `DeleteText`/`InsertText` | same as X11 |
| Edit read-back | AX value + selected range (UTF-16 → chars) | AT-SPI Text `GetText` / caret / selection | same |
| Tier 2 paste | ⌘V via `CGEventPostToPid` (addressed to the app) | `XSendEvent` Ctrl+V to the captured window | none → `Copied — paste unsupported on this session` |
| Clipboard | NSPasteboard, changeCount token, Transient/Concealed markers | own CLIPBOARD owner (`x11rb`), ownership token, KDE password hint | `wl-copy`/`wl-paste` when present, else XWayland selection |
| Microphone | AVCaptureSession bound to the chosen device | `pw-record` (PipeWire), chosen node | same |
| System audio (`Others`) | Core Audio process tap (macOS 14.2+) | `pw-record` of the default sink monitor | same |
| Local recognition | SFSpeechRecognizer, `requiresOnDeviceRecognition` | none guaranteed on-device → refuses | same |
| Key storage | Keychain (generic password) | Secret Service (gnome-keyring, KWallet) | same |
| Open at login | SMAppService main app | XDG autostart entry | same |
| Sleep during a meeting | `NSProcessInfo` activity with `IdleSystemSleepDisabled`; `NSWorkspaceWillSleepNotification` stops the meeting | systemd-logind inhibitor locks while recording (`idle` block, `sleep` delay); `PrepareForSleep(true)` from `org.freedesktop.login1` stops the meeting, then the locks are released. No system bus or no logind: nothing held, no error | same as X11 |
| HUD window | egui window patched to status level, ignores mouse, all Spaces, never activates (accessory app) | override-redirect, notification type, empty input shape, sticky; XShape pill when uncomposited | XWayland override-redirect window |
| Control surface | menu-bar status item → popover | SNI tray when a watcher exists; else the launcher entry `cantrip popover` (stock GNOME) | same |

## Choices DESIGN.md leaves open

- **HUD note beside a delivery outcome.** When a take's text was produced by
  the Raw fallback (`Raw (cleanup skipped)`) or on-device, and the delivery
  is anything but a confirmed insert (copy-only, `Sent`/`Posted —
  unconfirmed`, `Not delivered`, a Retry copy), the pill shows the outcome,
  then the note, then the clipboard claim (`Copied — target uncertain │ Raw
  (cleanup skipped) │ Text is on the clipboard`); the claim's sticky drop and
  the dwell follow the outcome (at least 2 s). A confirmed insert shows the
  note alone, as DESIGN.md draws it.
- **Deadlines are checked after blocking calls return.** The 250 ms capture
  worker reads the session epoch and the target inside the deadline; an edit
  read-back that completes after its confirmation window never confirms, and a
  clipboard restore is never started after it.
- **Archive flush chain.** A first publication and a recorded-path Retry both
  flush the note, then `MM`, `YYYY`, `cantrip-meeting` and the Meeting folder
  itself; any failure keeps staging and the meeting `pending-archive`. When the
  recorded note verifies but does not flush and the folder chosen now is the
  one it lives in, Retry does not publish a duplicate there (it would hit the
  same flush); a different folder chosen now gets a fresh publication.
- **Sleep.** A meeting stopped by sleep shows "Meeting stopped: the computer
  went to sleep", and its outcome notice is prefixed "Stopped by sleep.".

## Data

- Settings: `~/.config/cantrip/v2/settings.json` (Linux),
  `~/Library/Application Support/Cantrip/v2/settings.json` (macOS).
- Staging (owner-only `0700`, durable, never a temp dir):
  `~/.local/share/cantrip/v2/` / `~/Library/Application Support/Cantrip/v2/`.
  v1 data is never read, moved or deleted.
- The OpenRouter key lives only in the OS secret store.
- Test hooks (not settings): `CANTRIP_OPENROUTER_BASE` (loopback stub),
  `CANTRIP_STAGING` and `--key-env` (`cantrip e2e` only, including
  `cantrip e2e meeting-retry`). The archive's directory flushes go through
  `meeting::archive::ArchiveFs` so tests inject `EIO`; the compiled binary is
  fault-injected with `scripts/e2e/fsync_fault.c` (`LD_PRELOAD`).

## Verification boundaries

Verified **in a Linux sandbox** (X11 + openbox, PipeWire, AT-SPI,
gnome-keyring, no GPU), with the compiled binary. Details, commands and numbers
are in the builder evidence for the PR:

- The whole core suite (shortcut, delivery, speech, offline gate, dictation,
  meeting, archive, control surface).
- Live OpenRouter runs of Raw, Clean and Elevated, plus dual-track meetings
  archived into a real Meeting folder.
- The real app driven end to end: global shortcut → HUD → PipeWire virtual
  microphone → live transcription → AT-SPI tier-1 insert into a GTK3 field.
- Copy-only on a real focus change, and the clipboard claim dropping on a real
  foreign copy.
- Recovery Retry and Discard from the popover, Secret Service key storage,
  and light/dark screenshots.
- Plain Escape cancelling a take on a stock Openbox session (`Alt+Escape`
  bound by the window manager), in an isolated Xephyr display: HUD hidden and
  audio deleted, zero requests.
- `EIO` on the Meeting folder's fsync (`LD_PRELOAD`) across a first
  publication and recorded-path Retry, from `cantrip e2e` and from the running
  app's popover: staging kept until the fault clears, then one note, staging
  deleted.
- Rewrite HTTP 500 on uncertain-target and focus-changed takes: the HUD's
  accessible text and pixels show the copy reason and `Raw (cleanup
  skipped)`; Raw is on the clipboard.
- Sleep: against a fake logind on a private bus, the app holds `idle`/`sleep`
  inhibitors only while a meeting records, `PrepareForSleep(true)` stops the
  meeting with its message and releases them; the real logind lists the same
  locks; no system bus or no logind degrades silently. No real suspend was
  performed.

**Type-checked only, never run.** The macOS platform module passes
`cargo check` and `cargo clippy` for `aarch64-apple-darwin` and
`x86_64-apple-darwin`, and its OS-free helpers are unit-tested on Linux. Every
one of the following still needs an attended macOS 15+ run (the
`Native macOS` workflow links and tests on a real Mac, but cannot judge focus
or permissions):

- AX insertion and read-back per app.
- The ⌘V `CGEventPostToPid` paste per app (Electron/Catalyst unknown).
- NSPasteboard read prompts.
- The HUD's non-activation under eframe/winit.
- The Carbon hot key for Escape.
- AVCaptureAudioFileOutput's WAV settings and device binding through
  Bluetooth route changes.
- The Core Audio process tap and its permission denial behaviour.
- SFSpeechRecognizer on-device refusal.
- SMAppService.
- `NSWorkspaceWillSleepNotification` stopping a meeting before sleep.
- Signing and notarization (not part of this slice).

**Not verified, Linux.** No real Wayland compositor (KDE Plasma, GNOME,
wlroots) was available, so the portal shortcuts, `wl-copy` clipboard and
AT-SPI-only focus path are untested, and the release matrix per desktop is
still to be recorded. GTK3 ignores synthetic `XSendEvent` keys, so tier 2 on
X11 reaches only toolkits that accept them; most GTK fields take tier 1
instead. No real microphone, Bluetooth headset or human speech was used
(neural TTS through a virtual PipeWire source).
