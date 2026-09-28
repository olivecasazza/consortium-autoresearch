//! Deterministic [`RoundExecutor`] implementation.
//!
//! Computes per-edge duration from `closure_size / bandwidth + latency`,
//! consults a [`FailureSchedule`] to
//! decide whether each edge succeeds or fails, and returns the result
//! map the cascade coordinator expects.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use consortium_nix::cascade::{CascadeError, CascadeNode, NetworkProfile, NodeId, RoundExecutor};

use crate::fixtures::FailureSchedule;

/// Default bandwidth used when the network profile has no entry.
const DEFAULT_BW_BYTES_SEC: u64 = 100 * 1024 * 1024; // 100 MB/s

/// Deterministic round executor for the cascade primitive.
///
/// Tracks the *current cascade round* internally — the executor
/// increments its round counter every `dispatch()` call, which lets
/// the [`FailureSchedule`] inject failures keyed to specific rounds.
pub struct DeterministicExecutor {
    pub closure_bytes: u64,
    pub default_bandwidth: u64,
    pub schedule: FailureSchedule,
    /// `dispatch()` call counter (mutable inside &self because
    /// RoundExecutor::dispatch takes &self).
    round: Mutex<u32>,
}

impl DeterministicExecutor {
    pub fn new(closure_bytes: u64, schedule: FailureSchedule) -> Self {
        Self {
            closure_bytes,
            default_bandwidth: DEFAULT_BW_BYTES_SEC,
            schedule,
            round: Mutex::new(0),
        }
    }

    pub fn with_default_bandwidth(mut self, bw: u64) -> Self {
        self.default_bandwidth = bw;
        self
    }
}

impl RoundExecutor for DeterministicExecutor {
    fn dispatch(
        &self,
        _nodes: &[CascadeNode],
        edges: &[(NodeId, NodeId)],
        net: &NetworkProfile,
    ) -> HashMap<(NodeId, NodeId), Result<Duration, CascadeError>> {
        let round = {
            let mut g = self.round.lock().unwrap();
            let r = *g;
            *g += 1;
            r
        };

        // Pre-compute fan-out counts for contention math: one pass over edges
        // before mapping to avoid O(E²) recomputation per edge.
        let mut src_out_counts: HashMap<NodeId, u64> = HashMap::new();
        let mut tgt_in_counts: HashMap<NodeId, u64> = HashMap::new();
        for (src, tgt) in edges {
            *src_out_counts.entry(*src).or_insert(0) += 1;
            *tgt_in_counts.entry(*tgt).or_insert(0) += 1;
        }

        edges
            .iter()
            .map(|(src, tgt)| {
                let outcome = if let Some(err) = self.schedule.failure_for(round, *src, *tgt) {
                    Err(err)
                } else if net.is_partitioned(*src, *tgt) {
                    Err(CascadeError::Partitioned {
                        src: *src,
                        tgt: *tgt,
                    })
                } else {
                    let src_out = *src_out_counts.get(src).unwrap_or(&1);
                    let tgt_in = *tgt_in_counts.get(tgt).unwrap_or(&1);
                    let bw = net.effective_bandwidth(
                        *src,
                        *tgt,
                        src_out,
                        tgt_in,
                        self.default_bandwidth,
                    );
                    let lat = net.latency_of(*src, *tgt, Duration::ZERO);
                    let secs = self.closure_bytes as f64 / bw as f64;
                    Ok(Duration::from_secs_f64(secs) + lat)
                };
                ((*src, *tgt), outcome)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::FailureSchedule;
    use std::collections::HashSet;

    #[test]
    fn duration_proportional_to_closure_size_over_bandwidth() {
        let exec = DeterministicExecutor::new(100 * 1024 * 1024, FailureSchedule::default());
        let mut net = NetworkProfile::default();
        net.bandwidth
            .insert((NodeId(0), NodeId(1)), 50 * 1024 * 1024);

        let edges = vec![(NodeId(0), NodeId(1))];
        let nodes = vec![
            CascadeNode::new(NodeId(0), "a"),
            CascadeNode::new(NodeId(1), "b"),
        ];
        let outcomes = exec.dispatch(&nodes, &edges, &net);
        let dur = outcomes
            .get(&(NodeId(0), NodeId(1)))
            .unwrap()
            .as_ref()
            .unwrap();
        // 100 MB / 50 MB/s = 2.0 s
        assert!((dur.as_secs_f64() - 2.0).abs() < 0.01, "got {:?}", dur);
    }

    #[test]
    fn partition_returns_partitioned_error() {
        let exec = DeterministicExecutor::new(1024, FailureSchedule::default());
        let mut net = NetworkProfile::default();
        net.partitions.insert((NodeId(0), NodeId(1)));

        let edges = vec![(NodeId(0), NodeId(1))];
        let nodes = vec![
            CascadeNode::new(NodeId(0), "a"),
            CascadeNode::new(NodeId(1), "b"),
        ];
        let outcomes = exec.dispatch(&nodes, &edges, &net);
        assert!(matches!(
            outcomes.get(&(NodeId(0), NodeId(1))),
            Some(Err(CascadeError::Partitioned { .. }))
        ));
    }

    #[test]
    fn failure_schedule_kills_target_at_specific_round() {
        let mut killed = HashSet::new();
        killed.insert(NodeId(2));
        let schedule = FailureSchedule::KillNodeAtRound {
            node: NodeId(2),
            round: 1,
        };
        let exec = DeterministicExecutor::new(1024, schedule);
        let net = NetworkProfile::default();
        let nodes = vec![
            CascadeNode::new(NodeId(0), "a"),
            CascadeNode::new(NodeId(1), "b"),
            CascadeNode::new(NodeId(2), "c"),
        ];

        // round 0: nothing killed
        let edges = vec![(NodeId(0), NodeId(1))];
        let r0 = exec.dispatch(&nodes, &edges, &net);
        assert!(r0.get(&(NodeId(0), NodeId(1))).unwrap().is_ok());

        // round 1: node 2 killed
        let edges = vec![(NodeId(1), NodeId(2))];
        let r1 = exec.dispatch(&nodes, &edges, &net);
        assert!(r1.get(&(NodeId(1), NodeId(2))).unwrap().is_err());

        let _ = killed;
    }
}
