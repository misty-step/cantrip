---
name: powder
description: Use Powder for cantrip work selection, claims, questions, release, and proof-backed completion.
---

# Powder work ledger

Powder is the sole work ledger for `misty-step/cantrip`; there is no fallback tracker. Origin is `POWDER_URL`, else `POWDER_API_BASE_URL`; identity is `POWDER_AGENT` (`--agent` overrides it). Commands emit JSON on stdout and JSON errors with a `code` on stderr.

When `POWDER_AGENT` is unset, do not call Powder; return a clean no-work summary.

## Lifecycle

1. `powder list --mine "$POWDER_AGENT" --repo <forest.yaml repo>` — resume one held job for this repository when no `forest/<id>/*` branch exists.
2. `powder list --takeable --repo <repo>` — choose one eligible nonempty spec.
3. `powder show <id>` — read the spec before taking it.
4. `powder take <id>` — claim before branching; `already_holding` requires finishing, asking, or releasing the held job.
5. Builders publish review-request evidence; Verifiers publish Checks/Verdict and run `powder done <id> --proof <revision>` only after approval.

Keep one live lease per agent and one `POWDER_AGENT` per Kernel. Available commands:

```sh
powder list --takeable --repo REPO
powder list --mine AGENT --repo REPO
powder show ID
powder take ID
powder release ID
powder ask ID --question '...'
powder done ID --proof PROOF
```
