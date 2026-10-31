# 0034: Native AppKit presentation on macOS; Linux keeps egui and Omarchy

- **Status:** accepted
- **Date:** 2026-10-30
- **Primitives:** hud, settings, actions, theme, platform
- **Powder:** —

## Context

The Mac menu is a native `NSMenu`, and the HUD's shared pixel model draws into a
native `NSPanel`. Settings and Recordings/Recovery, however, are egui windows
spawned as separate `cantrip settings` / `cantrip actions` processes (historical
[0007](historical/0007-settings-gui-eframe.md)). On macOS that shape draws
non-native controls, outlives the app that spawned it, and queries
process-local state such as microphone authorization in the wrong process
([#198](https://github.com/misty-step/cantrip/issues/198)). Apple's macOS 26
design (Liquid Glass) is applied automatically to standard AppKit controls and
offered to custom views through `NSGlassEffectView`.

## Decision

On macOS, Cantrip presents its surfaces natively. Settings and
Recordings/Recovery are AppKit windows that run in the app process, and the HUD
panel sits on a system material behind the shared pixel field. The minimum stays
macOS 13.3: `NSGlassEffectView` is used on 26 and later, and `NSVisualEffectView`
on earlier releases. Linux keeps egui windows, the layer-shell HUD and the active
Omarchy theme. Both platforms share the engine, IPC and pure-Rust view-models
(form state, validation, recording rows, HUD presentation). Each platform binds
its views directly to its toolkit; there is no cross-platform widget
abstraction, and theming shares palette tokens only.

## Consequences

The macOS window code is AppKit through `objc2`; there is no Swift toolchain.
Native windows read and write through the same IPC as every other client, so
an app attached to an external daemon still reports that owner's state. The
HUD's pixel field is unchanged on both platforms; only its macOS backing
changes. The panel keeps its light or dark appearance over any content, so
over a material the container keeps most of that appearance's window
background and the HUD's colors are fitted to it: captions read at WCAG 4.5:1
whatever is behind the panel. Reduce Transparency and Increase Contrast remove
the material.
Offscreen NSView pixels are not live panel/focus proof; native surfaces are
verified in a disposable `macvm` VM. Historical 0007 still governs Linux.

Revisit if: a supported Swift/SwiftUI build path becomes part of packaging, or
the macOS minimum rises to 26 so the 13.3 fallback can be removed.
