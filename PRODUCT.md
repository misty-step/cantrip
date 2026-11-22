# Cantrip v2 — Product

Cantrip v2 is a macOS menu-bar app with two jobs: dictate text into whatever app
has focus, and record meetings into Markdown notes. One shortcut, three modes,
one vocabulary list, one API key. Settings that do not serve those two jobs do
not exist.

Status: product intent for new macOS work (Slice #42, 2026-11-22). Nothing in
this document is built yet. [DESIGN.md](DESIGN.md) holds the architecture and
the measurements behind the numbers here.

## Supersession

v1 is the Rust workspace in this repository. It is the Linux dictation app and,
as of v0.1.15, also a Developer ID-signed, notarized arm64 macOS app; README
still says no macOS release is published. v2 replaces
[SPEC.md](SPEC.md) and [VISION.md](VISION.md) as the statement of intent for the
**macOS** product. Those files stay unchanged in this slice and continue to
describe v1. v1 remains the Linux product and is not retired by this document.

v2 changes these v1 commitments. Each is a deliberate decision here:

| v1 commitment | v2 |
| --- | --- |
| Local speech recognition by default (VISION.md, AGENTS.md) | OpenRouter cloud by default; on-device Apple Speech when there is no key or no network, and only when on-device recognition is supported and authorized. Otherwise Cantrip refuses to recognize and keeps the recording |
| macOS manual paste only (ADR 0031, which allows reversal by "an explicit product decision") | Automatic delivery in two tiers, with copy-only when focus moved or the target is uncertain (see below) |
| Automatic keys require uninterrupted verified destination history (ADR 0019) | Target captured at the start and checked again at delivery; not uninterrupted history (see below) |
| Every stopped take is retained; only confirmed Forget deletes audio (ADR 0020) | Audio of a succeeded dictation is deleted automatically; failed audio is kept until Retry succeeds or the user discards it (see below) |
| A cloud failure completes on local Parakeet (ADR 0020) | A cloud error keeps the recording for Retry. On-device recognition runs only with no key or no network |
| Clipboard contents are never read or restored (AGENTS.md) | Target-bound insertion does not touch the clipboard. A `Cmd+V` paste snapshots it and restores it only when the insertion is confirmed; otherwise the dictation stays on the clipboard |
| No Swift toolchain (ADR 0034) | Native Swift/SwiftUI app |
| Clean uses DeepSeek, chosen on measured fidelity (ADR 0012) | Clean and Elevated use `openai/gpt-6-luna` |

### Delivery, retention and deletion against ADRs 0019, 0020 and 0031

- **ADR 0031 (manual paste only).** v2 reverses it for macOS. ADR 0031 is right
  that a frontmost-app snapshot cannot prove an uninterrupted destination, and v2
  does not claim that it does. The promise is the smaller one in journey 1.
- **ADR 0019 (verified-destination delivery).** v2 drops the requirement for
  uninterrupted destination history and the archive of every take. It keeps three
  rules: an unknown or changed target gets no keystroke and no insertion, only a
  copy; a handoff that may have completed is never retried through the other tier;
  and posting is not receipt. Cantrip says "Inserted" only after it reads the field
  before and after and sees exactly its own edit. Without that it says "Sent —
  unconfirmed", or "Posted — unconfirmed" for a paste, which is all it knows.
- **ADR 0020 (retain until confirmed Forget).** v2 reverses it for succeeded
  audio. A dictation's recording is deleted automatically once its text is in
  memory for `Copy last dictation` and a delivery outcome (inserted, unconfirmed or
  copied) is recorded. A meeting's recordings are deleted after its Markdown file
  is in place with every chunk transcribed; a finished meeting whose file could not
  be written keeps them. A recording whose transcription
  failed, came back empty, was refused, or was interrupted by a quit or crash, or
  whose text could not even be copied, stays on disk with no expiry until Retry
  succeeds or the user confirms Discard.
  Discard is v2's Forget. Escape during a dictation is an explicit discard of that
  take, so v2 does not keep cancelled takes. Each recording has its own file; a
  new one never overwrites an unresolved one.

## User journeys

### 1. Dictate into the focused app

Press the one global shortcut. Hold it to talk and release to finish, or tap it
to start and tap again to finish; the app tells the two apart by press length,
with no setting. A HUD pill appears the moment recording starts. When
transcription finishes, the text is delivered. Escape cancels and discards the
take.

What Cantrip promises about the destination:

1. **Copy only when focus moved or the target is uncertain.** Before either kind of
   delivery, and again as its last step, Cantrip checks that the app and window that
   had focus when dictation began still have it. If focus moved, the text goes to the
   clipboard and the HUD says `Copied — focus changed`. If the starting target could
   not be identified in time, or can no longer be checked, it is uncertain: the text is
   only copied, and the HUD says `Copied — target uncertain`.
2. **Target-bound insertion when the app supports it.** The text is written through
   Accessibility into the field that had focus when dictation began. The clipboard is
   not touched.
3. **Focus-checked paste otherwise** (expected for many browsers, Electron apps
   and terminals). Used only when the starting field was identified and reports that
   it does not support insertion. Cantrip posts `Cmd+V` to the starting app's process
   only.
4. **`Copy last dictation` is always available** for the last transcribed
   dictation until the app quits.

What it does not promise: macOS has no operation that checks focus and delivers text
as one step. The last check reads the window, then the app; a window switch after the
first read or an app switch after the second is not caught. The text is still
addressed to the starting app, which may ignore it or take it while no longer in
front. A paste may also land in another of that app's own fields or windows.
An app can reuse one field for several documents or conversations, so the same field may
hold a different conversation by the time the text arrives. Cantrip reports "Inserted"
only when the field's text before and after differs by exactly the dictation;
otherwise it reports "Sent — unconfirmed" or "Posted — unconfirmed". After an
unconfirmed paste the dictation stays on the clipboard in place of what was there.

With no key or no network, dictation uses Apple's on-device recognizer. It never
uses Apple's server recognizer, even when the Mac is online: if on-device
recognition is not supported, its language model is not installed, or Speech
Recognition permission is denied, Cantrip refuses, says what to change, and keeps
the recording for Retry or Discard.

### 2. Keep a Custom Vocabulary

The vocabulary is one list of terms (names, product names, jargon). The same
list biases speech recognition and guides Clean and Elevated rewriting. Seed
terms for Misty Step: Scry, Codex, Persephone, Serenity, Supercortex, Mistystack,
OpenRouter, Cloudflare, T3 Code, Kaylee, Phaedrus, Mercadia, Cantrip, Overmind.
Terms are added and removed in Settings. There are no aliases, rules or regexes
in v2.0.

### 3. Choose a mode

| Mode | Output |
| --- | --- |
| Raw | What the recognizer returned, with its punctuation. No language model. |
| Clean | Fixes punctuation, fragments, fillers, stutters, self-corrections and misheard vocabulary; keeps the speaker's words and tone. |
| Elevated | Crisp prose for messages, specs or email, in the speaker's person, without generic AI phrasing. |

A Post-Processing switch sits next to the modes. Off means every dictation is
delivered as Raw immediately and no text goes to the language model. On uses the
selected mode.

### 4. Record a meeting

Start meeting captures the microphone and the system's audio output as separate
tracks on one clock; the user stops it. Cantrip produces a Markdown file with a
title, a short summary, decisions, action items and a cleaned transcript whose
turns are labelled `You` and `Others` and ordered by when each turn began, and
saves it under `cantrip-meeting/YYYY/MM/` in the chosen folder. The default folder
is a `captures/voice/` directory of a Supercortex checkout; any folder works.
Cantrip writes files and never runs git.

Meetings need the OpenRouter key; without one, Start meeting says so and does not
record. If part of a meeting cannot be transcribed, its audio is kept and the
popover offers Retry and Discard. The user may also save partial notes, in which
each missing part reads `[transcription failed: MM:SS–MM:SS]`. A part whose cleanup
failed is kept uncleaned and marked. Short interjections are kept; a part whose turns
cannot be placed in order is kept as one block and marked. A quiet part is treated as
silence; a louder part in which no turn can be found and no words are recognized counts
as not transcribed, so its audio is kept.

A file appears whole or not at all, and Cantrip never overwrites an existing file,
including its own. Every meeting has one `source_id`, fixed when recording starts, and
every note of it carries that `source_id` with `status: partial` or `status: complete`.
Partial notes are `<utc>-<slug>-partial.md`. If Retry then completes the meeting, the
full notes are a new file, `<utc>-<slug>.md`, in the current Meeting folder. Cantrip
does not need the partial notes for that, so the user can edit, rename, move or delete
them, or choose another folder, and they stay as the user left them.

If the notes are finished but the folder cannot be written, the meeting is
**pending-archive**: every chunk is transcribed, the cleaned transcript and summary
are kept in staging, and only writing the final file failed. The popover says so and
offers Retry, which only writes that file: it makes no speech or language-model call.
It works once the folder is writable again or another one is chosen.

## Control surface

This is the whole list. A control that is not listed is excluded.

**Menu-bar popover**

1. Start/stop dictation.
2. Start/stop meeting.
3. Mode: Raw / Clean / Elevated.
4. Post-Processing on/off.
5. Copy last dictation.
6. Open Settings.
7. Quit.

**Menu-bar popover, contextual row** (shown whenever any staged recovery item exists:
a failed dictation WAV, failed meeting chunk WAVs, or a `pending-archive` meeting;
absent otherwise; a recording cut off by a quit or crash is shown too)

8. `Retry` / `Save partial notes` / `Discard`, for the newest staged item; the next
   one takes its place when it is resolved. Retry copies a recovered dictation to the
   clipboard and does not paste it; for a meeting with failed chunks it transcribes
   those chunks and finishes the notes; for a `pending-archive` meeting it only
   publishes the finished notes, with no API calls. `Save partial notes` is offered
   only for a meeting with failed chunks. Discard asks for confirmation and never
   removes a file from the meeting folder.

**Settings (one pane)**

1. Shortcut recorder.
2. Microphone (System default, or one device).
3. OpenRouter API key (stored in the login Keychain; never shown after saving).
4. Custom Vocabulary (add and remove terms).
5. Meeting folder.
6. Open at login.

Seven standing popover controls, one contextual row and six settings. System
permissions are requested when first needed and have no settings of their own.
Speech Recognition is requested only when a dictation first takes the on-device
path, so a user with a key who is online is never asked for it.

## Anti-goals

- No model picker, provider picker, temperature, or prompt editor.
- No per-app modes, word-replacement rules, regexes, or transformation scripts.
- No sound themes, volume ducking, HUD position, colour or size options.
- No dictation history browser, search, statistics, or retention settings.
- No secondary shortcuts, command palettes, voice commands, or labs.
- No streaming or live-typing mode.
- No accounts, telemetry, or in-app purchase.
- No Linux or Windows port. v1 stays the Linux product.

For scale: the archived Swift Hex has 27 stored settings across four tabs. The
active Hex has 50 settings fields, 78 Labs features and 11 window panes. These
counts are from the source read for this slice.

## Targets

The measured latencies are API wall-clock times; they exclude capture, network
distance from a Mac, and paste. Measured values come from the Slice #42 spike (5 synthetic clips of 7.3–10.0 s,
one sequential run from a Linux cloud sandbox, 2026-11-22). Sample size is small
and speech is synthetic, so targets for Raw, Clean and Elevated must be
re-measured with real microphone speech on a Mac before release.

