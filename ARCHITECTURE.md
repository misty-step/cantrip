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
| Linux | `x11rb` (X11 / XWayland), `dbus` (AT-SPI, Secret Service, portals, StatusNotifierItem via `ksni`), PipeWire `pw-record`, Sway IPC (i3-ipc protocol, own client), `smithay-client-toolkit` 0.19 + `wayland-client` 0.31 (the versions winit links) for the `wlr-layer-shell` HUD |

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
  ui/             egui popover, Settings, HUD, theme (contrast-tested), gallery;
                  raster.rs renders the same HUD pill to pixels (CPU) for
                  native overlay surfaces
  net.rs          ureq Transport (Offline vs Failed vs Timeout mapping)
  ipc.rs          Unix-socket single-instance channel (popover/settings/toggle,
                  and hotkey-down/hotkey-up/cancel lines for user-written
                  compositor bindings)
  cli.rs          `cantrip e2e …` headless verification over recorded audio
  platform/mod.rs Platform bundle + Hotkeys/Tray/UiPlatform traits
  platform/linux/ X11/XWayland, AT-SPI, PipeWire, Secret Service, SNI, portals,
                  Sway IPC (sway.rs), wlr-layer-shell HUD (layer_hud.rs)
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
| Global shortcut | Carbon hot key (`global-hotkey`): press + release | `XGrabKey` + XKB detectable auto-repeat: press + release | **Sway** (`$SWAYSOCK` answers; preferred there, since `xdg-desktop-portal-wlr` has no GlobalShortcuts): Sway IPC `bindsym --no-repeat <chord> nop cantrip-hotkey-down` and `bindsym --release <chord> nop cantrip-hotkey-up` (chord as keysyms, e.g. `Control+Mod1+space`): press + release. `nop` runs nothing, so a keypress spawns no process: one persistent `SUBSCRIBE ["binding","workspace","shutdown"]` connection receives Sway's `binding` event and its listener thread hands the marker straight to the core's key thread (sub-millisecond); a press while already down ends a hold whose release was lost. `swaymsg reload` drops runtime bindings and then emits a `workspace` event with `change: "reload"`: the listener re-reads the config and re-installs the chord (and Escape during a take); a lost event connection is re-subscribed and re-asserted the same way. Rebinding unbinds the old chord; Quit and SIGTERM/SIGINT/SIGHUP (now a graceful Quit) unbind everything Cantrip installed (and nothing else); a chord the user's Sway config binds is reported as taken, never overwritten. The config is read as Sway loads it: the `GET_CONFIG` main text plus every `include` (Sway's `-c` from the compositor's `/proc` entry found through `SO_PEERCRED`, else its search path; `$variables`, `~`, environment variables and globs, relative to the including file's directory, each file once), `bindsym`/`bindcode` with any flags, `bindsym { }` and `mode` blocks, line continuations and modifier aliases. **Elsewhere**: xdg-desktop-portal GlobalShortcuts: press + release where the portal sends `Deactivated` (KDE, GNOME, Hyprland), else tap-to-toggle |
| Escape during a take | Carbon hot key while active | `XGrabKey` of plain Escape while active, under the exact lock masks only (none, CapsLock, NumLock, both; never `AnyModifier`, which collides with window-manager chords such as stock Openbox's `Alt+Escape`) | **Sway**: `bindsym --no-repeat Escape nop cantrip-cancel` only while a take is active, `unbindsym Escape` as soon as it ends and on exit, so Escape belongs to apps whenever Cantrip is idle; skipped if the user's config (or any file it includes) binds plain Escape. Cantrip only removes an Escape binding it installed itself (`escape_owned_by_cantrip`), never at startup, and always removes the one it installed (take end, Quit), as it does its chord on rebind and Quit, even when an `include`d file saved since without a reload now binds the key (Sway's live table still holds Cantrip's `nop`), but never after a `swaymsg reload` has replaced the live table, even one the event listener has not processed yet. Whenever it installs runtime bindings (startup, after a reload, at Escape bind) Cantrip registers a sentinel binding mode, `mode "__cantrip_live_<pid>_<generation>" bindsym XF86Launch9 nop cantrip-sentinel`, which is never entered (the active mode stays `default`) and which a reload wipes like every runtime binding. Immediately before every `unbindsym` (take end, rebind, Quit) it checks `GET_BINDING_MODES` (IPC type 8) synchronously: sentinel listed, so no reload since and Cantrip's `nop`s are live, and they are removed; sentinel gone, so the bindings are already gone and whatever is bound now is the user's, so it removes nothing and clears its ownership. Sway cannot delete a mode, so a sentinel stays, inactive, until the next reload; if Sway refuses the sentinel, owned bindings are removed as before. A reload landing between the check and the `unbindsym` (one IPC round trip) is not covered. A leftover from a crashed Cantrip (a `nop`) stays until `swaymsg reload` or the next take's end. **Portal sessions**: not grabbable (caveat): no cancel; the popover's Stop, like the shortcut, finishes the take (transcribed and delivered), never discards it |
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
| HUD window | egui window patched to status level, ignores mouse, all Spaces, never activates (accessory app) | override-redirect, notification type, empty input shape, sticky; XShape pill when uncomposited | **With `zwlr_layer_shell_v1`** (wlroots/Sway, KDE, Hyprland): a layer surface on `Layer::Overlay`, `Anchor::BOTTOM` (horizontally centred) with the HUD's 72 px bottom margin above any panel's exclusive zone, `KeyboardInteractivity::None`, empty `wl_region` input region: never focused, click-through, on every workspace (layer surfaces belong to the output). Pixels: `ui::raster` runs the real `hud::paint_pill` with the same fonts and theme palette offscreen and fills its feathered meshes into an ARGB8888 `wl_shm` buffer at the output's integer scale; the surface is created on show and destroyed on hide. **Without it** (GNOME): the egui window as before |
| Control surface | menu-bar status item → popover | SNI tray when a watcher exists; else the launcher entry `cantrip popover` (stock GNOME) | same |

## Choices DESIGN.md leaves open

- **The egui root window on native Wayland.** eframe's root viewport is the
  HUD window everywhere else; with the layer-shell HUD it shows nothing but
  cannot be unmapped (Wayland reports no visibility, so eframe paints it), and
  it must stay on an output: winit paints a window only after its previous
  frame callback, and the popover and Settings are painted from the root's
  pass (in Sway's scratchpad they never opened). So it is transparent and
  click-through, sized at Sway's default floating minimum (75×50, so Sway adds
  no clickable padding) and keeps the HUD's accessible label; on Sway,
  `no_focus` and a `for_window` rule (set before it maps) keep it from taking
  focus and make it a borderless sticky float, and once mapped it is parked
  beyond the top-left corner with one pixel left on the output. EGL vsync is
  off on Wayland so a swap can never block the UI thread if that window is
  ever occluded. Other layer-shell compositors (KDE, Hyprland) get the
  transparent, click-through window without the Sway rules.

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
- **Atomic, no-clobber note publication.** A note's complete bytes are
  written and fsynced before it has any name, then it appears whole in one
  step that can never replace an existing name. Linux opens an unnamed
  `O_TMPFILE` inode in the `MM` directory and publishes it with
  `linkat(AT_FDCWD, "/proc/self/fd/N", AT_FDCWD, dst, AT_SYMLINK_FOLLOW)`.
  macOS writes a copy `publish-<unique>.tmp` inside the meeting's own staging
  directory (never the Meeting folder) and moves it with
  `renameatx_np(…, RENAME_EXCL)`. `EEXIST` keeps the same prepared bytes and
  tries `-2.md`, `-3.md`, …; the staged `note.md` is never touched. When the
  filesystem cannot (`EOPNOTSUPP`/`ENOTSUP`, `EISDIR`, `EINVAL`, `EXDEV`,
  `ENOENT`, e.g. no `O_TMPFILE`, no `/proc`, a cross-volume or FAT/network
  folder), the macOS copy is unlinked from staging and the note is created
  directly with `O_CREAT|O_EXCL|O_NOFOLLOW` and written in place — the
  documented non-atomic fallback. Either way the Meeting folder never holds
  a temp name, and nothing there is renamed, deleted, truncated or
  write-opened.
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
  `cantrip e2e meeting-retry`). The archive's directory flushes, `O_TMPFILE`
  open and note write go through `meeting::archive::ArchiveFs`, so tests
  inject `EIO`, an `EOPNOTSUPP` filesystem and a slow write; the compiled
  binary is fault-injected with `scripts/e2e/fsync_fault.c` and
  `scripts/e2e/slow_write.c` (`LD_PRELOAD`).

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
- Atomic publication on ext4 with the release binary: `write(2)` of the
  note slowed by 1.5 s (`LD_PRELOAD`) while a watcher scans the Meeting
  folder — the note is never seen empty or partial, only absent or whole
  (matching SHA-256), no temp name ever appears, and `strace` shows
  `O_TMPFILE` + `linkat` with no rename, unlink or truncate in the folder.
  With `O_TMPFILE` refused (`EOPNOTSUPP`) the direct exclusive create still
  archives, and the owner-only (`0600`) publication test passes on such a
  host (atomicity is asserted where `O_TMPFILE` works).
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

