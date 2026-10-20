# Install a published release

A Linux x86-64 user downloads a published Cantrip release without a compiler,
verifies its checksums and signed provenance, installs the single executable,
and confirms its version. A second plain install over it is refused.

## Sub-features

- `install-download` fetches the archive, `release.json`, `SHA256SUMS`, and `provenance.json` for one tag.
- `install-verify` checks checksums, `release.json` identity, and the GitHub attestation of both files.
- `install-binary` runs the bundled `install.sh install --prefix` and checks `--version` plus the binary SHA-256.
- `install-refuse-existing` refuses to overwrite an existing install without `update`.

## How to get to it (user POV)

- Website: "Download for Linux x86-64" on <https://cantrip.mistystep.io>.
- GitHub: <https://github.com/misty-step/cantrip/releases/latest>.
- Procedure: `docs/INSTALLATION.md` "Verify and install".

## Driving it with verify-cantrip

Preconditions:

- `gh` is logged in (the attestation verifier needs GitHub API access).
- `$RUN` is new or has no `prefix/`.

- **Install latest.** Run `$V install --out "$RUN"` (or `--version 0.1.8`).
  Every step prints `-> exit 0`; the final JSON shows `version`,
  `source_revision`, `binary_sha256`, and `"repeat_install_refused": true`.
- **Proof.** `$RUN/evidence/install.json` and the `sha256sum`, `gh attestation
  verify`, `install.sh`, and `--version` entries in `evidence/transcript.jsonl`.
- **Use it.** Pass `$RUN/prefix/bin/cantrip` to `$V up --binary`, so every
  later feature runs the binary users download.

## Gotchas

- `gh attestation verify` prints nothing on success when not on a TTY; exit 0 is the signal.
- The installer refuses a live daemon socket in its `XDG_RUNTIME_DIR`; the
  helper installs with private XDG dirs so the operator's running daemon does
  not block it and is not touched.
- `update` and `rollback` need `--backup PATH`; they are not driven yet.
- The archive also ships `sample.wav`; the journeys use the repo's identical
  public `samples/jfk.wav`.
