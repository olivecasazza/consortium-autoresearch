//! `tc netem` network shaping for the container test tier.
//!
//! The sim tier ([`consortium_nix::cascade::NetworkProfile`]) models a per-edge
//! network: bytes/sec per edge plus a latency. The container tier runs real
//! SSH between real processes, so the only way to make the two tiers
//! *comparable* is to impose a known network on the container side. That is
//! what `netem` does.
//!
//! Without shaping, a per-edge completion-time comparison between the tiers is
//! a loopback-vs-simulation fabrication rather than a calibration. See ADR 0001
//! §7.1.
//!
//! **The full sim→container mapping, including what a per-node qdisc can and
//! cannot express, is in `docs/testing/container-netem-shaping.md`.** Read that
//! before writing a calibration scenario; the short version is that one netem
//! qdisc is a *node* property (its egress), not an *edge* property, and the two
//! models are not one-to-one.
//!
//! # Design note: why command construction is separate from execution
//!
//! [`NetemProfile::qdisc_args`] and [`NetemProfile::clear_args`] are pure and
//! unit-tested without Docker. Only [`crate::DockerCluster::apply_netem`] and
//! [`crate::DockerCluster::clear_netem`] shell out, because the interesting
//! assertions (did the delay actually show up in a measured round trip?)
//! require a running container and belong in the `docker-tests` lane.
//!
//! # Three environment requirements
//!
//! These are the reasons shaping can silently not happen at all. Each is
//! enforced in code rather than left to the reader:
//!
//! 1. **`tc` must exist in the image.** `iproute2` in `tests/docker/Dockerfile.ssh-node`.
//! 2. **The node needs `CAP_NET_ADMIN`.** Docker's default capability set omits
//!    it, so `tc qdisc` fails `EPERM` in an unprivileged container even as root.
//!    Every generated service gets `cap_add: [NET_ADMIN]` from `plan_compose` —
//!    unit-tested, because a missing capability makes the whole lane a no-op
//!    with no failing test anywhere.
//! 3. **Shaping applies to egress only.** A qdisc on a node's `eth0` delays and
//!    caps what that node *sends*. Traffic arriving at the node is untouched. A
//!    round trip measured from outside therefore sees the delay once (the
//!    response), not twice, unless both endpoints are shaped.
//!
//! # The silent no-op this module exists to prevent
//!
//! `tc qdisc replace dev eth0 root netem` with no netem options is accepted by
//! `tc`, **exits 0, and shapes nothing**. A harness that only checked the exit
//! code would report a shaped cluster that is unshaped.
//! [`NetemProfile::qdisc_args`] therefore rejects an empty profile instead of
//! emitting that command.
//!
//! Teardown has a matching subtlety: `tc qdisc del` on an unshaped interface
//! fails with `RTNETLINK answers: No such file or directory`, which is
//! already-clear rather than an error. The kernel default on a container's veth
//! is `noqueue`, not `pfifo_fast`, so teardown asserts the absence of `netem`
//! rather than the presence of any particular default qdisc.

use std::time::Duration;

/// Default network interface inside the Alpine SSH node.
pub const DEFAULT_IFACE: &str = "eth0";

/// A `netem` network profile to impose on one node's interface.
///
/// All fields are optional because `netem` takes only the knobs you name. A
/// profile with no field set is rejected by [`NetemProfile::qdisc_args`] —
/// see the module docs for why that matters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetemProfile {
    /// Added one-way delay, e.g. `"20ms"`.
    pub delay: Option<String>,
    /// Random jitter around `delay`, e.g. `"5ms"`. Requires `delay`.
    pub jitter: Option<String>,
    /// Bandwidth cap, e.g. `"100mbit"`. See [`rate_from_bytes_per_sec`].
    pub rate: Option<String>,
    /// Packet loss, e.g. `"1%"`. Optional: the sim has no loss model, so a
    /// calibration scenario should normally leave this `None`.
    pub loss: Option<String>,
    /// Maximum packets queued in the qdisc, e.g. `"10000"`.
    ///
    /// `netem`'s default is 1000 packets. At a few Mbit/s that is under a
    /// couple of megabytes of buffer, so a bulk transfer overruns the queue and
    /// netem *drops* the excess — the transfer still completes, slowly, via TCP
    /// retransmits, and the measured time is then a loss-recovery artefact
    /// rather than the rate you asked for. Set this whenever `rate` is set and
    /// the transfer is larger than the queue.
    pub limit: Option<String>,
}

impl NetemProfile {
    /// A profile that only delays, the most common knob for calibration.
    pub fn with_delay(delay: impl Into<String>) -> Self {
        Self {
            delay: Some(delay.into()),
            ..Self::default()
        }
    }

