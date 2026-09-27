#!/usr/bin/env bash
# test-perf-gate-e2e.sh — acceptance proof for the perf gate.
#
# Usage: bash test-perf-gate-e2e.sh [--pct N] [--soft N] [--hard N]
#
# Runs the real cascade bench twice against the real gate:
#
#   1. clean  (CONSORTIUM_PERF_REGRESSION_PCT unset)  -> store as the baseline
#   2. regressed (CONSORTIUM_PERF_REGRESSION_PCT=$PCT) -> gate against it
#
# and asserts the gate fails. The regression is self-calibrating (see
# burn_regression_pct in the bench), so the injected slowdown really is ~PCT%
# of measured wall time on whatever host this runs on.
#
# This is the acceptance criterion from the issue: a deliberate ~10% regression
# in a key metric is caught.
#
# Cost: two bench runs. Pass --keep-criterion to reuse an existing
# target/criterion (skips run 1 — only valid if the current tree is clean).

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(git -C "$HERE" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

GATE="$HERE/perf-gate.sh"
BASELINE_SH="$HERE/perf-baseline.sh"

PCT=10
SOFT=5
HARD=10
STORE_DIR="$REPO_ROOT/autoresearch/perf-baselines"
EXPECT_VERDICT=fail
KEEP=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --pct)   PCT="$2"; shift 2 ;;
        --soft)  SOFT="$2"; shift 2 ;;
        --hard)  HARD="$2"; shift 2 ;;
        --store) STORE_DIR="$2"; shift 2 ;;
        --keep-criterion) KEEP=1; shift ;;
        *) echo "unknown arg $1" >&2; exit 2 ;;
    esac
done

command -v cargo >/dev/null 2>&1 || { echo "e2e: cargo not on PATH" >&2; exit 2; }
command -v jq    >/dev/null 2>&1 || { echo "e2e: jq not on PATH" >&2; exit 2; }

FILTER='^cascade_strategies/(uniform|bimodal)/256/'
echo "e2e: injecting a self-calibrating ~${PCT}% regression (soft=${SOFT}% hard=${HARD}%)"

# ── Run 1: clean baseline ──────────────────────────────────────────────────
if [[ "$KEEP" -eq 0 ]]; then
    rm -rf target/criterion
    echo "e2e: run 1/2 — clean baseline"
    if ! timeout 900 cargo bench -p consortium-fanout-sim --bench cascade_strategies -- \
            "$FILTER" --quick >/tmp/perf-e2e-clean.log 2>&1; then
        echo "e2e: clean bench run failed" >&2
        tail -n 40 /tmp/perf-e2e-clean.log >&2
        exit 1
    fi
else
    echo "e2e: run 1/2 — reusing existing target/criterion (--keep-criterion)"
fi

BASE_REF="e2e-baseline"
bash "$BASELINE_SH" store --ref "$BASE_REF" --sha "$(git rev-parse HEAD)" \
    --notes "e2e clean run" --est target/criterion --dir "$STORE_DIR" >/dev/null \
    || { echo "e2e: could not store baseline" >&2; exit 1; }
BASE_FILE="$STORE_DIR/refs/heads/${BASE_REF}.json"
echo "e2e: baseline -> $BASE_FILE"

# ── Run 2: regressed ───────────────────────────────────────────────────────
echo "e2e: run 2/2 — regressed"
rm -rf target/criterion
if ! CONSORTIUM_PERF_REGRESSION_PCT="$PCT" timeout 900 \
     cargo bench -p consortium-fanout-sim --bench cascade_strategies -- \
        "$FILTER" --quick >/tmp/perf-e2e-regressed.log 2>&1; then
    echo "e2e: regressed bench run failed" >&2
    tail -n 40 /tmp/perf-e2e-regressed.log >&2
    exit 1
fi

# ── Gate ───────────────────────────────────────────────────────────────────
echo
echo "e2e: gate verdict"
REPORT=$(bash "$GATE" --current target/criterion --baseline "$BASE_FILE" \
    --soft-pct "$SOFT" --hard-pct "$HARD" 2>/dev/null)
RC=$?
printf '%s\n' "$REPORT"

# How close did the injected regression land? The injector targets $PCT% of
# wall time, but criterion's own sampling error means the measured delta will
# not be exactly $PCT. Report the real deltas so the result is auditable.
echo
echo "e2e: measured deltas (target ~${PCT}%)"
printf '%s\n' "$REPORT" | grep -oE '[-+][0-9.]+% \|$' | tr -d ' |' | tr '\n' ' '
echo

echo
if [[ "$RC" == 1 ]]; then
    echo "e2e: PASS — the gate failed the build on a deliberate ~${PCT}% regression (exit 1)"
    exit 0
elif [[ "$RC" == 0 ]]; then
    echo "e2e: FAIL — the gate passed despite a deliberate ~${PCT}% regression (exit 0)" >&2
    exit 1
else
    echo "e2e: FAIL — gate returned $RC (expected 1); no-signal is not a catch" >&2
    exit 1
fi
