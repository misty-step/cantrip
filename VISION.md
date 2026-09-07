# Cantrip — vision

Optional product context, not a required workflow, live backlog, or authority over
the user's current request. This file records product constraints and rationale;
[README.md](README.md#work-and-documentation-ownership) explains work ownership.

## What this is

Cantrip is local-first dictation for Linux on Wayland. You hold a key, speak,
release, and text lands where the cursor is. A cantrip is a small spell you can
always cast; this product is that spell for prose.

It is a long-lived personal product (public MIT source), not a spike and not a
cloud SaaS. One Rust crate, one binary `cantrip`, operator-owned machine.

## Who it is for

Linux users who write in real apps (editors, browsers, chat) and want speech as
a first-class input without shipping audio to a vendor by default. Primary
operator today: a single power user on Hyprland/Omarchy. The design still aims at
any cold installer who can run `doctor` and bind one hotkey; unverified desktop
delivery falls back to an explicit operator choice, never an assumed destination.

## Job to be done

Capture speech, turn it into clean text, and deliver it atomically to the
focused client — fast enough that the habit sticks, private enough that the
habit is safe, honest enough that the status surface never lies about progress.

## Category and posture

Desktop input utility. Local STT (Parakeet ONNX) is the default lane. Optional
OpenAI-compatible cloud STT and cleanup are escapes, not the identity. Keys live
in the OS keyring. Transcript content is absent from operational logs; an
owner-private local history supports recovery and evaluation.

## Fundamentals (keep true when code changes)

1. **Local by default.** Speech stays on the machine unless the operator opts in.
   Installed local recognition is the automatic safety net for cloud failures.
2. **Paste-first delivery.** Paragraphs survive; type mode never touches the clipboard.
3. **Honest HUD.** Indeterminate activity stays distinct from progress; determinate
   fill advances only from measured multi-chunk STT.
4. **Small process model.** No async runtime; std threads + mpsc; one warm worker.
5. **Operator evidence.** Each stopped recording and its saved text remain
   independently recoverable in owner-private history. Cancellation and successful
   delivery do not delete audio; only explicit confirmed Forget does.
6. **Secrets out of the tree.** No API keys in files, logs, or git.
7. **Verified destination.** Focus and session uncertainty defer delivery; an
   uncertain keyboard or clipboard handoff never triggers an automatic retry.

## Engineering posture

Correctness over novelty. Record non-obvious accepted decisions with their
rationale; keep executable checks and contributor procedures in
[README.md](README.md#development), not a second workflow in this vision.

## Non-goals

- macOS/Windows ports, mobile, or a hosted multi-tenant service.
- GTK/Electron shells, always-on ambient listening, or always-on mic UX.
- Fake progress, notification spam, or a second durable work ledger in-repo.
- Provider-specific SDKs (OpenAI-compatible HTTP only).
- Competing with full voice assistants; this is dictation into existing apps.

## Bets

- A calm layer-shell capsule beats chatty notifications for dictation trust.
- Chunked local STT plus optional cleanup beats chasing every new cloud model.
- Strict injection modes and keyring secrets beat “it usually works” fallbacks.
- One deep daemon + thin clients stays cheaper than a plugin ecosystem.

## Excellent outcomes

**Core experience:** Install, doctor, hotkey, dictate a paragraph with guarded
paste-first delivery and a legible outcome. Failures leave independently
recoverable recordings. Repository checks prove the contracts above.

**Long-term product aim:** Indispensable daily driver on mainstream Wayland
setups: reliable long-form dictation, configurable cleanup without drama,
boring ops (timeouts, doctor truth, no hung inject children), and a public story
(README + site) that matches the binary. Still one crate. Still local-first.

## Decision lens

Prefer the change that keeps speech local, delivery atomic, progress honest, and
the daemon unblocked. Reject scope that adds platforms, UI frameworks, or cloud
identity. Lies about outcomes, hangs, and privacy holes are product risks, not
a priority queue maintained in this file.
