# Changelog

## Bug Fixes

- transient-vs-permanent error semantic + summary alignment
- add the missing `uplinks` field so the cascade bench compiles

## Features

- add log-N closure-distribution primitive + sim testbed
- builder + contention + event protocol + cli viz
- random failures + orphan re-routing in level-tree
- a container-side RoundExecutor so the Docker tier can run a cascade ([#58](https://github.com/olivecasazza/consortium-autoresearch/pull/58))

## Testing

- tighten loose strategy assertions
- tighten loose assertions across sim test suite
- pin peer-SSH topology behavior — full-mesh + seed-only

