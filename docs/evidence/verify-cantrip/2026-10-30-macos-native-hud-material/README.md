# macOS HUD on a native material: live panel captures

All captures come from disposable `macvm` VMs (macOS 27.0, 26A428, 2048×1536 at
2x) running the real ad-hoc packaged Cantrip.app, driven through `cantrip
toggle`, the menu bar and System Settings clicks. Each JPEG is the same 900×400
crop around the bottom-centred panel. Contrast is measured on the PNG crop
before JPEG encoding: the box's dominant background against the 0.5% of its
pixels farthest from it in luminance (glyph cores or the brightest cells), WCAG
formula.

- **Before:** `origin/master` 4944dc5, painted opaque square container.
- **Bare glass:** 45de4e3, the first material build (5cae6b1 differs only in
  comments).
- **Legible glass:** 287d97e, the HUD code at the PR head.

States: the failure is a refused capture (`audio_source` set to a missing UID);
recording plays the public `samples/jfk.wav` into BlackHole 2ch; processing
holds a cloud STT request against a local listener that never answers. The
handoff take uses `[handoff.pepper]` labelled "Kaylee".

## Legibility over any backdrop (287d97e, 2026-10-31)

Liquid Glass keeps the panel's light or dark appearance over any content, so on
bare glass a Dark panel over a light page put light captions on light glass. At
287d97e the HUD view fills the container with 80% of the appearance's window
background over the material, and the HUD palette is fitted to that container
over pure black and pure white. Captures are guest `screencapture` PNGs of the
failure panel over solid wallpapers.

| Appearance | Behind the panel | Title (foreground) | Action line (attention) | Capture |
| --- | --- | --- | --- | --- |
| Light | white | 14.44:1 | 7.01:1 | [jpg](legibility-light-over-white.jpg) |
| Light | gray #808080 | 13.23:1 | 6.42:1 | [jpg](legibility-light-over-gray.jpg) |
| Light | black | 11.97:1 | 5.81:1 | [jpg](legibility-light-over-black.jpg) |
| Dark | white | 9.86:1 | 6.91:1 | [jpg](legibility-dark-over-white.jpg) |
| Dark | gray #808080 | 10.00:1 | 7.01:1 | [jpg](legibility-dark-over-gray.jpg) |
| Dark | black | 12.55:1 | 8.80:1 | [jpg](legibility-dark-over-black.jpg) |

Bare glass (45de4e3) over mismatched content measured title 3.34:1 (Dark over
light) and 4.44:1 (Light over dark) on PNG crops, and action lines 2.01:1 and
1.60:1 on the committed JPEG crops
([Dark](investigation-dark-over-light-failure.jpg),
[Light](investigation-light-over-dark-failure.jpg)). The handoff label "to
Kaylee" measures 5.83:1 (Light) and 7.51:1 (Dark) on glass at 287d97e.

The failure field's centre row is drawn at 55% of the attention color by
design, so it measures 2.46–3.57:1 on glass, against 2.19–3.37:1 on the opaque
backdrop below. Field colors at full intensity are fitted to 4.5:1 like the
captions (unit test `a_translucent_container_keeps_its_text_readable_over_any_backdrop`).

## Live panel matrix (287d97e, 2026-10-31)

Light appearance over the #f2f2f2 wallpaper, Dark over #141414. Reduce
Transparency and Increase Contrast were switched in System Settings →
Accessibility → Display; the panel took the opaque backdrop within its 2-second
preference poll.

| Backdrop | Appearance | Recording | Processing | Failure | Failure title / action |
| --- | --- | --- | --- | --- | --- |
| Liquid Glass | Light | [jpg](matrix-glass-light-recording.jpg) | [jpg](matrix-glass-light-processing.jpg) | [jpg](matrix-glass-light-failure.jpg) | 14.31:1 / 6.95:1 |
| Liquid Glass | Dark | [jpg](matrix-glass-dark-recording.jpg) | [jpg](matrix-glass-dark-processing.jpg) | [jpg](matrix-glass-dark-failure.jpg) | 12.00:1 / 8.41:1 |
| Reduce Transparency | Light | [jpg](matrix-reduce-transparency-light-recording.jpg) | [jpg](matrix-reduce-transparency-light-processing.jpg) | [jpg](matrix-reduce-transparency-light-failure.jpg) | 14.81:1 / 4.51:1 |
| Reduce Transparency | Dark | [jpg](matrix-reduce-transparency-dark-recording.jpg) | [jpg](matrix-reduce-transparency-dark-processing.jpg) | [jpg](matrix-reduce-transparency-dark-failure.jpg) | 12.27:1 / 7.47:1 |
| Increase Contrast | Light | [jpg](matrix-increase-contrast-light-recording.jpg) | [jpg](matrix-increase-contrast-light-processing.jpg) | [jpg](matrix-increase-contrast-light-failure.jpg) | 20.82:1 / 4.52:1 |
| Increase Contrast | Dark | [jpg](matrix-increase-contrast-dark-recording.jpg) | [jpg](matrix-increase-contrast-dark-processing.jpg) | [jpg](matrix-increase-contrast-dark-failure.jpg) | 16.29:1 / 8.08:1 |
| Glass, handoff | Light | [jpg](matrix-handoff-light-recording.jpg) | [jpg](matrix-handoff-light-processing.jpg) | — | label 5.83:1 |
| Glass, handoff | Dark | [jpg](matrix-handoff-dark-recording.jpg) | [jpg](matrix-handoff-dark-processing.jpg) | — | label 7.51:1 |

