#!/usr/bin/env bash
# test-perf-gate-e2e.sh — acceptance proof for the perf gate.
#
# Usage: bash test-perf-gate-e2e.sh [--phase all|baseline|gate] [options]
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
# ── Why --phase exists ───────────────────────────────────────────────────────
# Each phase is one `cargo bench`, and two of them do not reliably fit inside a
# single agent heartbeat run cap. Running the whole thing in one go is what
# killed the first attempt at this acceptance (see CON-21). Split it:
#
#   bash test-perf-gate-e2e.sh --phase baseline   # one bench run, stores baseline
#   bash test-perf-gate-e2e.sh --phase gate       # one bench run, asserts the fail
#
# Both phases write into --store, which defaults to the repo's own
# autoresearch/perf-baselines so phase gate can find phase baseline's output
# even across separate runs and separate worktrees.
#
# Cost: one bench run per phase. --keep-criterion reuses an existing
# target/criterion instead of running the bench (only valid if the current tree
# is clean and the estimates on disk are the ones you want).

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(git -C "$HERE" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

GATE="$HERE/perf-gate.sh"
BASELINE_SH="$HERE/perf-baseline.sh"

PCT=10
SOFT=5
HARD=10
PHASE=all
STORE_DIR="$REPO_ROOT/autoresearch/perf-baselines"
BASE_REF="e2e-baseline"
EXPECT_VERDICT=fail
KEEP=0
BENCH_TIMEOUT="${PERF_E2E_BENCH_TIMEOUT:-900}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --pct)   PCT="$2"; shift 2 ;;
        --soft)  SOFT="$2"; shift 2 ;;
        --hard)  HARD="$2"; shift 2 ;;
        --phase) PHASE="$2"; shift 2 ;;
        --base-ref) BASE_REF="$2"; shift 2 ;;
        --store) STORE_DIR="$2"; shift 2 ;;
        --keep-criterion) KEEP=1; shift ;;
        *) echo "unknown arg $1" >&2; exit 2 ;;
    esac
done

case "$PHASE" in
    all|baseline|gate) ;;
    *) echo "e2e: --phase must be all|baseline|gate (got '$PHASE')" >&2; exit 2 ;;
esac

command -v cargo >/dev/null 2>&1 || { echo "e2e: cargo not on PATH" >&2; exit 2; }
command -v jq    >/dev/null 2>&1 || { echo "e2e: jq not on PATH" >&2; exit 2; }

FILTER='^cascade_strategies/(uniform|bimodal)/256/'
BASE_FILE="$STORE_DIR/refs/heads/${BASE_REF}.json"

run_bench() {
    local label="$1" pct="$2"
    echo "e2e: cargo bench ($label) — timeout ${BENCH_TIMEOUT}s"
    if [[ -n "$pct" ]]; then
        CONSORTIUM_PERF_REGRESSION_PCT="$pct" timeout "$BENCH_TIMEOUT" \
            cargo bench -p consortium-fanout-sim --bench cascade_strategies -- \
                "$FILTER" --quick >"/tmp/perf-e2e-$label.log" 2>&1
    else
        timeout "$BENCH_TIMEOUT" \
            cargo bench -p consortium-fanout-sim --bench cascade_strategies -- \
                "$FILTER" --quick >"/tmp/perf-e2e-$label.log" 2>&1
    fi
    local rc=$?
    if [[ $rc -ne 0 ]]; then
        echo "e2e: $label bench run failed (exit $rc)" >&2
        tail -n 40 "/tmp/perf-e2e-$label.log" >&2
        return 1
    fi
    return 0
}

# ── Phase: baseline ─────────────────────────────────────────────────────────
if [[ "$PHASE" == all || "$PHASE" == baseline ]]; then
    echo "e2e: phase baseline — capturing a clean reference for $BASE_REF"
    if [[ "$KEEP" -eq 0 ]]; then
        rm -rf target/criterion
        run_bench clean "" || exit 1
    else
        echo "e2e:   reusing existing target/criterion (--keep-criterion)"
    fi

    bash "$BASELINE_SH" store --ref "$BASE_REF" --sha "$(git rev-parse HEAD)" \
        --notes "e2e clean run" --est target/criterion --dir "$STORE_DIR" >/dev/null \
        || { echo "e2e: could not store baseline" >&2; exit 1; }
    echo "e2e: baseline -> $BASE_FILE"
    [[ -f "$BASE_FILE" ]] || { echo "e2e: baseline file missing after store" >&2; exit 1; }
    echo "e2e: phase baseline OK — now run: bash $(basename "$0") --phase gate"
fi

if [[ "$PHASE" == baseline ]]; then
    echo "e2e: PASS (phase baseline complete; gate verdict not yet evaluated)"
    exit 0
fi

# ── Phase: gate ─────────────────────────────────────────────────────────────
if [[ ! -f "$BASE_FILE" ]]; then
    echo "e2e: no baseline at $BASE_FILE" >&2
    echo "  run 'bash $(basename "$0") --phase baseline' first" >&2
    exit 1
fi

echo
echo "e2e: phase gate — injecting a self-calibrating ~${PCT}% regression (soft=${SOFT}% hard=${HARD}%)"
if [[ "$KEEP" -eq 0 ]]; then
    rm -rf target/criterion
    run_bench regressed "$PCT" || exit 1
else
    echo "e2e:   reusing existing target/criterion (--keep-criterion)"
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
