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

## Not proven

- The first-presentation stale frame above is reproduced once (first failure
  after launch, Dark) and not fixed; later presentations repainted correctly.
- Reduce Transparency and Increase Contrast (opaque backdrop) were not exercised
  live; only the offscreen `hud --screenshot` path draws that backdrop.
- The macOS 13.3–15 `NSVisualEffectView` fallback was not exercised.
- No capture over a mismatched background (Light appearance over dark content,
  or the reverse), so Liquid Glass's adaptive appearance flip is unobserved.
- After-build recording/processing in Light and the handoff-route tint were not
  captured.
