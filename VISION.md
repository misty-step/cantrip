# Cantrip — vision

## What this is

Cantrip is local-first dictation for Linux on Wayland. You hold a key, speak,
release, and text lands where the cursor is. A cantrip is a small spell you can
always cast; this product is that spell for prose.

It is a long-lived personal product (public MIT source), not a spike and not a
cloud SaaS. One Rust crate, one binary `cantrip`, operator-owned machine.

## Who it is for

Linux users who write in real apps (editors, browsers, chat) and want speech as
a first-class input without shipping audio to a vendor by default. Primary
operator today: a single power user on COSMIC/wlroots-class compositors. The
design still aims at any cold installer who can run `doctor` and bind one hotkey.

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
2. **Paste-first delivery.** Paragraphs survive; type mode never touches the clipboard.
3. **Honest HUD.** Static phases; determinate fill only from measured multi-chunk STT.
4. **Small process model.** No async runtime; std threads + mpsc; one warm worker.
5. **Operator evidence.** Failed audio, the last transcript, and owner-private
   transcript history remain locally available without support theater.
6. **Secrets out of the tree.** No API keys in files, logs, or git.

## Standards

- Correctness over novelty. ADRs before non-obvious behavior changes.
- `cargo fmt`, `clippy -D warnings`, and tests that defend observable contracts.
- Cold agents read `VISION.md`, then `AGENTS.md`, then ADRs.
- The product board of record is the **Powder ledger** (`misty-step/cantrip`
  jobs on the Sanctum origin); epic jobs block on their children and are
  taken only for closeout. GitHub Issues remain a read-only archive of
  pre-2026-08-21 decisions.

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

**Near (weeks):** Install, doctor, hotkey, dictate a paragraph with paste-first
delivery and a legible Success flash. Failures leave a recoverable WAV or last
transcript. CI and forest checks prove the contracts above.

**Horizon (~6–12 months):** Indispensable daily driver on mainstream Wayland
setups: reliable long-form dictation, configurable cleanup without drama,
boring ops (timeouts, doctor truth, no hung inject children), and a public story
(README + site) that matches the binary. Still one crate. Still local-first.

## Decision lens

Prefer the change that keeps speech local, delivery atomic, progress honest, and
the daemon unblocked. Reject scope that adds platforms, UI frameworks, or cloud
identity. When two tickets both help, pick the one that removes a lie, a hang,
or a privacy hole before polish.
