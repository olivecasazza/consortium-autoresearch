//! A container-side [`RoundExecutor`] so the Docker tier can run a cascade.
//!
//! [`run_cascade`] and [`run_cascade_with_events`] are generic over
//! `&dyn RoundExecutor`, so a Docker-side executor needs no new abstraction —
//! only an implementation of the one trait method. That is what this module
//! provides, plus the failure-injection parity that makes the two tiers
//! comparable.
//!
//! # Structural calibration, not timing calibration
//!
//! This executor deliberately reuses the *same* decision order as
//! [`DeterministicExecutor`]:
//!
//! 1. `FailureSchedule::failure_for(round, src, tgt)` — injected failure wins.
//! 2. `NetworkProfile::is_partitioned(src, tgt)` — a real partition.
//! 3. Otherwise the per-edge transport runs for real.
//!
//! Because both tiers consult the same two predicates in the same order, any
//! divergence in **round count** or **parent chain** between them is
//! attributable to the transport, never to differing injection logic. That is
//! the calibration CON-99 needs. Timing is deliberately *not* calibrated here:
//! without `tc netem` (CON-215) a container timing comparison would be a
//! fabrication, so only the structural bounds are asserted.
//!
//! # No new result schema
//!
//! Nothing here invents a result type. Per-round and per-edge outcomes are
//! produced by the existing `consortium_nix::cascade_trace` /
//! `consortium_nix::cascade_events` machinery that CON-20 consumes, and the
//! failure shape is the same `CascadeError` the coordinator already folds into
//! its error tree.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use consortium_fanout_sim::{DeterministicExecutor, FailureSchedule};
use consortium_nix::cascade::{CascadeError, CascadeNode, NetworkProfile, NodeId, RoundExecutor};
use consortium_nix::cascade_trace::TraceRecorder;

use crate::DockerCluster;

/// SSH connect timeout, in seconds, for every hop this module makes.
const CONNECT_TIMEOUT_SECS: u32 = 5;

/// Default per-edge budget. Matches `NixCopyExecutor`'s default.
const DEFAULT_EDGE_TIMEOUT: Duration = Duration::from_secs(300);

/// Where a `NodeId` physically lives, as seen from the host running the test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHop {
    /// `user@host`, e.g. `root@127.0.0.1`.
    pub addr: String,
    /// Host-published SSH port for that container.
    pub port: u16,
}

/// The per-edge action a [`DockerCascadeExecutor`] delegates to.
///
/// Split out from the executor so the failure/partition decision logic — the
/// part that must stay identical to the sim tier — is testable without Docker,
/// and so a future transport (e.g. one that pushes from `src` rather than
/// streaming through the host) is a drop-in.
pub trait EdgeTransport: Send + Sync {
    /// Move the payload `src` holds to `tgt` within `timeout`, returning the
    /// wall-time of the real hop. Errors must be transient-shaped
    /// ([`CascadeError::Copy`]) so the coordinator retries the target from an
    /// alternate source rather than declaring it dead.
    fn transfer(
        &self,
        src: NodeId,
        tgt: NodeId,
        timeout: Duration,
    ) -> Result<Duration, CascadeError>;

    /// Stage the payload on a node before the cascade starts. Defaults to a
    /// no-op for transports that need no staging step.
    fn seed(&self, _node: NodeId) -> Result<(), CascadeError> {
        Ok(())
    }
}

/// A real transport: the payload is streamed `src` -> host -> `tgt` over two
/// SSH connections, then read back from `tgt` to prove the bytes landed.
///
/// The read-back is not decoration. Streaming a truncated payload still exits
/// `0` on the receiving side, so without it a partial transfer would be
/// reported as a converged edge.
pub struct DockerClusterTransport {
    hops: HashMap<NodeId, SshHop>,
    identity_file: String,
    remote_path: String,
    payload: Vec<u8>,
    verify: bool,
}

impl DockerClusterTransport {
    /// Path the payload is staged at inside each container.
    pub const REMOTE_PATH: &'static str = "/tmp/consortium-cascade-payload";

    fn ssh_args(&self, hop: &SshHop) -> Vec<String> {
        vec![
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "PasswordAuthentication=no".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
            "-i".into(),
            self.identity_file.clone(),
            "-p".into(),
            hop.port.to_string(),
        ]
    }

    fn hop(&self, node: NodeId) -> Result<&SshHop, CascadeError> {
        self.hops.get(&node).ok_or(CascadeError::Copy {
            node,
            stderr: format!("no container hop registered for node {node}"),
        })
    }

    fn spawn_ssh(
        &self,
        node: NodeId,
        hop: &SshHop,
        remote_cmd: String,
        stdin: Stdio,
    ) -> Result<std::process::Child, CascadeError> {
        Command::new("ssh")
            .args(self.ssh_args(hop))
            .arg(&hop.addr)
            .arg(remote_cmd)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CascadeError::Copy {
                node,
                stderr: format!("ssh hop to {node} failed to spawn: {e}"),
            })
    }

    fn drain_stderr(child: &mut std::process::Child) -> String {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        err.trim().to_string()
    }
}