    /// A profile that only caps bandwidth.
    pub fn with_rate(rate: impl Into<String>) -> Self {
        Self {
            rate: Some(rate.into()),
            ..Self::default()
        }
    }

    /// Builder-style [`NetemProfile::with_delay`].
    pub fn delay(mut self, delay: impl Into<String>) -> Self {
        self.delay = Some(delay.into());
        self
    }

    /// Builder-style [`NetemProfile::with_rate`].
    pub fn rate(mut self, rate: impl Into<String>) -> Self {
        self.rate = Some(rate.into());
        self
    }

    /// Builder-style jitter. Requires a `delay` to be set.
    pub fn jitter(mut self, jitter: impl Into<String>) -> Self {
        self.jitter = Some(jitter.into());
        self
    }

    /// Builder-style loss percentage.
    pub fn loss(mut self, loss: impl Into<String>) -> Self {
        self.loss = Some(loss.into());
        self
    }

    /// Builder-style qdisc queue limit. See [`NetemProfile::limit`].
    pub fn limit(mut self, limit: impl Into<String>) -> Self {
        self.limit = Some(limit.into());
        self
    }

    /// True when no knob at all is set.
    pub fn is_empty(&self) -> bool {
        self.delay.is_none()
            && self.jitter.is_none()
            && self.rate.is_none()
            && self.loss.is_none()
            && self.limit.is_none()
    }

    /// Build the argument vector for `tc qdisc ...` on `iface`.
    ///
    /// Returns `Err` for an empty profile (the silent no-op) and for jitter
    /// without delay (`netem` rejects `jitter` on its own, so letting it
    /// through would surface as an opaque `tc` failure at exec time instead of
    /// a clear message at construction time).
    pub fn qdisc_args(&self, iface: &str) -> Result<Vec<String>, String> {
        if self.is_empty() {
            return Err(
                "empty NetemProfile: `tc qdisc replace root netem` with no netem \
                 options exits 0 and shapes nothing. Set at least one of \
                 delay/rate/loss."
                    .to_string(),
            );
        }

        let mut args: Vec<String> = vec![
            "qdisc".into(),
            "replace".into(),
            "dev".into(),
            iface.to_string(),
            "root".into(),
            "netem".into(),
        ];

        if let Some(limit) = &self.limit {
            args.push("limit".into());
            args.push(limit.clone());
        }

        if let Some(delay) = &self.delay {
            args.push("delay".into());
            args.push(delay.clone());
            if let Some(jitter) = &self.jitter {
                // netem takes jitter as a second positional delay argument.
                args.push(jitter.clone());
            }
        } else if self.jitter.is_some() {
            return Err("NetemProfile.jitter requires NetemProfile.delay to be set".to_string());
        }

        if let Some(rate) = &self.rate {
            args.push("rate".into());
            args.push(rate.clone());
        }

        if let Some(loss) = &self.loss {
            args.push("loss".into());
            args.push(loss.clone());
        }

        Ok(args)
    }

    /// Arguments that remove any shaping from `iface`, restoring the kernel
    /// default qdisc.
    pub fn clear_args(iface: &str) -> Vec<String> {
        vec![
            "qdisc".into(),
            "del".into(),
            "dev".into(),
            iface.to_string(),
            "root".into(),
        ]
    }
}

/// Render bytes/sec as a `netem` rate string (`tc` parses `bit`, `kbit`,
/// `mbit`, `gbit`).
///
/// [`consortium_nix::cascade::NetworkProfile::bandwidth`] is `bytes/sec`, so
/// this is the conversion a calibration scenario needs to express a sim-side
/// bandwidth on the container side. Truncates to one decimal place.
///
/// The `*8` is load-bearing: `tc` rate units are **bits** per second, and
/// passing a bytes/sec figure straight through labels the number as `mbit` when
/// it is really `MByte`/s — shaping the link 8x tighter than the model it is
/// supposed to represent. The failure is invisible: every command succeeds and
/// the container tier just gets slower than the sim predicted, which is exactly
/// the divergence a calibration exists to measure. Compute in `u128` so an
/// absurd bytes/sec cannot wrap around into a *smaller*, silently wrong rate.
pub fn rate_from_bytes_per_sec(bytes_per_sec: u64) -> String {
    let bits = bytes_per_sec as u128 * 8;

    const KBIT: u128 = 1_000;
    const MBIT: u128 = 1_000_000;
    const GBIT: u128 = 1_000_000_000;

    if bits >= GBIT {
        scaled(bits, GBIT, "gbit")
    } else if bits >= MBIT {
        scaled(bits, MBIT, "mbit")
    } else if bits >= KBIT {
        scaled(bits, KBIT, "kbit")
    } else {
        format!("{bits}bit")
    }
}

