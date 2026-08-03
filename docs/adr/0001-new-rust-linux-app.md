# ADR 0001: New Rust Linux app instead of porting Vox

Date: 2026-08-03. Status: accepted.

## Context

Vox is a Swift/SwiftPM macOS dictation app (menu bar, AppKit/SwiftUI, AVFoundation,
Carbon hotkeys). The primary machine is now Linux (COSMIC/Wayland). Swift on Linux
has no AppKit or SwiftUI, so none of Vox's UI or platform layer ports. Research
(2026-08-03) found every load-bearing dependency for a Linux dictation app exists
as a maintained Rust crate: transcribe-rs (Parakeet/Whisper/Moonshine ONNX engine,
proven in Handy), gtk4-rs + libadwaita + gtk4-layer-shell, pipewire-rs, ashpd
(XDG portals), ksni (tray). Go equivalents are solo-maintained (gotk4),
experimental (puregotk), or proof-of-concept (PipeWire bindings).

## Decision

Build `cantrip`, a fresh Rust application. Do not port Vox. Carry over Vox's
design assets as concepts: the session state machine, decorator-based provider
composition, recovery store, and the privacy rule (no transcript content in logs,
char counts only).

## Consequences

- One language, one toolchain; agents and humans share one build.
- Rust deps churn (transcribe-rs 0.3.x, pipewire-rs is WIP): pin versions,
  vendor if breakage bites.
- Local-first inverts Vox's cloud-first STT: Parakeet local by default, BYOK
  cloud optional later.
