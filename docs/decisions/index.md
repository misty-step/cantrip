# Decisions

A steering veto list. Open it before proposing a new primitive, surface, or
storage home. Write a record after deciding *not* to build something the next
agent will otherwise re-propose. Do not write one per PR. Start from
[0000-template.md](0000-template.md).

## Steering

- [0004](0004-postprocessing.md): one cleanup wire protocol, no provider SDKs.
- [0008](0008-daemon-owned-hud.md): the Linux daemon owns HUD lifecycle.
- [0012](0012-eval-driven-postprocessing.md): evaluate cleanup fidelity instead of production text heuristics.
- [0019](0019-per-take-recovery-and-verified-delivery.md): one per-take archive and verified delivery, no uncertain retries.
- [0020](0020-local-completion-and-explicit-audio-deletion.md): retain every stopped take; only confirmed Forget deletes audio.
- [0022](0022-verified-linux-release.md): one verified CPU-only Linux release, no second release service.
- [0025](0025-source-install-builds-this-tree.md): source install builds this tree, never copies a stale artifact.
- [0026](0026-typesafe-system-one-decisions.md): structured decisions are opt-in gates, never dictated actions.
- [0027](0027-named-handoff-targets.md): named local handoffs never fall back to desktop injection.
- [0030](0030-local-only-handoff-targets.md): local-only policy belongs to the handoff target.
- [0031](0031-shared-engine-native-macos.md): one shared engine; macOS manual paste without silent downgrade.
- [0032](0032-cloud-stt-without-inference-deadlines.md): wait for cloud STT without inference deadlines; cleanup honors its explicit timeout.
- [0033](0033-capture-owner-microphone-authorization.md): microphone authorization is read and requested only through the capture owner.
- [0034](0034-native-macos-presentation.md): native AppKit surfaces on macOS; Linux keeps egui and Omarchy.

## Historical

Former ADRs, with relative links rebased and supersession pointers added where
needed; original decisions remain as history. Their numbers stay reserved.

- [0001](historical/0001-new-rust-linux-app.md): new Rust Linux app instead of porting Vox.
- [0002](historical/0002-milestone1-pragmatics.md): milestone-1 pragmatics.
- [0003](historical/0003-persistent-status-ui.md): persistent status indicator via long-lived notification.
- [0005](historical/0005-config-v2-and-settings-surface.md): config schema v2 and the settings surface.
- [0006](historical/0006-layer-shell-hud.md): status via a layer-shell HUD overlay.
- [0007](historical/0007-settings-gui-eframe.md): configuration via an egui settings window.
- [0009](historical/0009-paste-first-injection.md): paste-first injection.
- [0010](historical/0010-warm-minimal-hud.md): the HUD is a Warm Minimal capsule (static phases, no fake progress).
- [0011](historical/0011-chunked-local-stt.md): chunk long local STT audio before Parakeet inference.
- [0013](historical/0013-local-transcript-history.md): owner-private local transcript history.
- [0014](historical/0014-live-capture-signal.md): the recording HUD shows measured input signal.
- [0015](historical/0015-command-status-ipc-boundary.md): command acknowledgements and status snapshots use distinct IPC types.
- [0016](historical/0016-typed-processing-stage.md): processing stage remains typed across the IPC boundary.
- [0017](historical/0017-powder-board-of-record.md): powder is the board of record.
- [0018](historical/0018-bounded-remote-stt-and-recovery.md): bound remote upload sizes and preserve recoverable failures; elapsed-time policy superseded by [0032](0032-cloud-stt-without-inference-deadlines.md).
- [0021](historical/0021-signed-pixel-waveform.md): signed pixel waveform from independent PCM measurements.
- [0023](historical/0023-persistent-pixel-field.md): one persistent pixel field, animated by active cells.
- [0028](historical/0028-handoff-destination-tint.md): handoff takes are tinted and labelled; the default flow is not.
- [0029](historical/0029-quiet-bar-mark.md): a quiet Cantrip mark in the Omarchy bar, without a backlog count.
