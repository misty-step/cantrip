# Stories

<!-- Root artifact: what users must be able to do. One file, ids never
reused, criteria a check can fail on. skill://user-stories guides edits. -->

## Capability: Local Voice Dictation

## US-001 Dictate text into the focused window

Statement: When I hold the dictation hotkey and speak into my microphone, I want speech transcribed locally on my CPU and delivered to my active application, so I can enter text quickly without sending voice data to a third-party service.

Criteria:
1. WHEN I press and hold the configured capture shortcut, THE SYSTEM SHALL record audio from the PipeWire session.
2. WHEN I release the shortcut, THE SYSTEM SHALL run CPU-only speech-to-text inference with the default installed local model.
3. WHEN transcription succeeds and destination safety is verified, THE SYSTEM SHALL deliver the transcribed text to the active window.
4. IF the local speech-to-text model is not installed, THEN THE SYSTEM SHALL report a missing model error without attempting transcription.

No-gos: no automatic downloading of models without explicit operator action.

Evidence: `crates/engine/src/pipeline.rs`, `crates/engine/src/stt.rs`, `src/capture/linux.rs`

## US-002 Observe dictation and processing state via the HUD

Statement: When I dictate, I want unobtrusive visual feedback on the desktop, so I know whether Cantrip is listening, processing, or finished without focus being stolen.

Criteria:
1. WHILE recording audio, THE SYSTEM SHALL display a Wayland layer-shell HUD capsule reflecting live input levels.
2. WHILE multi-chunk transcription runs, THE SYSTEM SHALL display determinate progress advancing only with completed chunks.
3. WHEN delivery completes, THE SYSTEM SHALL hold an acknowledgement indicator for a bounded duration before fading.
4. THE SYSTEM SHALL present HUD feedback without stealing window keyboard focus from the active client.
5. WHILE recording, THE SYSTEM SHALL keep capturing through silence and at any take length until a toggle, stop, cancel, or daemon shutdown ends the take.
6. WHEN a take stops recording, THE SYSTEM SHALL log which of those ended it, as a reason class without transcript text.

No-gos: no interactive notification popups or chatty notification daemon alerts.

Evidence: `src/hud/wayland.rs`, `crates/engine/src/engine.rs`

## US-012 See Cantrip's state in the Omarchy bar at a glance

Statement: When Cantrip runs on Omarchy, I want one quiet, branded mark in the bar that shows where a take is going and whether my last take needs me, without a backlog counter I can't clear.

Criteria:
1. WHILE idle with no outcome needing attention, THE SYSTEM SHALL show only the dimmed Cantrip mark, with no count or text.
2. WHILE a take records or processes, THE SYSTEM SHALL color the mark with that take's route color: the theme accent, or the handoff target's color.
3. WHEN the latest outcome needs attention, THE SYSTEM SHALL color the mark with the theme's urgent color until the next take starts or the outcome is dismissed.
4. WHEN I middle-click the mark while it shows attention, THE SYSTEM SHALL dismiss that outcome without deleting any recording.
5. THE SYSTEM SHALL keep the mark in one fixed-width slot in every state.

No-gos: no count of unresolved recordings in the bar; no text or width changes in the bar item.

Evidence: `integrations/omarchy/BarWidget.qml`, `integrations/omarchy/Status.js`, `docs/adr/0029-quiet-bar-mark.md`

## Capability: Guarded Delivery and Session Safety

## US-003 Guard against uncertain delivery destinations

Statement: When dictation finishes, I want Cantrip to verify the focused application has not changed or locked, so my words are never typed into the wrong window or leaked to a lock screen.

Criteria:
1. WHEN delivery begins, THE SYSTEM SHALL verify that the target window focus, compositor epoch, and session lock state match the stop event.
2. IF the active window, session lock, or interactive layer changed after recording stopped, THEN THE SYSTEM SHALL defer automatic delivery.
3. IF a delivery attempt fails or is interrupted after keys or clipboard transfer may have occurred, THEN THE SYSTEM SHALL mark the handoff uncertain and not retry.

No-gos: no blind keyboard retries into uncertain window targets.

Evidence: `src/desktop/linux.rs`, `src/inject/linux.rs`

## US-004 Select between clipboard paste and virtual keyboard injection

Statement: When I dictate into applications with distinct input capabilities, I want to control whether text is delivered via clipboard paste or virtual typing, so I can dictate multiline text without clobbering my clipboard when typing is required.

