# Changelog

## Bug Fixes

- multi-dim pattern expansion, drain process output, group resolver, configparser 3.x
- mark env-mutating tests as unsafe, bump nextest retries to 2
- declare [lib] name = "consortium" where cargo actually reads it

## Features

- RangeSet Rust implementation with 43 passing tests
- complete library + CLI migration (14.4k LOC, 348 tests)
- add NixOS deployment (cast) with generic DAG executor
- add tool integrations (ansible, slurm, ray, skypilot) and test improvements
- add versioned documentation with GitHub Pages publishing
- add docs.rs metadata, fix semantic-release success comments
- rename core crate to consortium-crate for crates.io publishing
- a container-side RoundExecutor so the Docker tier can run a cascade ([#58](https://github.com/olivecasazza/consortium-autoresearch/pull/58))

## Testing

- add Docker integration tests for SSH and DAG execution
- expand Docker integration tests to 17 (SCP, errors, scale, deployment)
- add real nix/ansible integration tests with specialized Docker images
- add functional proof tests for DAG parallelism, pipelining, and concurrency
- port upstream RangeSetTest cases to Rust ([#38](https://github.com/olivecasazza/consortium-autoresearch/pull/38))


## Bug Fixes

- multi-dim pattern expansion, drain process output, group resolver, configparser 3.x
- mark env-mutating tests as unsafe, bump nextest retries to 2

## Features

- RangeSet Rust implementation with 43 passing tests
- complete library + CLI migration (14.4k LOC, 348 tests)
- add NixOS deployment (cast) with generic DAG executor
- add tool integrations (ansible, slurm, ray, skypilot) and test improvements
- add versioned documentation with GitHub Pages publishing
- add docs.rs metadata, fix semantic-release success comments

## Testing

- add Docker integration tests for SSH and DAG execution
- expand Docker integration tests to 17 (SCP, errors, scale, deployment)
- add real nix/ansible integration tests with specialized Docker images
- add functional proof tests for DAG parallelism, pipelining, and concurrency

