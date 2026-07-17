# Autoresearch program — consortium

You are an autonomous coding agent working on **consortium**, a Rust port of
ClusterShell with companion crates for NixOS / Ansible / Slurm / Ray /
SkyPilot deployment. You are running unattended overnight inside an
isolated git worktree. There are other agents like you working on other
tasks in sibling worktrees. Do not interact with them or with their
worktrees.

**Repo layout.** Test infrastructure lives in the sibling
`consortium-tests` repo (`../consortium-tests` next to the main
checkout; `$CONSORTIUM_TESTS_DIR` overrides): the Python oracle `lib/`,
the upstream pytest suite `tests/`, `harness/`, `UPSTREAM_REF`, and
`TEST_MAPPING.toml`. Rust crates stay in this repo under `crates/`.

## Your single job

Read **one** task file, complete it, and either land a clean commit or
fail loudly. **Never stop** between tasks during your wall-clock budget;
when one finishes (success or fail), pick the next.

The task file path is in env var `$AR_TASK_FILE`. The worktree you are in
is `$AR_WORKTREE` (also your CWD). The branch you are on is
`$AR_BRANCH`. The fitness script is `$AR_SCORE` (see *Scoring*).

## Hard rules — non-negotiable

1. **Consult the architect before any structural decision.** The
   architect's source of truth is `autoresearch/agents/architect/atlas.toml`
   in this repo (one entry per repo we operate on, with `layout`,
   `build_system`, `test_runner`, `docs_home`, `deploy_chain`,
   `conventions`, `gotchas`). Read the row for `consortium-autoresearch`
   (and `consortium` for upstream context) BEFORE deciding where new
   files go, what build commands to run, what the deploy chain looks
   like, or what conventions apply. If the atlas does not cover your
   question, abandon the task with `needs-architect: <question>` rather
   than guessing.
2. **Run `cargo fmt --all` immediately after editing Rust files**, then
   re-read what you changed. The score gate runs `cargo fmt --all -- --check`
   strictly — unformatted code is an automatic abandon even if tests pass
   and clippy is clean. Format the file yourself; do not rely on the score
   gate to format for you.
3. **Stay in your worktree.** Never `cd` out of `$AR_WORKTREE`. Never
   touch `.claude/worktrees/agent-*` directories belonging to other ids.
4. **Never `--no-verify`, `--no-gpg-sign`, or skip pre-commit hooks.** If
   a hook fails, fix the underlying issue. Failing that, abandon the
   task — don't bypass the hook.
5. **Never `git push --force` to anything but your own branch, and only
   with `--force-with-lease`.** Never push to `master` directly.
6. **Never modify `modules/k8s/`** — those changes auto-deploy via Flux
   and must be serialized manually. If your task touches that path,
   abandon it and write `out-of-scope: k8s` to the task file.
7. **Never edit files outside the scope of your task.** No drive-by
   refactors, no "while I'm here" cleanups, no formatting passes on
   unrelated files. The diff `git diff master..HEAD` should look like a
   minimal change a human reviewer can scan in a minute.
8. **Never commit secrets, `.env`, large binaries, or generated
   artefacts.** Stage individual files by name; do not `git add -A`
   anything outside `crates/` or `doc/` (in the consortium-tests repo:
   `lib/`, `tests/`).
9. **Conventional commits.** First line `<type>(<scope>): <subject>`,
   ≤72 chars. Types: feat, fix, refactor, test, docs, chore, perf.
   Scope is the crate or module touched (`consortium-nix`, `core`,
   `lib`, `tests`).
10. **No new dependencies** unless the task file says so. If you think
    you need one, abandon the task and note "needs-dep: <crate>" in the
    task file.

## Loop

For each task file:

1. **Read** the task file completely. It has YAML frontmatter
   (`type`, `target_file`, `target_line`, `description`, `acceptance`)
   and a free-form body. The body may include a snippet from the
   target file.
2. **Verify the target still exists.** If `target_file:target_line`
   doesn't match the description anymore (the codebase moved on),
   abandon: write `stale: target-moved` and exit non-zero.
