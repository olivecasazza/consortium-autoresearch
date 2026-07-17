#!/usr/bin/env bash
# finalize.sh — commit, push, open PR for a successful agent run.
#
# Usage: bash finalize.sh <worktree> <branch> <task-file> <commit-msg>
#
# Prerequisites: score.sh has already passed. The worktree has a clean
# diff (the agent's changes). This script does NOT run tests itself.
set -euo pipefail

if [[ $# -ne 4 ]]; then
    echo "usage: finalize.sh <worktree> <branch> <task-file> <commit-msg>" >&2
    exit 2
fi

WORKTREE="$1"
BRANCH="$2"
TASK_FILE="$3"
COMMIT_MSG="$4"

cd "$WORKTREE"

# Reject if no changes — we don't make empty commits.
if git diff --quiet && git diff --cached --quiet; then
    echo "finalize: no changes to commit" >&2
    exit 4
fi

# Reject if k8s manifests touched — guard from program.md hard rule #4.
if git diff --name-only HEAD | grep -qE '^modules/k8s/'; then
    echo "finalize: refusing to commit changes to modules/k8s/" >&2
    exit 5
fi

# Stage only files under the safe roots. Anything else needs human review.
# (lib/ and tests/ moved to the sibling consortium-tests repo; python-side
# task changes are committed there, not in this repo.)
git add -- crates/ doc/ Cargo.toml Cargo.lock 2>/dev/null || true

# Commit. Pre-commit hooks (rustfmt, etc.) are enforced — never bypass.
if ! git commit -m "$COMMIT_MSG"; then
    echo "finalize: commit failed (likely a pre-commit hook). Inspect $WORKTREE manually." >&2
    exit 6
fi

# Push only if origin exists (during local-only dev runs it won't yet).
if git remote get-url origin >/dev/null 2>&1; then
    git push -u origin "$BRANCH" --force-with-lease >&2
    if command -v gh >/dev/null 2>&1; then
        # Use gh api directly — `gh pr create` has a known bug that
        # races on freshly-pushed branches with slashes in the name.
        REPO_FULL=$(git remote get-url origin \
            | sed -E 's|.*[:/]([^/]+/[^/]+)(\.git)?$|\1|; s|\.git$||')
        BODY_JSON=$(python3 -c 'import json,sys; print(json.dumps(open(sys.argv[1]).read()))' "$TASK_FILE" 2>/dev/null \
            || printf '"%s"' "$(tr -d '\n' < "$TASK_FILE" | sed 's/"/\\"/g')")
        gh api -X POST "/repos/$REPO_FULL/pulls" \
            -f base=master \
            -f head="$BRANCH" \
            -f title="$COMMIT_MSG" \
            -f body="$(cat "$TASK_FILE")" >&2 || true
    fi
fi

# Move task file to done/.
DONE_DIR="$(git -C "$WORKTREE" rev-parse --show-toplevel)/autoresearch/queue/done"
# But the task file lives in the main repo's queue, not the worktree's
# (worktrees share .git but each has its own working tree). Find the main
# repo path.
MAIN_REPO="$(git -C "$WORKTREE" worktree list --porcelain | head -1 | awk '{print $2}')"
mkdir -p "$MAIN_REPO/autoresearch/queue/done"
mv "$TASK_FILE" "$MAIN_REPO/autoresearch/queue/done/"
