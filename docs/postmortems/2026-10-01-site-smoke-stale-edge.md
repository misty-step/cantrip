# Postmortem: site smoke judged the edge before the deploy propagated

- **Incident date:** 2026-10-01
- **Tracker:** MIS-202
- **Impact:** Website run [36908552927](https://github.com/misty-step/cantrip/actions/runs/36908552927) attempt 1 went red on a good deploy. Attempt 2 passed minutes later. The site was never down or wrong.

## Mechanism

`wrangler deploy` returned, and about two seconds later the smoke ran `cmp` of
`site/dist/index.html` against the live home page once. The edge still served
the previous build. The difference was byte 2257: the patch digit of
`download/v0.1.3` in the Linux download URL, served as `v0.1.2` (0.1.3 had just
been released). Not a build stamp; today's live page is byte-identical to the
build, and substituting `v0.1.2` reproduces `first difference at byte 2257`.

## Pokayoke

`scripts/verify-site` replaces the one-shot `cmp`. Following misty-step PR 327,
a healthy edge serving different bytes gets at most 180 seconds, polled every 5
seconds. HTTP and transport errors fail immediately. A page that never
converges fails naming the first differing byte, sizes, hashes, and surrounding
text. A stale edge cannot fail a good deploy; a real mismatch cannot pass.
`tests/test_verify_site.py`, part of `scripts/check`, pins those behaviors.

Repair: [PR 119](https://github.com/misty-step/cantrip/pull/119). Master run
[36914200378](https://github.com/misty-step/cantrip/actions/runs/36914200378)
deployed and passed the new smoke.
