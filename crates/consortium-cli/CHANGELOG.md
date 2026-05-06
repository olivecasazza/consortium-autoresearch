# Changelog

## Bug Fixes

- use alt screen buffer to fix multi-n0 stacking
- replicate nom's exact rendering protocol
- pending count + revert testbed from claw to cascade-viz
- transient-vs-permanent error semantic + summary alignment
- off-by-one in erase loop + ||-separator header
- truncate frame lines to terminal width
- pre-populate pending nodes + allow transient retries

## Features

- builder + contention + event protocol + cli viz
- live in-place tree re-rendering on RoundCompleted
- --per-round-delay flag for watchable live demos
- per-node spinner state + claw --testbed deploy mode
- --tb-seeds for multi-seed testbed deploys
- LevelTreeFanOut strategy + claw default
- nom-fidelity truncation+sort+frame-gate + shared OutputArgs
- random failures + orphan re-routing in level-tree
- multi-line headers + minimal-by-default verbosity
- production wiring — NixCopyExecutor + cascade-copy bin
- wire cascade primitive into deploy — peer-to-peer copy fan-out

## Refactoring

- use event_render instead of inline workarounds

## style

- || separators in summary row


## Bug Fixes

- multi-dim pattern expansion, drain process output, group resolver, configparser 3.x

## Features

- complete library + CLI migration (14.4k LOC, 348 tests)
- add nh-inspired progress bars to claw
- add NixOS deployment (cast) with generic DAG executor
- add tool integrations (ansible, slurm, ray, skypilot) and test improvements
- add --flake flag to cast for cross-repo deployments
- add docs.rs metadata, fix semantic-release success comments