The opaque rows show the system window background with a one-point separator
edge (stronger under Increase Contrast) and the unfitted system colors. A
refused handoff start shows the default failure panel, because the take never
began.

## First presentation (2026-10-31): not reproduced, not settled

Earlier captures showed the first failure panel after launch keeping an early
frame (centre row unlit) in `macvm shot`s for seconds after the app had drawn
its final frame ([original](after-dark-failure-first-presentation-stale.jpg),
[sheet](investigation-stale-content-view.jpg)). This round tried to catch it
with an independent view of the screen at the same moment. Each trial
relaunched the app (reinstalling the 287d97e copy first), refused a capture so
the failure panel was the first HUD after launch, then took rounds of: a
`macvm shot` (macvm's
long-lived incremental VNC copy), a fresh VNC read (a new connection asking
tart's server for the whole screen, non-incrementally), a guest `screencapture`
and another `macvm shot`; the CPU and stall trials took the fresh VNC read
first. The HUD only moves forward to its final frame, so a shot showing an
earlier frame than a capture taken before it is a stale copy.

| Condition | Build | Trials | Captures earlier than a preceding capture |
| --- | --- | --- | --- |
| Fresh VM, load average 330 falling to 60 | 287d97e, reinstalled per trial | 10 | none |
| Same VM with 16 guest CPU hogs, load 33–46 | 287d97e, reinstalled per trial | 5 | none |
| WindowServer stopped (`SIGSTOP`) 1.5–2.5 s during the first presentation | 287d97e, reinstalled per trial | 4 | none |
| Fresh VM, load 138 falling to 34 | 45de4e3, relaunched per trial | 6 | none |

Early frames appeared only in the first round, within 1.7 s of the toggle,
while the transition was still running; every later capture of every trial was
the final frame. No product defect was observed, and no stale `macvm shot` was
caught beside a correct guest capture, so the earlier observations remain
unexplained. Together with the 24 paired trials before, 49 trials with an
independent capture have not shown a lasting stale frame. The next check is an
instrumented build that logs each frame's wall-clock draw time, with
`macvm shot`s alone for 5 s (the protocol that caught it before), followed by a
fresh VNC read and a guest capture.

A fresh VNC client must advertise the desktop-size pseudo-encodings that macvm
uses: tart's server aborted the whole VM for a client offering only raw
encoding.

## Earlier captures (45de4e3 and 4944dc5, 2026-10-31 03:33–03:42 UTC)

Backgrounds were solid #f2f2f2 (Light) and #141414 (Dark).

| File | Shows |
| --- | --- |
| `before-light-failure.jpg`, `before-dark-failure.jpg` | Old opaque square container with the attention rail. |
| `before-light-recording.jpg`, `before-light-processing.jpg` | Old recording waveform and processing activity. |
| `after-light-failure.jpg`, `after-dark-failure.jpg` | Bare Liquid Glass, 12-point corners, rail clipped to the shape. |
| `after-dark-recording.jpg`, `after-dark-processing.jpg` | Recording waveform and processing activity on bare glass. |
| `after-dark-glass-over-window.jpg` | The failure over a System Settings window: content behind shows through bare glass. |
| `after-dark-failure-first-presentation-stale.jpg` | The first panel after launch in a `macvm shot`, still showing an early frame. |

Caption title contrast on those captures: before Light 14.81:1, before Dark
12.27:1, bare glass Light 12.42:1, bare glass Dark 10.85:1.

## First-presentation investigation (45de4e3, 2026-10-31)

Fresh app launches in four fresh `macvm` VMs, each showing the refused-capture
failure as the first HUD after launch, with temporary logging builds of 348c941
(never committed) and the pre-material build 4944dc5.

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
  `4944dc5`, a `macvm shot` showed the early frame while a guest
  `screencapture` about 1 s later showed the final one
  ([sheet](investigation-before-build-vnc-vs-guest.jpg)).
- **`macvm shot` returns a VNC client's copy (read in `macvm`).** It keeps one
  connection to tart's `--vnc-experimental` server, follows incremental
  updates and returns its own copy.

## Not proven

- The cause of the stale first frame in `macvm shot`s, and whether a real
  display ever shows it (see above).
- The macOS 13.3–15 `NSVisualEffectView` fallback was not exercised. The
  binary references no AppKit class at link time (`nm -u` lists no AppKit
  `OBJC_CLASS` symbols), so a 13.3 system can load it, and only
  `available!(macos = 26.0)` reaches `NSGlassEffectView`. Its container gets the
  same 80% window background and fitted palette as Liquid Glass.
