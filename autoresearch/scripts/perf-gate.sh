#!/usr/bin/env bash
# perf-gate.sh — threshold gate over criterion estimates for a PR run.
#
# Usage:
#   bash perf-gate.sh --current <estimates-root> --baseline <perf-baseline.json> [options]
#
# --current <dir>   Directory containing criterion output, i.e. the parent of
#                   `cascade_strategies/...` (normally "$WORKTREE/target/criterion").
# --baseline <file> A stored perf baseline (see perf-baseline.sh). JSON shape:
#                   { "ref": "...", "sha": "...", "metrics": { "<metric>": <ns> } }
#
# Metric naming is the same dotted form compute-baseline.sh emits:
#   perf_cascade.<topology>_<nodes>.<strategy>          e.g. perf_cascade.bimodal_256.log2-fanout
#   perf.<bench>.<variant>_ns                           e.g. perf.dag_executor.flat_33_ns
# Lower is better for all of them (nanoseconds).
#
# Thresholds (percent, higher-vs-baseline):
#   --soft-pct N   default 5   regression between soft and hard => WARN (comment, exit 0)
#   --hard-pct N   default 10  regression at/over hard      => FAIL (exit 1)
#   --noise-pct N  default 2   changes smaller than this are reported as noise, not a verdict
#
# Improvement beyond --improve-pct (default 5) is reported as an improvement and
# never penalized.
#
# Output:
#   stdout  a Markdown table (one row per metric) suitable for a PR comment body
#   stderr  a short human summary of the verdict
#   $GITHUB_STEP_SUMMARY, when set, gets the same table
#
# Exit codes:
#   0  pass, or warn-only (soft threshold exceeded, comment posted, build stays green)
#   1  fail (hard threshold exceeded)
#   2  usage error
#   3  no signal — neither baseline nor current estimates usable
#
# A "no signal" exit is deliberately *not* a pass: score.sh refuses to PASS
# perf gates with no measurement, so a silently-dead bench cannot fake green.

set -uo pipefail

CURRENT_DIR=""
BASELINE_FILE=""
SOFT_PCT=5
HARD_PCT=10
NOISE_PCT=2
IMPROVE_PCT=5
REPORT_ONLY=0

usage() {
    sed -n '2,40p' "$0" >&2
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --current)     CURRENT_DIR="${2:-}"; shift 2 ;;
        --baseline)    BASELINE_FILE="${2:-}"; shift 2 ;;
        --soft-pct)    SOFT_PCT="${2:-}"; shift 2 ;;
        --hard-pct)    HARD_PCT="${2:-}"; shift 2 ;;
        --noise-pct)   NOISE_PCT="${2:-}"; shift 2 ;;
        --improve-pct) IMPROVE_PCT="${2:-}"; shift 2 ;;
        # Emit the table but never change the exit code. Used by the
        # acceptance harness to show the verdict without failing the run.
        --report-only) REPORT_ONLY=1; shift ;;
        -h|--help)     usage ;;
        *) echo "perf-gate: unknown arg $1" >&2; usage ;;
    esac
done

[[ -n "$CURRENT_DIR" && -n "$BASELINE_FILE" ]] || usage
command -v jq >/dev/null 2>&1 || { echo "perf-gate: jq required" >&2; exit 2; }
[[ -d "$CURRENT_DIR" ]] || { echo "perf-gate: --current $CURRENT_DIR is not a directory" >&2; exit 2; }
[[ -f "$BASELINE_FILE" ]] || { echo "perf-gate: --baseline $BASELINE_FILE missing" >&2; exit 2; }

# ── Collect current estimates ──────────────────────────────────────────────
# Every <group path>/<param>/new/estimates.json becomes one metric. The group
# path relative to --current is joined with the parameter, which reproduces the
# names compute-baseline.sh already emits:
#   cascade_strategies/bimodal/256/log2-fanout/new/estimates.json
#     -> perf_cascade.bimodal_256.log2-fanout
CURRENT_METRICS=$(mktemp)
trap 'rm -f "$CURRENT_METRICS" "$BASE_METRICS" "${TMP_ROWS:-}"' EXIT

while IFS= read -r est; do
    rel="${est#"$CURRENT_DIR"/}"                       # …/new/estimates.json
    rel="${rel%/new/estimates.json}"                   # strip the suffix
    group="${rel%%/*}"                                 # first path segment
    param="${rel#*/}"                                 # the strategy name
    # criterion 0.5 flattens the benchmark group's slashes into the directory
    # name, so the group arrives as cascade_strategies_bimodal_256 rather than
    # cascade_strategies/bimodal/256. Underscores inside the scenario name are
    # preserved, so this split is unambiguous for our bench names.
    case "$group" in
        cascade_strategies_*)
            metric="perf_cascade.${group#cascade_strategies_}.${param}" ;;
        dag_executor_*)
            metric="perf.dag_executor.${group#dag_executor_}_ns" ;;
        *)
            metric="perf.${group}.${param}_ns" ;;
    esac
    ns=$(jq -r '.mean.point_estimate // empty' "$est" 2>/dev/null || true)
    [[ -n "$ns" && "$ns" != "null" ]] || continue
    printf '%s\t%s\n' "$metric" "$ns" >> "$CURRENT_METRICS"
