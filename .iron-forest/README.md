# Local Linear profile

`linear.py` owns Linear selection; the Kernel does not interpret ticket labels.
Only open, unarchived issues in this repository's configured Linear project are
eligible, with one of these labels:

- `Agent: Land` emits `authority: land`.
- `Agent: Review` emits `authority: review`.
- Legacy `Agent: Ready` emits `authority: review`, never land.

When Land appears with Review or Ready, review wins. `Operator Action` always
excludes an issue, regardless of execution labels. Poll diagnostics and the
retained builder prompt name the matching labels and selected authority.

Builder and Fixer copy authority from their retained request into immutable
review-request evidence. Verifier and Fixer intake binds one exact candidate's
Subject, branch, SHA, complete work snapshot, and authority from Git evidence;
it does not re-query the ticket's current labels. Missing authority on legacy
Linear evidence becomes review. Invalid authority or absent/ambiguous work-bound
candidates are no-work. Historical request evidence without a work snapshot does
not wake the verifier, even if a generic Kernel poll would report it.
Intake first fetches origin's immutable native evidence refs; publication and
Kernel-private fetches do not populate the adapter's local cache. Non-Linear
work is excluded, and a failed evidence refresh is an error, never stale intake.

If either the candidate or approving Run says review, the Verifier publishes
exact Checks and Verdict without advancing primary. After successful approval it
opens the candidate PR with `gh pr create --base master --head <candidate branch>`
(or uses its existing PR), then runs
`python3 "$FOREST_ROOT/.iron-forest/linear.py" review-receipt verifier --revision <sha> --pr <PR URL>`
before the same native Verifier Run ends. The helper posts the existing
`forest.review.v1` marker and exact six-field payload. Published Checks/Verdict,
timestamps, and immutable refs are retained in `summary`, never extra top-level
fields. `python3 .iron-forest/test_review_receipt.py` checks the emitted contract.
GitHub supplies comment timestamps; an ended Run
cannot be backfilled into verified evidence. It never merges, enables
auto-merge, or force-pushes.
Land retains the existing Kernel-approved fast-forward publication path. Explicit
land does not elevate a non-git-native delivery profile.

Offline selection fixtures:

```sh
FOREST_ROOT="$PWD" python3 .iron-forest/linear.py poll builder --fixture land
FOREST_ROOT="$PWD" FOREST_RUN_ID=fixture python3 .iron-forest/linear.py request builder --fixture review
```

Also available: `ready`, `ambiguous`, `operator`, `zero-label`, and `empty`.
The first two emit review; the last three return no-work (exit 1).
