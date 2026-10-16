# 0022: One verified CPU-only Linux release, no second release service

- **Status:** accepted
- **Date:** 2026-09-07
- **Primitives:** release, installer
- **Powder:** —

## Context

A downloadable dictation app needs artifacts, provenance, checksums, and notes
for one revision, without Rust/GPU/CUDA at install time. Landmark already owns
versioning and public notes. The CPU runtime cannot honestly target Ubuntu 22.04;
see [installation](../INSTALLATION.md).

## Decision

Ship one CPU-only x86_64-unknown-linux-gnu archive built and verified on Ubuntu
24.04 (glibc 2.39), using pinned Rust and locked no-default-features builds.
Use the pinned Landmark CLI with model policy off, not another release service,
container registry, or LLM credential.

The [shared release convention](https://github.com/misty-step/harness/blob/master/docs/release-convention.md)
defines the interface. This workspace is one release unit: the native host and
engine inherit one Cargo version; the static site is a separate deployment,
not another version generator. Keep Landmark's existing pre-stable policy
(minor signals become patch releases before 1.0); never reset the immutable
`v0.1.3` baseline at `895b000d0b04de306b8005bb7fc2d5e676d6c318`.
Future squash commits use the reviewed Conventional Commit PR title only.

`docs/releases/vVERSION.md` is the canonical public notes source. The release
adapter adds the verified build identity and Landmark renders its other formats;
GitHub and the build-time `/changelog` use those same signed Markdown bytes.
The technical changelog is generator evidence, not a second public notes author.
Approve the bot's native exact-head PR workflows, never dispatch substitute
checks. A mismatched artifact source or failed/skipped runtime proof cannot publish.

Port [Overmind PR105](https://github.com/misty-step/overmind/pull/105)'s monotonic
promotion rule: shipped-source <= tested-source <= current-master by ancestry,
not branch-tip equality. Native non-cancelling release/site concurrency serializes
the respective writers. The immutable published tag and version bind the release
baseline; the active Cloudflare version's original `git:SHA run:ID` annotation
binds website source, including after rollback. Unknown source fails explicitly.
The sole legacy website exception is version
`7737d4a8-38a9-458a-b9fc-2443ef90492f`, proven by successful
[Website run38524346387](https://github.com/misty-step/cantrip/actions/runs/38524346387)
to serve `ad4fac9f31ce0acebee1bcdab5117fb45a6d5127`; no other unannotated
version inherits that identity. An interrupted draft resumes only from its
original source; a later master neither blocks that source nor rewrites its bytes.

There is no unattended host updater or safe background promotion mechanism.
The live desktop's startup owner and retained takes make an automatic restart
unsafe. Keep promotion attended through the existing binary-only installer:
verify the downloaded release, stop the owner after the take is settled, update
with an explicit private backup, then verify readiness. Never interrupt the
daemon for release experiments. The prior archive and backup remain rollback
targets; rollback changes code, not accepted data. macOS development artifacts
remain unpublished and are never promoted as production releases.

## Consequences

A reviewable Landmark release PR starts publication only after merge and clean
baseline runtime proof. Assets and notes name the source revision; interrupted
draft uploads resume only for identical bytes. The binary-only installer changes
only PREFIX/bin/cantrip and publishes a private update backup, preserving user
state. Release assets supply website version data. Automated public-fixture proof
does not replace attended microphone, HUD, or focused-delivery checks.

Revisit if: the native CPU dependency stack or supported distribution targets
require a different verified runtime baseline or artifact set.
