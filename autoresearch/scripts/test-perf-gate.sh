#!/usr/bin/env bash
# test-perf-gate.sh — threshold behaviour for perf-gate.sh.
#
# Usage: bash test-perf-gate.sh
#
# Uses synthetic criterion trees so the thresholds are pinned exactly and the
# suite stays fast (no bench build, no measurement noise). The end-to-end proof
# that a *real* ~10% regression is caught lives in the CI job and in
# test-perf-gate-e2e.sh.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
GATE="$HERE/perf-gate.sh"
TMP=$(mktemp -d -t perf-gate-test.XXXXXX)
trap 'rm -f "$TMP"/perf-gate-table.*; rm -rf "$TMP"' EXIT

PASS=0
FAIL=0

# mk_tree <dir> <metric-name> <ns>
# Lays out a criterion-shaped tree so perf-gate.sh's discovery path is exercised
# for real, not stubbed.
mk_tree() {
    local root="$1" metric="$2" ns="$3"
    # perf_cascade.bimodal_256.log2-fanout -> cascade_strategies_bimodal_256/log2-fanout
    local group param
    case "$metric" in
        perf_cascade.*)
            group="cascade_strategies_${metric#perf_cascade.}"; group="${group%.*}"
            param="${metric##*.}" ;;
        perf.dag_executor.*)
            group="dag_executor_${metric#perf.dag_executor.}"; group="${group%_ns}"
            param="" ;;
        *) group="${metric%%.*}"; param="${metric##*.}" ;;
    esac
    local d="$root/$group${param:+/$param}/new"
    mkdir -p "$d"
    printf '{"mean":{"point_estimate":%s}}\n' "$ns" > "$d/estimates.json"
}

# check <name> <expected-exit> <expected-verdict> <baseline-ns> <current-ns>
check() {
    local name="$1" want_exit="$2" want_verdict="$3" base_ns="$4" cur_ns="$5"
    local tree="$TMP/$name" base="$TMP/$name.baseline.json"
    rm -rf "$tree"; mkdir -p "$tree"
    mk_tree "$tree" perf_cascade.bimodal_256.log2-fanout "$cur_ns"
    jq -n --argjson b "$base_ns" \
        '{ref:"master", sha:"deadbeef", metrics:{"perf_cascade.bimodal_256.log2-fanout": $b}}' > "$base"

    local out rc
    out=$(bash "$GATE" --current "$tree" --baseline "$base" 2>&1)
    rc=$?

    local got_verdict
    got_verdict=$(printf '%s' "$out" | sed -n 's/.*verdict=\([a-z]*\).*/\1/p' | tail -1)

    if [[ "$rc" == "$want_exit" && "$got_verdict" == "$want_verdict" ]]; then
        printf 'ok   %-46s exit=%s verdict=%s\n' "$name" "$rc" "$got_verdict"
        PASS=$((PASS + 1))
    else
        printf 'FAIL %-46s exit=%s (want %s) verdict=%s (want %s)\n' \
            "$name" "$rc" "$want_exit" "${got_verdict:-none}" "$want_verdict"
        printf '%s\n' "$out" | sed 's/^/       | /'
        FAIL=$((FAIL + 1))
    fi
}

BASE=1000000

echo "# perf-gate threshold behaviour (synthetic, exact)"