| Measure | Measured in the spike | Target |
| --- | --- | --- |
| Raw: transcription call, p50 / max | 352 ms / 2052 ms | p50 ≤ 600 ms, p95 ≤ 2.5 s over ≥ 50 real dictations |
| Clean: transcription plus rewrite, p50 / max | 1479 ms / 2964 ms | p50 ≤ 1.6 s, p95 ≤ 3.5 s |
| Elevated: transcription plus rewrite, p50 / max | 1187 ms / 3002 ms | p50 ≤ 1.6 s, p95 ≤ 3.5 s |
| Shortcut press to HUD visible | not measured | ≤ 100 ms |
| Vocabulary terms spelled exactly, Raw | 21/21 (100%); 14/21 (66.7%) without `keyterms` | ≥ 95% on a real-voice corpus |
| Vocabulary terms spelled exactly, Clean / Elevated | 20/21 (95.24%) / 19/21 (90.48%) | ≥ 95% |
| Meaning preserved, read by eye, Clean / Elevated | 4/5 / 5/5 | 100% on a real-voice corpus |
| Ordinary words wrongly turned into vocabulary terms | 0 of 9 Clean text probes; 1 audio clip: "cloud flare" became "Cloudflare" | 0 on a 50-sentence negative set |
| Cost per 1,000 dictations, Raw / Clean / Elevated | $0.261 / $0.312 / $0.307 | ≤ $0.50 |
| Meeting cost | not measured; $0.10 per hour of audio per track at the catalog rate | report per meeting |

