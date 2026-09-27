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
//! # Design note: why command construction is separate from execution
//!
//! [`NetemProfile::qdisc_args`] and [`NetemProfile::clear_args`] are pure and
//! unit-tested without Docker. Only [`crate::DockerCluster::apply_netem`] and
//! [`crate::DockerCluster::clear_netem`] shell out, because the interesting
//! assertions (did the delay actually show up in a measured round trip?)
//! require a running container and belong in the `docker-tests` lane.
//!
//! # The silent no-op this module exists to prevent
//!
//! `tc qdisc replace dev eth0 root netem` with no netem options is accepted by
//! `tc`, **exits 0, and shapes nothing**. A harness that only checked the exit
//! code would report a shaped cluster that is unshaped. [`qdisc_args`] therefore
//! rejects an empty profile instead of emitting that command.

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

    /// True when no knob at all is set.
    pub fn is_empty(&self) -> bool {
        self.delay.is_none() && self.jitter.is_none() && self.rate.is_none() && self.loss.is_none()
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
pub fn rate_from_bytes_per_sec(bytes_per_sec: u64) -> String {
    const KBIT: u64 = 1_000;
    const MBIT: u64 = 1_000_000;
    const GBIT: u64 = 1_000_000_000;

    if bytes_per_sec >= GBIT {
        scaled(bytes_per_sec, GBIT, "gbit")
    } else if bytes_per_sec >= MBIT {
        scaled(bytes_per_sec, MBIT, "mbit")
    } else if bytes_per_sec >= KBIT {
        scaled(bytes_per_sec, KBIT, "kbit")
    } else {
        format!("{bytes_per_sec}bit")
    }
}

fn scaled(value: u64, unit: u64, suffix: &str) -> String {
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
    scaled(micros as u64, 1_000, "ms")
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
        // 100 MiB/s is 104857600 B/s = 104.8576 Mbit/s, truncated to one
        // decimal. The sim's own default bandwidth is 100 MiB/s
        // (DeterministicExecutor::DEFAULT_BW_BYTES_SEC), so this is the value a
        // calibration scenario hits first.
        assert_eq!(rate_from_bytes_per_sec(100 * 1024 * 1024), "104.8mbit");
        assert_eq!(rate_from_bytes_per_sec(1_000_000_000), "1gbit");
        assert_eq!(rate_from_bytes_per_sec(1_500_000), "1.5mbit");
        assert_eq!(rate_from_bytes_per_sec(512_000), "512kbit");
        assert_eq!(rate_from_bytes_per_sec(999), "999bit");
        assert_eq!(rate_from_bytes_per_sec(0), "0bit");
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
        assert!(args.contains(&"104.8mbit".to_string()));
        assert!(args.contains(&"20ms".to_string()));
    }
}
