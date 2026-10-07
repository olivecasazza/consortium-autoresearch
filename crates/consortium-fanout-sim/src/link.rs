//! Link-layer cost model: packet loss, jitter, and asymmetry.
//!
//! [`crate::executor::DeterministicExecutor`] and the deploy-DAG
//! simulator both need a *transfer time* for pushing `closure_bytes`
//! over one directed edge. The naive model is
//! `bytes / bandwidth + latency`. That ignores three effects an
//! HPC fabric always has:
//!
//! 1. **Packet loss.** A transfer is a sequence of MTU-sized chunks.
//!    Losing a chunk forces a retransmit, so the expected number of
//!    round trips grows with the byte count, not just with bandwidth.
//! 2. **Jitter.** Per-attempt latency is not the mean; it has a tail.
//! 3. **Asymmetry.** Real cluster links are asymmetric (the builder's
//!    downlink is the bottleneck pushing to a node, its uplink is
//!    what pulls the closure). Direction is already modelled by
//!    [`consortium_nix::cascade::NodeSpec`]'s separate `uplink` /
//!    `downlink`; this module makes the *cost* respect it.
//!
//! Everything here is a pure function of the model plus the edge
//! identity, so two runs with the same seed produce byte-identical
//! durations. There is no wall clock and no I/O.

use std::time::Duration;

use consortium_nix::cascade::NodeId;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// TCP-MSS-ish payload per round trip. One datagram plus a small
/// header. Matches the ~1460 B MSS that Ethernet + IPv4 + TCP gives.
pub const MTU_BYTES: u64 = 1460;

/// Per-attempt overhead: one datagram leaves, an ACK comes back, the
/// sender's congestion window advances. A transfer is not pure
/// `bytes / bandwidth` because each round trip also costs a wire
/// latency and a window-limited idle gap.
const PER_ATTEMPT_OVERHEAD: Duration = Duration::from_micros(250);

fn rng_from_edge(seed: u64, src: NodeId, tgt: NodeId, salt: u64) -> ChaCha8Rng {
    // FxHash-style mix so per-edge streams do not correlate.
    let mut h = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((src.0 as u64) << 32)
        .wrapping_add(tgt.0 as u64)
        .wrapping_add(salt.wrapping_mul(0xD1B5_4A32_D192_ED03));
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 29;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^= h >> 32;
    ChaCha8Rng::seed_from_u64(h)
}

/// How packets are lost on a link.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum PacketLoss {
    /// No loss. Transfer cost degrades to `bytes / bandwidth + latency`.
    #[default]
    None,
    /// Independent loss probability per chunk, in `[0, 1)`.
    /// Values `>= 0.999` are clamped: a link that drops everything
    /// never converges, which is a partition, not a slow link.
    PerChunk(f64),
    /// Loss probability that grows with the *squared* chunk index,
    /// modelling a link that degrades as a large transfer occupies it
    /// (buffer overrun on the bottleneck). Always `<= PerChunk` cost.
    RampByPosition { base: f64, ceiling: f64 },
}

impl PacketLoss {
    /// Loss probability for chunk `i` of a transfer that has
    /// `n_chunks` chunks in total.
    pub fn prob_at(&self, i: u64, n_chunks: u64) -> f64 {
        match *self {
            PacketLoss::None => 0.0,
            PacketLoss::PerChunk(p) => p.clamp(0.0, 0.999),
            PacketLoss::RampByPosition { base, ceiling } => {
                let lo = base.clamp(0.0, 0.999);
                let hi = ceiling.clamp(lo, 0.999);
                // The ramp is relative to *this transfer's* chunk
                // count, so a small transfer sees the same shape as a
                // large one instead of collapsing to the ceiling.
                let pos = if n_chunks <= 1 {
                    0.0
                } else {
                    (i as f64 / n_chunks as f64).clamp(0.0, 1.0)
                };
                lo + (hi - lo) * pos
            }
        }
    }

