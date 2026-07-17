# Task template: upstream-sync

Used for porting one cea-hpc/clustershell upstream commit (or short range)
into both the Python `lib/` (in the sibling `consortium-tests` repo) and,
where applicable, the Rust port (`crates/`, this repo).

## Frontmatter

```yaml
type: upstream-sync
upstream_sha: <sha>
upstream_subject: <commit subject from upstream>
files_touched: [<list of lib/ paths the upstream commit changed, relative to the consortium-tests repo root>]
acceptance:
  - python lib/ files (consortium-tests repo) updated to match upstream semantics
  - pytest tests/ -v passes (run in the consortium-tests checkout)
  - if Rust port has equivalent code: ported (and `cargo nextest run` passes)
  - if Rust port differs structurally: a follow-up task is filed at
    autoresearch/queue/pending/parity-<sha>.task.toml
  - UPSTREAM_REF (at the consortium-tests root) bumped only when the full
    range is integrated (multi-task)
```

## Body

All Python paths below are relative to the sibling `consortium-tests`
repo (`../consortium-tests` next to the main checkout;
`$CONSORTIUM_TESTS_DIR` overrides). That repo carries an `upstream`
remote for cea-hpc/clustershell — `upstream-diff.sh` adds it on first
run and fetches. To see the diff:

```sh
git -C "$CONSORTIUM_TESTS_DIR" show <upstream_sha> -- lib/
```

Steps:
1. Apply the patch to `lib/` in the consortium-tests checkout — prefer
   `git -C "$CONSORTIUM_TESTS_DIR" cherry-pick -n -X theirs
   <upstream_sha>` if the touched paths haven't diverged, else apply
   manually. (Cherry-pick must run inside that repo; the two repos
   share no history with each other.)
2. Run `pytest tests/` in the consortium-tests checkout. Fix or skip
   mismatched tests one at a time. Don't blanket-skip.
3. For each `lib/` file changed (paths relative to consortium-tests),
   grep `crates/` in this repo for an analogous Rust module:
   - consortium-tests `lib/ClusterShell/RangeSet.py` ↔ `crates/consortium-crate/src/rangeset.rs`
   - consortium-tests `lib/ClusterShell/NodeSet.py`  ↔ `crates/consortium-crate/src/nodeset.rs`
   - consortium-tests `lib/ClusterShell/Task.py`     ↔ `crates/consortium-crate/src/task.rs`
   - consortium-tests `lib/ClusterShell/Worker/*.py` ↔ `crates/consortium-crate/src/worker*.rs`
   - consortium-tests `lib/ClusterShell/Gateway.py`  ↔ `crates/consortium-crate/src/gateway.rs`
4. If the Rust analogue exists and the upstream change is straightforward
   (bug fix, new edge case), port it. If structurally different (new
   feature, large refactor), file a follow-up parity task and stop.

Do **not** bump `UPSTREAM_REF` (at the consortium-tests root) in this
task unless the queue is empty of all `upstream-sync` tasks — that's a
separate concern.