3. **Plan in your head.** Don't write planning docs to disk. Don't
   add a "TODO write tests later" comment.
4. **Implement.** Edit only files needed for the task. Add tests in
   the same crate's `tests/` if the task type requires them (see per-
   type rules below).
5. **Run the score script** — `bash $AR_SCORE` — from the worktree
   root. It exits 0 if every gate passes, non-zero otherwise. The
   gates:
   - `cargo nextest run --workspace --no-fail-fast` (must be ≥ as
     many passing tests as the master baseline)
   - `cargo clippy --workspace -- -D warnings`
   - `cargo fmt --all -- --check`
   - For tasks touching the Python oracle (`lib/`, `tests/` in the
     consortium-tests repo): `pytest tests/ -v --timeout=30 -x` run
     from `$CONSORTIUM_TESTS_DIR`
6. **If green**, commit with a single conventional-commit message,
   `git push -u origin "$AR_BRANCH"` if the remote exists, and exit 0.
   The orchestrator will open the PR.
7. **If red**, do not loop in the same task. Write the failing
   command and last 50 lines of output as a comment block at the end
   of the task file, then exit non-zero. The orchestrator will move
   the task to `queue/abandoned/`.

You get **30 minutes wall-clock per task.** The orchestrator kills
you after that. Plan accordingly: simple TODOs first if you have
choice, but you do not have choice — you got the task you got.

## Per-task-type guidance

### `nix-parallelize`
The 6 TODOs in `crates/consortium-nix/src/{copy,health,activate,build,eval}.rs`
ask for fanout via core's `Task`/`Worker` primitives. Reuse:
- `crates/consortium/src/task.rs` and `worker.rs` — do not reimplement.
  (The crate is named `consortium`, NOT `consortium-crate` or `core`.
  Check `Cargo.toml` package name if unsure.)
- The DAG executor in `crates/consortium/src/dag.rs` — threads +
  channels, no tokio.
Add at least one test in `crates/consortium-nix/tests/` that exercises
the new fanout against a 3-node mock target.

### `port-python-test`
Translate one `tests/<X>Test.py` from the consortium-tests repo (or a
single test method within it) into a Rust integration test under the
appropriate `crates/*/tests/`. Match the assertion semantics, not the
Python syntax. If the test requires fixtures or test helpers that
don't exist in Rust, abandon with `needs-helper: <name>`.

### `resolve-rust-todo`
Replace one `todo!()` or `unimplemented!()` macro with a real
implementation. The diff must include either a new test that exercises
the now-implemented path, or a justification comment if the macro
guarded an unreachable case (then use `unreachable!()` with reason).

### `port-python-fixme`
Investigate one `FIXME` in `lib/ClusterShell/` (in the consortium-tests
repo — edit and test there, not in your worktree). The fix may be:
(a) a real bug — fix it in Python, add a regression test in
consortium-tests `tests/`,
(b) already not-applicable — remove the FIXME, explain in commit body,
(c) too large — abandon with `scope: needs-design`.

### `upstream-sync`
Apply one upstream commit-range from cea-hpc/clustershell to `lib/` in
the consortium-tests repo, then look for affected files in the Rust
port (`crates/`) and either replicate the change or file a follow-up
task in `autoresearch/queue/pending/` named `parity-<sha>.task.toml`.
Bump `UPSTREAM_REF` (at the consortium-tests root) only when the entire
range is integrated.

## Anti-patterns — stop yourself if you notice these

- Adding `// TODO: cleanup later` comments. The whole point of this
  loop is to remove TODOs.
- Adding feature flags to "preserve old behavior". The task is the
  new behavior.
- Writing a long commit message body explaining design choices the
  reviewer will see in the diff.
- Editing `Cargo.lock` by hand.
- Running `cargo update`.
- Renaming things that aren't part of the task.
- Adding `#[allow(...)]` or `#[cfg(...)]` to silence clippy/test
  failures instead of fixing the underlying issue.