impl EdgeTransport for DockerClusterTransport {
    fn transfer(
        &self,
        src: NodeId,
        tgt: NodeId,
        timeout: Duration,
    ) -> Result<Duration, CascadeError> {
        let started = Instant::now();
        let src_hop = self.hop(src)?;
        let tgt_hop = self.hop(tgt)?;

        // 1. Pull the payload off `src`.
        let mut child_src = self.spawn_ssh(
            src,
            src_hop,
            format!("cat {}", self.remote_path),
            Stdio::null(),
        )?;
        let src_stdout = child_src.stdout.take().ok_or(CascadeError::Copy {
            node: src,
            stderr: "ssh produced no stdout pipe for the source hop".into(),
        })?;

        // 2. Push it into `tgt`. `src` closing its stdout is what ends this.
        let mut child_tgt = self.spawn_ssh(
            tgt,
            tgt_hop,
            format!("cat > {}", self.remote_path),
            Stdio::from(src_stdout),
        )?;

        // 3. Wait for both halves under a single budget.
        let deadline = started + timeout;
        let (src_status, tgt_status) = loop {
            let src_done = child_src.try_wait().map_err(|e| CascadeError::Copy {
                node: src,
                stderr: format!("waiting on the source hop failed: {e}"),
            })?;
            let tgt_done = child_tgt.try_wait().map_err(|e| CascadeError::Copy {
                node: tgt,
                stderr: format!("waiting on the target hop failed: {e}"),
            })?;
            if src_done.is_some() && tgt_done.is_some() {
                break (src_done, tgt_done);
            }
            if Instant::now() >= deadline {
                let _ = child_src.kill();
                let _ = child_tgt.kill();
                return Err(CascadeError::Copy {
                    node: tgt,
                    stderr: format!(
                        "edge {src} -> {tgt} exceeded its {timeout:?} transport budget"
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        // 4. The receiving side exits 0 even on a truncated stream, so the
        //    source's exit status is the one that decides completeness.
        if !src_status.is_some_and(|s| s.success()) {
            return Err(CascadeError::Copy {
                node: tgt,
                stderr: format!(
                    "source hop {src} -> {tgt} failed: {}",
                    Self::drain_stderr(&mut child_src)
                ),
            });
        }
        if !tgt_status.is_some_and(|s| s.success()) {
            return Err(CascadeError::Copy {
                node: tgt,
                stderr: format!(
                    "target hop {src} -> {tgt} failed: {}",
                    Self::drain_stderr(&mut child_tgt)
                ),
            });
        }

        // 5. Prove the bytes actually match.
        if self.verify {
            self.verify_bytes(tgt, tgt_hop)?;
        }

        Ok(started.elapsed())
    }

    fn seed(&self, node: NodeId) -> Result<(), CascadeError> {
        let hop = self.hop(node)?;
        let mut child = self.spawn_ssh(
            node,
            hop,
            format!("cat > {}", self.remote_path),
            Stdio::piped(),
        )?;
        child
            .stdin
            .take()
            .ok_or(CascadeError::Copy {
                node,
                stderr: "seed hop produced no stdin pipe".into(),
            })?
            .write_all(&self.payload)
            .map_err(|e| CascadeError::Copy {
                node,
                stderr: format!("seeding the payload onto {node} failed: {e}"),
            })?;
        let status = child.wait().map_err(|e| CascadeError::Copy {
            node,
            stderr: format!("waiting on the seed hop for {node} failed: {e}"),
        })?;
        if !status.success() {
            return Err(CascadeError::Copy {
                node,
                stderr: format!(
                    "seed hop for {node} exited non-zero: {}",
                    Self::drain_stderr(&mut child)
                ),
            });
        }
        Ok(())
    }
}

impl DockerClusterTransport {
    fn verify_bytes(&self, node: NodeId, hop: &SshHop) -> Result<(), CascadeError> {
        let out = Command::new("ssh")
            .args(self.ssh_args(hop))
            .arg(&hop.addr)
            .arg(format!("cat {}", self.remote_path))
            .output()
            .map_err(|e| CascadeError::Copy {
                node,
                stderr: format!("verification hop to {node} failed to spawn: {e}"),
            })?;
        if !out.status.success() {
            return Err(CascadeError::Copy {
                node,
                stderr: format!(
                    "verification hop to {node} exited non-zero: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
        if out.stdout != self.payload {
            return Err(CascadeError::Copy {
                node,
                stderr: format!(
                    "payload on {node} is {} bytes, expected {}",
                    out.stdout.len(),
                    self.payload.len()
                ),
            });
        }
        Ok(())
    }
}

/// A [`RoundExecutor`] that runs one cascade round over a live
/// [`DockerCluster`], one real SSH hop per edge.
pub struct DockerCascadeExecutor {
    hops: HashMap<NodeId, SshHop>,
    /// Container name behind each `NodeId`, for failure messages and lookups.
    names: HashMap<NodeId, String>,
    schedule: FailureSchedule,
    transport: Box<dyn EdgeTransport>,
    /// `dispatch()` call counter, matching `DeterministicExecutor`'s
    /// convention: the first dispatch is round 0. `RoundExecutor::dispatch` is
    /// not handed the round number, so the executor counts its own dispatches
    /// — and counts them the way the sim does, which is what keeps
    /// `KillNodeAtRound { round }` meaning the same thing in both tiers.
    round: Mutex<u32>,
    seed: NodeId,
    /// Per-edge transport budget, owned centrally so the transport does not
    /// need its own copy.
    timeout: Duration,
    /// Every edge dispatched, in order, for the tier-comparison test.
    attempted: Mutex<Vec<(u32, NodeId, NodeId)>>,
}

impl DockerCascadeExecutor {
    /// Bind `names` (in order) to live containers.
    ///
    /// Every name must resolve to a published SSH port; an unresolved name is
    /// an error rather than a silently skipped node, because a node that never
    /// receives the closure would otherwise be indistinguishable from a
    /// transport failure.
    pub fn from_cluster(
        cluster: &DockerCluster,
        names: &[String],
        seed_name: &str,
        schedule: FailureSchedule,
    ) -> Result<Self, String> {
        if !names.contains(&seed_name.to_string()) {
            return Err(format!("seed {seed_name} is not among the cascade nodes"));
        }
        if names.len() < 2 {
            return Err("a cascade needs at least a seed and one target".into());
        }

        let mut hops = HashMap::new();
        let mut names_by_id = HashMap::new();
        for (idx, name) in names.iter().enumerate() {
            let port = cluster
                .port_for(name)
                .ok_or_else(|| format!("no published SSH port for container {name}"))?;
            let id = NodeId(idx as u32);
            hops.insert(
                id,
                SshHop {
                    addr: "root@127.0.0.1".to_string(),
                    port,
                },
            );
            names_by_id.insert(id, name.clone());
        }

        let transport = DockerClusterTransport {
            hops: hops.clone(),
            identity_file: cluster.ssh_key_path().to_string_lossy().to_string(),
            remote_path: DockerClusterTransport::REMOTE_PATH.to_string(),
            payload: default_payload(),
            verify: true,
        };

        Ok(Self::from_hops(
            hops,
            names_by_id,
            schedule,
            Box::new(transport),
            NodeId(0),
        ))
    }

    /// Bind an executor to an explicit set of hops.
    ///
    /// This is the single place every field default is set — the round counter
    /// in particular, since `KillNodeAtRound { round }` only means the same
    /// thing in both tiers if the counter starts where the coordinator's does.
    /// [`from_cluster`](Self::from_cluster) funnels through here, and so do the
    /// tests, so a drift in the defaults is observable rather than shadowed by
    /// a test that builds the struct by hand.
    pub fn from_hops(
        hops: HashMap<NodeId, SshHop>,
        names: HashMap<NodeId, String>,
        schedule: FailureSchedule,
        transport: Box<dyn EdgeTransport>,
        seed: NodeId,
    ) -> Self {
        Self {
            hops,
            names,
            schedule,
            transport,
            // Round 0 is the first dispatch, matching the coordinator's own
            // `round` and `DeterministicExecutor`'s counter.
            round: Mutex::new(0),
            seed,
            timeout: DEFAULT_EDGE_TIMEOUT,
            attempted: Mutex::new(Vec::new()),
        }
    }

    /// Use a caller-supplied transport. Used by the non-Docker structural
    /// calibration test, and by any future container-side push transport.
    pub fn with_transport(mut self, transport: Box<dyn EdgeTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Override the failure-injection schedule.
    pub fn with_schedule(mut self, schedule: FailureSchedule) -> Self {
        self.schedule = schedule;
        self
    }

    /// Override the per-edge transport budget.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Stage the payload on the seed container. Call once before the cascade.
    pub fn stage_seed(&self) -> Result<(), CascadeError> {
        self.transport.seed(self.seed)
    }

    /// The `CascadeNode` list for a run, in the order the nodes were bound.
    pub fn cascade_nodes(&self) -> Vec<CascadeNode> {
        let mut nodes: Vec<CascadeNode> = self
            .hops
            .keys()
            .copied()
            .map(|id| CascadeNode::new(id, self.hops[&id].addr.clone()))
            .collect();
        nodes.sort_by_key(|n| n.id);
        nodes
    }

    /// The pre-seeded set: just the seed node.
    pub fn seeded(&self) -> HashSet<NodeId> {
        let mut s = HashSet::new();
        s.insert(self.seed);
        s
    }

    /// The `NodeId` bound to a container name.
    pub fn node_id_for(&self, container_name: &str) -> Option<NodeId> {
        self.names
            .iter()
            .find(|(_, n)| n.as_str() == container_name)
            .map(|(id, _)| *id)
    }

    /// The container name behind a `NodeId`.
    pub fn container_name(&self, node: NodeId) -> Option<&str> {
        self.names.get(&node).map(|s| s.as_str())
    }

    /// Every `(round, src, tgt)` edge this executor was asked to dispatch, in
    /// order. Edges whose failure was injected *before* the transport are
    /// included, so this is the full attempt log rather than just the hops
    /// that reached the wire.
    pub fn attempted_edges(&self) -> Vec<(u32, NodeId, NodeId)> {
        self.attempted.lock().map(|a| a.clone()).unwrap_or_default()
    }
}

impl RoundExecutor for DockerCascadeExecutor {
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

        if let Ok(mut attempted) = self.attempted.lock() {
            attempted.extend(edges.iter().map(|(s, t)| (round, *s, *t)));
        }

        let mut out = HashMap::with_capacity(edges.len());
        for (src, tgt) in edges {
            // Same decision order as DeterministicExecutor: injected failure
            // first, then a real partition, then the transport.
            let outcome = if let Some(err) = self.schedule.failure_for(round, *src, *tgt) {
                Err(err)
            } else if net.is_partitioned(*src, *tgt) {
                Err(CascadeError::Partitioned {
                    src: *src,
                    tgt: *tgt,
                })
            } else {
                self.transport.transfer(*src, *tgt, self.timeout)
            };
            out.insert((*src, *tgt), outcome);
        }
        out
    }
}

/// The staged payload. Small, non-empty, and stable: a zero-byte file would
/// make the read-back verification vacuous.
fn default_payload() -> Vec<u8> {
    b"consortium container cascade payload\n".to_vec()
}

/// A transport that performs no I/O, for calibrating the executor's decision
/// logic without Docker.
pub struct RecordingTransport {
    /// Per-edge fixed duration, so round wall-times are reproducible.
    pub edge_latency: Duration,
    log: Mutex<Vec<(NodeId, NodeId)>>,
}

impl RecordingTransport {
    pub fn new(edge_latency: Duration) -> Self {
        Self {
            edge_latency,
            log: Mutex::new(Vec::new()),
        }
    }

    /// Edges that reached the transport, in order.
    pub fn log(&self) -> Vec<(NodeId, NodeId)> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

impl EdgeTransport for RecordingTransport {
    fn transfer(
        &self,
        src: NodeId,
        tgt: NodeId,
        _timeout: Duration,
    ) -> Result<Duration, CascadeError> {
        if let Ok(mut log) = self.log.lock() {
            log.push((src, tgt));
        }
        Ok(self.edge_latency)
    }
}

impl<T: EdgeTransport + ?Sized> EdgeTransport for std::sync::Arc<T> {
    fn transfer(
        &self,
        src: NodeId,
        tgt: NodeId,
        timeout: Duration,
    ) -> Result<Duration, CascadeError> {
        (**self).transfer(src, tgt, timeout)
    }

    fn seed(&self, node: NodeId) -> Result<(), CascadeError> {
        (**self).seed(node)
    }
}

/// A transport that drops specific targets, the shape of a real container hop
/// being unreachable.
///
/// `fail_forever` never succeeds. `flap_once` fails the first attempt and then
/// behaves — the case that proves a transient error is retried from another
/// source rather than being written off.
pub struct DroppingTransport {
    fail_forever: HashSet<NodeId>,
    flap_once: HashSet<NodeId>,
    attempts: Mutex<HashMap<NodeId, usize>>,
    edge_latency: Duration,
}

impl DroppingTransport {
    pub fn new(fail_forever: HashSet<NodeId>, flap_once: HashSet<NodeId>) -> Self {
        Self {
            fail_forever,
            flap_once,
            attempts: Mutex::new(HashMap::new()),
            edge_latency: Duration::from_millis(5),
        }
    }

    /// How many times each target was actually attempted on the wire.
    pub fn attempts(&self) -> HashMap<NodeId, usize> {
        self.attempts.lock().map(|a| a.clone()).unwrap_or_default()
    }
}

impl EdgeTransport for DroppingTransport {
    fn transfer(
        &self,
        _src: NodeId,
        tgt: NodeId,
        _timeout: Duration,
    ) -> Result<Duration, CascadeError> {
        let n = {
            let mut attempts = self.attempts.lock().unwrap();
            let counter = attempts.entry(tgt).or_insert(0);
            *counter += 1;
            *counter
        };
        if self.fail_forever.contains(&tgt) {
            return Err(CascadeError::Copy {
                node: tgt,
                stderr: format!("ssh: connect to host port 22: Connection refused (tgt {tgt})"),
            });
        }
        if self.flap_once.contains(&tgt) && n == 1 {
            return Err(CascadeError::Copy {
                node: tgt,
                stderr: format!("connection reset by peer (tgt {tgt})"),
            });
        }
        Ok(self.edge_latency)
    }
}

/// The structural comparison between the two tiers.
///
/// The bound asserted by CON-99 for these three quantities is **zero**: the
/// tiers must agree exactly. A non-zero bound here would be a licence to let
/// real divergence through, and the round/parent-chain quantities do not
/// depend on timing at all.
#[derive(Debug, PartialEq, Eq)]
pub struct StructuralDelta {
    /// `container_rounds - sim_rounds`.
    pub rounds: i64,
    /// Parent links that differ between tiers, as `(child, sim_parent, container_parent)`.
    pub parent_chain_diffs: Vec<(NodeId, Option<NodeId>, Option<NodeId>)>,
    /// Edges issued by only one tier, across all rounds.
    pub edge_diffs: Vec<(u32, NodeId, NodeId)>,
}

impl StructuralDelta {
    /// Whether the tiers agree on every structural quantity.
    pub fn is_calibrated(&self) -> bool {
        self.rounds == 0 && self.parent_chain_diffs.is_empty() && self.edge_diffs.is_empty()
    }
}

/// The final parent chain from a trace recorder, read off the last snapshot.
///
/// `run_cascade*` takes `nodes` by value, so the caller's copy never sees the
/// coordinator's `parent` assignments. A snapshot carries the cumulative chain,
/// so the recorder is the supported way to observe the final shape.
pub fn final_parent_chain(recorder: &TraceRecorder) -> HashMap<NodeId, NodeId> {
    recorder
        .snapshots()
        .last()
        .map(|s| s.parent_chain.clone())
        .unwrap_or_default()
}

/// Every edge both tiers planned, tagged with the round it was planned in.
pub fn planned_edges(recorder: &TraceRecorder) -> Vec<(u32, NodeId, NodeId)> {
    recorder
        .snapshots()
        .iter()
        .flat_map(|s| {
            s.plan
                .assignments
                .iter()
                .map(move |(src, tgt)| (s.round, *src, *tgt))
        })
        .collect()
}

/// Run one reference scenario through both tiers and diff the structure.
///
/// `closure_bytes` only reaches the sim tier — the container tier measures real
/// wall-time — so it does not enter the structural delta.
pub fn compare_tiers(
    nodes: Vec<CascadeNode>,
    seeded: HashSet<NodeId>,
    net: NetworkProfile,
    strategy: &dyn consortium_nix::cascade::CascadeStrategy,
    sim: &DeterministicExecutor,
    container: &DockerCascadeExecutor,
    max_rounds: u32,
) -> StructuralDelta {
    let sim_trace = TraceRecorder::new();
    let sim_result = consortium_nix::cascade::run_cascade(
        nodes.clone(),
        seeded.clone(),
        net.clone(),
        strategy,
        sim,
        max_rounds,
        Some(&sim_trace),
    );

    let container_trace = TraceRecorder::new();
    let container_result = consortium_nix::cascade::run_cascade(
        nodes,
        seeded,
        net,
        strategy,
        container,
        max_rounds,
        Some(&container_trace),
    );

    let sim_parents = final_parent_chain(&sim_trace);
    let container_parents = final_parent_chain(&container_trace);

    let mut all_nodes: Vec<NodeId> = sim_parents
        .keys()
        .chain(container_parents.keys())
        .copied()
        .collect();
    all_nodes.sort();
    all_nodes.dedup();
    let parent_chain_diffs: Vec<(NodeId, Option<NodeId>, Option<NodeId>)> = all_nodes
        .into_iter()
        .map(|id| {
            (
                id,
                sim_parents.get(&id).copied(),
                container_parents.get(&id).copied(),
            )
        })
        .filter(|(_, a, b)| a != b)
        .collect();

    StructuralDelta {
        rounds: i64::from(container_result.rounds) - i64::from(sim_result.rounds),
        parent_chain_diffs,
        edge_diffs: diff_edges(planned_edges(&sim_trace), planned_edges(&container_trace)),
    }
}

/// Edges present a different number of times in `a` than in `b`.
fn diff_edges(
    a: Vec<(u32, NodeId, NodeId)>,
    b: Vec<(u32, NodeId, NodeId)>,
) -> Vec<(u32, NodeId, NodeId)> {
    let tally = |v: &[(u32, NodeId, NodeId)]| -> HashMap<(u32, NodeId, NodeId), usize> {
        let mut m: HashMap<(u32, NodeId, NodeId), usize> = HashMap::new();
        for e in v {
            *m.entry(*e).or_insert(0) += 1;
        }
        m
    };
    let (ca, cb) = (tally(&a), tally(&b));
    let mut keys: Vec<(u32, NodeId, NodeId)> = ca.keys().chain(cb.keys()).copied().collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| ca.get(k) != cb.get(k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use consortium_nix::cascade::Log2FanOut;
    use std::collections::HashSet;

    const N: u32 = 5;
    const MAX_ROUNDS: u32 = 16;

    fn nodes() -> Vec<CascadeNode> {
        (0..N)
            .map(|i| CascadeNode::new(NodeId(i), format!("root@127.0.0.1:{i}")))
            .collect()
    }

    fn seeded() -> HashSet<NodeId> {
        let mut s = HashSet::new();
        s.insert(NodeId(0));
        s
    }

    /// An executor with no cluster behind it. Built through the production
    /// constructor on purpose: the round counter and the decision order are
    /// exactly the properties that must match the sim tier, so a test that
    /// hand-built the struct would be testing its own defaults instead.
    fn executor(
        schedule: FailureSchedule,
    ) -> (DockerCascadeExecutor, std::sync::Arc<RecordingTransport>) {
        let transport = std::sync::Arc::new(RecordingTransport::new(Duration::from_millis(5)));
        let hops = (0..N)
            .map(|i| {
                (
                    NodeId(i),
                    SshHop {
                        addr: format!("root@127.0.0.1:{i}"),
                        port: 2200 + i as u16,
                    },
                )
            })
            .collect();
        let names = (0..N)
            .map(|i| (NodeId(i), format!("compute-{i:02}")))
            .collect();
        let ex = DockerCascadeExecutor::from_hops(
            hops,
            names,
            schedule,
            Box::new(CloneTransport(transport.clone())),
            NodeId(0),
        )
        .with_timeout(Duration::from_secs(30));
        (ex, transport)
    }

    /// Lets a test hold a handle on the transport the executor owns.
    #[derive(Clone)]
    struct CloneTransport(std::sync::Arc<RecordingTransport>);

    impl EdgeTransport for CloneTransport {
        fn transfer(
            &self,
            src: NodeId,
            tgt: NodeId,
            timeout: Duration,
        ) -> Result<Duration, CascadeError> {
            self.0.transfer(src, tgt, timeout)
        }
    }

    fn run(ex: &dyn RoundExecutor, net: NetworkProfile) -> (u32, HashMap<NodeId, NodeId>) {
        let rec = TraceRecorder::new();
        let res = consortium_nix::cascade::run_cascade(
            nodes(),
            seeded(),
            net,
            &Log2FanOut,
            ex,
            MAX_ROUNDS,
            Some(&rec),
        );
        (res.rounds, final_parent_chain(&rec))
    }

    /// The reference shape with 5 nodes and one seed under `Log2FanOut`:
    /// 3 rounds, parents 1->0 2->0 3->1 4->0.
    #[test]
    fn five_node_cascade_converges_in_three_rounds() {
        let (ex, _) = executor(FailureSchedule::None);
        let (rounds, parents) = run(&ex, NetworkProfile::default());
        assert_eq!(rounds, 3, "expected a 3-round log2 fan-out");
        assert_eq!(parents.get(&NodeId(1)), Some(&NodeId(0)));
        assert_eq!(parents.get(&NodeId(2)), Some(&NodeId(0)));
        assert_eq!(parents.get(&NodeId(3)), Some(&NodeId(1)));
        assert_eq!(parents.get(&NodeId(4)), Some(&NodeId(0)));
    }

    /// The whole point of CON-99's structural calibration: the two tiers must
    /// agree exactly, so the bound is zero rather than a tolerance.
    #[test]
    fn structural_delta_against_the_sim_is_zero() {
        let (ex, _) = executor(FailureSchedule::None);
        let sim = DeterministicExecutor::new(64 * 1024, FailureSchedule::None);
        let delta = compare_tiers(
            nodes(),
            seeded(),
            NetworkProfile::default(),
            &Log2FanOut,
            &sim,
            &ex,
            MAX_ROUNDS,
        );
        assert!(
            delta.is_calibrated(),
            "tiers diverged structurally: rounds delta={} parents={:?} edges={:?}",
            delta.rounds,
            delta.parent_chain_diffs,
            delta.edge_diffs
        );
    }

    /// Same, but with a partition — a partition is a structural fact both
    /// tiers must agree on, not a timing artefact.
    #[test]
    fn structural_delta_with_a_partition_is_zero() {
        let mut net = NetworkProfile::default();
        net.partitions.insert((NodeId(0), NodeId(2)));
        let (ex, _) = executor(FailureSchedule::None);
        let sim = DeterministicExecutor::new(64 * 1024, FailureSchedule::None);
        let delta = compare_tiers(nodes(), seeded(), net, &Log2FanOut, &sim, &ex, MAX_ROUNDS);
        assert!(delta.is_calibrated(), "tiers diverged: {delta:?}");
    }

    /// A partition must actually change the tree, otherwise the calibration
    /// above would be passing on a shape nothing perturbs.
    #[test]
    fn a_partition_reroutes_the_affected_target() {
        let mut net = NetworkProfile::default();
        net.partitions.insert((NodeId(0), NodeId(2)));
        let (ex, _) = executor(FailureSchedule::None);
        let (_, parents) = run(&ex, net);
        assert_eq!(
            parents.get(&NodeId(2)),
            Some(&NodeId(1)),
            "2 should be re-sourced from 1 once 0 is partitioned from it"
        );
    }

    /// An injected failure is decided *before* the transport, so it never
    /// reaches the wire — and it lands in the same round number the sim uses.
    ///
    /// The assertion is on convergence, not on the parent chain: the
    /// coordinator records a parent for a failed edge too (that is how the
    /// error tree keeps its shape), so a killed node still appears in the
    /// chain while being absent from `converged`.
    #[test]
    fn injected_failure_never_reaches_the_transport() {
        let schedule = FailureSchedule::KillNodeAtRound {
            node: NodeId(3),
            round: 1,
        };
        let (ex, transport) = executor(schedule);
        let rec = TraceRecorder::new();
        let res = consortium_nix::cascade::run_cascade(
            nodes(),
            seeded(),
            NetworkProfile::default(),
            &Log2FanOut,
            &ex,
            MAX_ROUNDS,
            Some(&rec),
        );

        assert!(
            !res.converged.contains(&NodeId(3)),
            "a node killed from round 1 on must not converge: {:?}",
            res.converged
        );
        assert!(
            !transport.log().iter().any(|(_, t)| *t == NodeId(3)),
            "the killed node's edges must be decided before the transport: {:?}",
            transport.log()
        );
        assert_eq!(
            ex.attempted_edges().last(),
            Some(&(2, NodeId(0), NodeId(4))),
            "the cascade should have carried on past the dead node"
        );
    }

    /// `dispatch` counts its own rounds starting at 0, matching the
    /// coordinator's own counter. If the two drifted apart, `KillNodeAtRound
    /// { round }` would mean different rounds in each tier.
    #[test]
    fn dispatch_round_counter_starts_at_zero_and_advances() {
        let (ex, _) = executor(FailureSchedule::None);
        let edges = [(NodeId(0), NodeId(1))];
        let net = NetworkProfile::default();
        ex.dispatch(&nodes(), &edges, &net);
        ex.dispatch(&nodes(), &edges, &net);
        assert_eq!(
            ex.attempted_edges(),
            vec![(0, NodeId(0), NodeId(1)), (1, NodeId(0), NodeId(1))]
        );
    }

    /// An injected failure outranks a partition on the same edge, and both
    /// outrank the transport.
    #[test]
    fn injected_failure_outranks_a_partition() {
        let schedule = FailureSchedule::Explicit(HashMap::from([(
            (0, NodeId(0), NodeId(1)),
            CascadeError::Copy {
                node: NodeId(1),
                stderr: "injected".into(),
            },
        )]));
        let (ex, transport) = executor(schedule);
        let mut net = NetworkProfile::default();
        net.partitions.insert((NodeId(0), NodeId(1)));
        let outcome = ex
            .dispatch(&nodes(), &[(NodeId(0), NodeId(1))], &net)
            .remove(&(NodeId(0), NodeId(1)))
            .expect("an outcome for every dispatched edge");

        match outcome {
            Err(CascadeError::Copy { stderr, .. }) => {
                assert_eq!(stderr, "injected", "the schedule must win the tie");
            }
            other => panic!("expected the injected Copy error, got {other:?}"),
        }
        assert!(
            transport.log().is_empty(),
            "neither predicate should fall through to the transport"
        );
    }

    /// A partition alone still fails the edge, with the partitioned shape.
    #[test]
    fn a_partitioned_edge_reports_partitioned_and_skips_the_transport() {
        let (ex, transport) = executor(FailureSchedule::None);
        let mut net = NetworkProfile::default();
        net.partitions.insert((NodeId(0), NodeId(1)));
        let outcome = ex
            .dispatch(&nodes(), &[(NodeId(0), NodeId(1))], &net)
            .remove(&(NodeId(0), NodeId(1)))
            .expect("an outcome for every dispatched edge");

        assert!(
            matches!(outcome, Err(CascadeError::Partitioned { .. })),
            "expected a Partitioned error, got {outcome:?}"
        );
        assert!(transport.log().is_empty(), "the transport must not run");
    }

    /// Every dispatched edge gets an outcome. A missing entry is folded into a
    /// transient error by the coordinator, which would silently look like a
    /// flaky hop rather than an executor bug.
    #[test]
    fn every_dispatched_edge_gets_an_outcome() {
        let (ex, _) = executor(FailureSchedule::None);
        let edges = [(NodeId(0), NodeId(1)), (NodeId(1), NodeId(2))];
        let outcomes = ex.dispatch(&nodes(), &edges, &NetworkProfile::default());
        for e in edges {
            assert!(outcomes.contains_key(&e), "missing outcome for {e:?}");
        }
    }

    /// A cluster binding that silently drops a node would look exactly like a
    /// transport failure, so the constructor refuses it.
    #[test]
    fn a_seed_outside_the_node_list_is_rejected() {
        // Exercised through the real constructor with a cluster that is never
        // started: the guard runs before any port lookup.
        let result = DockerCascadeExecutor::from_cluster(
            &crate::DockerCluster {
                compose_file: "unused".into(),
                project_name: "unused".into(),
                docker_dir: "unused".into(),
                ssh_key_path: "unused".into(),
                node_ports: HashMap::new(),
                topology: crate::ClusterTopology::default(),
            },
            &["compute-00".to_string(), "compute-01".to_string()],
            "login-00",
            FailureSchedule::None,
        );
        assert!(
            result.is_err(),
            "a seed outside the node list must be refused"
        );
    }
}

#[cfg(test)]
mod transport_failure_tests {
    use super::tests_support::*;
    use super::*;

    /// A hop that drops once is the real container failure mode: the edge
    /// reports a transient error, the coordinator retries the target from a
    /// different source, and the cascade still converges. The parent chain
    /// shifts, which is exactly why the calibration test's parent-chain
    /// assertion has any sensitivity at all.
    #[test]
    fn a_dropped_hop_is_retried_from_another_source() {
        let dropped = std::sync::Arc::new(DroppingTransport::new(
            HashSet::new(),
            HashSet::from([NodeId(2)]),
        ));
        let ex = executor(FailureSchedule::None, dropped.clone());
        let rec = TraceRecorder::new();
        let res = run_cascade(&ex, NetworkProfile::default(), &rec);

        assert!(
            res.converged.contains(&NodeId(2)),
            "a node that drops one hop should still converge: {:?}",
            res.converged
        );
        assert_eq!(
            final_parent_chain(&rec).get(&NodeId(2)),
            Some(&NodeId(1)),
            "the retry should re-source node 2 from node 1"
        );
        assert_eq!(
            dropped.attempts().get(&NodeId(2)).copied(),
            Some(2),
            "node 2 should have been attempted twice: once failing, once succeeding"
        );
    }

    /// A hop that is permanently down must not be retried forever, and the
    /// failure must surface on the result rather than being swallowed.
    #[test]
    fn a_permanently_unreachable_node_never_converges() {
        let dead = std::sync::Arc::new(DroppingTransport::new(
            HashSet::from([NodeId(2)]),
            HashSet::new(),
        ));
        let ex = executor(FailureSchedule::None, dead.clone());
        let rec = TraceRecorder::new();
        let res = run_cascade(&ex, NetworkProfile::default(), &rec);

        assert!(
            !res.converged.contains(&NodeId(2)),
            "an unreachable node must not converge"
        );
        assert!(
            res.failed.is_some(),
            "the cascade must surface the failure, not report success"
        );
        let attempts = dead.attempts().get(&NodeId(2)).copied().unwrap_or(0);
        assert!(
            attempts >= 2,
            "a transient Copy error must be retried from other sources, got {attempts} attempt(s)"
        );
        assert!(
            final_parent_chain(&rec).contains_key(&NodeId(2)),
            "the error tree keeps the dead node's shape so the failure is attributable"
        );
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;
    use std::sync::Arc;

    pub const N: u32 = 5;
    pub const MAX_ROUNDS: u32 = 16;

    pub fn nodes() -> Vec<CascadeNode> {
        (0..N)
            .map(|i| CascadeNode::new(NodeId(i), format!("root@127.0.0.1:{i}")))
            .collect()
    }

    pub fn seeded() -> HashSet<NodeId> {
        let mut s = HashSet::new();
        s.insert(NodeId(0));
        s
    }

    /// An executor bound to synthetic hops and a caller-supplied transport,
    /// built through the production constructor.
    pub fn executor(
        schedule: FailureSchedule,
        transport: Arc<dyn EdgeTransport>,
    ) -> DockerCascadeExecutor {
        let hops = (0..N)
            .map(|i| {
                (
                    NodeId(i),
                    SshHop {
                        addr: format!("root@127.0.0.1:{i}"),
                        port: 2200 + i as u16,
                    },
                )
            })
            .collect();
        let names = (0..N)
            .map(|i| (NodeId(i), format!("compute-{i:02}")))
            .collect();
        DockerCascadeExecutor::from_hops(hops, names, schedule, Box::new(transport), NodeId(0))
            .with_timeout(Duration::from_secs(30))
    }

    pub fn run_cascade(
        ex: &dyn RoundExecutor,
        net: NetworkProfile,
        rec: &TraceRecorder,
    ) -> consortium_nix::cascade::CascadeResult {
        consortium_nix::cascade::run_cascade(
            nodes(),
            seeded(),
            net,
            &consortium_nix::cascade::Log2FanOut,
            ex,
            MAX_ROUNDS,
            Some(rec),
        )
    }
}
