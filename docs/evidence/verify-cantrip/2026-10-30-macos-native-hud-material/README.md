# macOS HUD on a native material: live panel captures

Captured 2026-10-31 03:33–03:42 UTC in a disposable `macvm` VM (macOS 27.0,
26A428, 2048×1536 at 2x) with the real Cantrip.app, driven through the menu bar
and `cantrip toggle`. Each JPEG is the same 900×400 crop around the bottom-centred
panel of a full-screen `macvm shot`.

- **Before:** `origin/master` 4944dc5 (`git archive` export, ad-hoc package).
- **After:** 45de4e3 on `feat/macos-native-hud-material` (ad-hoc package,
  `LC_BUILD_VERSION` minos 13.3, sdk 27.0).

States: the failure is a refused capture (`audio_source` set to a missing UID);
recording uses BlackHole 2ch fed with the public `samples/jfk.wav`; processing
holds a cloud STT request against a local listener that never answers.
Backgrounds are solid #f2f2f2 (Light) and #141414 (Dark) wallpapers; appearance
was switched by clicking System Settings → Appearance.

| File | Shows |
| --- | --- |
| `before-light-failure.jpg`, `before-dark-failure.jpg` | Old opaque square container with the attention rail. |
| `before-light-recording.jpg`, `before-light-processing.jpg` | Old recording waveform and processing activity. |
| `after-light-failure.jpg`, `after-dark-failure.jpg` | Liquid Glass container, 12-point corners, rail clipped to the shape; captions unchanged. |
| `after-dark-recording.jpg`, `after-dark-processing.jpg` | Recording waveform and processing activity on glass. |
| `after-dark-glass-over-window.jpg` | The same failure over a System Settings window: content behind shows through the glass. |
| `after-dark-failure-first-presentation-stale.jpg` | **Defect:** the first panel shown after launch kept an early transition frame (centre row unlit) until the content behind changed. |

Caption title contrast, measured on the captures (dominant background against
glyph cores, WCAG formula): before Light 14.81:1, before Dark 12.27:1, after
Light 12.42:1, after Dark 10.85:1.

## First-presentation investigation (2026-10-31)

Fresh app launches in four fresh `macvm` VMs (macOS 27.0, 26A428), each
showing the refused-capture failure as the first HUD after launch, with
temporary logging builds of 348c941 (never committed) and the pre-material
build 4944dc5.

- **The app draws every frame it paints (measured).** Across 34 logged launches
  AppKit drew each painted frame, including the final one; the only exceptions
  are single frames AppKit coalesced
  ([draw logs](investigation-first-presentation-draw-logs.txt)).
- **Early after a VM boots, the WindowServer stalls the first presentation
  (measured).** With load averages of 100–280 on four cores, the main thread
  blocked 0.2–2.5 s in synchronous WindowServer calls:
  `-[NSWindow _windowMoved:]` → `SLSIsDisplayChangedSeedValid` (240 of 525
  main-thread samples) and `CABackingStoreGetFrontTexture`. Later launches
  draw every 16–60 ms.
- **The stale frame appeared only in that window, with and without the glass's
  content view (measured).** With the HUD view as the glass's content view, the
  final frame was drawn about 2.4 s after the panel was ordered front, and the
  `macvm shot` at about 5 s still showed an early frame
  ([sheet](investigation-stale-content-view.jpg)). With the HUD view moved
  above the glass, a shot at about 1.5 s still showed an early frame, although
  the final frame had been drawn at about 0.4 s
  ([sheet](investigation-stale-sibling-view.jpg)). The glass's content-view
  portal is therefore not the cause, and that change was dropped.
- **The pre-material build shows an early frame too (measured).** For
  `4944dc5` (painted container, no material), a `macvm shot` showed the early
  frame while a guest `screencapture` about 1 s later showed the final one
  ([sheet](investigation-before-build-vnc-vs-guest.jpg)).
- **`macvm shot` returns a VNC client's copy (read in `macvm`).** It keeps one
  connection to tart's `--vnc-experimental` server, follows incremental
  updates and returns its own copy. A region the server never resends stays
  stale until something else changes there, which matches "refreshed only
  when the content behind changed". This is an inference: the 24 trials that
  paired a shot with a guest `screencapture` never caught a lasting stale
  frame, so it is unproven whether the guest compositor or only the VNC copy
  held it.
- **No adaptive appearance (measured).** Liquid Glass did not switch the HUD's
  appearance over mismatched content. With Light over a dark wallpaper, the glass
  was #8c8c8c and the title text #272727 (4.44:1,
  [capture](investigation-light-over-dark-failure.jpg)). With Dark over a light
  wallpaper, the glass was #767676 and the title text #dddddd (3.34:1, below
  4.5:1, [capture](investigation-dark-over-light-failure.jpg)). The docs and
  comments that said the glass adapts the HUD's appearance are corrected.

## Not proven

- Whether a real display shows the stale first frame. Next check: when a
  `macvm shot` is stale, take a guest `screencapture` and a fresh full-frame VNC
  read at once, or watch the first HUD after login on a physical Mac.
- Reduce Transparency and Increase Contrast (opaque backdrop) were not exercised
  live; only the offscreen `hud --screenshot` path draws that backdrop.
- The macOS 13.3–15 `NSVisualEffectView` fallback was not exercised. The
  binary references no AppKit class at link time (`nm -u` lists no
  `OBJC_CLASS` symbols), so a 13.3 system can load it, and only
  `available!(macos = 26.0)` reaches `NSGlassEffectView`.
- After-build recording/processing in Light and the handoff-route tint were not
  captured.
