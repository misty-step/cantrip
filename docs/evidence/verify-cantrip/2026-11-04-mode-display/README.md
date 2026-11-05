# Mode display: live HUD, menu and Settings captures

v0.1.13 named the active mode with a raw line: "Clean · STT Parakeet v3 (local)
→ aliases → gpt-6.1-sol". The candidate shows each mode as a name, a pixel
glyph and a promise, says where the take goes, and lights the stages the
engine reports. Start with [compare.png](compare.png).

## Method (measured, 2026-11-05 UTC)

- Disposable `macvm` VMs: macOS 27.0 (26A428), 1440×900 at 1x, Dock visible.
  Light and Dark were switched in System Settings → Appearance.
- **Before:** the v0.1.13 development archive built from `4a4840f`
  (binary SHA-256 `64bd5448…893e09d`).
- **After:** `4420f4f`, ad-hoc packaged from a clean tree (binary SHA-256
  `c9b8bc1c…2c16c6`). See [run.json](run.json) for full hashes.
- The guest transcribed locally with Parakeet v3 (int8). Clean and Elevate
  used the endpoint `http://openrouter.ai:9900/v1`. The guest's `/etc/hosts`
  mapped `openrouter.ai` to loopback, where a synthetic OpenAI-compatible
  endpoint replied after seven seconds with a fixed public sentence and logged
  only route metadata. The HUD therefore names the real "OpenRouter" cue
  without network access, credentials or paid requests.
- Each take: `cantrip start --postproc MODE`, then `samples/jfk.wav` played
  twice into BlackHole 2ch, then `cantrip stop`. Frames came from guest
  `screencapture`, with the stage from `cantrip status --json` recorded beside
  each one, and were cut to the HUD's one-point frame. Menu and Settings were
  driven by `macvm` clicks and arrow keys.

## Files

| Surface | Light | Dark |
| --- | --- | --- |
| HUD, Raw | [recording](hud-raw-recording-light.png), [transcribing](hud-raw-transcribing-light.png) | [recording](hud-raw-recording-dark.png), [transcribing](hud-raw-transcribing-dark.png) |
| HUD, Clean | [recording](hud-clean-recording-light.png), [transcribing](hud-clean-transcribing-light.png), [refining](hud-clean-refining-light.png) | [recording](hud-clean-recording-dark.png), [transcribing](hud-clean-transcribing-dark.png), [refining](hud-clean-refining-dark.png) |
| HUD, Elevate | [recording](hud-elevate-recording-light.png), [transcribing](hud-elevate-transcribing-light.png), [refining](hud-elevate-refining-light.png) | [recording](hud-elevate-recording-dark.png), [transcribing](hud-elevate-transcribing-dark.png), [refining](hud-elevate-refining-dark.png) |
| Menu picker | [menu](menu-light.png), [Clean highlighted](menu-clean-highlighted-light.png), [after saving Clean](menu-clean-saved-light.png) | [menu](menu-dark.png), [Elevate highlighted](menu-elevate-highlighted-dark.png) |
| Settings → Modes | [pane](settings-light.png), [Details open](settings-details-light.png), [saved notice](settings-saved-light.png), [whole screen height](screen-settings-details-light.png) | [pane](settings-dark.png), [Details open](settings-details-dark.png) |
| Linux (Tokyo Night / Catppuccin Latte) | [HUD](linux-hud-catppuccin-latte.png), [Settings](linux-settings-catppuccin-latte.png) | [HUD](linux-hud-tokyo-night.png), [Settings](linux-settings-tokyo-night.png) |

Before and after: [compare.png](compare.png) (Light overview),
[compare-hud.png](compare-hud.png), [compare-menu.png](compare-menu.png),
[compare-settings.png](compare-settings.png).

## Checks (measured)

- **HUD.** At rest the line shows the glyph, the name, the promise and the
  privacy cue ("on this Mac" or "↗ OpenRouter"). While the engine reports a
  stage, the stage lights and the working word replace the promise. The cue
  lights while OpenRouter receives the text.
