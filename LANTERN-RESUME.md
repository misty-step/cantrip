# Lantern: parked

Status: parked on 2026-09-25 and not merged (PR #108 is closed). Phaedrus compared it with the installed
Cantrip and wasn't convinced it is better. This branch keeps the work.

## What Lantern tried

A polish pass over every surface, inside the existing contracts (local STT by default, metadata-only
Actions, confirmed Forget, comment-preserving Settings writes, HUD honesty from ADR 0021/0023).

- HUD (`src/hud.rs`):
  - An LED instrument behind glass: a rounded, shadowed housing lit only by the presented cells.
  - Centred Hack captions whose action lines name the exact button to press.
  - Colour and rim settle with the field; captions change at once.
  - A 120 ms appear fade, and success that settles from the centre outward.
- Shared window material (`src/ui.rs`, `src/theme.rs`, `src/fonts.rs`):
  - Palette-derived tones and bundled Geist/Geist Mono (OFL, shipped as `FONTS-LICENSE.txt`).
  - An LED-matrix wordmark.
  - Buttons with a focus ring, segmented controls, switches, cell stamps.
- Actions: a status-first card, Waiting/All segments, painted rows with relative days, and Forget in a
  danger zone.
- Settings:
  - An explicit "On this computer / Cloud provider" mode.
  - Vocabulary one term per line, and collapsible cleanup.
  - HUD previews built from production stills, problems shown first, and a pinned save footer.
- Spec and decision record: `docs/DESIGN.md`, `docs/adr/0028-lantern-instrument.md`. The ADR also records
  why the Palette, live Mirror, Spellbook and Settings-sidebar concepts were rejected.

## Worth salvaging without the new look

1. `cantrip hud-gallery --export DIR` (`src/hud/gallery.rs`, `hud::write_png`). It writes every HUD still
   and journey frame through the production painter, offline. Visual review never has to map a layer on
   the live compositor, which invalidates in-flight delivery permits.
2. **Edit file text** for validation failures on values Settings has no control for (handoff commands,
   telemetry, decision routing). Master has the same dead end: it offers text repair only for unparseable
   TOML. Commit `0be1118`.
3. `scripts/verify-release`: master accepts a release candidate that has no manual. The rollback-only
   `legacy=True` exception in `f56eb4e` fixes that separately from the fonts.
4. Found here, not fixed on any branch:
   - `Config::validate` accepts a non-HTTP STT endpoint such as `ftp://…`.
   - `contrib/install.sh`: the first `/proc/net/unix` scan can miss a live socket. The second scan still
     refuses, but a published backup is left behind.
   - `daemon::tests::handoff_timeout_kills_a_wrapper_scripts_children` is timing-flaky.
5. Small ideas that transfer:
   - Action lines that name the button, for example "Copy transcript in Cantrip Actions".
   - No disabled Stop/Cancel at idle.
   - Relative-day capture times.
   - Copy that passes `design-check`, for example "length unknown" and "your configured speech provider".

## Evidence

- Before/after sheets of every state in three palettes, using synthetic data: <https://gist.github.com/moomooskycow/bb2f7355c495d00e00ccb656b0dc8dcf>.
  The PR #108 description links to them.
- The local gallery, the design boards and the throwaway capture harness (Xvfb plus a mock status socket)
  were deleted at wind-down. Regenerate HUD evidence with `hud-gallery --export`.
- If window captures are ever rebuilt, use synthetic fixture values and a config path outside `$HOME`.
  The first evidence upload leaked real configuration values and had to be deleted.
