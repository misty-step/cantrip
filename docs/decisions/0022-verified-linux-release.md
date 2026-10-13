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

## Consequences

A reviewable Landmark release PR starts publication only after merge and clean
baseline runtime proof. Assets and notes name the source revision; interrupted
draft uploads resume only for identical bytes. The binary-only installer changes
only PREFIX/bin/cantrip and publishes a private update backup, preserving user
state. Release assets supply website version data. Automated public-fixture proof
does not replace attended microphone, HUD, or focused-delivery checks.

Revisit if: the native CPU dependency stack or supported distribution targets
require a different verified runtime baseline or artifact set.
