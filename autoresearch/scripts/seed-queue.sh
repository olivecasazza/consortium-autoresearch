#!/usr/bin/env bash
# seed-queue.sh — walk the repo and emit one task file per pending item.
#
# Usage: bash seed-queue.sh [--dry-run]
#
# Sources:
#   - TODO comments in crates/consortium-nix/src/*.rs              → nix-parallelize
#   - todo!() / unimplemented!() in crates/*/src/**/*.rs            → resolve-rust-todo
#   - FIXME / XXX comments in consortium-tests lib/ClusterShell/**/*.py  → port-python-fixme
#   - test functions in consortium-tests tests/*Test.py without a Rust
#     analogue                                                          → port-python-test (sampled)
#
# Idempotent: if a task file with the same id already exists in any
# queue/ subdir (pending, in-progress, done, abandoned), skip it.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"
CONSORTIUM_TESTS_DIR="${CONSORTIUM_TESTS_DIR:-$REPO_ROOT/../consortium-tests}"

QUEUE="$REPO_ROOT/autoresearch/queue"
PENDING="$QUEUE/pending"
mkdir -p "$PENDING"

DRY_RUN=0
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=1

# Already-known task ids (any state). Used to dedupe.
known_ids() {
    find "$QUEUE" -maxdepth 2 -name '*.task.toml' -type f -printf '%f\n' 2>/dev/null \
        | sed 's/\.task\.toml$//'
}
KNOWN_IDS_FILE=$(mktemp)
trap 'rm -f "$KNOWN_IDS_FILE"' EXIT
known_ids > "$KNOWN_IDS_FILE"

emit() {
    local id="$1" type="$2" file="$3" line="$4" desc="$5" extra="${6:-}"
    if grep -Fxq "$id" "$KNOWN_IDS_FILE"; then
        return 0
    fi
    local out="$PENDING/$id.task.toml"
    if [[ $DRY_RUN -eq 1 ]]; then
        echo "DRY: $out"
        return 0
    fi
    {
        printf 'type = "%s"\n' "$type"
        printf 'target_file = "%s"\n' "$file"
        printf 'target_line = %s\n' "$line"
        printf 'description = "%s"\n' "${desc//\"/\\\"}"
        printf 'created = "%s"\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        [[ -n "$extra" ]] && printf '%s\n' "$extra"
    } > "$out"
    echo "wrote $out"
}

# 1. consortium-nix TODOs (high signal — these are the explicit asks).
while IFS=: read -r file line rest; do
    [[ -z "$file" ]] && continue
    desc=$(echo "$rest" | sed -E 's/^[[:space:]]*\/\/[[:space:]]*//; s/^TODO[[:space:]]*:?//I; s/^[[:space:]]+//; s/[[:space:]]+$//')
    relfile="${file#./}"
    sha=$(echo "$relfile:$line" | sha1sum | cut -c1-8)
    id="nixpar-$sha"
    emit "$id" "nix-parallelize" "$relfile" "$line" "$desc"
done < <(grep -rn -E '^\s*//\s*TODO' crates/consortium-nix/src/ 2>/dev/null || true)

# 2. todo!() / unimplemented!() across crates.
while IFS=: read -r file line rest; do
    [[ -z "$file" ]] && continue
    macro=$(echo "$rest" | grep -oE '(todo|unimplemented)!\(\)' | head -1)
    [[ -z "$macro" ]] && continue
    relfile="${file#./}"
    sha=$(echo "$relfile:$line" | sha1sum | cut -c1-8)
    id="rusttodo-$sha"
    emit "$id" "resolve-rust-todo" "$relfile" "$line" "$macro at $relfile:$line" "macro = \"$macro\""
done < <(grep -rn -E '\b(todo|unimplemented)!\(\)' crates/ --include='*.rs' 2>/dev/null || true)

# 3. lib/ FIXMEs and XXX. The Python oracle lives in the sibling
# consortium-tests repo; target_file is recorded relative to its root.
if [[ -d "$CONSORTIUM_TESTS_DIR/lib" ]]; then
    while IFS=: read -r file line rest; do
        [[ -z "$file" ]] && continue
        desc=$(echo "$rest" | sed -E 's/^[[:space:]]*#[[:space:]]*//; s/^[[:space:]]+//; s/[[:space:]]+$//')
        relfile="${file#./}"
        sha=$(echo "$relfile:$line" | sha1sum | cut -c1-8)
        id="pyfix-$sha"
        emit "$id" "port-python-fixme" "$relfile" "$line" "$desc" 'repo = "consortium-tests"'
    done < <(cd "$CONSORTIUM_TESTS_DIR" && grep -rn -E '#\s*(FIXME|XXX)' lib/ 2>/dev/null || true)
else
    echo "WARN: $CONSORTIUM_TESTS_DIR/lib not found — skipping lib/ FIXME seeding" >&2
fi

echo
echo "queue/pending/ now contains $(find "$PENDING" -name '*.task.toml' | wc -l) tasks"
