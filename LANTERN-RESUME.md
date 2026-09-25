# Lantern: parked

Parked on 2026-09-25 and not merged (PR #108 closed). Phaedrus compared it with the installed Cantrip and
wasn't convinced it is better.

## What Lantern tried

- HUD: an LED instrument behind glass. The housing is lit only by the presented cells, captions are
  centred and name the exact button, and success settles from the centre outward.
- Windows: one palette-derived style with bundled Geist fonts and an LED-matrix wordmark.
- Actions: a status-first layout, Waiting/All segments, and Forget in a danger zone.
- Settings: an explicit local/cloud mode, a HUD preview, and a pinned save footer.
- Spec and rejected concepts: `docs/DESIGN.md`, `docs/adr/0028-lantern-instrument.md`.

## Worth salvaging without the new look

- `cantrip hud-gallery --export DIR` renders every HUD still and journey offline through the production
  painter. Visual review never has to map a layer on the live desktop.
- Settings' **Edit file text** for validation failures on values it has no control for. Master has the
  same dead end. See commit `0be1118`.
- `scripts/verify-release`: master accepts a release candidate with no manual; commit `f56eb4e` limits
  that exception to the rollback baseline.

Before/after evidence, synthetic data only: <https://gist.github.com/moomooskycow/bb2f7355c495d00e00ccb656b0dc8dcf>
