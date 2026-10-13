# 0025: Source install builds this tree, never copies a stale artifact

- **Status:** accepted
- **Date:** 2026-09-16
- **Primitives:** installer
- **Powder:** —

## Context

Separate build and copy commands shipped an older binary after source changes:
a running compile did not restart when git changed the tree. The verified
release installer is a separate binary-only route.

## Decision

Use [scripts/install-from-source](../../scripts/install-from-source) for source
installation and updates. It always builds the current tree with release/locked
settings and atomically replaces the destination; a previously compiled target
artifact is never its input. Do not turn contrib/install.sh into a source build.

## Consequences

First install and updates use the same command. Symlink/non-file destinations
and live daemons are refused; the installer neither starts nor stops services.
Dirty trees are allowed because the intended input is this tree. Compile-only
cargo build remains separate. Running-process identity versus the on-disk binary
is a distinct status concern.

Revisit if: a replacement source-install route can prove that the installed
artifact was built from the intended current tree and retain safe replacement.
