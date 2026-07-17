#!/usr/bin/env bash
# score.sh — differential fitness gate for an autoresearch task.
#
# Usage: bash score.sh [<worktree>]
# Default worktree is $PWD.
#
# A run passes iff, relative to the master baseline in
# autoresearch/.baseline.json:
#   - cargo fmt --check passes (absolute)
#   - tests_passing(branch) >= tests_passing(master)
#   - clippy_errors(branch) <= clippy_errors(master)
#   - if Python paths are touched (lib/, tests/ — these live in the
#     sibling consortium-tests repo): pytest -x passes there (absolute)
#
# This means agents are not penalized for pre-existing clippy noise, but
# they cannot regress test counts or add new clippy errors.
#
# Exit 0 on pass, non-zero otherwise. Stdout: human summary. Stderr:
# tail of any failing tool's output.
set -uo pipefail

WORKTREE="${1:-$PWD}"
cd "$WORKTREE" || { echo "score: cannot cd to $WORKTREE" >&2; exit 2; }

# Self-activate the consortium-autoresearch nix devshell so cargo / nextest /
# clippy / fmt are on PATH regardless of caller. Without this, callers like
# gc-supervisor (whose unit PATH lacks the rust toolchain) hit
# `cargo: command not found` and run-once misclassifies the abandon as
# score-fail. nix develop is cached after the first run.
if ! command -v cargo >/dev/null 2>&1; then
    FLAKE_DIR="$WORKTREE"
    if [[ ! -f "$FLAKE_DIR/flake.nix" ]]; then
        FLAKE_DIR="$(git -C "$WORKTREE" worktree list --porcelain | head -1 | awk '{print $2}')"
    fi
    exec nix develop "$FLAKE_DIR" --command bash "$0" "$@"
fi

# Find the main repo (worktrees share one .git dir; the baseline file
# lives in the main checkout).
MAIN_REPO="$(git worktree list --porcelain | head -1 | awk '{print $2}')"
BASELINE="$MAIN_REPO/autoresearch/.baseline.json"
# Python oracle + upstream tests live in the sibling consortium-tests repo.
CONSORTIUM_TESTS_DIR="${CONSORTIUM_TESTS_DIR:-$MAIN_REPO/../consortium-tests}"
if [[ ! -f "$BASELINE" ]]; then
    echo "score: $BASELINE missing — run autoresearch/scripts/compute-baseline.sh first" >&2
    exit 9
fi

BASE_TESTS=$(awk -F'[:,]' '/"tests_passing"/{gsub(/[[:space:]]/,"",$2); print $2; exit}' "$BASELINE")
BASE_CLIPPY=$(awk -F'[:,]' '/"clippy_errors"/{gsub(/[[:space:]]/,"",$2); print $2; exit}' "$BASELINE")
: "${BASE_TESTS:=0}"
: "${BASE_CLIPPY:=999}"

TMP=$(mktemp -d -t ar-score.XXXXXX)
trap 'rm -rf "$TMP"' EXIT
FAIL=0
SUMMARY=""

# Gate 1 (absolute): cargo fmt --check
if cargo fmt --all -- --check >"$TMP/fmt.log" 2>&1; then
    SUMMARY+="PASS  fmt"$'\n'
else
    SUMMARY+="FAIL  fmt"$'\n'
    echo "=== FAIL: fmt ===" >&2
    tail -n 50 "$TMP/fmt.log" >&2
    FAIL=$((FAIL + 1))
fi

# Gate 2 (differential): cargo clippy. We don't error on warnings — we
# count them and compare to baseline.
cargo clippy --workspace --all-targets -- -D warnings >"$TMP/clippy.log" 2>&1 || true
BRANCH_CLIPPY=$(grep -cE '^error(\[|: )' "$TMP/clippy.log" 2>/dev/null || true)
BRANCH_CLIPPY=${BRANCH_CLIPPY:-0}
if [[ "$BRANCH_CLIPPY" -le "$BASE_CLIPPY" ]]; then
    SUMMARY+="PASS  clippy ($BRANCH_CLIPPY <= $BASE_CLIPPY baseline)"$'\n'
