# Task template: port-python-fixme

Used for `FIXME` and `XXX` comments in the legacy Python `lib/` (which
lives in the sibling `consortium-tests` repo).

## Frontmatter

```yaml
type: port-python-fixme
target_file: lib/ClusterShell/<file>.py   # relative to the consortium-tests repo root
target_line: <N>
description: <FIXME comment text verbatim>
acceptance:
  - FIXME removed (because fixed) or rewritten as a clear NOTE if not-a-bug
  - if fixed: regression test added in tests/ (consortium-tests)
  - pytest tests/ -v --timeout=30 still passes (run in consortium-tests)
```

## Body

All Python paths below are in the sibling `consortium-tests` repo
(`../consortium-tests` next to the main checkout; `$CONSORTIUM_TESTS_DIR`
overrides) — edit and run pytest there, not in this repo's worktree.

These are legacy Python issues. Three valid resolutions:

1. **Real bug, fix it.** Patch the Python, add a regression test in
   `tests/<X>Test.py`, remove the FIXME line.
2. **Stale FIXME.** If reading the surrounding code shows the issue
   no longer applies, remove the FIXME and explain in the commit
   body. Don't replace with a non-actionable NOTE.
3. **Out of scope.** Abandon with `scope: needs-design`. Do not
   speculate.

This task type does not require Rust changes. The Rust port may
already handle the case correctly — that's not your problem here.
