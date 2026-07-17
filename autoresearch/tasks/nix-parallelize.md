# Task template: nix-parallelize

Used for the 6 TODOs in `crates/consortium-nix/src/*.rs` asking for
fanout via core's Task/Worker.

## Frontmatter (filled per task)

```yaml
type: nix-parallelize
target_file: crates/consortium-nix/src/<X>.rs
target_line: <N>
description: <one-line summary copied from the TODO comment>
acceptance:
  - target file no longer contains the TODO
  - new test under crates/consortium-nix/tests/ exercises fanout >= 3 nodes
  - cargo nextest run --workspace passes
  - cargo clippy passes with -D warnings
needs:
  - read crates/consortium-crate/src/{task,worker,dag}.rs first
```

## Body

The Nix orchestration crate currently runs `<X>` sequentially. The core
crate already has `Task::run_many(workers: &[Worker])` (or DAG-based
fanout via `Dag::execute`); use it. Don't pull in tokio. Don't add a
new threadpool.

Hint files to read before editing:
- `crates/consortium-crate/src/task.rs`
- `crates/consortium-crate/src/worker.rs`
- `crates/consortium-crate/src/dag.rs`
- whatever the test harness in the consortium-tests repo's `test-harness/`
  crate exposes for spinning up mock SSH/Nix nodes (it moved out of
  `crates/` with the test-infrastructure split).