else
    SUMMARY+="FAIL  clippy ($BRANCH_CLIPPY > $BASE_CLIPPY baseline)"$'\n'
    echo "=== FAIL: clippy regression ($BRANCH_CLIPPY new vs $BASE_CLIPPY baseline) ===" >&2
    grep -E '^error' "$TMP/clippy.log" | tail -n 20 >&2
    FAIL=$((FAIL + 1))
fi

# Gate 3 (differential): cargo nextest test count.
cargo nextest run --workspace --no-fail-fast --status-level pass >"$TMP/test.log" 2>&1 || true
BRANCH_TESTS=$(grep -oE '[0-9]+ passed' "$TMP/test.log" | awk '{s+=$1} END {print s+0}')
if [[ "$BRANCH_TESTS" -ge "$BASE_TESTS" ]]; then
    SUMMARY+="PASS  test ($BRANCH_TESTS >= $BASE_TESTS baseline)"$'\n'
else
    SUMMARY+="FAIL  test regression ($BRANCH_TESTS < $BASE_TESTS baseline)"$'\n'
    echo "=== FAIL: test regression ===" >&2
    tail -n 50 "$TMP/test.log" >&2
    FAIL=$((FAIL + 1))
fi

# Gate 4 (absolute, conditional): pytest if Python paths touched. Since
# the test-infrastructure split, lib/ and tests/ live in the sibling
# consortium-tests repo. The gate fires when this repo's diff touches
# them (legacy), or when the sibling checkout has pending lib//tests
# changes (python-side task in flight), and pytest runs there.
BASE_REMOTE=origin
git -C "$WORKTREE" remote get-url "$BASE_REMOTE" >/dev/null 2>&1 || BASE_REMOTE=consortium
BASE_REF="$BASE_REMOTE/master"
git -C "$WORKTREE" rev-parse --verify "$BASE_REF" >/dev/null 2>&1 || BASE_REF=master

PY_TOUCHED=0
if git diff --name-only "$BASE_REF"...HEAD 2>/dev/null | grep -qE '^(lib/|tests/.*\.py$)'; then
    PY_TOUCHED=1
elif [[ -d "$CONSORTIUM_TESTS_DIR/.git" ]] \
        && [[ -n "$(git -C "$CONSORTIUM_TESTS_DIR" status --porcelain -- lib/ tests/ 2>/dev/null)" ]]; then
    PY_TOUCHED=1
fi

if [[ $PY_TOUCHED -eq 1 ]]; then
    if command -v pytest >/dev/null 2>&1; then
        if (cd "$CONSORTIUM_TESTS_DIR" && pytest tests/ -v --timeout=30 -x) >"$TMP/pytest.log" 2>&1; then
            SUMMARY+="PASS  pytest"$'\n'
        else
            SUMMARY+="FAIL  pytest"$'\n'
            echo "=== FAIL: pytest ===" >&2
            tail -n 50 "$TMP/pytest.log" >&2
            FAIL=$((FAIL + 1))
        fi
    else
        SUMMARY+="SKIP  pytest (not installed)"$'\n'
    fi
fi

# Gate 5 (differential, conditional): perf bench dispatched on AR_TASK_TYPE.
# Standalone score.sh callers (e.g. CI smoke) leave AR_TASK_TYPE unset and
# skip this gate. Each perf task type has its own scoring script.
case "${AR_TASK_TYPE:-}" in
    perf-dag-executor)        PERF_SCRIPT="$(dirname "$0")/score-perf.sh" ;;
    perf-cascade-strategy)    PERF_SCRIPT="$(dirname "$0")/score-perf-cascade.sh" ;;
    *)                        PERF_SCRIPT="" ;;
esac
if [[ -n "$PERF_SCRIPT" ]]; then
    PERF_OUT=$(bash "$PERF_SCRIPT" "$WORKTREE" 2>"$TMP/perf.err")
    PERF_EXIT=$?
    if [[ "$PERF_EXIT" -eq 0 ]]; then
        SUMMARY+="${PERF_OUT}"$'\n'
    else
        SUMMARY+="FAIL  perf"$'\n'
        echo "=== FAIL: perf ===" >&2
        cat "$TMP/perf.err" >&2
        FAIL=$((FAIL + 1))
    fi
fi

echo "$SUMMARY"
echo "tests_passed=$BRANCH_TESTS clippy_errors=$BRANCH_CLIPPY"
exit "$FAIL"
