# ADR 0022: Verified CPU-only Linux release

Date: 2026-09-07. Status: accepted.

## Context

Cantrip's public story is a downloadable Linux dictation app, not a source-only
crate. The artifact, checksums, provenance, and notes must name one source
revision. Installation must not require Rust, a GPU, or CUDA, and must not
disturb configuration, models, credentials, history, or startup ownership.

Landmark already versions, changelogs, and renders public notes. A second
release service, container registry, or LLM credential would add moving parts
without changing the download users install.

The host toolchain's ONNX Runtime objects need GLIBC 2.38. Ubuntu 22.04 cannot
link that CPU stack, so 22.04 is not an honest runtime baseline.

## Decision

Ship one CPU-only `x86_64-unknown-linux-gnu` archive. Build and verify it on
Ubuntu 24.04 (glibc 2.39). Pin the exact Rust channel in `rust-toolchain.toml`
and build with `--locked --no-default-features`.

Use Landmark's native CLI, pinned by URL and SHA-256, with `model.policy: off`.
A reviewable `landmark/release` PR carries the version, public Markdown, and
technical changelog. Merging that PR is what starts baseline build, clean
Ubuntu 24.04 runtime proof, signed provenance, and GitHub publication.

The installer changes only `PREFIX/bin/cantrip`. Updates publish an explicit
private backup before replacement. GitHub Release assets, including
`release.json` and Landmark's `releases.json`, are the versioned data the
website consumes.

## Consequences

- Ubuntu 22.04 is unsupported for the downloadable binary. Source builds there
  fail to link current ONNX Runtime objects.
- Publication is gated on a merged release plan and a passing runtime proof.
  Interrupted draft uploads may resume only for identical bytes.
- Public notes are reviewable Markdown. HTML, text, and JSON feeds are rendered
  from that Markdown at publication time.
- Microphone, HUD, and focused desktop delivery remain attended checks. The
  automated proof covers native version/help, explicit model download, offline
  local transcription, install/update/rollback/uninstall, keyring preservation,
  and live-daemon refusal.
