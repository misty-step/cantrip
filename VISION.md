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

## Accepted product direction

This describes the destination, not a list of shipped features. The
[README](README.md) owns current capabilities and executable procedures; Linear
owns sequencing, acceptance, and unresolved proposals.

### Native identity and themes

Cantrip's own interaction design is a reason to build it, not something that
needs justification through feature parity with another dictation app. Preserve
the current native, pixelated, smooth desktop experience as the visual reference.
The old marketing scaffold is not the brand direction for the new site.

Keep desktop palette integration and make the choice explicit: Follow desktop
alongside selectable Tokyo Night, Rosé Pine, Catppuccin, and Gruvbox palettes.
Build on the existing palette boundary rather than adding a theme engine.

### Agent-first operation and first use

Agents should be able to install, configure, inspect, diagnose, and help repair
Cantrip through stable commands and useful machine-readable results. Cover local
models, opt-in cloud configuration, keyring credential management, active versus
saved settings, and privacy-safe diagnostics suitable for a bug report.
Automation must retain operator approval and verified desktop/session boundaries;
it must not expose credential values or private dictation to become convenient.

Keep a thoughtful human interface over those same capabilities. First use should
form one complete journey: install without a Rust toolchain, understand desktop
support, deliberately install a model, establish one startup owner, configure a
shortcut, and dictate into the intended application. Explain retained recordings
and recovery without obstructing the first successful dictation. Reuse existing
Actions, Settings, doctor, and CLI mechanisms; do not add a competing setup ledger.

### Public site and documentation

Replace the old landing page with a branded, statically generated Astro site at
`cantrip.mistystep.io`, hosted with Cloudflare Workers Static Assets. The website
is a separate build surface, not a React application, SSR service, CMS, database,
account system, or change to the native runtime.

Show the real application: public or synthetic speech, the actual HUD, and text
appearing in an ordinary application. Demonstrations need captions, playback
controls, and a reduced-motion alternative, not an animated imitation of the HUD.
Keep installation, supported desktops, configuration, recovery, privacy, and
release documentation consistent with the downloadable binary. Render
repository-owned documentation rather than maintaining duplicate manuals.

### Public distribution and releases

Use Landmark's release tooling with conventional commits to produce coherent
versions, technical changelogs, and public-facing release notes. Build and verify
the downloadable artifact before publication. Binaries, checksums, build
provenance, and release notes must identify the same source revision; the site's
`/releases` surface should consume the same release data.

Begin with one explicitly supported CPU-only Linux x86-64 artifact and a defined
runtime/ABI baseline. Installation, update, uninstall, and rollback must preserve
configuration, models, credentials, and recording history. Package formats and
additional platforms should follow demonstrated need, not launch simultaneously.

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

Preserve the proven dictation path. Reduce competing policy owners and unclear
boundaries before splitting files or replacing components to reach a line-count
target. Prefer consequential regression checks and observable user journeys over
wording tests or test counts. Measure stop-to-outcome latency and resource use
before replacing the inference engine, file store, or process model; warm
short-clip inference timings are not universal end-to-end performance claims.

## Non-goals

- macOS/Windows ports and mobile in the current Linux/Omarchy release scope;
  cross-platform remains possible later, not a prerequisite for this direction.
- A hosted multi-tenant service or cloud identity.
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
the daemon unblocked. Keep native-platform expansion and native UI-framework
replacement outside the current direction; the static website does not change
the dictation runtime. Lies about outcomes, hangs, and privacy holes are product
risks, not a priority queue maintained in this file.