The three Clean and Elevated misses are names the speaker retracted ("Serenity",
"Cloudflare") and were correctly dropped with the retraction, so the strict score
understates them. In the clips, MAI-Transcribe-2 heard `Scry`, `Codex` and
`confusing` correctly even without `keyterms`; the owner's reported
misrecognitions were not reproduced on synthetic speech.

## Open risks

- The spike used neural text-to-speech, not a human microphone, room noise or an
  accent.
- The numbers in this slice were tuned on the same clips that measured them. The
  held-out repair probe recovered 5 of 9 vocabulary terms.
- Clean can change meaning when the speaker corrects themselves: on one of five
  clips it deleted the correction and kept the retracted claim. The term score did
  not catch it. This blocks release of Clean, not of Raw.
- Clean and Elevated returned identical text on three of five clips. If that holds
  on real dictation, one of the two modes should go.
- Meeting capture and its transcription are designed, not tested. OpenRouter
  documents segment timestamps for MAI-Transcribe-2, but no probe recorded in
  DESIGN.md requested them. Turn order rests on them or on the slower local split;
  where neither works, up to five minutes of one track is kept as one block.
- The microphone track has no echo cancellation. Without headphones, the other
  side's voices can reach the microphone and appear under `You` as well.
- Which apps accept Accessibility insertion, and which act on a `Cmd+V` posted to
  their process, is not known until it is tested on a Mac. An app that does
  neither, or that does not expose its focused window and field to Accessibility,
  gets copy-only delivery.
- Most apps that need a paste are expected not to let Cantrip confirm it, so a paste
  will usually leave the dictation on the clipboard in place of what was there.
- On-device recognition, its permission prompt and its refusal path are designed,
  not tested.
- Reading the clipboard to snapshot it can make macOS show its pasteboard-access
  alert.
- A bare "Scrum means Scry" rule rewrites ordinary speech: an earlier prompt
  turned "the scrum starts at ten" into "the Scry starts at ten". The final
  prompt keeps ordinary uses in every text probe, but a larger negative set must
  confirm it.
- Raw is what the recognizer returns. One clip came back without the "um" and
  "uh" in its script, and the spike cannot tell whether the voice or the model
  dropped them.