# No change: inside the noise floor.
check identity            0 pass "$BASE" 1000000
# Small improvement: never penalized.
check big-improvement     0 pass "$BASE" 500000
# Inside the noise floor (2%) — reported, not a verdict.
check sub-noise           0 pass "$BASE" 1015000
# Between noise and soft: drift, still pass.
check drift-4pct          0 pass "$BASE" 1040000
# At/above soft (5%), below hard (10%): warn, build stays green.  <-- fail-soft
check soft-7pct           0 warn "$BASE" 1070000
# Just under hard: still warn, not fail.
check warn-9pct           0 warn "$BASE" 1090000
# At the hard threshold (10%): fail.  <-- fail-hard
check hard-10pct          1 fail "$BASE" 1100000
# Well past hard: fail.
check hard-25pct          1 fail "$BASE" 1250000
# One bad metric fails the whole gate even when the rest are clean.
echo "# single-metric isolation"
TREE="$TMP/multi"; BASE2="$TMP/multi.baseline.json"
rm -rf "$TREE"; mkdir -p "$TREE"
mk_tree "$TREE" perf_cascade.bimodal_256.log2-fanout 1000000
mk_tree "$TREE" perf_cascade.bimodal_256.steiner-greedy 1300000
jq -n '{ref:"master", sha:"deadbeef", metrics:{
        "perf_cascade.bimodal_256.log2-fanout": 1000000,
        "perf_cascade.bimodal_256.steiner-greedy": 1000000}}' > "$BASE2"
OUT=$(bash "$GATE" --current "$TREE" --baseline "$BASE2" 2>&1); RC=$?
if [[ "$RC" == 1 ]] && printf '%s' "$OUT" | grep -q 'steiner-greedy.*FAIL'; then
    printf 'ok   %-46s exit=1 verdict=fail\n' "one-regressed-metric-fails-gate"; PASS=$((PASS + 1))
else
    printf 'FAIL %-46s exit=%s\n' "one-regressed-metric-fails-gate" "$RC"
    printf '%s\n' "$OUT" | sed 's/^/       | /'; FAIL=$((FAIL + 1))
fi

# No signal must not read as green.
echo "# no-signal handling"
NOSIG="$TMP/nosig"; mkdir -p "$NOSIG"
BASE3="$TMP/nosig.baseline.json"
jq -n '{ref:"master", sha:"deadbeef", metrics:{"perf_cascade.bimodal_256.log2-fanout": 1000000}}' > "$BASE3"
OUT=$(bash "$GATE" --current "$NOSIG" --baseline "$BASE3" 2>&1); RC=$?
if [[ "$RC" == 3 ]]; then
    printf 'ok   %-46s exit=3 (not a pass)\n' "no-current-estimates-refuse-to-pass"; PASS=$((PASS + 1))
else
    printf 'FAIL %-46s exit=%s (want 3)\n' "no-current-estimates-refuse-to-pass" "$RC"
    printf '%s\n' "$OUT" | sed 's/^/       | /'; FAIL=$((FAIL + 1))
fi

OUT=$(bash "$GATE" --current "$NOSIG" --baseline "$TMP/does-not-exist.json" 2>&1); RC=$?
if [[ "$RC" == 2 ]]; then
    printf 'ok   %-46s exit=2 (usage)\n' "missing-baseline-is-usage-error"; PASS=$((PASS + 1))
else
    printf 'FAIL %-46s exit=%s (want 2)\n' "missing-baseline-is-usage-error" "$RC"; FAIL=$((FAIL + 1))
fi

# A metric with no baseline entry is surfaced, not silently dropped.
echo "# baseline coverage"
TREE4="$TMP/uncov"; mkdir -p "$TREE4"
mk_tree "$TREE4" perf_cascade.bimodal_256.log2-fanout 1000000
BASE4="$TMP/uncov.baseline.json"
jq -n '{ref:"master", sha:"deadbeef", metrics:{"perf_cascade.bimodal_256.steiner-greedy": 1000000}}' > "$BASE4"
OUT=$(bash "$GATE" --current "$TREE4" --baseline "$BASE4" 2>&1); RC=$?
if printf '%s' "$OUT" | grep -q 'baseline missing'; then
    printf 'ok   %-46s exit=%s surfaced\n' "unbaselined-metric-surfaced" "$RC"; PASS=$((PASS + 1))
else
    printf 'FAIL %-46s not surfaced\n' "unbaselined-metric-surfaced"
    printf '%s\n' "$OUT" | sed 's/^/       | /'; FAIL=$((FAIL + 1))
fi

echo
echo "passed=$PASS failed=$FAIL"
[[ "$FAIL" -eq 0 ]] || exit 1