    /// Expected number of attempts to land chunk `i`.
    ///
    /// For independent per-chunk loss this is `1 / (1 - p)`.
    fn expected_attempts(&self, i: u64, n_chunks: u64) -> f64 {
        1.0 / (1.0 - self.prob_at(i, n_chunks))
    }

    /// Mean attempts per chunk over the whole transfer, sampled at a
    /// bounded number of positions. Sampling keeps this O(1) per edge
    /// so a 1024-host scenario stays cheap.
    fn mean_attempts(&self, n_chunks: u64) -> f64 {
        if n_chunks <= 1 {
            return self.expected_attempts(0, n_chunks);
        }
        const PROBES: u64 = 8;
        let mut acc = 0.0;
        for k in 0..PROBES {
            let idx = n_chunks.saturating_mul(k) / PROBES;
            acc += self.expected_attempts(idx, n_chunks);
        }
        acc / PROBES as f64
    }
}

/// Per-edge latency spread.
///
/// A single mean latency hides the tail. `spread` is a multiplier on a
/// uniformly sampled offset in `[1 - spread, 1 + spread]`, so
/// `spread = 0.0` is a fixed-latency fabric and larger values model a
/// noisier network. Sampling is seeded per edge, so it is
/// reproducible.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Jitter {
    /// Relative spread around the nominal latency, in `[0, 1)`.
    pub spread: f64,
}

impl Default for Jitter {
    fn default() -> Self {
        Jitter { spread: 0.0 }
    }
}

impl Jitter {
    /// Nominal latency adjusted by a seeded per-edge jitter draw.
    pub fn sample(&self, seed: u64, src: NodeId, tgt: NodeId, nominal: Duration) -> Duration {
        if self.spread <= 0.0 {
            return nominal;
        }
        let span = self.spread.clamp(0.0, 0.99);
        let mut rng = rng_from_edge(seed, src, tgt, JITTER_SALT);
        let draw: f64 = rng.gen_range(1.0 - span..=1.0 + span);
        Duration::from_secs_f64(nominal.as_secs_f64() * draw)
    }
}

const JITTER_SALT: u64 = 0x4A49_5454_4552; // "JITTER"

/// How the source's link is shared when several transfers leave it at
/// once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkDirection {
    /// Uplink is the bottleneck: many streams *out* of the source
    /// share `uplink`. This is the copy/activate push direction.
    #[default]
    UplinkLimited,
    /// Downlink is the bottleneck: many streams *into* the target
    /// share `downlink`. This is the eval/build pull direction.
    DownlinkLimited,
}

/// Full parameterization of a link transfer.
///
/// Built with [`LinkModel::new`] and tuned with the `with_*` setters
/// so call sites read as a spec sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkModel {
    loss: PacketLoss,
    jitter: Jitter,
    direction: LinkDirection,
    mtu_bytes: u64,
}

impl Default for LinkModel {
    fn default() -> Self {
        Self {
            loss: PacketLoss::None,
            jitter: Jitter::default(),
            direction: LinkDirection::UplinkLimited,
            mtu_bytes: MTU_BYTES,
        }
    }
}

