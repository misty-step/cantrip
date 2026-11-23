# Cantrip v2 — Product

Cantrip v2 is a desktop tray app for **macOS and Linux** with two jobs: dictate text
into whatever app has focus, and record meetings into Markdown notes. One shortcut,
three modes, one vocabulary list, one API key. Settings that do not serve those two
jobs do not exist.

**Platforms.** macOS 15 and later, and Linux desktops on Wayland and on X11. Both are
first-class: every promise below holds on each. Where an OS or desktop cannot support a
promise, Cantrip takes the safer outcome (copy instead of paste, refuse instead of
guess) and says so. No Windows port in v2.0.

Status: product intent, updated 2026-11-23 from Slice #42 (2026-11-22). Nothing here is
built yet. [DESIGN.md](DESIGN.md) holds the behavior contracts, the UI and the
measurements behind the numbers here. Architecture is deliberately left open
([DESIGN.md › Architecture freedom](DESIGN.md#architecture-freedom)).

## Supersession

v1 is the Rust workspace in this repository: the Linux dictation app and, as of
v0.1.15, also a signed, notarized macOS app. v2 supersedes [SPEC.md](SPEC.md) and
[VISION.md](VISION.md) as the present-tense intent for Cantrip on **both macOS and
Linux**. Those files and the decision records in `docs/decisions/` stay in place,
unchanged, as historical references for v1; where they disagree with this document,
this document wins. v2 never deletes or rewrites v1's data.

v2 changes these v1 commitments deliberately:

| v1 commitment | v2 |
| --- | --- |
| Local speech recognition by default (VISION.md, AGENTS.md) | OpenRouter cloud by default. Local recognition only with no key or no network, and only where it is supported, installed and authorized on this machine and guaranteed to stay on it. Otherwise Cantrip refuses and keeps the recording |
| A cloud failure completes on local recognition (ADR 0020) | A cloud error keeps the recording for Retry; it never falls back to local recognition |
| macOS manual paste only (ADR 0031) | Automatic two-tier delivery on both OSes, copy-only when focus moved or the target is uncertain |
| Automatic keys need uninterrupted verified destination history (ADR 0019) | Target captured at start and checked again at delivery; not uninterrupted history |
| Every stopped take is retained until confirmed Forget (ADR 0020) | Succeeded dictation audio is deleted automatically; failed audio is kept until Retry succeeds or the user confirms Discard |
| Clipboard contents are never read or restored (AGENTS.md) | Direct insertion never touches the clipboard. A paste snapshots it and restores it only when the edit is confirmed; otherwise the dictation stays on it |
| Successful transcripts are kept as local history (AGENTS.md) | No history. The last dictation's text is kept in memory until quit |
| Vocabulary aliases, Clean only, never sent to providers (AGENTS.md, ADR 0012) | One flat term list, no aliases; it biases the recognizer and guides the rewrite |
| Clean uses DeepSeek, chosen on measured fidelity (ADR 0012) | Clean and Elevated use `openai/gpt-6-luna` |
| v1 architecture: one shared engine (ADR 0031), per-OS presentation stack (ADR 0034), std-thread process model (AGENTS.md) | Not binding. Architecture is the implementer's choice |

Three v1 safety rules survive the reversals. An unknown or changed target gets no
insertion and no keystroke, only a copy. A handoff that may have completed is never
retried through the other tier. Posting is not receipt: Cantrip says "Inserted" only
after reading the field before and after and seeing exactly its own edit.

## User journeys

### 1. Dictate into the focused app

Press the one global shortcut. **Hold** it (≥ ~300 ms) to talk and release to stop, or
**tap** it to start and tap again to stop; Cantrip tells the two apart by press length,
with no setting. A small HUD pill appears within 100 ms and never takes focus. Escape
cancels and discards the take. When transcription finishes, the text is delivered.

What Cantrip promises about the destination:

1. **Capture the target at the start.** The focused app, window and, where the OS
   exposes it, the focused field are recorded when recording begins.
2. **Copy only when focus moved or the target is uncertain.** Before delivery, and
   again as its last step, Cantrip checks that the starting app and window still have
   focus. Moved: text is copied, HUD `Copied — focus changed`. Not identified in time,
   or no longer checkable: text is copied, HUD `Copied — target uncertain`.
3. **Direct insertion when the target supports it.** Text is written into the captured
   field itself. The clipboard is not touched.
4. **Guarded paste otherwise.** Only for an identified field that reports it cannot
   take direct insertion: snapshot the clipboard, place the text (marked transient
   where the OS supports it), send a paste to the captured target, and restore the old
   clipboard **only** when the edit is confirmed. Unconfirmed: the dictation stays on
   the clipboard, so nothing is lost.
5. **Honest outcome.** `✓ Inserted` (edit read back), `Sent — unconfirmed` (direct write
   made), `Posted — unconfirmed` (paste sent), `Copied — focus changed`,
   `Copied — target uncertain`, or `Not delivered` (even the copy failed).
6. **`Copy last dictation`** always holds the last dictation, in memory, until quit.

What it does not promise: no supported OS checks focus and delivers text in one step, so
a switch just after the last check can still let the starting app take the text in the
background. A paste may land in another field or window of the starting app, and one
field can show a different conversation by the time text arrives. Where an OS cannot
address a paste to one app, the paste goes to whatever holds keyboard focus at that
instant. Many Wayland desktops do not tell apps which window has focus; there,
delivery is copy-only unless the target can be identified. Details:
[DESIGN.md › Delivery contract](DESIGN.md#delivery-contract).

**No key or no network.** Dictation uses local speech recognition only when it is
supported, installed and authorized on this machine and guaranteed not to send audio
off it. Cantrip never downloads a model on its own and never uses a vendor's server
recognizer. Otherwise it refuses, names the fix, and keeps the recording for Retry or
Discard.

### 2. Keep a Custom Vocabulary

One flat list of terms (names, products, jargon), edited in Settings. It biases speech
recognition and guides Clean and Elevated. Misty Step seed terms: Scry, Codex,
Persephone, Serenity, Supercortex, Mistystack, OpenRouter, Cloudflare, T3 Code, Kaylee,
Phaedrus, Mercadia, Cantrip, Overmind. No aliases, rules or regexes in v2.0.

### 3. Choose a mode

| Mode | Output |
| --- | --- |
| Raw | What the recognizer returned, with its punctuation. No language model. |
| Clean | Fixes punctuation, fragments, fillers, stutters and misheard vocabulary; on a self-correction, deletes the whole retracted claim with the correction phrase and keeps only the final statement. Keeps the speaker's words, tone and every other fact. |
| Elevated | Crisp prose for messages, specs or email, in the speaker's person, without generic AI phrasing. |

A **Post-Processing** switch sits next to the modes. Off: every dictation is delivered
as Raw and no text goes to a language model. On: the selected mode applies (Raw still
never calls one). If the rewrite fails, is empty, is cut off or takes over 4 s, Raw is
delivered and the HUD says `Raw (cleanup skipped)`. If transcription fails or returns
nothing, the recording is kept for Retry or Discard.

### 4. Record a meeting

Start meeting captures two tracks on one clock: `You` (microphone) and `Others` (the
system's audio output). The user stops it. Cantrip writes one Markdown file — title,
summary, decisions, action items and a cleaned, timestamped transcript ordered by when
each turn began — under `cantrip-meeting/YYYY/MM/` in the chosen Meeting folder. The
default folder is the `captures/voice/` directory of a Supercortex checkout; any folder
works unless it overlaps Cantrip's private staging area. Cantrip writes files and never
runs git.

Meetings need the OpenRouter key; without one, Start meeting says so and does not
record. The HUD shows a red dot and elapsed time for the whole meeting.

**Recovery, never loss.** If part of a meeting cannot be transcribed, its audio is kept
and the popover offers Retry, Save partial notes (each gap reads
`[transcription failed: MM:SS–MM:SS]`) and a confirmed Discard. If the notes are finished
but the folder cannot be written, the meeting is **pending-archive**: Retry only writes
the file — zero speech or language-model calls — once the folder is writable or another
is chosen. A part whose cleanup failed is kept uncleaned and marked.

**The Meeting folder is append-only for Cantrip.** It never overwrites, truncates,
renames or deletes a file there, including its own, and never leaves a temporary file
there. A taken name gets `-2.md`, `-3.md`. A note is checked byte-for-byte and flushed to
disk before its staging is deleted. On a folder where the OS offers no atomic exclusive
publish (another volume, many network shares), an interrupted write can leave an
incomplete file; Cantrip leaves it alone and Retry publishes a whole note under a new name.

Every meeting has one `source_id`, fixed when recording starts; each note carries it with
`status: partial` or `status: complete`. Partial notes are `<utc>-<slug>-partial.md`.
Completing after partial notes publishes a new `<utc>-<slug>.md` and never needs, opens or
changes the partial file, so the user may edit, move or delete it freely.

## Control surface

This is the whole list. A control that is not listed is excluded.

**Tray / menu-bar popover — 7 standing controls**

1. Start/stop dictation.
2. Start/stop meeting.
3. Mode: Raw / Clean / Elevated.
4. Post-Processing on/off.
5. Copy last dictation.
6. Settings…
7. Quit.

**Contextual recovery row** — shown only while a staged recovery item exists (a failed or
interrupted dictation, a meeting with failed parts, an interrupted meeting, or a
`pending-archive` meeting):

8. `Retry` / `Save partial notes` / `Discard…` for the newest item; the next takes its
   place when it is resolved. Retry copies a recovered dictation and does not paste it;
   for failed parts it transcribes them and finishes the notes; for `pending-archive` it
   only publishes. `Save partial notes` appears only for failed parts. Discard asks first
   and never touches the Meeting folder.

**Settings — one pane, 6 controls**

1. Shortcut recorder.
2. Microphone (system default, or one device).
3. OpenRouter API key (kept in the OS secret store; never shown after saving).
4. Custom Vocabulary (add and remove terms).
5. Meeting folder.
6. Open at login.

OS permissions are requested just in time, when a feature first needs them, never at
launch, and have no settings of their own.

## Anti-goals

- No model picker, provider picker, temperature, or prompt editor.
- No per-app modes, word-replacement rules, regexes, or transformation scripts.
- No sound themes, volume ducking, HUD position, colour or size options.
- No dictation history browser, search, statistics, or retention settings.
- No secondary shortcuts, command palettes, voice commands, or labs.
- No streaming or live-typing mode.
- No accounts, telemetry, or in-app purchase.
- No Windows port in v2.0.

## Targets

Measured values come from the Slice #42 spike: 5 synthetic clips of 7.3–10.0 s, one
sequential run from a Linux cloud sandbox, 2026-11-22. They are API wall-clock times
and exclude capture, upload distance and delivery. Sample size is small and speech is
synthetic, so every target is re-measured with real microphone speech **on both macOS
and Linux** before release.

| Measure | Measured in the spike | Target (each OS) |
| --- | --- | --- |
| Raw: transcription, p50 / max | 352 ms / 2052 ms | p50 ≤ 600 ms, p95 ≤ 2.5 s over ≥ 50 real dictations |
| Clean: transcription + rewrite, p50 / max | 1479 ms / 2964 ms | p50 ≤ 1.6 s, p95 ≤ 3.5 s |
| Elevated: transcription + rewrite, p50 / max | 1187 ms / 3002 ms | p50 ≤ 1.6 s, p95 ≤ 3.5 s |
| Shortcut press to HUD visible | not measured | ≤ 100 ms |
| Vocabulary spelled exactly, Raw | 21/21 (100%); 14/21 (66.7%) without `keyterms` | ≥ 95% on a real-voice corpus |
| Vocabulary spelled exactly, Clean / Elevated | 20/21 (95.24%) / 19/21 (90.48%) | ≥ 95% |
| Meaning preserved, read by eye, Clean / Elevated | 4/5 / 5/5 | 100% on a real-voice corpus |
| Ordinary words wrongly turned into terms | 0 of 9 Clean text probes; 1 audio clip ("cloud flare" → "Cloudflare") | 0 on a 50-sentence negative set |
| Cost per 1,000 dictations, Raw / Clean / Elevated | $0.261 / $0.312 / $0.307 | ≤ $0.50 |
| Meeting cost | not measured; $0.10 per audio hour per track (catalog) | report per meeting |

The three Clean/Elevated term misses are names the speaker retracted, correctly dropped
with the retraction. MAI-Transcribe-2 heard `Scry`, `Codex` and `confusing` correctly even
without `keyterms`; the owner's reported misrecognitions did not reproduce on synthetic
speech.

## Open risks

- Speech was neural text-to-speech (no human microphone, noise or accent), and prompts were
  tuned on the clips that scored them; held-out repair recovered 5 of 9 terms.
- Clean kept a retracted claim on 1 of 5 clips. The Clean prompt now tells the model to
  delete the whole retracted clause; that wording is unmeasured and blocks release of
  Clean (not Raw) until a fidelity check on unseen clips passes.
- Clean and Elevated returned identical text on 3 of 5 clips. If that holds on real
  dictation, one mode should go.
- Meeting capture, timestamps and turn order are designed, not tested. No probe has
  requested segment timestamps from MAI-Transcribe-2.
- The microphone track has no echo cancellation; without headphones, other voices can
  also appear under `You`.
- Which apps accept direct insertion or a guarded paste is unknown until tested on each
  OS. Apps that do neither, or hide their focused window or field, get copy-only.
- Most paste targets will not let Cantrip confirm the edit, so a paste usually leaves the
  dictation on the clipboard in place of what was there.
- **Linux.** Wayland desktops differ: some do not expose the focused window (copy-only
  there), some need user consent for global shortcuts or input injection, some report no
  key release (hold-to-talk cannot work there), and stock GNOME shows no tray. Each gap
  is closed or reported per desktop before release; none may add a setting.
- Local recognition, its prompt and its refusal path are designed, not tested. Most Linux
  distributions ship no local recognizer, so the no-key path will usually refuse there.
- Snapshotting the clipboard can trigger an OS clipboard-access prompt.
- A bare "Scrum means Scry" rule rewrites ordinary speech; the final vocabulary prompt
  passed every text probe, but a larger negative set must confirm it.
- One Raw clip came back without its scripted "um" and "uh"; the spike cannot tell
  whether the voice or the model dropped them.