- **A default change applies to the next take.** During an Elevate take the
  default was switched to Clean with `cantrip mode clean`. The HUD
  ([switched](hud-elevate-kept-after-default-clean-light.png),
  [refining](hud-elevate-kept-refining-light.png)) and the status pipeline
  stayed on Elevate through transcribing and refining. Afterwards the default
  read Clean, and Raw was then restored.
- **Settings saves the default.** Selecting Clean and clicking Save & Apply
  wrote `mode = "clean"`, and the menu's radio moved to Clean. Raw was restored
  the same way.
- **Save & Apply clears the Dock at 1440×900.** Window heights at 1x:
  - Details closed: 370 pt.
  - Details open: 755 pt, bottom edge at y 818.
  - Details open with the "Saved and applied" notice: 779 pt, still ending at
    y 818.
  - For comparison, v0.1.13's pane was 770 pt.
- **Provider metadata.** The synthetic endpoint logged five chat requests:
  two Clean (Light, Dark) and three Elevate (Light, the mid-take switch,
  Dark). Every one went to host `openrouter.ai:9900` with model
  `openai/gpt-6.1-sol`. The Raw takes sent nothing.

Contrast is WCAG, measured on the live captures. Each value is the
strongest-contrast pixel of the element against its background.

| Element | Light | Dark |
| --- | --- | --- |
| HUD mode name | 14.55:1 | 12.04:1 |
| HUD promise | 4.54:1 | 4.52:1 |
| HUD privacy cue at rest | 4.67:1 | 4.58:1 |
| HUD glyph and working word (accent) | 4.51:1 | 4.54:1 |
| HUD privacy cue, lit | 4.47:1 | 4.47:1 |
| Settings explanations, glyphs, Details toggle | 14.94:1 | 10.71:1 |

The lit cue used the same accent as the working word (4.51:1 and 4.54:1), but
its anti-aliased 12-point strokes never painted the full accent at 1x. An
offscreen render of 4420f4f in Dark reproduced the gap: (17,130,253) on
(30,30,30), 4.47:1.

**Fixed after this proof, in a0336c4 (measured by unit test, not re-captured).**
On macOS the take line's promise, stage at work and destination are fitted, in
lightness only, until their most covered painted pixel reads at 4.5:1. The test
`the_take_line_reads_at_4_5_to_1_as_painted` reads those pixels in Light and
Dark. The VM was not rerun.

## Found during the proof and fixed in 4420f4f

1. **The macOS 27 status menu draws no `NSMenuItem` images.** A probe app's
   menu hid SF Symbol, bitmap and drawn item images alike, but drew a text
   attachment in the title. So the glyph now rides in the title as an
   attachment.
2. **With Details open, the fffa9eb pane pushed Save & Apply under the Dock.**
   The fix has three parts:
   - Details drops the recognizer row, which the Transcription pane already
     owns.
   - Details rows sit 8 pt apart.
   - The Clean instructions field is 50 pt tall.
3. **Two Details controls measured below 4.5:1 in Light.** AppKit's disclosure
   triangle measured 1.84:1 and the borderless "Details" title 4.00:1. They are
   now one borderless toggle with a chevron, in the label color.

## Not proven

- Retina (2x) rendering. The VM ran at 1x. Glyph cells are whole points, so
  they should land on device pixels at 2x (inferred).
- Menu tooltips. `macvm` cannot hover; the tooltip text is covered by the
  `ModeView` unit tests.
- How VoiceOver reads the menu titles that carry an attachment.
- macOS 13.3–15, which draw item images; the attachment there was not
  exercised.
- A live Linux take. Linux HUD states are `cantrip hud --screenshot` fixtures
  drawn through the real layer surface in headless sway. Linux Settings was
  rendered with Details closed.
- Unchanged and pre-existing: the footer's "Ready" status measures 3.95:1 in
  Light. The menu's existing warning symbol on attention lines is likely also
  undrawn on macOS 27 (inferred from the probe).
