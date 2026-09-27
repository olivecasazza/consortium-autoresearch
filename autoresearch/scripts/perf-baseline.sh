#!/usr/bin/env bash
# perf-baseline.sh — store and retrieve criterion baselines per ref.
#
# Usage:
#   bash perf-baseline.sh store  --ref <ref> [--sha <sha>] [--notes <text>] --est <criterion-root>
#   bash perf-baseline.sh show   [--ref <ref>] [--dir <dir>]
#   bash perf-baseline.sh list   [--dir <dir>]
#   bash perf-baseline.sh resolve --ref <ref> [--dir <dir>] [--fallback-ref <ref>]
#
# Storage layout (committed to the repo so baselines are reviewable and survive
# CI artifact expiry; the store dir is .gitignore-ignorable for local use):
#
#   <dir>/refs/heads/<sanitized>.json
#   <dir>/refs/tags/<sanitized>.json
#
# Each file: { ref, ref_type, sha, measured_at, host, metrics: {name: ns} }
#
# Metric names match compute-baseline.sh / perf-gate.sh:
#   perf_cascade.<topology>_<nodes>.<strategy>
#   perf.<bench>.<variant>_ns
#
# `resolve` prints the path of the baseline to gate against: the exact ref if
# present, else the nearest named fallback (typically the base branch), else
# nothing (exit 3). Exit 3 is the caller's "no baseline" signal and is
# deliberately distinct from a perf failure.

set -uo pipefail

DEFAULT_DIR="autoresearch/perf-baselines"

CMD="${1:-}"; shift || true

REF=""; SHA=""; NOTES=""; EST=""; DIR="$DEFAULT_DIR"; FALLBACK=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --ref)           REF="${2:-}"; shift 2 ;;
        --sha)           SHA="${2:-}"; shift 2 ;;
        --notes)         NOTES="${2:-}"; shift 2 ;;
        --est)           EST="${2:-}"; shift 2 ;;
        --dir)           DIR="${2:-}"; shift 2 ;;
        --fallback-ref)  FALLBACK="${2:-}"; shift 2 ;;
        *) echo "perf-baseline: unknown arg $1" >&2; exit 2 ;;
    esac
done

# Sanitize a ref into a filesystem-safe name: slashes become __ so that
# feat/foo -> feat__foo, keeping the mapping reversible and readable.
sanitize() { printf '%s' "${1//\//__}"; }

classify() {
    case "$1" in
        refs/tags/*) printf 'tags' ;;
        *)           printf 'heads' ;;
    esac
}

rel_path() {
    local ref="$1"
    printf '%s/refs/%s/%s.json' "$DIR" "$(classify "$ref")" "$(sanitize "$ref")"
}

cmd_store() {
    [[ -n "$REF"  ]] || { echo "perf-baseline: store needs --ref" >&2; exit 2; }
    [[ -n "$EST"  ]] || { echo "perf-baseline: store needs --est <criterion-root>" >&2; exit 2; }
    [[ -d "$EST" ]] || { echo "perf-baseline: --est $EST is not a directory" >&2; exit 2; }
    command -v jq >/dev/null 2>&1 || { echo "perf-baseline: jq required" >&2; exit 2; }

    local out; out="$(rel_path "$REF")"
    mkdir -p "$(dirname "$out")"

    local metrics; metrics=$(mktemp)
    local est metric ns
    while IFS= read -r est; do
        rel="${est#"$EST"/}"; rel="${rel%/new/estimates.json}"
        group="${rel%%/*}"; param="${rel#*/}"
        # criterion 0.5 flattens the benchmark group's slashes into the
        # directory name, so the group arrives as cascade_strategies_bimodal_256
        # (not cascade_strategies/bimodal/256). Underscores inside the scenario
        # name are preserved, so this split is unambiguous for our bench names.
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
        printf '%s\t%s\n' "$metric" "$ns" >> "$metrics"
    done < <(find "$EST" -type f -path '*/new/estimates.json' 2>/dev/null | sort)

    local n; n=$(wc -l < "$metrics" | tr -d ' ')
    if [[ "$n" -eq 0 ]]; then
        rm -f "$metrics"
        echo "perf-baseline: refusing to store an empty baseline for $REF (no estimates under $EST)" >&2
        exit 3
    fi

    # Build the whole document in one jq pass: the metrics arrive as a TSV of
    # name<TAB>ns and become a JSON object. Emitting the envelope by hand and
    # splicing the object in is how you end up shipping invalid JSON to a gate
    # that must not fail on parsing.
    jq -Rn --arg ref "$REF" \
          --arg ref_type "$(classify "$REF")" \
          --arg sha "${SHA:-$(git rev-parse HEAD 2>/dev/null || echo unknown)}" \
          --arg measured_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
          --arg host "$(uname -srm)" \
          --arg notes "$NOTES" \
          --argjson n "$n" '
        [inputs | select(length > 0) | split("\t")
         | {(.[0]): (.[1] | tonumber | round)}] | add // {}
      | {ref: $ref, ref_type: $ref_type, sha: $sha,
         measured_at: $measured_at, host: $host,
         metric_count: $n, notes: $notes, metrics: .}
    ' "$metrics" > "$out"
    rm -f "$metrics"

    echo "perf-baseline: stored $n metrics for $REF -> $out" >&2
    cat "$out"
}

cmd_show() {
    local p; p="$(rel_path "${REF:-HEAD}")"
    [[ -f "$p" ]] || { echo "perf-baseline: no baseline for ${REF:-HEAD} (looked in $p)" >&2; exit 3; }
    cat "$p"
}

cmd_list() {
    if [[ -d "$DIR/refs" ]]; then
        find "$DIR/refs" -name '*.json' -type f | sort | while read -r f; do
            printf '%s\n' "$(jq -r '"\(.ref)\t\(.sha // "?")[0:12]\t\(.measured_at // "?")\t\(.metric_count // 0) metrics\t\(.ref_type)"' "$f" 2>/dev/null)"
        done
    else
        echo "perf-baseline: no store at $DIR" >&2
        exit 3
    fi
}

cmd_resolve() {
    [[ -n "$REF" ]] || { echo "perf-baseline: resolve needs --ref" >&2; exit 2; }
    local exact; exact="$(rel_path "$REF")"
    if [[ -f "$exact" ]]; then printf '%s\n' "$exact"; exit 0; fi
    if [[ -n "$FALLBACK" ]]; then
        local fb; fb="$(rel_path "$FALLBACK")"
        if [[ -f "$fb" ]]; then
            echo "perf-baseline: no baseline for $REF, falling back to $FALLBACK" >&2
            printf '%s\n' "$fb"; exit 0
        fi
    fi
    echo "perf-baseline: no baseline for $REF${FALLBACK:+ (or fallback $FALLBACK)}" >&2
    exit 3
}

case "$CMD" in
    store)   cmd_store ;;
    show)    cmd_show ;;
    list)    cmd_list ;;
    resolve) cmd_resolve ;;
    *) echo "usage: perf-baseline.sh {store|show|list|resolve} [options]" >&2; exit 2 ;;
esac
