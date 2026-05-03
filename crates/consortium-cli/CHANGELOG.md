# Changelog

## Features

- builder + contention + event protocol + cli viz
- live in-place tree re-rendering on RoundCompleted
- --per-round-delay flag for watchable live demos
- per-node spinner state + claw --testbed deploy mode

## Refactoring

- use event_render instead of inline workarounds


## Bug Fixes

- multi-dim pattern expansion, drain process output, group resolver, configparser 3.x

## Features

- complete library + CLI migration (14.4k LOC, 348 tests)
- add nh-inspired progress bars to claw
- add NixOS deployment (cast) with generic DAG executor
- add tool integrations (ansible, slurm, ray, skypilot) and test improvements
- add --flake flag to cast for cross-repo deployments
- add docs.rs metadata, fix semantic-release success comments

