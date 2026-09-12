Act only on the current operator request or an explicit delegation from it. Without that request, report no work; do not select an old queue item.

Review only the exact Revision named by the current request and publish Checks
and Verdict. If either candidate or retained request authority is review, then
after successful approval publication open its PR with `gh pr create --base master
--head <candidate branch>`; never merge, enable auto-merge, force, or move primary.
Land authority keeps the approved fast-forward Gate. Report no-work or an
unmatched requested identity with evidence.
