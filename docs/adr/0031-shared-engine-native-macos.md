# ADR 0031: Shared engine and native macOS host

Date: 2026-10-05. Status: accepted.

## Context

Cantrip began as a Linux application rather than a port of Vox ([ADR 0001](0001-new-rust-linux-app.md)). Adding macOS should not fork its dictation state machine, durable recovery history, or safety policy, nor weaken Linux's verified Hyprland/logind delivery. AppKit main-thread ownership, native audio, Keychain, and app signing are platform mechanisms; they are not reasons to create another workflow or work ledger.

The operator explicitly chose clipboard/manual paste for macOS. Frontmost-app snapshots or Accessibility permission alone cannot establish the uninterrupted destination/session history required for automatic keys. An existing explicit Auto/Paste/Type choice must not become permission to overwrite the clipboard instead.

## Decision

### One workflow and store, native mechanisms behind ports

Use two Rust workspace crates with one version authority in `workspace.package.version`:

- `cantrip-engine` owns the workflow in `crates/engine/src/engine.rs`, cancellation, per-take identities, durable archive/recovery, configuration, IPC, model management, STT, cleanup, and telemetry. Platform-specific filesystem/Keychain details remain target-gated where needed; the shared engine is not a claim of support for arbitrary operating systems.
- The root `cantrip` crate owns the CLI, desktop composition root, capture, delivery/session adapters, HUD, Settings/Actions, and native application lifecycle. `src/daemon.rs` is a thin host for the same engine on both platforms.
- `crates/engine/src/ports.rs` defines `Platform`, `Recorder`, and `DeliveryPermit`. `Platform` supplies capture creation, delivery preparation/permits, local handoff color, and content-free peer identity. `Recorder` owns live input signals and stop/finalization. `DeliveryPermit` supplies stop-time session/destination history and an effect-aware native delivery mechanism. The engine owns policy and durable transitions; adapters do not introduce another state machine.

Keep std threads and mpsc coordination. No new async runtime, alternate archive, compatibility aliases, or per-OS durable work ledger is introduced. UI hosts consume the shared IPC/status contract rather than maintaining parallel dictation state.

Native capture failure is also part of the port contract: quiesce producers and finalize any accepted WAV prefix before returning an error. Only conclusively empty, unaccepted startup headers can be removed. The shared engine must durably retain a surviving prefix before consuming runtime originals, just as it retains stopped/cancelled/gracefully interrupted takes before STT. Storage uncertainty preserves the original and surfaces the failure.

### Preserve Linux; compose a native Mac host

Linux keeps PipeWire `pw-record` capture with SIGINT/wait finalization, direct Hyprland/logind guarded delivery, existing clipboard tools, and its supervised Wayland layer-shell HUD child. Its published release provenance, binary-only installer, source installer, rollback, and graphical-session service route are unchanged. macOS packaging is separate; Linux installations do not acquire Apple dependencies.

The macOS accessory/menu-bar app owns AppKit and its passive NSPanel on the main thread. The engine runs on an owned std thread through the same host, or the app attaches to an already running engine without duplicating it. Owned shutdown waits for shared finalization; quitting an attached app does not kill an externally owned daemon. On macOS, no arguments or `app` starts this host; `daemon` is engine-only, without the menu bar or HUD. Deliberately opened Settings and recording/recovery windows use the shared contracts.

The HUD state machine and painter are shared; native hosts supply only their surface and resolved palette. AppKit semantic accents can fall below readable contrast on light surfaces. Mac text/route colors retain their native hue and saturation, adjusting lightness to at least 4.5:1 against the resolved background and surface; Linux theme colors remain unchanged.

Mac capture uses AVAudioEngine/CoreAudio on an owned native thread, with stable input-device UIDs. A bounded preallocated realtime queue feeds worker-side downmix/resampling to 16 kHz mono PCM16; the callback does not own durable workflow policy. An unavailable configured UID is refused, not silently replaced with another microphone.

Permission setup is explicit and uses supported APIs: microphone access for capture, Carbon global shortcut registration, and an explicit SMAppService Open At Login choice. Supported behavior does not require Accessibility, Input Monitoring, or Screen Recording grants. Do not replace unknown lock/session history with private screen-lock APIs or permission-based assumptions.

### Explicit manual paste, no silent downgrade

New Mac configuration defaults to Clipboard; Linux's Auto default remains unchanged. Native NSPasteboard copy preserves Unicode and paragraphs, uses CurrentHostOnly to avoid automatic Universal Clipboard publication, and never reads/restores previous clipboard contents. Copy acknowledgement is not destination-application receipt.

Mac delivery exposes only explicit clipboard/manual paste. Existing Auto/Paste/Type values remain visible, preserved, and unsupported: they reach Deferred before opening/claiming the pasteboard or sending keys. Settings does not silently migrate those values. Shared `ClipboardFallback::ExplicitOnly` encodes that boundary, including incomplete text; Linux retains its existing `OnDeferral` policy. Strict Type never reads or writes a clipboard on either platform. Unknown history defers, and a potentially completed external handoff is Uncertain with no alternate-backend retry.

Dismiss acknowledges feedback, not deletion. Confirmed Forget still removes only the selected take's retained audio and incomplete text; complete archived transcript text stays. Local STT is the default; configured-cloud whole-take fallback, explicit model downloads, opt-in cleanup/telemetry, and content-free operational logging retain their shared contracts.

### Native private paths and credentials

Mac defaults use `~/Library/Application Support/cantrip/` for configuration and models, with `state/daemon.log` and `state/transcripts/` for durable state. The short owner-private runtime is `/private/tmp/cantrip-$UID/cantrip/`, accommodating Darwin Unix-socket path limits; it is not guaranteed RAM-backed or reboot-persistent. Explicit absolute XDG overrides support isolated runs without touching the operator's native state.

