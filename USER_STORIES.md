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

Evidence: `src/pipeline.rs`, `src/stt.rs`, `src/capture.rs`

## US-002 Observe dictation and processing state via the HUD

Statement: When I dictate, I want unobtrusive visual feedback on the desktop, so I know whether Cantrip is listening, processing, or finished without focus being stolen.

Criteria:
1. WHILE recording audio, THE SYSTEM SHALL display a Wayland layer-shell HUD capsule reflecting live input levels.
2. WHILE multi-chunk transcription runs, THE SYSTEM SHALL display determinate progress advancing only with completed chunks.
3. WHEN delivery completes, THE SYSTEM SHALL hold an acknowledgement indicator for a bounded duration before fading.
4. THE SYSTEM SHALL present HUD feedback without stealing window keyboard focus from the active client.

No-gos: no interactive notification popups or chatty notification daemon alerts.

Evidence: `src/hud.rs`, `src/daemon.rs`

## Capability: Guarded Delivery and Session Safety

## US-003 Guard against uncertain delivery destinations

Statement: When dictation finishes, I want Cantrip to verify the focused application has not changed or locked, so my words are never typed into the wrong window or leaked to a lock screen.

Criteria:
1. WHEN delivery begins, THE SYSTEM SHALL verify that the target window focus, compositor epoch, and session lock state match the stop event.
2. IF the active window, session lock, or interactive layer changed after recording stopped, THEN THE SYSTEM SHALL defer automatic delivery.
3. IF a delivery attempt fails or is interrupted after keys or clipboard transfer may have occurred, THEN THE SYSTEM SHALL mark the handoff uncertain and not retry.

No-gos: no blind keyboard retries into uncertain window targets.

Evidence: `src/desktop.rs`, `src/inject.rs`

## US-004 Select between clipboard paste and virtual keyboard injection

Statement: When I dictate into applications with distinct input capabilities, I want to control whether text is delivered via clipboard paste or virtual typing, so I can dictate multiline text without clobbering my clipboard when typing is required.

Criteria:
1. WHERE injection mode is set to type, THE SYSTEM SHALL send virtual keystrokes via Wayland protocols without reading or writing the system clipboard.
2. WHERE injection mode is set to paste, THE SYSTEM SHALL copy text to the Wayland clipboard helper and emit Ctrl+Shift+V to the target window.
3. IF injection fails due to helper timeout or process exit, THEN THE SYSTEM SHALL terminate the helper process group cleanly.

No-gos: no clipboard read-and-restore hacks.

Evidence: `src/inject.rs`, `tests/inject_timeout.rs`

## Capability: Recording History and Durability

## US-005 Retain private local take history for recovery

Statement: When I complete a dictation take, I want audio and text saved locally in private history, so I can review or recover past speech even if the active application crashed.

Criteria:
1. WHEN recording stops, THE SYSTEM SHALL durably retain the raw audio WAV file under the take identity before running speech recognition.
2. WHEN transcription succeeds, THE SYSTEM SHALL write transcript text atomically to owner-only local storage under XDG_STATE_HOME.
3. THE SYSTEM SHALL omit spoken transcript text and audio content from operational logs and telemetry.

No-gos: no automatic cloud syncing of local history files.

Evidence: `src/archive.rs`, `src/recovery.rs`

## US-006 Forget retained take audio on explicit confirmation

Statement: When I want to remove a sensitive recording, I want to permanently delete the retained audio while keeping text history, so I can control sensitive voice recordings.

Criteria:
1. WHEN an operator confirms a forget command for a take identity, THE SYSTEM SHALL delete the retained audio file from local storage.
2. WHERE a take has completed archived transcript text, THE SYSTEM SHALL preserve the complete text archive while deleting the audio file.

No-gos: no automatic expiration or background purge of unconfirmed takes.

Evidence: `src/recovery.rs`, `src/archive.rs`

## Capability: Transcript Post-Processing and Triage

## US-007 Refine transcript text with opt-in cleanup

Statement: When I dictate spontaneous speech, I want an optional language model cleanup pass to strip filler words and punctuate sentences, so I get clean prose without losing my exact phrasing.

Criteria:
1. WHERE postproc is enabled in configuration, THE SYSTEM SHALL refine raw transcripts through the configured OpenAI-compatible endpoint.
2. THE SYSTEM SHALL instruct the cleanup model to preserve speaker meaning, questions, and commands without answering them.
3. IF cleanup fails or returns an empty response, THEN THE SYSTEM SHALL fall back to delivering the raw speech-to-text transcript.

No-gos: no automatic enabling of cloud cleanup without explicit configuration.

Evidence: `src/postproc.rs`, `tests/http_clients.rs`

## US-008 Bypass generative cleanup on clean takes via decision triage

Statement: When I dictate clear, error-free prose, I want a fast System One decision check to bypass the generative model, so I receive my text with minimal latency and zero unnecessary token spend.

Criteria:
1. WHERE postproc.decision_model is configured, THE SYSTEM SHALL evaluate the raw transcript with a structured decision triage question before generative passes.
2. WHEN decision triage determines the transcript is already clean with high confidence, THE SYSTEM SHALL bypass the generative model and deliver the raw transcript immediately.
3. IF the decision triage request fails or times out, THEN THE SYSTEM SHALL fail open and proceed with the standard post-processing pipeline.

No-gos: no blocking the user's dictation on decision endpoint errors.

Evidence: `src/postproc.rs`, `src/typesafe.rs`, `tests/http_clients.rs`

## US-009 Reject conversational hallucinations via decision watchdog

Statement: When an LLM cleanup model answers a dictated question instead of transcribing it, I want an automated watchdog to catch the error, so I never inject an assistant reply into my editor or chat.

Criteria:
1. WHERE postproc.decision_model is configured and generative cleanup has produced a candidate, THE SYSTEM SHALL evaluate candidate fidelity against the source transcript using structured decision questions.
2. WHEN the decision watchdog detects an answer-to-question failure or severe content distortion, THE SYSTEM SHALL reject the candidate and deliver the raw speech-to-text transcript.
3. IF the decision watchdog request fails, times out, or returns incomplete answer keys, THEN THE SYSTEM SHALL fail open and deliver the generated candidate.

No-gos: no silent adoption of answered questions or distorted text.

Evidence: `src/postproc.rs`, `src/typesafe.rs`, `tests/http_clients.rs`

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
2. WHEN complete text is available after optional cleanup, THE SYSTEM SHALL send its exact bytes on the command's stdin and provide `CANTRIP_TAKE_ID`, without a shell, keyboard, or clipboard.
3. IF the command exits nonzero or times out, THEN THE SYSTEM SHALL mark delivery failed and preserve recovery artifacts without automatically retrying.
4. IF transcription is partial, empty, or cancelled before dispatch, THEN THE SYSTEM SHALL NOT start the handoff command.

No-gos: no transcript or child output in logs or telemetry; no desktop fallback.

Evidence: `src/daemon.rs`, `src/config.rs`, `src/ipc.rs`, `docs/adr/0027-named-handoff-targets.md`
