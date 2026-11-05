# 0035: Release-only CI, full qualification before publication

- **Status:** accepted
- **Date:** 2026-11-03
- **Primitives:** CI, runner, release
- **Work:** [MIS-814](https://linear.app/misty-step/issue/MIS-814/bring-complete-cantrip-ci-feedback-below-ten-minutes)

## Context and measurements

The owner's 2026-11-03 delivery correction places CI and one critical-correctness/
security assessment only at the pre-deploy release gate. Authors prove the
running product, then merge without PR CI or a review gate. No PR or push event
starts a workflow. This supersedes the initial push-to-green merge-gate design;
the latency work remains useful at the release gate, not as a reason to retain
per-change CI.

Measured on 2026-11-03, before this decision:

| PR run | Required checks green after run creation | Limiter |
| --- | --- | --- |
| [41681872406](https://github.com/misty-step/cantrip/actions/runs/41681872406) | 30m40s | `check` queued 24m53s; title check waited until the end of the single runner queue |
| [41688675524](https://github.com/misty-step/cantrip/actions/runs/41688675524) | 14m41s | approval/queue delay plus a 6m36s `check` job |

Measured step timings in those runs: the canonical Linux gate took 2m00s and
2m40s; restoring the Rust cache took 1m48s and 59s. The second run spent another
2m20s saving the cache. Inferred: serial admission and cache I/O, not test
execution alone, dominate required-check latency.

Measured native qualification in [41681866029](https://github.com/misty-step/cantrip/actions/runs/41681866029):
Intel took 68m39s, including 58m56s preparing its pinned ONNX Runtime and 59s
saving the validated runtime. [PR #250](https://github.com/misty-step/cantrip/pull/250)
made the runtime reusable: [warm run 41693359965](https://github.com/misty-step/cantrip/actions/runs/41693359965)
took 9m41s on Intel (42s restore, 10s validation), and 4m01s on arm64. Runtime
cache loss or a compiler/image change can still require a cold build; a warm
cache is an optimization, never a prerequisite for correctness.

The implementation PR's Evidence section owns historical PR measurements and
the new release-qualification dispatch measurement, with exact source SHAs and
cold/warm cache conditions. Do not copy a changing run ledger here.

## Decision

Use the standard GitHub-hosted runners already available to this public
repository. [GitHub documents these runners as free for public repositories](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#standard-github-hosted-runners-for-public-repositories).
No paid runner, runner purchase, new credential, or additional Hetzner machine
is authorized. None of the current portable jobs requires private networking
or a persistent host, so none needs the single Hetzner runner. This change does
not decommission that machine or its other workloads.

| Entry and tier | What it proves | Publication barrier |
| --- | --- | --- |
| Author before merge | changed real-product success, rejection and recovery | author-owned proof, no GitHub required check |
| `release.yml` dispatch on master: prepare | Landmark's pinned version/notes generation and release-plan identity | metadata-only PR, no compilation or bot-workflow approval |
| Dispatch after the generated plan is merged: `ci.yml` | formatting, locked all-target Clippy, every workspace test, offline evaluation, Omarchy/install/release/site contracts and both secret scanners | successful `qualification` |
| The same qualification, in parallel: both native architectures | Intel ABI/runtime and arm64 behavior, native workspace, development packaging, relocated public-file STT, capture refusal and HUD/windows | successful `qualification` |
| The same candidate: build → downloaded artifact verification → attestation/publication | CPU-only Ubuntu baseline, checksums/source identity, public-file STT, install/update/rollback/uninstall and retained-state preservation on the exact published bytes | successful `build`, `verify` and `qualification` |
| Website dispatch on master, or after app publication | build/check, current published assets, monotonic promotion and public-route proof | static build succeeds before deployment; post-publication refresh preserves trusted live website source |
| Twice-daily `macos.yml` schedule | only restore/validate/prepare/save the pinned Intel runtime cache | no qualification claim, artifact publication or website deployment |

`ci.yml` has only `workflow_call` and `workflow_dispatch` entry points. Its
Rust, Python contracts, secret scan and native matrix start independently.
Linux `check` accepts only successful Rust and contract shards; `release-gate`
accepts only successful `check`, `secret-scan` and the complete native matrix.
Both aggregate jobs use `always()` and reject failed, cancelled or skipped
dependencies. No scanner, Linux test, native packaging or public-fixture
proof is deleted. Native qualification always includes arm64 and Intel;
the schedule is deliberately only cache maintenance.

`release.yml` is dispatched explicitly on master. If the source needs a plan,
it prepares the Landmark PR and stops without qualification. The author merges
the generated metadata without CI. The conductor performs the single
critical-correctness/security release assessment on that candidate, then
dispatches `release.yml` again. Its valid plan selects publishing mode, running
qualification alongside the one Linux release build and downloaded-artifact
verification. `publish` depends on all three, so failed/skipped native or
portable qualification cannot publish. The attested/published archive is the
artifact already built and exercised, never a rebuild in the publish job.

```sh
gh workflow run release.yml --repo misty-step/cantrip --ref master
# After author proof, generated-plan merge and the release assessment:
gh workflow run release.yml --repo misty-step/cantrip --ref master
# Qualification-only measurement/diagnosis on an exact candidate branch:
gh workflow run ci.yml --repo misty-step/cantrip --ref <candidate-branch>
```

### Local macOS publication and credential custody

For [#238](https://github.com/misty-step/cantrip/issues/238), the operator
requires the Developer ID private key and notarization profile to remain in
the login Keychain on his Mac. The hosted native matrix remains **ad-hoc
development qualification**, never production signing. No `.p12`, Apple
Account password, app-specific password, signing-key password, or notary profile
is uploaded to GitHub secrets or a worker.

After `release.yml` publishes its qualified tag, the operator runs
[`scripts/publish-macos`](../../scripts/publish-macos) from a clean worktree
at that exact released tag. The
[installation runbook](../INSTALLATION.md#publish-macos-assets-for-an-already-released-tag)
owns the commands and
[Apple-account handoff](../INSTALLATION.md#production-signing-setup-on-the-operators-mac).
The publisher requires a valid login-Keychain Developer ID Application
identity and authenticated profile, signs/notarizes/staples/Gatekeeper-assesses
the app, uploads target-qualified ZIP/manifest/checksums to that existing release,
and verifies their downloaded bytes. It never replaces existing release assets
or changes Linux's attested assets. `release.yml` records this local handoff
in its job summary but neither waits for nor claims Mac production assets.

This is an explicit exception to unattended cross-platform publication, not
a silent CI gap. Signing in CI could automate both native architectures, but
would require exporting the private key and delegating notarization credentials,
contrary to the custody requirement. That alternative is rejected, not scaffolded.
The arm64 host cannot use the repository's native-only Intel runtime/build
contract. This setup does not authorize moving credentials to an Intel Mac or
runner. Intel publication requires a future operator-approved, verified build
boundary that still signs on this Mac. Do not turn CI's ad-hoc Intel app into
an advertised release or create a second signing home.

Developer ID Keychain persistence is measured, not assumed from notarization.
The [disposable-VM experiment](../INSTALLATION.md#measure-keychain-grants-across-code-updates)
checks both an item's creator and a separate explicit Always Allow grant against
a never-before-run cdhash. Its dev-identity and ad-hoc controls distinguish a
stable designated requirement from the separate Keychain partition check.
The [Developer ID receipt](https://github.com/misty-step/cantrip/pull/267#issuecomment-6663383405)
measures both grants surviving an unseen cdhash on macOS 27.0 (26A428), while
the [no-team control receipt](https://github.com/misty-step/cantrip/pull/267#issuecomment-6662780933)
records both failing. Notarization/profile authentication and production asset
publication remain separate qualification, not consequences of this result.



There are **no required GitHub checks for merging**, including the Landmark
release PR. Remove the former `check`, `secret-scan` and `pr-title` branch
requirements; do not replace them with `release-gate`. The checkout-free PR
metadata workflow is removed with the other PR triggers. Conventional Commit
titles remain the source convention, not another merge-time workflow.

## Concurrency and website

Qualification/native concurrency groups use the unique `github.run_id` and
never cancel earlier work. A later dispatch or cache warm-up cannot replace
another candidate waiting behind the same branch reference.

The release workflow alone retains the shared `cantrip-release` writer group,
with `cancel-in-progress: false` and `queue: max`. [GitHub documents](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#concurrency)
that `max` preserves up to 100 waiting runs; a later arrival never replaces
one already queued. At capacity GitHub rejects additional arrivals rather than
evicting pending release heads. Only explicit master release dispatches enter
this queue; a cache warm-up cannot take their place. The default single-pending policy is
not safe for this writer. Existing source/ancestry guards still reject stale
promotion, since queue order is based on time entering the queue, not dispatch.

The website is a reusable workflow with an explicit master dispatch entry.
It is independent of native qualification for website-only releases, and
performs its own focused static-build pre-deploy gate. App publication calls
it to refresh published-asset data, so release notes do not wait for a later
push. One non-cancelling, `queue: max` website group serializes both forms of
promotion.
For post-publication refresh, the existing Cloudflare deployment annotation
selects the already-qualified live website source while that writer lock is
held. If a newer master website has already deployed, refresh keeps its code
instead of rebuilding the older release checkout or an unqualified branch tip.
The same source/ancestry parser owns selection and the final promotion check;
unknown, split, mismatched or foreign source identity still fails closed.

Only the existing `CLOUDFLARE_API_TOKEN` is forwarded, explicitly by name,
through `release.yml → site.yml` for post-publication refresh. Direct website
dispatch uses that same repository secret. No `secrets: inherit`, new credential
or API scope is used. Both entries require master `workflow_dispatch`; PRs and
pushes start no website workflow and never reach deployment. Qualification-only
dispatch has no Cloudflare secret.

The website job has a 30-minute watchdog. Its four sequential `verify-site`
commands may each spend 180 seconds waiting for edge convergence plus the
15-second HTTP fetch allowance. The watchdog leaves room for those waits and
setup/build/deploy, instead of killing the job before the verifier's diagnosis.


## Caches and sharding

Keep `rust-toolchain.toml` as the compiler authority and `Cargo.lock` as the
dependency authority. `rust-cache` retains dependency build artifacts, keyed by
compiler, environment and lockfile, with distinct Linux/native/release caches.
The Linux development/test profiles omit debug information, not assertions or
tests: smaller artifacts reduce compilation and cache transfer, while release
artifact settings stay unchanged. A cache miss executes the same locked gates.

Keep #250's separately keyed, verified Intel runtime cache and immediate save
after successful preparation. It contains source/revision metadata and static
archives, not CMake objects; target, pinned runtime/script, image, Xcode, SDK,
compiler and CMake identity remain part of its key. Never replace its integrity
checks with a cache-hit assertion. Upstream arm64 CPU prebuilts remain the fast
native runtime; do not introduce an unverified Intel download mirror.

The twice-daily schedule exists only to pre-warm the default-branch Intel
runtime cache before a release gate. It selects Intel alone and skips Rust
toolchain/cache work, workspace checks, app packaging, native runtime proof
and artifact upload. It cannot satisfy release qualification.

A PR-scoped cache is not readable from master or an unrelated candidate branch.
If no matching default-branch entry exists, pre-warming or the first release
qualification still builds the pinned runtime: measured 68m39s total with
58m56s preparation in the prior cold qualification. The eager save retains
validated archives before later app work. Image/key changes and eviction can
require another cold build; observe the matching master key rather than assume
warmth. Never copy a PR-scoped cache into trusted master scope or replace
qualification with warm-up success.


Do not stack sccache on top of these target caches without evidence. Its
[Rust constraints](https://github.com/mozilla/sccache/blob/main/docs/Rust.md)
exclude linker invocations and metadata-only compilation, so it cannot cache
the main application/test links or clippy's metadata-only work. The measured
limiter is admission and artifact transfer. A second cache adds storage,
network requests and eviction pressure without an observed benefit here.

Shard by independent contract family: Python safety tests do not wait for Rust
compilation or share its filesystem. Keep Rust workspace tests and the offline
evaluation on the same runner as clippy, reusing one dependency graph. Fine
per-test or per-crate sharding would duplicate dependency restore/codegen for
short test execution. Native architectures already run independently. Revisit
finer test sharding only when measured test execution, not compilation/cache
I/O, is the critical path; never use test retries to meet the latency target.

## Latency defects

[#202](https://github.com/misty-step/cantrip/issues/202): the timeout tests
assumed a shell would write a PID file within the production five-second
budget. A loaded runner could exhaust that budget before the shell ran, so
`NotFound` tested scheduling rather than process survival. Establish fixture
readiness before testing termination, keep the production absolute deadline
starting before spawn, and prove real child/group termination through FIFO EOF
and the direct child's SIGKILL instead of Linux `/proc` absence. Use `select`
for the fixture wait: measured on Darwin, `poll` reports FIFO data but misses
EOF, while `select` reports both. Do not extend production timeouts or rerun
failed tests to hide a race.

[#227](https://github.com/misty-step/cantrip/issues/227): release preparation
must not wait for bot-created PR workflows or their approval. There are no
PR-triggered workflows under this method. Remove the obsolete asynchronous
approval discovery/poller and its `actions: write` permission instead of
waiting for runs that will never be created. New and existing Landmark PR
preparation tests exercise the GitHub CLI boundary and reject any approval or
workflow-dispatch side effect. The actual post-merge release dispatch runs the
exact candidate's qualification directly, not substitute proof for an automatic
PR check.

## Reconsideration

Measure dispatch-to-`release-gate` wall time, including queueing, setup, cache
transfer and aggregate completion; measure app publication separately when a
real release is authorized. Inferred from measured phases: portable feedback
should remain under five minutes and a warm complete qualification should be
about ten minutes, dominated by the prior 9m41s Intel job. Build/runtime proof
overlap that qualification rather than adding to its critical path. A cold
runtime is an explicit rollout/cache-loss exception, not an acceptable steady
state. Fix the measured critical path before adding runners, finer sharding
or another cache. Paid capacity and changed repository visibility remain
operator decisions. No PR/push CI or branch-protection wait is reintroduced
to improve a release metric.
