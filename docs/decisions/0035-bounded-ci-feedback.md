# 0035: Bounded PR feedback, full qualification before release

- **Status:** accepted
- **Date:** 2026-11-03
- **Primitives:** CI, runner, release
- **Work:** [MIS-814](https://linear.app/misty-step/issue/MIS-814/bring-complete-cantrip-ci-feedback-below-ten-minutes)

## Context and measurements

The operator's target is less than five minutes from push to required checks
being green, with ten minutes the acceptance ceiling. Queue time and cache
transfer count; timing only `cargo test` does not measure that contract.

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

The implementation PR's Evidence section owns the after measurements, with the
source SHA and at least two `pull_request` runs. Do not copy a changing run ledger here.

## Decision

Use the standard GitHub-hosted runners already available to this public
repository. [GitHub documents these runners as free for public repositories](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#standard-github-hosted-runners-for-public-repositories).
No paid runner, runner purchase, new credential, or additional Hetzner machine
is authorized. None of the current portable jobs requires private networking
or a persistent host, so none needs the single Hetzner runner. This change does
not decommission that machine or its other workloads.

| Tier and trigger | What it proves | Gate |
| --- | --- | --- |
| Every PR: `scripts/check rust` | formatting, locked all-target clippy, every workspace test, offline evaluation example | required `check` |
| Every PR, in parallel: `scripts/check contracts` | Omarchy, source/binary installers, release identity/promotion, website smoke contracts | required `check` |
| Every PR, in parallel: full-history gitleaks and working-tree TruffleHog; literal Conventional Commit title | unchanged secret and squash-title contracts | required `secret-scan`, `pr-title` |
| Every PR, in parallel: native arm64 | locked workspace, development app packaging, relocated public-file STT, capture refusal, HUD/windows | native PR check |
| Master after Linux gates, and twice daily: both native architectures | Intel ABI/runtime/native artifact qualification; refresh default-branch caches | successful native and Linux jobs precede publication |
| Release candidate: existing build → downloaded artifact verification → attestation/publication | CPU-only Ubuntu baseline, checksums/source identity, offline public-file STT, install/update/rollback/uninstall and retained-state preservation on the exact published bytes | publication requires successful qualification and runtime proof |
| Master after Linux gates: website build, promotion and public-route proof | the current source with the latest already-published release assets | independent of native qualification |
| After release publication: repeat website synchronization and proof | newly published version/notes appear without waiting for another push | successful publication |

`check` remains the required context, not the name of a chosen shard. It runs
with `always()` and accepts only successful Rust **and** contract results;
failure, cancellation or skipping cannot turn an incomplete gate green.
There are no path filters that silently omit a required check.
Title validation has its own checkout-free `pr-title.yml` workflow. Editing a
PR title or evidence body reruns metadata validation, not compilation/scanning;
the code workflows run only on opening, reopening or a new head.

Intel qualification moves from each PR to master and the twice-daily full
battery. Release planning and native qualification run independently; the
metadata-only release PR need not wait for a compiler. Artifact build/runtime
proof also overlap native qualification. Publication explicitly depends on
successful native, build and runtime jobs for the same master SHA, so a failed
or skipped native job cannot publish. Scheduled runs qualify but do not publish
releases or deploy the website. Downloaded Linux release proof remains intact.
No Linux test, scanner, native packaging or public-fixture proof is deleted.
An Intel regression can still reach master before it is found; the release
barrier is deliberate, not a claim that arm64 proves Intel behavior.

## Concurrency and website

PR CI/native groups use the PR number and cancel superseded work. Non-PR
CI/native groups instead use the unique `github.run_id`; a push, dispatch or
schedule cannot replace another run waiting behind the same branch reference.
This also prevents a scheduled native run from cancelling an active release's
native qualification.

The release workflow alone retains the shared `cantrip-release` writer group,
with `cancel-in-progress: false` and `queue: max`. [GitHub documents](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#concurrency)
that `max` preserves up to 100 waiting runs; a later arrival never replaces
one already queued. At capacity GitHub rejects additional arrivals rather than
evicting pending release heads. Only publishing-capable master runs enter this
queue; a schedule cannot take their place. The default single-pending policy is
not safe for this writer. Existing source/ancestry guards still reject stale
promotion, since queue order is based on time entering the queue, not dispatch.

The website is a reusable workflow, called after Linux `check` and
`secret-scan` succeed, with no dependency on Mac qualification or the complete
CI workflow result. Release publication calls it again to refresh version data
from the newly published assets; ordinary website changes therefore need not
wait for native builds, while release notes do not remain stale after publishing.
One non-cancelling, `queue: max` website group serializes both kinds of promotion.
For post-publication refresh, the existing Cloudflare deployment annotation
selects the already-qualified live website source while that writer lock is
held. If a newer master website has already deployed, refresh keeps its code
instead of rebuilding the older release checkout or an unqualified branch tip.
The same source/ancestry parser owns selection and the final promotion check;
unknown, split, mismatched or foreign source identity still fails closed.

Only the existing `CLOUDFLARE_API_TOKEN` is forwarded, explicitly by name:
`CI → site.yml` for master deployment and `CI → release.yml → site.yml` for
post-publication refresh. No `secrets: inherit`, new credential or API scope is
used. These calls require master push/dispatch; PRs receive only the existing
build/check path, never the deployment step or forwarded secret.

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

Initial rollout: #250's PR-scoped Intel cache is not readable from master. If
no matching default-branch cache has been saved yet, the first post-merge
master qualification is an expected one-time cold build: measured 68m39s,
including 58m56s of runtime preparation. It runs outside PR feedback and must
complete before publication. The eager save preserves the validated runtime
before later app checks; subsequent qualification reads can reuse it. Twice-daily
qualification refreshes the default-branch cache, but image/key changes or
eviction can require another cold build. Never copy a PR-scoped cache into the
trusted master scope or replace qualification with a warm-up success claim.


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

[#227](https://github.com/misty-step/cantrip/issues/227): GitHub creates native
release-PR runs asynchronously. The release adapter waits at most 120 seconds
in total for exact-head `pull_request` runs, discovering missing workflows at
two-second intervals. Every API operation shares the remaining monotonic
budget. It rechecks the bot PR's repository/head identity before approvals,
approves each discovered blocked run once, and fails closed on API errors,
head movement or timeout. This is discovery synchronization, not failed-CI
retry, approval bypass, or substitute `workflow_dispatch` proof.

## Reconsideration

Measure push-to-green at the actual required-check completion timestamps,
including setup, queueing, cache post-steps and the aggregate gate. Compare at
least two PR runs and state cold/warm cache conditions. If the warm fast tier
exceeds five minutes or any required tier exceeds ten, identify its measured
critical path before adding runners, sharding or a cache. Paid capacity remains
an explicit operator decision. Changing repository visibility also requires
revisiting the free-hosted-runner assumption.
