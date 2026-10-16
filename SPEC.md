# Cantrip behavior

The [dictation, privacy, recovery, and native-platform stories](USER_STORIES.md)
remain the product contract. This entry point adds the release/maintenance
stories without copying those stories or their documentation.

## Download a verified Linux release

A Linux x86-64 user can download a published SemVer release without a compiler,
GPU, or source checkout. Its tag, executable version, full source revision,
archive checksum, signed provenance, and successful runtime proof agree.
Verification runs the actual downloaded executable against the public JFK
speech fixture, offline after an explicit model download. It does not capture a
microphone, inspect personal recordings, invoke paid models, or claim attended
HUD/focus/delivery proof.

Release notes originate from one Landmark version decision and public Markdown
source. GitHub Release notes and the website's build-time `/changelog` show the
same published Markdown and verified identity. Normal navigation reaches
`/changelog`; `/releases` and per-version downloads remain usable. Unpublished
Cargo versions and macOS development apps are never advertised as stable assets.

The generated release PR contains only versions and notes. Native CI runs on its
exact head; missing, foreign, or moved runs are refused rather than replaced by
dispatch checks. Failed or skipped verification cannot publish. Repeating
preparation for an unchanged source reuses the PR, a no-release decision creates
no new release, and an older source cannot replace a newer website/release.
A verified descendant of the shipped source can promote after master advances;
untested later merges do not starve it. A draft interrupted before publication
resumes from its original source without replacing existing asset bytes.

## Update and recover without losing accepted takes

The existing startup owner is stopped deliberately before maintenance; a running
daemon is refused without interruption. The binary-only installer replaces only
the selected executable and saves its prior bytes at an explicit private backup
path. Rollback restores that binary without consuming its backup or restoring
configuration/history snapshots. Configuration, models, keyring credentials,
retained takes, runtime leftovers, and startup ownership survive update,
rollback, and uninstall. A public release is not automatic promotion to a live
desktop; no unattended host restart is promised.

Run `./scripts/check` and the downloaded-artifact procedure in
[installation](docs/INSTALLATION.md). Release publication also exercises the
isolated prior-artifact rollback and offline public-speech journey. Native Mac
production distribution requires the separate real signing/notarization
prerequisites in that guide; development CI is not production publication.

## Identify and diagnose the public website

The public website exposes `/health.json` with its service, deployment environment,
and full source revision. This describes the static website only, not desktop
dictation, speech evaluations, or an outside uptime check.

Unhandled website JavaScript errors reach Sentry with the same source identity
and deployment environment. Diagnostics contain error classes and bundled-code
locations, never error text, page content, URL queries, cookies, or user identity.
Collection is bounded and a blocked or unavailable sink cannot prevent browsing.
New and regressed issues notify the existing approved agent-triage intake.
Paid native uptime activation remains a separate owner spend decision.