History operations use descriptor-relative enumeration instead of Linux `/proc` FD traversal, with target-gated Darwin atomic swap and Unix peer/connection support. Private application-owned directories are normalized to 0700; newly published or normalized files are 0600 with inherited Darwin ACL grants stripped. Existing native logs are normalized through their open descriptor, not a substituted pathname. History/audio are plaintext; private permissions are not encryption. API keys use the native login Keychain on macOS and the existing Secret Service backend on Linux, never config files or logs.

### Separate development and production app packaging

The Mac baseline is 13.3+, with separate matching-native arm64/Intel builds, not a Rosetta/cross build or a claimed universal binary. `scripts/package-macos` builds `Cantrip.app` with stable identifier `com.misty-step.cantrip`, the microphone purpose string and minimal audio-input entitlement, relocates every non-system dylib inside the bundle, and checks Mach-O architecture/minimum OS. The CLI remains `Cantrip.app/Contents/MacOS/cantrip`. The whole relocated bundle runs without source, Rust, Python, or Homebrew at runtime; models remain a deliberate separate download.

Packaging and runtime preparation share one native-target gate. It queries the calling Python process using Apple's documented [`sysctl.proc_translated`](https://developer.apple.com/videos/play/wwdc2020/10686/?time=871), not a child `sysctl` executable that might launch natively under a translated parent. Translated or unverifiable callers are refused; native Intel's documented ENOENT result is accepted.

Rust is pinned to 1.98.1; scripts require Python 3.11+ and selected Xcode command-line tools. Intel automatically builds CPU ONNX Runtime 1.24.2 from immutable commit `058787ceead760166e3c50a0a4cba8a833a6f53f` because locked `ort-sys 2.0.0-rc.12` has no Intel Mac prebuilt. That bootstrap needs CMake 3.28+, defaults to two jobs, and uses a project-local verified cache. It does not downgrade shared dependencies or install a global runtime. Arm64 uses checksum-pinned prebuilts unless `--source-runtime` is explicitly selected.

`--ad-hoc` is explicitly DEVELOPMENT ONLY: not hardened, not Developer ID signed, and not notarized. Without a Team ID, hardened library validation would reject the contained third-party dylibs; the development manifest must not claim otherwise.

Production mode requires a clean committed checkout, a valid Developer ID Application certificate/private key in Keychain, its matching ten-character Team ID, and an authenticated notarytool Keychain profile. It signs nested libraries/app with hardened runtime and secure timestamps, verifies identity/entitlements, requires Accepted notarization without error issues, staples/validates the ticket, and passes Gatekeeper assessment before writing final distributable output. It has no unsigned, unnotarized, or ad-hoc fallback and never overwrites an existing output. Packaging neither publishes releases nor grants permissions, launches the app, enables startup, or changes user data. No public Mac release has been published; the Linux attestation contract does not describe local Mac artifacts.

## Verification and limits

The completed native arm64 development-app smoke exercised strict workspace clippy, workspace tests including the consumer startup-audio retention regression, offline evaluation, and six real Mach-O relocation/signing tests. `scripts/verify-macos` passed against the relocated real app with disposable HOME/XDG directories and a system-only runtime PATH:

- Real default local Parakeet STT on the checked public JFK WAV after an explicit model download; missing-model refusal did not implicitly download anything.
- Denied/invalid-UID native capture refusal with no live WAV, explicit Auto/Paste/Type replay to terminal Deferred without config migration, and graceful daemon termination.
- Five production HUD states rendered as offscreen native NSView PNGs, with sanitized `verification.json`; no private transcript output entered the proof.

The final native-log smoke removed an existing owned log's ACL grant and refused symlinks, hardlinks, and a nonregular log without mutating unrelated targets or blocking startup. Native named-pasteboard tests exercised Unicode/paragraph fidelity and deadline refusal using process/case-isolated boards, not the general clipboard. Final warning text and the attention edge measured 4.55:1 on the rendered white surface; dark appearance is covered by contrast arithmetic, not a rendered dark-mode claim.

Linux `scripts/check` passed. The isolated headless Sway Settings journey exercised exact full 351-byte transcript copy, unchanged archive, stale-row copy refusal preserving prior clipboard, and refresh to empty history. This is Linux clipboard evidence, not evidence of Mac clipboard behavior.

Offscreen NSView pixels establish native rendering, not live NSPanel placement, focus behavior, or attended microphone-to-editor success. The native helper did not touch the operator's live display, grant TCC permissions, capture a microphone, or request general Mac clipboard/shortcut/login actions. Attended microphone permission/capture, real shortcut/menu actions, Open At Login, general clipboard/manual destination paste, Intel runtime execution, and production signing/notarization remain unexercised. The matching arm64/Intel CI workflow exists but was not dispatched for this smoke; its definition is not an Intel pass. No Apple certificate, permission grant, notarization ticket, or published Mac asset is inferred from the development proof.

## Consequences

- Workflow/recovery changes have one owner; native adapters implement mechanisms without changing policy or duplicating durable state.
- Linux retains its tested delivery and distribution route. Mac has an honest native app/development route and explicit fail-closed production prerequisites, not a Linux installer repurposed for bundles.
- Mac manual paste is a deliberate product boundary, not a temporary automatic-keys fallback. Adding guarded keyboard delivery would require a new supported history/effect contract and an explicit decision.
- Native packaging and public-fixture verification are reproducible, but do not substitute for attended platform-specific journeys or production signing proof. See [installation](../INSTALLATION.md#macos-native-app-from-source) for build commands and prerequisites.
