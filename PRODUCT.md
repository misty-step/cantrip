# Cantrip v2 — Product

Cantrip v2 is a desktop tray app for **macOS and Linux** with two jobs: dictate text into whatever
app has focus, and record meetings into Markdown notes. One shortcut, three modes, one vocabulary
list, one API key. Settings that do not serve those two jobs do not exist.

Status: product intent, updated 2026-11-23 from Slice #42; nothing is built yet. Contracts, UI and
measurements: [DESIGN.md](DESIGN.md). Architecture is left open ([DESIGN.md › Architecture freedom](DESIGN.md#architecture-freedom)).

## Release bar

Supported platforms: **macOS 15+** and **Linux** on Wayland (KDE Plasma, GNOME, wlroots
compositors) and on X11; no Windows port in v2.0. On every one, in every session, Cantrip provides:

1. **Shortcut dictation.** The global shortcut (default `⌃⌥Space` on macOS, `Ctrl+Alt+Space` on
   Linux) gives hold-to-talk and tap-to-toggle on macOS, on X11 and on Wayland compositors that
   report press and release; tap-to-toggle on Wayland compositors that report only activation. The
   release matrix records the verified behavior per desktop.
2. **Floating HUD.** A non-activating pill that never takes focus or clicks, appears within 100 ms
   of the press and is visible on every workspace.
3. **Full control surface.** The 7 standing controls, 1 contextual recovery row and 6-control
   Settings pane of PRODUCT.md's Control surface, from the tray or menu-bar icon, or on trayless
   sessions (e.g. stock GNOME) in an equivalent floating popover opened from Cantrip's app-launcher
   entry. Never icon-only, never a missing control.

Missing any of the three on any platform or session blocks the release; no known gap, icon-only
fallback or new setting ships. Where an OS cannot support another promise, Cantrip takes the safer
outcome (copy instead of paste, refuse instead of guess) and says so.

This bar is identical in [DESIGN.md › Release bar](DESIGN.md#release-bar), tested by its [checklist](DESIGN.md#verification-checklist).

## Supersession

v1 is this repository's Rust workspace: the Linux app and, since v0.1.15, a signed, notarized
macOS app. v2 supersedes [SPEC.md](SPEC.md) and [VISION.md](VISION.md) as present-tense intent on
**both macOS and Linux**; they and `docs/decisions/` stay, unchanged, as v1 history, and where they
disagree this document wins. v2 never deletes or rewrites v1's data.

| v1 commitment | v2 |
| --- | --- |
| Local speech recognition by default (VISION.md, AGENTS.md) | OpenRouter cloud by default. Local recognition only with no key or no network, and only where it is supported, installed and authorized on this machine and guaranteed to stay on it. Otherwise Cantrip refuses and keeps the recording |
| A cloud failure completes on local recognition (ADR 0020) | A cloud error keeps the recording for Retry; it never falls back to local recognition |
| macOS manual paste only (ADR 0031) | Automatic two-tier delivery on both OSes, always addressed to the captured target; copy-only when focus moved, the target is uncertain or the session cannot address a paste |
| Automatic keys need uninterrupted verified destination history (ADR 0019) | Target captured at start and verified again immediately before delivery; not uninterrupted history |
| Every stopped take is retained until confirmed Forget (ADR 0020) | Dictation audio is deleted once its text is delivered or copied; failed, refused, interrupted or `Not delivered` audio is kept until Retry succeeds or the user confirms Discard |
| Clipboard contents are never read or restored (AGENTS.md) | Direct insertion never touches the clipboard. An addressed paste snapshots it and restores it only when the edit is confirmed and the clipboard is unchanged since Cantrip wrote its text; otherwise the dictation stays on it unless something else replaces it |
| Successful transcripts are kept as local history (AGENTS.md) | No history. The last dictation's text is kept in memory until quit |
| Vocabulary aliases, Clean only, never sent to providers (AGENTS.md, ADR 0012) | One flat term list, no aliases; it biases the recognizer and guides the rewrite |
| Clean uses DeepSeek, chosen on measured fidelity (ADR 0012) | Clean and Elevated use `openai/gpt-6-luna` |
| v1 architecture: one shared engine (ADR 0031), per-OS presentation stack (ADR 0034), std-thread process model (AGENTS.md) | Not binding. Architecture is the implementer's choice |

Three v1 safety rules survive. An unknown or changed target gets only a copy, and no keystroke
ever goes blindly to whatever holds focus. A handoff that may have completed is never retried
through the other tier. Posting is not receipt: "Inserted" needs the field read back showing exactly
Cantrip's own edit.

## User journeys

### 1. Dictate into the focused app

Press the one global shortcut. **Hold** it (≥ ~300 ms) to talk and release to stop, or **tap** it
to start and tap again to stop; press length tells them apart, with no setting (activation-only
Wayland sessions: every press is a tap, see [Release bar](#release-bar)). The HUD pill appears
within 100 ms. Escape cancels and discards the take. After transcription, the text is delivered:

1. **Capture, then verify.** The focused app, window and, where the OS exposes it, field are
   recorded when recording begins. Immediately before delivering, Cantrip verifies that the
   captured app and window still have focus. Moved: text is copied, HUD `Copied — focus changed`.
   Not identified in time, or no longer checkable: text is copied, HUD `Copied — target uncertain`.
2. **Never a blind keystroke.** Every automatic write or paste is addressed to the captured
   target; nothing is ever sent to whatever holds focus. **Tier 1 — direct insertion** writes the
   text into the captured field itself, without touching the clipboard.
3. **Tier 2 — addressed paste**, only for an identified field that cannot take direct insertion,
   and only where the OS can address a paste to the captured app: snapshot the clipboard, place
   the text (marked transient where supported), send a paste addressed to the captured app, and
   restore the old clipboard **only** when the edit is confirmed and the clipboard is completely
   unchanged since Cantrip wrote its text. Where the session cannot address a paste, the text is
   copied instead: `Copied — paste unsupported on this session`.
4. **Honest outcome.** `✓ Inserted` (edit read back), `Sent — unconfirmed` (direct write made),
   `Posted — unconfirmed` (addressed paste sent), a `Copied — …` reason, or `Not delivered` (even
   the copy failed). "Text is on the clipboard" is shown only while the clipboard is unchanged
   since Cantrip wrote its text; if it changes before or while the HUD is visible, the claim drops
   at once for "Use Copy last dictation". **`Copy last dictation`** holds the last dictation in
   memory until quit; a `Not delivered` take also keeps its audio staged until
   Retry succeeds or the user confirms Discard.

Not promised: no supported OS checks focus and delivers in one step, so a switch right after the
final verification can still deliver to the captured app (its captured field, or for a paste
possibly another of its own windows), never to the newly focused app. Inside one app, a field can
show another conversation by the time text arrives. See
[DESIGN.md › Delivery contract](DESIGN.md#delivery-contract).

**No key or no network.** Local speech recognition is used only when supported, installed and
authorized here and guaranteed not to send audio off the machine; no implicit model download, no
vendor server recognizer. Otherwise Cantrip refuses, names the fix, and keeps the recording.

### 2. Keep a Custom Vocabulary

One flat list of terms (names, products, jargon), edited in Settings. It biases speech recognition
and guides Clean and Elevated. Misty Step seed terms: Scry, Codex, Persephone, Serenity,
Supercortex, Mistystack, OpenRouter, Cloudflare, T3 Code, Kaylee, Phaedrus, Mercadia, Cantrip,
Overmind. No aliases, rules or regexes in v2.0.

### 3. Choose a mode

| Mode | Output |
| --- | --- |
| Raw | What the recognizer returned, with its punctuation. No language model. |
| Clean | Fixes punctuation, fragments, fillers, stutters and misheard vocabulary; on a self-correction, deletes the whole retracted claim with the correction phrase and keeps only the final statement. Keeps the speaker's words, tone and every other fact. |
| Elevated | Crisp prose for messages, specs or email, in the speaker's person, without generic AI phrasing. |

A **Post-Processing** switch sits next to the modes. Off: every dictation is Raw and no text goes
to a language model. On: the selected mode applies (Raw never calls one). A rewrite that fails, is
empty, is cut off or takes over 4 s gives Raw, HUD `Raw (cleanup skipped)`. If transcription fails
or returns nothing, the recording is kept for Retry or Discard.

### 4. Record a meeting

Start meeting captures two tracks on one clock: `You` (microphone) and `Others` (the system's audio
output). The user stops it. Cantrip writes one Markdown file — title, summary, decisions, action
items and a cleaned, timestamped transcript ordered by when each turn began — under
`cantrip-meeting/YYYY/MM/` in the chosen Meeting folder (default: the `captures/voice/` directory
of a Supercortex checkout). Any folder works unless it, or a folder Cantrip publishes into beneath
it, resolves into Cantrip's private staging area. Cantrip writes files and never runs git. Meetings
need the OpenRouter key; without one, Start meeting says so and does not record. The HUD shows a
red dot and elapsed time for the whole meeting.

**Recovery, never loss.** A part is unresolved when its transcription fails or returns no words
although it contains speech. Any unresolved part keeps the note from being complete and keeps that
part's original audio staged, even when every other part succeeded; the popover offers Retry, Save
partial notes (gaps read `[transcription failed: MM:SS–MM:SS]`) and a confirmed Discard. Finished
notes whose folder cannot be written are **pending-archive**: Retry only writes the file (zero
speech or language-model calls). A part whose cleanup failed is kept uncleaned and marked.

**The Meeting folder is append-only for Cantrip.** It never overwrites, truncates, renames or
deletes a file there, even its own, and never leaves a temporary file there; a taken name gets
`-2.md`, `-3.md`. The finished note stays whole in staging until the published file is confirmed a
regular file with identical content, flushed to disk; only then is staging cleaned up. Without
atomic exclusive publication (many network shares), an interrupted write can leave an incomplete
file; Cantrip leaves it alone and Retry publishes the staged note under a new name.

Each note carries the meeting's `source_id` (fixed at start) and `status: partial | complete`.
Partial notes are `<utc>-<slug>-partial.md`; completing later publishes a new `<utc>-<slug>.md`
without needing, opening or changing a partial file, so the user may edit, move or delete it.

## Control surface

The whole list; a control not listed is excluded. Reached from the tray or menu-bar icon, or from
the app launcher on trayless sessions ([Release bar](#release-bar)).

**Popover — 7 standing controls:** (1) Start/stop dictation, (2) Start/stop meeting, (3) Mode: Raw /
Clean / Elevated, (4) Post-Processing on/off, (5) Copy last dictation, (6) Settings…, (7) Quit.

**Contextual recovery row** — shown only while a staged recovery item exists (a failed, interrupted
or `Not delivered` dictation, a meeting with unresolved parts, an interrupted meeting, or a
`pending-archive` meeting): (8) `Retry` / `Save partial notes` / `Discard…` for the newest item; the
next takes its place when it is resolved. Retry copies a recovered dictation and does not paste it;
for unresolved parts it transcribes them and finishes the notes; for `pending-archive` it only
publishes. `Save partial notes` appears only for unresolved parts. Discard asks first and never
touches the Meeting folder.

**Settings — one pane, 6 controls:** (1) Shortcut recorder, (2) Microphone (system default, or one
device), (3) OpenRouter API key (kept in the OS secret store; never shown after saving), (4) Custom
Vocabulary (add and remove terms), (5) Meeting folder, (6) Open at login. OS permissions are
requested just in time, when a feature first needs them, never at launch, and have no settings.

**Privacy.** Audio and text go only to OpenRouter, which routes them to upstream providers (e.g.
Azure for `microsoft/mai-transcribe-2`; OpenAI, Amazon Bedrock or Azure for `openai/gpt-6-luna`).
Cantrip sends `provider.data_collection: "deny"` on transcription and rewrite requests alike: a
routing preference, not proof of upstream retention
([DESIGN.md › Privacy](DESIGN.md#permissions-privacy-and-distribution)).

## Anti-goals

- No model picker, provider picker, temperature, prompt editor, per-app modes, word-replacement
  rules, regexes, or transformation scripts.
- No sound themes, volume ducking, HUD position, colour or size options; no dictation history
  browser, search, statistics, or retention settings.
- No secondary shortcuts, command palettes, voice commands, labs, or streaming / live-typing mode.
- No accounts, telemetry, or in-app purchase. No Windows port in v2.0.

## Targets

Measured: Slice #42 spike, 5 synthetic clips of 7.3–10.0 s, one sequential run from a Linux cloud
sandbox (2026-11-22), API wall-clock only. Every target is re-measured with real microphone speech
**on both macOS and Linux** before release.

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

The three Clean/Elevated term misses are retracted names, correctly dropped. The owner's reported
misrecognitions (`Scry`, `Codex`, `confusing`) did not reproduce on synthetic speech.

## Open risks

- Speech was neural text-to-speech (no human microphone, noise or accent), and prompts were tuned
  on the clips that scored them; held-out repair recovered 5 of 9 terms.
- Clean kept a retracted claim on 1 of 5 clips; the new prompt wording is unmeasured and blocks
  release of Clean (not Raw) until a fidelity check on unseen clips passes. Clean and Elevated
  matched on 3 of 5 clips; if that holds on real dictation, one mode should go.
- Meeting capture, timestamps and turn order are designed, not tested; no probe has requested
  segment timestamps. Without headphones, other voices can also appear under `You`.
- Per-app support for direct insertion or an addressed paste is unknown until tested on each OS.
  Most paste targets cannot confirm the edit, so a paste usually leaves the dictation on the
  clipboard. Snapshotting the clipboard can trigger an OS clipboard-access prompt.
- **Linux.** Wayland desktops differ: some hide the focused window (copy-only), some need consent
  for global shortcuts or input, some report activation but not release (tap-to-toggle only), most
  cannot address a paste to one app (fields that need one get copy-only), and stock GNOME has no
  tray and no standard non-activating overlay. The release bar still holds on each; meeting it
  there is unproven work and may add no setting.
- Local recognition is designed, not tested; most Linux distributions ship no local recognizer,
  so the no-key path will usually refuse there.
- A bare "Scrum means Scry" rule rewrites ordinary speech; a larger negative set must confirm the
  final vocabulary prompt.