fn scaled(value: u128, unit: u128, suffix: &str) -> String {
    let whole = value / unit;
    let tenth = (value % unit) * 10 / unit;
    if tenth == 0 {
        format!("{whole}{suffix}")
    } else {
        format!("{whole}.{tenth}{suffix}")
    }
}

/// Render a [`Duration`] as a `netem` delay string in milliseconds.
///
/// [`consortium_nix::cascade::NetworkProfile::latency`] is a `Duration`, so
/// this is the other half of the sim-to-container mapping. Sub-millisecond
/// values keep three decimals so a `500us` profile does not collapse to `0ms`.
pub fn delay_from_duration(d: Duration) -> String {
    let micros = d.as_micros();
    if micros == 0 {
        return "0ms".to_string();
    }
    if micros < 1_000 {
        // 500us -> "500us"; 1us granularity is finer than netem needs.
        return format!("{micros}us");
    }
    scaled(micros, 1_000, "ms")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_profile_is_rejected_rather_than_emitting_a_silent_no_op() {
        // This is the whole reason qdisc_args returns Result. `tc qdisc replace
        // dev eth0 root netem` exits 0 and shapes nothing, so a harness that
        // only checked the exit status would happily report a shaped cluster
        // that is unshaped.
        let err = NetemProfile::default().qdisc_args(DEFAULT_IFACE);
        assert!(err.is_err(), "empty profile must not build a command");
        let msg = err.unwrap_err();
        assert!(
            msg.contains("shapes nothing"),
            "error should explain the silent no-op, got: {msg}"
        );
    }

    #[test]
    fn delay_only_profile_builds_expected_qdisc_args() {
        let args = NetemProfile::with_delay("20ms")
            .qdisc_args(DEFAULT_IFACE)
            .expect("delay-only profile is valid");
        assert_eq!(
            args,
            vec!["qdisc", "replace", "dev", "eth0", "root", "netem", "delay", "20ms"]
        );
    }

    #[test]
    fn jitter_is_a_second_positional_delay_argument() {
        let args = NetemProfile::with_delay("20ms")
            .jitter("5ms")
            .qdisc_args(DEFAULT_IFACE)
            .expect("delay+jitter is valid");
        assert_eq!(
            args,
            vec!["qdisc", "replace", "dev", "eth0", "root", "netem", "delay", "20ms", "5ms"]
        );
    }

    #[test]
    fn jitter_without_delay_is_rejected_at_construction() {
        let err = NetemProfile::default()
            .jitter("5ms")
            .qdisc_args(DEFAULT_IFACE);
        assert!(
            err.is_err(),
            "jitter alone is invalid netem; must fail before exec"
        );
        assert!(err.unwrap_err().contains("requires"));
    }

    #[test]
    fn rate_and_loss_appear_after_delay_in_a_stable_order() {
        let args = NetemProfile::with_delay("10ms")
            .rate("100mbit")
            .loss("0.5%")
            .qdisc_args("eth1")
            .expect("full profile is valid");
        assert_eq!(
            args,
            vec![
                "qdisc", "replace", "dev", "eth1", "root", "netem", "delay", "10ms", "rate",
                "100mbit", "loss", "0.5%"
            ]
        );
    }

    #[test]
    fn rate_only_profile_is_valid() {
        let args = NetemProfile::with_rate("10mbit")
            .qdisc_args(DEFAULT_IFACE)
            .expect("rate-only profile is valid");
        assert_eq!(
            args,
            vec!["qdisc", "replace", "dev", "eth0", "root", "netem", "rate", "10mbit"]
        );
    }

    #[test]
    fn limit_is_emitted_first_so_a_low_rate_does_not_overrun_the_default_queue() {
        // netem's default queue is 1000 packets. At 2mbit that is under 1.5MB
        // of buffer, and a 2MB transfer overruns it — netem drops the excess and
        // the measured time becomes a TCP-retransmit artefact instead of the
        // rate that was asked for. The limit has to be settable for a timed
        // transfer to measure the rate at all.
        let args = NetemProfile::with_rate("2mbit")
            .limit("10000")
            .qdisc_args(DEFAULT_IFACE)
            .expect("rate+limit is valid");
        assert_eq!(
            args,
            vec![
                "qdisc", "replace", "dev", "eth0", "root", "netem", "limit", "10000", "rate",
                "2mbit"
            ]
        );
    }

    #[test]
    fn a_limit_on_its_own_is_not_an_empty_profile() {
        // `is_empty` guards the silent no-op. limit alone is a real change to
        // the qdisc, so it must not be rejected as a no-op.
        let profile = NetemProfile::default().limit("5000");
        assert!(!profile.is_empty());
        assert!(profile.qdisc_args(DEFAULT_IFACE).is_ok());
    }

    #[test]
    fn custom_interface_is_honoured() {
        let args = NetemProfile::with_delay("1ms")
            .qdisc_args("eth9")
            .expect("valid profile");
        assert!(args.windows(2).any(|w| w == ["dev", "eth9"]));
    }

    #[test]
    fn clear_args_targets_the_root_qdisc() {
        assert_eq!(
            NetemProfile::clear_args(DEFAULT_IFACE),
            vec!["qdisc", "del", "dev", "eth0", "root"]
        );
    }

    #[test]
    fn bytes_per_sec_maps_to_the_largest_exact_netem_unit() {
        // 100 MiB/s is 104857600 B/s = 838860800 bit/s = 838.8mbit. The sim's
        // own default edge bandwidth is exactly that
        // (consortium-nix cascade_strategies.rs: DEFAULT_BW_BYTES_SEC), so this
        // is the value a calibration scenario hits first.
        assert_eq!(rate_from_bytes_per_sec(100 * 1024 * 1024), "838.8mbit");
        assert_eq!(rate_from_bytes_per_sec(1_000_000_000), "8gbit");
        assert_eq!(rate_from_bytes_per_sec(1_500_000), "12mbit");
        assert_eq!(rate_from_bytes_per_sec(512_000), "4mbit");
        assert_eq!(rate_from_bytes_per_sec(1_250), "10kbit");
        assert_eq!(rate_from_bytes_per_sec(999), "7.9kbit");
        assert_eq!(rate_from_bytes_per_sec(0), "0bit");
    }

    #[test]
    fn rate_string_is_bits_per_second_not_bytes_per_second() {
        // The failure this guards is not a panic, it is a number that is wrong
        // in a way every command accepts. `tc` parses "1mbit" as 1_000_000
        // bits/sec = 125_000 bytes/sec, so rendering a bytes/sec figure without
        // converting shapes the link 8x tighter than the sim model, and the
        // resulting "divergence" is the conversion error rather than the
        // simulator's.
        let one_mib_per_sec = 1024 * 1024;
        assert_eq!(
            rate_from_bytes_per_sec(one_mib_per_sec),
            "8.3mbit",
            "1 MiB/s is 8.4 Mbit/s, not 1 Mbit/s"
        );
        // Cross-check against the hand-computed value, independent of the
        // scaling arithmetic: 125_000 bytes/sec is exactly 1mbit.
        assert_eq!(rate_from_bytes_per_sec(125_000), "1mbit");
        assert_eq!(rate_from_bytes_per_sec(1), "8bit");
    }

    #[test]
    fn absurd_bandwidth_does_not_wrap_into_a_smaller_rate() {
        // u64::MAX bytes/sec is 1.47e20 bit/s. Multiplying by 8 in u64 would wrap
        // negative and render a rate *smaller* than a realistic link — the same
        // silent-wrong-number failure as the unit mixup, one magnitude worse.
        let wrapped = rate_from_bytes_per_sec(u64::MAX);
        assert_eq!(wrapped, "147573952589.6gbit");
    }

    #[test]
    fn duration_maps_to_a_netem_delay_string() {
        assert_eq!(delay_from_duration(Duration::from_millis(20)), "20ms");
        assert_eq!(delay_from_duration(Duration::from_millis(1)), "1ms");
        assert_eq!(delay_from_duration(Duration::from_micros(500)), "500us");
        assert_eq!(delay_from_duration(Duration::from_secs(2)), "2000ms");
        assert_eq!(delay_from_duration(Duration::ZERO), "0ms");
    }

    #[test]
    fn sim_profile_fields_round_trip_into_a_usable_command() {
        // The end-to-end mapping a calibration scenario performs: take the two
        // numbers the sim stores and produce one container-side command.
        let bandwidth_bytes_per_sec = 100 * 1024 * 1024u64;
        let latency = Duration::from_millis(20);
        let profile = NetemProfile::with_delay(delay_from_duration(latency))
            .rate(rate_from_bytes_per_sec(bandwidth_bytes_per_sec));
        let args = profile.qdisc_args(DEFAULT_IFACE).expect("mapped profile");
        assert!(args.contains(&"838.8mbit".to_string()), "{args:?}");
        assert!(args.contains(&"20ms".to_string()), "{args:?}");
    }
}