Criteria:
1. WHERE injection mode is set to type, THE SYSTEM SHALL send virtual keystrokes via Wayland protocols without reading or writing the system clipboard.
2. WHERE injection mode is set to paste, THE SYSTEM SHALL copy text to the Wayland clipboard helper and emit Ctrl+Shift+V to the target window.
3. IF injection fails due to helper timeout or process exit, THEN THE SYSTEM SHALL terminate the helper process group cleanly.

No-gos: no clipboard read-and-restore hacks.

Evidence: `src/inject/linux.rs`, `tests/inject_timeout.rs`

## Capability: Recording History and Durability

## US-005 Retain private local take history for recovery

Statement: When I complete a dictation take, I want audio and text saved locally in private history, so I can review or recover past speech even if the active application crashed.

Criteria:
1. WHEN recording stops, THE SYSTEM SHALL durably retain the raw audio WAV file under the take identity before running speech recognition.
2. WHEN transcription succeeds, THE SYSTEM SHALL write transcript text atomically to owner-only local storage under XDG_STATE_HOME.
3. THE SYSTEM SHALL omit spoken transcript text and audio content from operational logs and telemetry.
4. WHEN I open Settings, THE SYSTEM SHALL show past transcripts newest first with local time, audio duration when known, and opening words, including completed takes from older archives, with several complete rows and unclipped Copy actions visible at the default window size.
5. WHEN I choose Copy for a past transcript in Settings, THE SYSTEM SHALL copy its full saved text to the clipboard without retranscribing, sending keys, or requiring a running daemon.

No-gos: no automatic cloud syncing of local history files.

Evidence: `crates/engine/src/archive.rs`, `crates/engine/src/recovery.rs`, `src/settings.rs`, `scripts/verify-settings-history.py`

## US-006 Forget retained take audio on explicit confirmation

Statement: When I want to remove a sensitive recording, I want to permanently delete the retained audio while keeping text history, so I can control sensitive voice recordings.

Criteria:
1. WHEN an operator confirms a forget command for a take identity, THE SYSTEM SHALL delete the retained audio file from local storage.
2. WHERE a take has completed archived transcript text, THE SYSTEM SHALL preserve the complete text archive while deleting the audio file.

No-gos: no automatic expiration or background purge of unconfirmed takes.

Evidence: `crates/engine/src/recovery.rs`, `crates/engine/src/archive.rs`

## Capability: Transcript Post-Processing and Triage

## US-007 Refine transcript text with opt-in cleanup

Statement: When I dictate spontaneous speech, I want an optional language model cleanup pass to strip filler words and punctuate sentences, so I get clean prose without losing my exact phrasing.

Criteria:
1. WHERE postproc is enabled in configuration, THE SYSTEM SHALL refine raw transcripts through the configured OpenAI-compatible endpoint.
2. THE SYSTEM SHALL instruct the cleanup model to preserve speaker meaning, questions, and commands without answering them.
3. IF cleanup fails or returns an empty response, THEN THE SYSTEM SHALL fall back to delivering the raw speech-to-text transcript.

No-gos: no automatic enabling of cloud cleanup without explicit configuration.

Evidence: `crates/engine/src/postproc.rs`, `tests/http_clients.rs`

## US-008 Bypass generative cleanup on clean takes via decision triage

Statement: When I dictate clear, error-free prose, I want a fast System One decision check to bypass the generative model, so I receive my text with minimal latency and zero unnecessary token spend.

Criteria:
1. WHERE postproc.decision_model is configured, THE SYSTEM SHALL evaluate the raw transcript with a structured decision triage question before generative passes.
2. WHEN decision triage determines the transcript is already clean with high confidence, THE SYSTEM SHALL bypass the generative model and deliver the raw transcript immediately.
3. IF the decision triage request fails or times out, THEN THE SYSTEM SHALL fail open and proceed with the standard post-processing pipeline.

No-gos: no blocking the user's dictation on decision endpoint errors.

Evidence: `crates/engine/src/postproc.rs`, `crates/engine/src/typesafe.rs`, `tests/http_clients.rs`

## US-009 Reject conversational hallucinations via decision watchdog

Statement: When an LLM cleanup model answers a dictated question instead of transcribing it, I want an automated watchdog to catch the error, so I never inject an assistant reply into my editor or chat.

