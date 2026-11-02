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
offered to custom views through `NSGlassEffectView`. The 2026-10-30 decision put
the HUD panel on that material. The accepted 2026-11-02 amendment takes it off:
after dictating with the live app, the operator found the rounded glass
container out of keeping with Cantrip and asked for the live HUD to keep the
same Omarchy look as on Linux, with sharp corners.

## Decision

On macOS, Cantrip presents its windows natively: Settings and
Recordings/Recovery are AppKit windows that run in the app process. The live
HUD keeps Cantrip's own Omarchy look on both platforms: the macOS panel is
clear, and the shared painter draws the square, opaque container with its
one-point edge, the field and the captions as it does on Linux, in the palette
of the panel's light or dark appearance. Linux keeps egui windows, the
layer-shell HUD and the active Omarchy theme. Both platforms share the engine,
IPC and pure-Rust view-models (form state, validation, recording rows, HUD
presentation). Each platform binds its views directly to its toolkit; there is
no cross-platform widget abstraction, and theming shares palette tokens only.

## Consequences

The macOS window code is AppKit through `objc2`; there is no Swift toolchain.
Native windows read and write through the same IPC as every other client, so
an app attached to an external daemon still reports that owner's state. One
painter draws the HUD on both platforms; only the palette and the captions'
menu names differ, and nothing behind the panel shows through it. On macOS the
field and caption colors are fitted to read at WCAG 4.5:1 on the opaque
container, including a handoff take's route wash, and the edge to 3:1 non-text
contrast. Offscreen NSView pixels are
not live panel/focus proof; native surfaces are verified in a disposable
`macvm` VM. Historical 0007 still governs Linux.

Revisit if: a supported Swift/SwiftUI build path becomes part of packaging.
