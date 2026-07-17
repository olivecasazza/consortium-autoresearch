#!/usr/bin/env bash
# upstream-diff.sh — fetch cea-hpc/clustershell and emit one upstream-sync
# task per upstream commit not yet integrated.
#
# Usage: bash upstream-diff.sh [--limit N] [--dry-run]
#
# UPSTREAM_REF (at the consortium-tests repo root) holds the
# last-integrated tag/sha. Since the test-infrastructure split, lib/ and
# UPSTREAM_REF live in the sibling consortium-tests repo — a standalone
# repo with no shared history with cea-hpc/clustershell, so it carries
# its own `upstream` remote (added on first run below). We diff
# UPSTREAM_REF..upstream/master there, restrict to lib/ paths, and
# create one task per commit. Multi-task commits are kept atomic — the
# agent will either port the whole commit or abandon it.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
CONSORTIUM_TESTS_DIR="${CONSORTIUM_TESTS_DIR:-$REPO_ROOT/../consortium-tests}"

LIMIT=20
DRY_RUN=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --limit) LIMIT="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ ! -d "$CONSORTIUM_TESTS_DIR/.git" ]]; then
    echo "consortium-tests repo not found at $CONSORTIUM_TESTS_DIR — set CONSORTIUM_TESTS_DIR" >&2
    exit 3
fi

UPSTREAM_REF_FILE="$CONSORTIUM_TESTS_DIR/UPSTREAM_REF"
if [[ ! -f "$UPSTREAM_REF_FILE" ]]; then
    echo "UPSTREAM_REF file missing — cannot compute diff" >&2
    exit 3
fi
LAST=$(tr -d '[:space:]' < "$UPSTREAM_REF_FILE")

# Ensure the upstream remote exists in the consortium-tests repo (it was
# created fresh — no shared clone history — so the first run adds it).
if ! git -C "$CONSORTIUM_TESTS_DIR" remote get-url upstream >/dev/null 2>&1; then
    git -C "$CONSORTIUM_TESTS_DIR" remote add upstream https://github.com/cea-hpc/clustershell.git
fi

git -C "$CONSORTIUM_TESTS_DIR" fetch upstream --tags --quiet

# Resolve LAST to a sha we can diff from. If it's a tag, use refs/tags/<X>.
# UPSTREAM_REF names an upstream commit, which the fetch above provides.
BASE_SHA=$(git -C "$CONSORTIUM_TESTS_DIR" rev-parse "refs/tags/$LAST" 2>/dev/null \
    || git -C "$CONSORTIUM_TESTS_DIR" rev-parse "$LAST" 2>/dev/null \
    || true)
if [[ -z "$BASE_SHA" ]]; then
    echo "cannot resolve UPSTREAM_REF=$LAST in upstream/" >&2
    exit 5
fi

QUEUE="$REPO_ROOT/autoresearch/queue/pending"
mkdir -p "$QUEUE"

KNOWN_IDS_FILE=$(mktemp)
trap 'rm -f "$KNOWN_IDS_FILE"' EXIT
find "$REPO_ROOT/autoresearch/queue" -maxdepth 2 -name '*.task.toml' -printf '%f\n' \
    | sed 's/\.task\.toml$//' > "$KNOWN_IDS_FILE"

count=0
while IFS= read -r sha; do
    [[ -z "$sha" ]] && continue
    id="upstream-${sha:0:8}"
    if grep -Fxq "$id" "$KNOWN_IDS_FILE"; then continue; fi

    subject=$(git -C "$CONSORTIUM_TESTS_DIR" log -1 --format='%s' "$sha")
    files=$(git -C "$CONSORTIUM_TESTS_DIR" show --name-only --format= "$sha" -- 'lib/*' 2>/dev/null | grep -v '^$' | head -20)
    [[ -z "$files" ]] && continue  # commit didn't touch lib/

    out="$QUEUE/$id.task.toml"
    if [[ $DRY_RUN -eq 1 ]]; then
        echo "DRY: $out  ($subject)"
    else
        {
            printf 'type = "upstream-sync"\n'
            printf 'repo = "consortium-tests"\n'
            printf 'upstream_sha = "%s"\n' "$sha"
            printf 'upstream_subject = %s\n' "$(printf '%s' "$subject" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read().rstrip()))' 2>/dev/null || printf '"%s"' "${subject//\"/\\\"}")"
            printf 'files_touched = [\n'
            while IFS= read -r f; do
                printf '  "%s",\n' "$f"
            done <<< "$files"
            printf ']\n'
            printf 'created = "%s"\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        } > "$out"
        echo "wrote $out"
    fi
    count=$((count + 1))
    [[ $count -ge $LIMIT ]] && break
done < <(git -C "$CONSORTIUM_TESTS_DIR" log --format='%H' --reverse "$BASE_SHA..upstream/master" -- 'lib/*' 2>/dev/null)

echo
echo "emitted $count upstream-sync tasks (LIMIT=$LIMIT)"
