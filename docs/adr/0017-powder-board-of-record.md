# ADR 0017: Powder is the board of record

Date: 2026-08-21. Status: accepted (operator decision, same day).

## Problem

VISION named GitHub Issues the product board of record while the
organization work ledger is Powder. Running both meant two live boards for
one factory. The backlog had grown to 37 open issues, several already
implemented on `master`, and this repository's Iron Forest declarations were
stale copies from before upstream ADR 0023 ("One Subject identity"): they
selected only `forest:ready` GitHub Issues, wrote
`forest.review-request.v1` payloads with `"issue": <n>`, and could not name
a Powder job. The local Kernel binary predated even `agents:` in
`forest.yaml` and failed to parse the repository configuration.

## Decision

1. **Powder owns scheduling.** Every open GitHub Issue became a Powder job
   `cantrip-<n>-<slug>` in repository `misty-step/cantrip` on the Sanctum
   origin, carrying its original body verbatim plus source link, labels,
   priority, and epic reference. GitHub Issues are a read-only archive:
   thirty unfinished issues are closed with reason `not planned` and a
   pointer comment; the seven jobs already complete at migration time are
   closed `completed` and their Powder jobs carry file-and-line proof.
2. **New work starts in Powder.** New findings are filed with
   `powder create --repo misty-step/cantrip`; no new GitHub Issues.
3. **Epics block on their children.** An epic job lists its children and is
   `blocked_by` them, so it becomes takeable only for closeout after every
   child is terminal. Children never point back at the epic.
4. **The factory speaks Subject v2, Powder-only.** This repository's `agents/`
   declarations are refreshed from the canonical Iron Forest checkout (its
   ADR 0023) and then specialized: selection lists only Powder jobs, the
   Builder files new findings as Powder jobs, and an unset `POWDER_AGENT`
   fails the pass instead of falling back to GitHub Issues. Branches are
   `forest/<subject>/<slug>`, review-request payloads are
   `forest.review-request.v2` with `subject`, Builder takes before
   branching and releases on failure, Fixer reuses the subject, and
   Verifier calls `powder done` after a successful approve.
5. **One instance, one identity.** The `forest@cantrip` service environment
   (`~/.config/iron-forest/cantrip.env`) sets `POWDER_AGENT=forest-cantrip`,
   one identity for this Kernel, not shared across repositories.

## Consequences

There is one board. Poll wakes on takeable Powder jobs for
`repo: misty-step/cantrip` with a nonempty spec; leftover
`forest/<n>-<slug>` branch tips from schema v1 are unread by Poll and
harmless. Job ids encode their origin issue number, so archive lookups stay
trivial. The cost is that GitHub stars, watchers, and external issue
traffic no longer feed the factory automatically; humans who find a bug
need Powder access or an intermediary.

## Verification

- All 37 jobs created; blockers wired (#59 → ten children, #47 → #55,
  #63 → #66, #71 → #68, #72 → Langfuse project gate, #15/#19 → test
  harness, #24 → #25).
- Seven completed jobs terminal with proof; 30 archive issues
  `not_planned`, verified through the REST read-back.
- Canonical sibling installer rebuilt the Kernel from Iron Forest
  `f6b7900`: `selfcheck: ok`, `forest@cantrip` active, Builder trigger
  dispatching with zero errors against the Powder lane.
- The repository `has_issues` flag is now `false` (captured before/after in
  the evidence packet), making the read-only-archive claim enforced rather
  than aspirational; re-enabling is a single API call if public intake
  returns.
