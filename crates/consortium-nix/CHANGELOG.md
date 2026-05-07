# Changelog

## Bug Fixes

- transient-vs-permanent error semantic + summary alignment
- pre-populate pending nodes + allow transient retries
- cascade deploy reports eval failures, not misleading "no output"
- run per-toplevel groups concurrently, not sequentially
- unify renderer view across per-toplevel groups
- per-group dense NodeIds + remap at sink boundary

## Features

- add NixOS deployment (cast) with generic DAG executor
- add tool integrations (ansible, slurm, ray, skypilot) and test improvements
- add versioned documentation with GitHub Pages publishing
- add docs.rs metadata, fix semantic-release success comments
- parallelize builds with DAG executor ([#2](https://github.com/olivecasazza/consortium-autoresearch/pull/2))
- add log-N closure-distribution primitive + sim testbed
- builder + contention + event protocol + cli viz
- LevelTreeFanOut strategy + claw default
- random failures + orphan re-routing in level-tree
- production wiring — NixCopyExecutor + cascade-copy bin
- wire cascade primitive into deploy — peer-to-peer copy fan-out
- shared-base + per-host diff — both patterns at once
- closure-intersection cascade — true shared-substrate fan-out

## Testing

- tighten loose strategy assertions


## Features

- add NixOS deployment (cast) with generic DAG executor
- add tool integrations (ansible, slurm, ray, skypilot) and test improvements
- add versioned documentation with GitHub Pages publishing
- add docs.rs metadata, fix semantic-release success comments