impl LinkModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_loss(mut self, loss: PacketLoss) -> Self {
        self.loss = loss;
        self
    }

    pub fn with_jitter(mut self, spread: f64) -> Self {
        self.jitter = Jitter { spread };
        self
    }

    pub fn with_direction(mut self, direction: LinkDirection) -> Self {
        self.direction = direction;
        self
    }

    pub fn with_mtu(mut self, mtu_bytes: u64) -> Self {
        self.mtu_bytes = mtu_bytes.max(1);
        self
    }

    pub fn loss(&self) -> &PacketLoss {
        &self.loss
    }

    pub fn jitter(&self) -> Jitter {
        self.jitter
    }

    pub fn direction(&self) -> LinkDirection {
        self.direction
    }

    pub fn mtu_bytes(&self) -> u64 {
        self.mtu_bytes
    }

    /// Effective bytes/second for one edge, applying contention and
    /// direction.
    ///
    /// `uplink` / `downlink` come from
    /// [`consortium_nix::cascade::NodeSpec`]. A node with no
    /// `NodeSpec` should pass `u64::MAX`, which drops out of the
    /// `min` and leaves the per-edge `bandwidth` as the limit — that
    /// is the degenerate case the cascade already relies on.
    pub fn effective_bandwidth(
        &self,
        edge_bandwidth: u64,
        src_uplink: u64,
        tgt_downlink: u64,
        src_out_degree: u64,
        tgt_in_degree: u64,
    ) -> u64 {
        let shared = match self.direction {
            LinkDirection::UplinkLimited => src_uplink / src_out_degree.max(1),
            LinkDirection::DownlinkLimited => tgt_downlink / tgt_in_degree.max(1),
        };
        edge_bandwidth.min(shared).max(1)
    }

    /// Time to move `bytes` over one edge.
    ///
    /// `effective_bandwidth` must already be contention-adjusted
    /// (see [`LinkModel::effective_bandwidth`]). The model is the
    /// standard bandwidth/latency decomposition of a TCP transfer, plus
    /// a loss term:
    ///
    /// ```text
    /// time = bytes / bw                     bandwidth-limited floor
    ///      + rtt_penalty_rounds * latency   slow-start latency penalty
    ///      + rtt_penalty_rounds * overhead  per-round-trip cost
    ///      + bytes * (attempts - 1) / bw    retransmitted bytes
    /// ```
    ///
    /// `rtt_penalty_rounds` is `ceil(log2(chunks)) + 1`: a congestion
    /// window starts at one packet and doubles per round trip, so a
    /// transfer pays a *logarithmic* number of round trips, not one per
    /// chunk. Charging latency per chunk instead would model a
    /// stop-and-wait protocol and over-price loss-free transfers by
    /// orders of magnitude.
    ///
    /// Returns [`Duration::ZERO`] only for a zero-byte transfer.
    pub fn transfer_time(
        &self,
        seed: u64,
        src: NodeId,
        tgt: NodeId,
        bytes: u64,
        effective_bandwidth: u64,
        nominal_latency: Duration,
    ) -> Duration {
        if bytes == 0 {
            return Duration::ZERO;
        }
        let latency = self.jitter.sample(seed, src, tgt, nominal_latency);
        let bw = effective_bandwidth.max(1) as f64;
        let bytes_f = bytes as f64;

        let n_chunks = bytes.div_ceil(self.mtu_bytes).max(1);
        let attempts = self.loss.mean_attempts(n_chunks);

        // log2(chunks) rounded up, plus the initial window. Bounded so
        // a huge closure over a tiny MTU cannot overflow the shift.
        let rtt_rounds = (64 - n_chunks.max(1).leading_zeros()).clamp(1, 48) as f64 + 1.0;

        let bandwidth_floor = Duration::from_secs_f64(bytes_f / bw);
        let retransmit = Duration::from_secs_f64(bytes_f * (attempts - 1.0).max(0.0) / bw);
        let rtt_penalty = Duration::from_secs_f64(
            rtt_rounds * (latency.as_secs_f64() + PER_ATTEMPT_OVERHEAD.as_secs_f64()),
        );

        bandwidth_floor + retransmit + rtt_penalty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes() -> (NodeId, NodeId) {
        (NodeId(0), NodeId(1))
    }

    #[test]
    fn no_loss_transfer_is_bandwidth_floor_plus_a_log_rtt_penalty() {
        let (s, t) = nodes();
        let bytes = 100 * 1024 * 1024;
        let bw = 100 * 1024 * 1024;
        let lat = Duration::from_millis(2);
        let d = LinkModel::new().transfer_time(7, s, t, bytes, bw, lat);

        // 100 MB / 100 MB/s = 1.0 s of pure wire time.
        let floor = 1.0;
        // 71945 chunks => ceil(log2) = 17, +1 = 18 round trips, each
        // costing latency + per-attempt overhead.
        let rtt = 18.0 * (0.002 + 0.00025);
        let want = floor + rtt;
        assert!(
            (d.as_secs_f64() - want).abs() < 1e-6,
            "expected {:.6}s, got {:?}",
            want,
            d
        );
        // The penalty is logarithmic, so a loss-free transfer stays
        // close to the bandwidth floor. This is the property that a
        // per-chunk-latency model would violate.
        assert!(
            d.as_secs_f64() < 1.10,
            "rtt penalty should be logarithmic, got {:?}",
            d
        );
    }

    #[test]
    fn transfer_time_is_linear_in_bytes_when_bandwidth_dominates() {
        let (s, t) = nodes();
        let bw = 1024 * 1024 * 1024;
        let lat = Duration::from_micros(100);
        let m = LinkModel::new();
        let one = m.transfer_time(1, s, t, 50 * 1024 * 1024, bw, lat);
        let two = m.transfer_time(1, s, t, 100 * 1024 * 1024, bw, lat);
        // Same log-rtt term for both, so the *difference* is exactly
        // linear in the byte count.
        let delta = (two - one).as_secs_f64();
        let want = 50.0 * 1024.0 * 1024.0 / bw as f64;
        assert!((delta - want).abs() < 1e-3, "delta {} want {}", delta, want);
    }

    #[test]
    fn zero_byte_transfer_is_free() {
        let (s, t) = nodes();
        assert_eq!(
            LinkModel::new().transfer_time(1, s, t, 0, 1024, Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn loss_strictly_increases_transfer_time() {
        let (s, t) = nodes();
        let bytes = 50 * 1024 * 1024;
        let bw = 100 * 1024 * 1024;
        let lat = Duration::from_millis(1);
        let clean = LinkModel::new().transfer_time(1, s, t, bytes, bw, lat);
        let lossy = LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.01))
            .transfer_time(1, s, t, bytes, bw, lat);
        assert!(lossy > clean, "lossy {:?} <= clean {:?}", lossy, clean);
    }

    #[test]
    fn ramp_loss_keeps_its_shape_for_a_small_transfer() {
        // The ramp is relative to the transfer's own chunk count, so a
        // 10 MB transfer does not collapse to the ceiling the way a
        // fixed-1024 ramp would.
        let (s, t) = nodes();
        let m = LinkModel::new().with_loss(PacketLoss::RampByPosition {
            base: 0.001,
            ceiling: 0.2,
        });
        assert!(m.loss().prob_at(0, 4) < 0.01);
        assert!(m.loss().prob_at(3, 4) > 0.15);
        let d = m.transfer_time(
            1,
            s,
            t,
            10 * 1024 * 1024,
            100 * 1024 * 1024,
            Duration::from_millis(1),
        );
        assert!(
            d.as_secs_f64() < 5.0,
            "small ramped transfer diverged: {:?}",
            d
        );
    }

    #[test]
    fn ramp_loss_costs_at_least_as_much_as_its_floor() {
        let (s, t) = nodes();
        let bytes = 50 * 1024 * 1024;
        let bw = 100 * 1024 * 1024;
        let lat = Duration::from_millis(1);
        let floor = LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.005))
            .transfer_time(3, s, t, bytes, bw, lat);
        let ramp = LinkModel::new()
            .with_loss(PacketLoss::RampByPosition {
                base: 0.005,
                ceiling: 0.05,
            })
            .transfer_time(3, s, t, bytes, bw, lat);
        assert!(ramp > floor, "ramp {:?} <= floor {:?}", ramp, floor);
    }

    #[test]
    fn loss_penalty_scales_with_the_bytes_actually_resent() {
        let (s, t) = nodes();
        let bw = 100 * 1024 * 1024;
        let lat = Duration::from_millis(1);
        let clean = LinkModel::new();
        let lossy = LinkModel::new().with_loss(PacketLoss::PerChunk(0.02));

        let small = 1024 * 1024u64;
        let large = 200 * 1024 * 1024u64;
        let penalty = |m: &LinkModel, b: u64| {
            m.transfer_time(1, s, t, b, bw, lat) - clean.transfer_time(1, s, t, b, bw, lat)
        };
        let p_small = penalty(&lossy, small);
        let p_large = penalty(&lossy, large);
        // A flat per-chunk loss rate resends a *fixed fraction* of the
        // bytes, so the wasted time scales with the transfer size.
        assert!(
            p_large > p_small * 100,
            "small {:?} large {:?}",
            p_small,
            p_large
        );
    }

    #[test]
    fn total_loss_is_clamped_so_links_stay_reachable() {
        let (s, t) = nodes();
        // A probability above 1 is nonsense; the model clamps rather
        // than producing a negative or infinite retry factor.
        let d = LinkModel::new()
            .with_loss(PacketLoss::PerChunk(5.0))
            .transfer_time(1, s, t, 1024, 1024, Duration::ZERO);
        assert!(d.as_secs_f64() < 1.0e4, "clamped model diverged: {:?}", d);
        assert_eq!(PacketLoss::PerChunk(5.0).prob_at(0, 1), 0.999);
    }

    #[test]
    fn jitter_is_reproducible_and_bounded_by_spread() {
        let (s, t) = nodes();
        let m = LinkModel::new().with_jitter(0.25);
        let lat = Duration::from_millis(10);
        let a = m.jitter().sample(42, s, t, lat);
        let b = m.jitter().sample(42, s, t, lat);
        assert_eq!(a, b, "jitter not deterministic for a fixed seed");
        let lo = lat.as_secs_f64() * 0.75;
        let hi = lat.as_secs_f64() * 1.25;
        let got = a.as_secs_f64();
        assert!(
            (lo..=hi).contains(&got),
            "jitter {:?} outside [{}, {}]",
            a,
            lo,
            hi
        );
    }

    #[test]
    fn zero_spread_jitter_is_the_nominal_latency() {
        let (s, t) = nodes();
        let lat = Duration::from_millis(7);
        assert_eq!(Jitter::default().sample(9, s, t, lat), lat);
    }

    #[test]
    fn direction_decides_which_link_is_shared() {
        let m = LinkModel::new();
        // 1000 B/s edge, source uplink 1000, target downlink 100.
        let up = m.effective_bandwidth(1000, 1000, 100, 2, 4);
        assert_eq!(up, 500, "uplink 1000 split 2 ways = 500");
        let down = LinkModel::new()
            .with_direction(LinkDirection::DownlinkLimited)
            .effective_bandwidth(1000, 1000, 100, 2, 4);
        assert_eq!(down, 25, "downlink 100 split 4 ways = 25");
    }

    #[test]
    fn absent_node_spec_falls_back_to_edge_bandwidth() {
        // u64::MAX models "no NodeSpec recorded".
        let m = LinkModel::new();
        assert_eq!(m.effective_bandwidth(4096, u64::MAX, u64::MAX, 8, 8), 4096);
    }

    #[test]
    fn smaller_mtu_costs_more_at_the_same_loss_rate() {
        let (s, t) = nodes();
        let bytes = 10 * 1024 * 1024;
        let bw = 100 * 1024 * 1024;
        let lat = Duration::from_millis(1);
        let big = LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.01))
            .transfer_time(1, s, t, bytes, bw, lat);
        let small = LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.01))
            .with_mtu(512)
            .transfer_time(1, s, t, bytes, bw, lat);
        assert!(small > big, "small-mtu {:?} <= big-mtu {:?}", small, big);
    }
}
