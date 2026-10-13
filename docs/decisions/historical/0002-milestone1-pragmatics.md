# ADR 0002: Milestone-1 pragmatics

Date: 2026-08-03. Status: accepted (revisit each item at milestone 2).

## Capture: `pw-record` child process, not pipewire-rs/cpal

pipewire-rs self-labels work-in-progress; cpal needs ALSA headers absent on the
target machine (and no sudo in the build session). `pw-record --rate 16000
--channels 1 --format s16` produces exactly the WAV transcribe-rs wants, ships
with every PipeWire install, and resamples in the server. Stop is SIGINT + wait
so the WAV header is finalized; SIGKILL corrupts the file. Cost: ~one process
spawn per dictation (irrelevant at human cadence). Revisit for streaming STT.

## Trigger: Unix socket + CLI, not GlobalShortcuts portal

Compositor keybindings run `cantrip toggle`, which talks to the daemon socket.
This works identically on COSMIC, GNOME, KDE, and wlroots with zero portal
negotiation, and it is the pattern Handy users asked for. Toggle (not
push-to-talk) because compositor custom shortcuts fire on key press only.
GlobalShortcuts portal + hold-to-talk arrive with the GTK settings UI.

## Injection: runtime-detected chain wtype → ydotool → clipboard

wtype needs `zwp_virtual_keyboard_v1` (COSMIC/wlroots: yes; GNOME: no).
ydotool works everywhere but needs its daemon and uinput permission. wl-copy
always works. Detect at runtime, prefer typing, fall back to clipboard with a
notification telling the user to paste. Type mode never touches the clipboard.

## STT: Parakeet v3 int8 on CPU, CUDA behind a feature flag

Parakeet TDT 0.6B int8 runs ~20x realtime on Zen 3 CPUs; a 9950X finalizes a
10 s utterance well under a second with no GPU dependency, no CUDA toolkit
requirement, and no VRAM contention with the compositor. `--features cuda`
enables `transcribe-rs/ort-cuda` for those who want it.

## No GTK in milestone 1

GTK4/libadwaita dev packages are not installed on the target machine and the
session has no sudo. The daemon is UI-free; notifications go over DBus
(notify-rust). The GTK settings window + layer-shell HUD are milestone 2 and
slot in as socket clients — no daemon changes required.
