# ADR 0025: Source install builds this tree

Date: 2026-09-16. Status: accepted.

## Context

Source-built updates were two copy-pastes: `cargo build --release --locked`,
then copy `target/release/cantrip`. An in-flight compile does not restart when
git changes `src/`; a later copy ships whatever finished. That shipped a binary
without the persistent pixel field and 3.2× listening gain after those commits
were already on `master`.

`contrib/install.sh` copies a verified release archive. It is not a source
build, and must not become one.

## Decision

`scripts/install-from-source` is the only source-install path. It always runs
`cargo build --release --locked` in the current tree, then atomically replaces
`PREFIX/bin/cantrip`. A previously compiled `target/` artifact is never an
input. First install and later updates are the same command.

It refuses a symlink destination, a non-file destination, and a live daemon
(runtime socket or a process whose `exe` is the destination, including
`(deleted)`). It does not start or stop a service. Dirty trees install: the
point is this tree, not a clean index.

## Consequences

- README source-install snippets that copy `target/release/cantrip` are gone.
  `cargo build` remains for compile-only development.
- Release archives still use `contrib/install.sh`.
- Runtime identity of a *running* process versus the file on disk is a
  separate fact (status `binary.integrity`), not this installer's job.