Criteria:
1. WHERE postproc.decision_model is configured and generative cleanup has produced a candidate, THE SYSTEM SHALL evaluate candidate fidelity against the source transcript using structured decision questions.
2. WHEN the decision watchdog detects an answer-to-question failure or severe content distortion, THE SYSTEM SHALL reject the candidate and deliver the raw speech-to-text transcript.
3. IF the decision watchdog request fails, times out, or returns incomplete answer keys, THEN THE SYSTEM SHALL fail open and deliver the generated candidate.

No-gos: no silent adoption of answered questions or distorted text.

Evidence: `crates/engine/src/postproc.rs`, `crates/engine/src/typesafe.rs`, `tests/http_clients.rs`

## Capability: Evaluation and Model Grading

## US-010 Grade post-processing lanes with an additive model judge

Statement: When evaluating transcript post-processing models on synthetic benchmarks, I want an objective System One judge scoring role fidelity and negation preservation, so I can rank models without brittle exact-string matching.

Criteria:
1. WHEN the evaluation behavior suite runs with the model judge enabled, THE SYSTEM SHALL score each candidate output against the input on role fidelity, content preservation, and disfluency removal.
2. THE SYSTEM SHALL derive the composite pass or fail verdict from deterministic criteria thresholds where low role fidelity or dropped negations fail.
3. THE SYSTEM SHALL report model judge verdicts as an additive signal that never overrides deterministic protocol failures or exact accepted strings.

No-gos: no sending private operator audio or non-synthetic dictations to the evaluation judge.

Evidence: `examples/eval/judge.rs`, `examples/eval/main.rs`, `docs/adr/0026-typesafe-system-one-decisions.md`

## Capability: Local Agent Handoff

## US-011 Hand a completed take to a named local command

Statement: When dictating to a local agent, I want a separate shortcut to send
the finished transcript directly to its configured command, without using my
clipboard or the focused window.

Criteria:
1. WHEN I start or toggle recording with `--handoff NAME`, THE SYSTEM SHALL reject unknown names before capture and snapshot the configured command for that take.
2. WHEN complete text is available after optional cleanup, THE SYSTEM SHALL send its exact bytes on the command's stdin and provide `CANTRIP_TAKE_ID`, plus `CANTRIP_TAKE_AUDIO` with the retained recording's path when it is available, without a shell, keyboard, or clipboard.
3. IF the command exits nonzero or times out, THEN THE SYSTEM SHALL mark delivery failed and preserve recovery artifacts without automatically retrying.
4. IF transcription is partial, empty, or cancelled before dispatch, THEN THE SYSTEM SHALL NOT start the handoff command.
5. WHEN a toggle's `--handoff` differs from the recording take's, including a missing one, THE SYSTEM SHALL keep recording and neither deliver nor hand off that take.
6. IF a handoff command fails or times out, THEN THE SYSTEM SHALL kill its whole process group before reporting the failure.
7. WHEN a target sets `label`, THE SYSTEM SHALL show that label instead of the target name in delivery messages.
8. WHILE a handoff take records, processes, or shows its outcome, THE SYSTEM SHALL tint the HUD in that target's own theme-derived color and show "to LABEL" in its upper left, use the same color and words in the bar widget and actions window, and leave default takes unchanged.

9. WHEN a target sets `local_only`, THE SYSTEM SHALL transcribe its takes with the installed default local model only, SHALL NOT run cleanup or any cloud attempt for them, SHALL refuse the take before capture if the local model is missing or cleanup was asked for, and SHALL run its command with `CANTRIP_LOCAL_ONLY=1`, which no other target's command sees.

No-gos: no transcript or child output in logs or telemetry; no desktop fallback; no destination indicator on default takes.

Evidence: `crates/engine/src/engine.rs`, `crates/engine/src/config.rs`, `crates/engine/src/ipc.rs`, `src/hud.rs`, `src/theme.rs`, `docs/adr/0027-named-handoff-targets.md`, `docs/adr/0028-handoff-destination-tint.md`, `docs/adr/0030-local-only-handoff-targets.md`

## Capability: Native macOS Clipboard Dictation

## US-013 Dictate on Mac and deliberately paste into my editor

Statement: When I use Cantrip on my Mac, I want a native menu-bar client to
control local dictation and copy my finished words for manual paste, so I can
choose their destination without keyboard injection or broad desktop permissions.

