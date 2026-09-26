//! CON-97 canary: a deliberately failing test.
//!
//! Throwaway branch. Proves a red test makes the ci.yml `unit` job and the
//! migration-scorecard `Run Rust unit tests` step go red.

#[test]
fn con97_canary_deliberate_failure() {
    assert_eq!(
        2 + 2,
        5,
        "CON-97 canary: this test is red on purpose. If you see this in a merged \
         commit, a canary branch leaked."
    );
}