**Verified live on Sway 1.9 (wlroots, headless backend)** with the release
binary, stock `/etc/sway/config` (minus its systemd import), swaybar,
`xdg-desktop-portal-wlr` 0.7.1 present (no GlobalShortcuts), a private D-Bus,
PipeWire virtual microphone, gnome-keyring with a dummy key and the loopback
OpenRouter stub (`scripts/e2e/sway_check.py`; numbers in the builder evidence):

- `Ctrl+Alt+Space` (driven through Sway's bindings with `wtype`) starts a take
  with a GTK3 receiver focused; the receiver keeps focus throughout.
- The HUD pill appears bottom-centre (centred to the pixel, 72 px above the
  output's bottom edge) on workspace 1, stays there on workspace 2 while
  recording, and is still there back on workspace 1; a pointer click on the
  pill reaches the receiver beneath it (wlroots virtual pointer,
  `scripts/e2e/vptr`).
- Plain Escape during the take: the receiver receives no Escape, the HUD is
  gone and the staged WAV deleted within tens of milliseconds, zero stub
  requests follow; when idle, Escape reaches the receiver again (`unbindsym
  Escape` reports no binding).
- A hold-to-talk take and a tap-to-toggle take each run end to end through the
  stub (transcription + rewrite, `data_collection: "deny"`) and are inserted
  into the receiver over AT-SPI (`✓ Inserted`).
- Rebinding in Settings (Record → `Ctrl+Alt+D`): the old chord's press and
  release bindings are gone and it no longer starts a take, the new chord
  starts one and Escape cancels it. After Quit (SIGTERM), no chord or Escape
  binding is left.
- The popover and Settings open on workspace 1 and 2 with the layer-shell HUD
  active.

**Pass 6, live on headless Sway 1.9** (`scripts/e2e/sway_ipc_check.py`, release
binary; numbers in the builder evidence):

- HUD latency: 15 of 15 focused-receiver taps (two runs of 15) show the pill
  within 23 ms of Sway's `binding` event, measured by a separate IPC
  subscriber and `grim` polling (the capture itself is ~16 ms); the previous
  `exec cantrip ipc …` build measured 10 of 15, worst 215 ms, on the same
  harness. Instrumented, event → core is ~0.1 ms and event → layer-surface
  commit ~3 ms: `Dictation::start` publishes `Listening` and wakes the UI
  before the durable staging write, and the layer-shell thread is woken by a
  pipe rather than a timer.
- `swaymsg reload` while idle: 1 s later the chord starts a take and Escape
  cancels it, no restart; a reload during a take keeps Escape cancelling; after
  reloads while idle, Escape reaches the focused app.
- A config whose `include`d file binds `Ctrl+Alt+space` and plain `Escape`:
  both user bindings fire while Cantrip is idle, during a `cantrip toggle`
  take, and after it quits; the shortcut is reported as taken. (The previous
  build clobbered both, and they stayed gone after it quit.)

**Pass 7, live on headless Sway 1.9** (release binary, loopback stub, zero
provider calls): a take is started with the chord while the `include`d file
binds nothing; `bindsym Escape exec …` is then saved to that file without
`swaymsg reload`. Escape cancels the take (Sway runs `nop cantrip-cancel`,
staged WAV deleted within 60 ms, receiver gets no Escape); afterwards, idle and
after Quit, Escape reaches the GTK receiver with no Sway binding run, and after
a reload the saved user binding fires. The `fff8dd4` build on the same harness
left `nop cantrip-cancel` bound: idle and after Quit, Escape was swallowed.

**Pass 8, live on headless Sway 1.9** (`scripts/e2e/sway_sentinel_check.py`,
release binary, loopback stub, zero provider requests): the Pass 7 no-reload
include edit still cancels cleanly (Sway runs `nop cantrip-cancel`, HUD gone in
23 ms, WAV deleted in 35 ms; the sentinel is still listed, so Cantrip unbinds
its own `nop`, and idle and after Quit Escape reaches the GTK receiver). The
reload-before-SIGTERM race: during a take, a user Escape and chord binding are
saved (once in the root config, once in an `include`d file), only Cantrip's
`cantrip-sway-events` listener thread is stopped (per-thread ptrace stop,
`scripts/e2e/thread_pause.py`; `SIGSTOP` would stop the whole process) and
`swaymsg reload` runs. The sentinel disappears from `GET_BINDING_MODES`, the
user's reloaded Escape fires, Cantrip exits 0 on SIGTERM without unbinding, and
both user bindings still fire after it exits. The `43c9a8c` build on the same
harness removed both: after it exited, Escape reached the receiver and the
chord ran nothing. `GET_BINDING_STATE` stayed `default` throughout.

Sway boundaries: Sway 1.9's `GET_CONFIG` returns only the main config's text,
never the included files Sway holds in memory, and no IPC request returns
them, so conflict checks read `include`d files from disk and reflect their
last saved state; a file edited without `swaymsg reload` can differ from Sway's
live bindings until the next reload. The Pass 6 review reproduced a Sway 1.9
compositor segfault when a physically held key spans `swaymsg reload`, also in
a Cantrip-free Sway control; virtual-keyboard (`wtype`) holds across a reload
did not crash Sway here.

Headless caveats: the seat has no physical keyboard or pointer, so the check
keeps one virtual keyboard alive (GTK reports its window active only while a
keyboard exists) and clicks with a virtual pointer. Only an integer output
scale of 1 was exercised; Sway ≥ 1.10 (scene graph), Hyprland's and KDE's
layer shells, and the Sway-specific window rules on a real multi-output
layout are not run.

**Not verified, Linux.** KDE Plasma and GNOME Wayland sessions were not
available, so the portal shortcuts on them, and the release matrix per desktop,
are still to be recorded. GTK3 ignores synthetic `XSendEvent` keys, so tier 2 on
X11 reaches only toolkits that accept them; most GTK fields take tier 1
instead. No real microphone, Bluetooth headset or human speech was used
(neural TTS through a virtual PipeWire source).