Criteria:
1. WHERE I use a matching-architecture Cantrip.app on macOS 13.3 or newer, THE SYSTEM SHALL provide a native menu-bar client and passive nonactivating panel without stealing editor keyboard focus or accepting pointer input in the panel.
2. WHEN I open the app with no engine running, THE SYSTEM SHALL start one shared engine; WHEN an engine already runs, THE SYSTEM SHALL attach without starting a duplicate.
3. WHERE I invoke `daemon` on Mac, THE SYSTEM SHALL run only the engine without owning a menu, panel, or app shortcut.
4. WHEN I deliberately allow microphone access in Settings or the app menu, THE SYSTEM SHALL request the supported macOS microphone grant; IF access is denied, restricted, or not yet granted, THEN THE SYSTEM SHALL refuse capture with actionable feedback without requesting Accessibility, Input Monitoring, or Screen Recording.
5. WHEN I choose a microphone, THE SYSTEM SHALL persist its stable CoreAudio input UID; IF that selected UID is unavailable, THEN THE SYSTEM SHALL refuse capture without falling back to another input.
6. WHEN I press and release the configured global shortcut, THE SYSTEM SHALL toggle capture using editable Control+Option+Space by default; IF a replacement cannot register, THEN THE SYSTEM SHALL retain the previous shortcut and report the conflict or failure.
7. THE SYSTEM SHALL expose Start/Stop, Cancel, Settings, Check Setup, selected-outcome Copy/recovery/dismiss, and recording-history controls from the native app without a second dictation workflow.
8. WHERE I create new Mac configuration, THE SYSTEM SHALL default to Clipboard while preserving the Linux Auto default; WHERE existing Mac Auto/Paste/Type is configured, THE SYSTEM SHALL preserve and expose that value with an unsupported warning and defer complete and partial delivery without clipboard effects, keys, or automatic clipboard fallback.
9. WHEN explicit Clipboard delivery succeeds, THE SYSTEM SHALL copy the composed Unicode transcript and paragraph breaks through native NSPasteboard using CurrentHostOnly without reading or restoring prior contents; THE SYSTEM SHALL leave Command+V and inspection of the actual editor text to me.
10. IF the installed local model is missing, THEN THE SYSTEM SHALL report its absence without implicitly downloading models or switching providers; WHERE I choose the local trial, THE SYSTEM SHALL keep cleanup and telemetry opt-in.
11. WHEN capture stops or graceful owned shutdown occurs, THE SYSTEM SHALL retain available audio under its take identity before transcription or runtime removal and preserve the shared recovery, cancellation, Forget, logging, and cloud-content contracts.
12. WHERE no XDG override is set, THE SYSTEM SHALL use native Application Support configuration/models and durable state/history, a short owner-private runtime path, and native login Keychain credentials; WHERE an absolute XDG override is set, THE SYSTEM SHALL honor the validated override without silently changing storage roots.
13. THE SYSTEM SHALL normalize private application-owned directories to 0700 and durable files it publishes or normalizes to 0600 without inherited ACL grants, and SHALL NOT describe these plaintext permissions or temporary runtime storage as encryption or reboot-durable retention.
14. WHEN I choose Open at Login, THE SYSTEM SHALL explicitly register or unregister its own SMAppService login item and report required approval or failure without silently enabling startup.
15. WHEN I choose Quit Cantrip, THE SYSTEM SHALL wait for its owned engine to finalize and exit; WHERE the app attached to an externally owned daemon, THE SYSTEM SHALL leave that daemon running for its actual owner to stop.

No-gos: no automatic Mac typing/paste fallback, clipboard read/restore, implicit model downloads, broad desktop permission grants, duplicate engine or durable ledger; no weakening of the existing Linux story criteria. Clipboard managers may retain copies. Offscreen/headless proof is not attended mic-to-editor, panel-focus, Intel hardware, or published/notarized-release proof.

Evidence: `src/macos.rs`, `src/macos/shortcut.rs`, `src/capture/macos.rs`, `src/desktop/macos.rs`, `src/inject/macos.rs`, `src/hud/macos.rs`, `src/settings.rs`, `crates/engine/src/engine.rs`, `crates/engine/src/paths.rs`, `crates/engine/src/keys.rs`, `crates/engine/tests/capture_start.rs`, `scripts/verify-macos`, `scripts/package-macos`, `tests/test_release_macos.py`, `docs/USAGE.md`, `docs/DESKTOP.md`, `docs/PRIVACY.md`
