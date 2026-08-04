#!/usr/bin/env bash
# Point this checkout at the repository's git hooks.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
echo "hooks installed: core.hooksPath = $(git config core.hooksPath)"
echo "pre-commit: fmt + clippy + staged gitleaks scan"
echo "pre-push:   cargo test + working-tree trufflehog scan"