done < <(find "$CURRENT_DIR" -type f -path '*/new/estimates.json' 2>/dev/null | sort)

# ── Load baseline ──────────────────────────────────────────────────────────
BASE_METRICS=$(mktemp)
jq -r '(.metrics // {}) | to_entries[] | "\(.key)\t\(.value)"' "$BASELINE_FILE" 2>/dev/null \
    | sort > "$BASE_METRICS" || true

N_CUR=$(wc -l < "$CURRENT_METRICS" | tr -d ' ')
N_BASE=$(wc -l < "$BASE_METRICS" | tr -d ' ')

if [[ "$N_CUR" -eq 0 || "$N_BASE" -eq 0 ]]; then
    echo "perf-gate: no signal (current=$N_CUR metrics, baseline=$N_BASE metrics)." >&2
    echo "  baseline=$BASELINE_FILE" >&2
    echo "  current =$CURRENT_DIR" >&2
    echo "  A perf gate with no measurement is not a pass — refusing to report green." >&2
    echo "| perf gate | no signal |" >&2
    echo "| --- | --- |" >&2
    echo "| current metrics | $N_CUR |" >&2
    echo "| baseline metrics | $N_BASE |" >&2
    exit 3
fi

# ── Compare ────────────────────────────────────────────────────────────────
# verdict: pass | warn | fail  (worst wins)
VERDICT=pass
ROWS=$(mktemp)
TMP_ROWS="$ROWS"

while IFS=$'\t' read -r metric ns; do
    base=$(awk -F'\t' -v m="$metric" '$1==m{print $2; exit}' "$BASE_METRICS")
    if [[ -z "$base" || "$base" == "0" ]]; then
        printf '| `%s` | new | %s ns | — | baseline missing |\n' "$metric" "$ns" >> "$ROWS"
        continue
    fi
    IFS='|' read -r status delta base_fmt cur_fmt <<<"$(awk -v b="$base" -v c="$ns" \
        -v soft="$SOFT_PCT" -v hard="$HARD_PCT" -v noise="$NOISE_PCT" \
        -v improve="$IMPROVE_PCT" '
        BEGIN {
            delta = (c - b) / b * 100
            if (delta <= -improve)              s = "improved"
            else if (delta >= hard)             s = "fail"
            else if (delta >= soft)             s = "warn"
            else if (delta >  noise)            s = "drift"
            else if (delta <  -noise)           s = "improved"
            else                                s = "noise"
            printf "%s|%.1f|%.0f|%.0f", s, delta, b, c
        }')"
    case "$status" in
        fail)     [[ "$VERDICT" != fail ]] && VERDICT=fail ;;
        warn)     [[ "$VERDICT" == pass ]] && VERDICT=warn ;;
    esac
    icon=""
    case "$status" in
        fail)      icon="🔴 **FAIL**" ;;
        warn)      icon="🟡 **WARN**" ;;
        improved)  icon="🟢 faster" ;;
        drift)     icon="⚪ drift" ;;
        *)         icon="⚪ noise" ;;
    esac
    printf '| `%s` | %s | %s ns | %s ns | %+.1f%% |\n' \
        "$metric" "$icon" "$base_fmt" "$cur_fmt" "$delta" >> "$ROWS"
done < "$CURRENT_METRICS"

BASELINE_REF=$(jq -r '.ref // "unknown"' "$BASELINE_FILE" 2>/dev/null)
BASELINE_SHA=$(jq -r '.sha // "unknown"' "$BASELINE_FILE" 2>/dev/null)

{
    echo "| metric | verdict | baseline | current | delta |"
    echo "| --- | --- | ---: | ---: | ---: |"
    cat "$ROWS"
} > /tmp/perf-gate-table.$$
TABLE_FILE=/tmp/perf-gate-table.$$

case "$VERDICT" in
    pass)
        HEADLINE="✅ **perf gate: pass** — no key metric regressed more than ${SOFT_PCT}% against \`${BASELINE_REF}\`."
        ;;
    warn)
        HEADLINE="🟡 **perf gate: warn** — regression between ${SOFT_PCT}% and ${HARD_PCT}% against \`${BASELINE_REF}\`. Failing the build for a few percent is how you teach everyone to ignore perf gates."
        ;;
    fail)
        HEADLINE="🔴 **perf gate: fail** — regression at or above the ${HARD_PCT}% hard threshold against \`${BASELINE_REF}\`."
        ;;
esac

REPORT="$(cat <<EOF
${HEADLINE}

Baseline: \`${BASELINE_REF}\` @ \`${BASELINE_SHA:0:12}\`
Soft threshold: ${SOFT_PCT}% (comment, build stays green) · hard threshold: ${HARD_PCT}% (build fails) · noise floor: ${NOISE_PCT}%

$(cat "$TABLE_FILE")
EOF
)"

printf '%s\n' "$REPORT"

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    printf '%s\n' "$REPORT" >> "$GITHUB_STEP_SUMMARY"
fi

echo "perf-gate: verdict=$VERDICT soft=${SOFT_PCT}% hard=${HARD_PCT}% (baseline $BASELINE_REF@${BASELINE_SHA:0:12})" >&2

[[ "$REPORT_ONLY" -eq 1 ]] && exit 0
[[ "$VERDICT" == fail ]] && exit 1
exit 0
