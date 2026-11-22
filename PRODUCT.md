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
still says no macOS release is published. v2 supersedes
[SPEC.md](SPEC.md) and [VISION.md](VISION.md) as the statement of intent for the
**macOS** product. Those files stay unchanged in this slice and continue to
describe v1. v1 remains the Linux product and is not retired by this document.

v2 changes these v1 commitments. Each is a deliberate decision here:

| v1 commitment | v2 |
| --- | --- |
| Local speech recognition by default (VISION.md, AGENTS.md) | OpenRouter cloud by default; on-device Apple Speech when offline or no key |
| macOS manual paste only (ADR 0031, which allows reversal by "an explicit product decision") | Automatic paste at the cursor, with the clipboard restored |
| Clipboard contents are never read or restored (AGENTS.md) | The clipboard is snapshotted before a paste and restored after it |
| No Swift toolchain (ADR 0034) | Native Swift/SwiftUI app |
| Clean uses DeepSeek, chosen on measured fidelity (ADR 0012) | Clean and Elevated use `openai/gpt-6-luna` |

## User journeys

### 1. Dictate into the focused app

Press the one global shortcut. Hold it to talk and release to finish, or tap it
to start and tap again to finish; the app tells the two apart by press length,
with no setting. A HUD pill appears the moment recording starts. When
transcription finishes, text is pasted at the cursor and the previous clipboard
contents are put back. Escape cancels. If focus moved to another app before the
paste, the text goes to the clipboard and the HUD says so; Cantrip never pastes
into an app the user did not start dictating in.

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
tracks; the user stops it. Cantrip produces a Markdown file with a title, a
short summary, decisions, action items and the transcript, and saves it under
`cantrip-meeting/YYYY/MM/` in the chosen folder. The default folder is a
`captures/voice/` directory of a Supercortex checkout; any folder works. Cantrip
writes files and never runs git.

## Control surface

This is the whole list. A control that is not listed is excluded.

**Menu-bar popover**

1. Start/stop dictation.
2. Start/stop meeting.
3. Mode: Raw / Clean / Elevated.
4. Post-Processing on/off.
5. Copy last dictation.
6. Open Settings. 7. Quit.

**Settings (one pane)**

1. Shortcut recorder.
2. Microphone (System default, or one device).
3. OpenRouter API key (stored in the login Keychain; never shown after saving).
4. Custom Vocabulary (add and remove terms).
5. Meeting folder.
6. Open at login.

Seven popover controls and six settings. System permissions are requested when first needed
and have no settings of their own.

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
- Meeting capture and its transcription are designed, not tested.
- A bare "Scrum means Scry" rule rewrites ordinary speech: an earlier prompt
  turned "the scrum starts at ten" into "the Scry starts at ten". The final
  prompt keeps ordinary uses in every text probe, but a larger negative set must
  confirm it.
- Raw is what the recognizer returns. One clip came back without the "um" and
  "uh" in its script, and the spike cannot tell whether the voice or the model
  dropped them.
