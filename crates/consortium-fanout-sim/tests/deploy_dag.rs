//! Deploy-DAG correctness at fleet scale.
//!
//! The unit tests in `dag_sim` pin the cost model and the scheduler's
//! internal consistency on small inputs. This file exercises the same
//! simulator at the scales the harness actually has to survive — 64,
//! 256, and 1024 hosts — and asserts the three invariants that matter
//! for a real deploy:
//!
//! 1. **No host is left behind.** Every host appears in all five
//!    stages, and every healthy host reaches `activate`. When a host
//!    fails, the *only* thing that changes is that host's own chain.
//! 2. **Ordering is respected.** A stage never starts before its
//!    predecessor finished, and the real `DagExecutor` never reports an
//!    ordering violation.
//! 3. **Convergence and timing are reproducible.** The same seed
//!    produces a byte-identical report, per-stage, at every scale.

use std::time::Duration;

use consortium_fanout_sim::dag_sim::{
    DagFailures, DagNetwork, DagSimConfig, DeployDagSim, DeployStage, StageCost, StageSchedule,
};
use consortium_fanout_sim::fixtures::{BandwidthDistribution, UplinkDistribution};
use consortium_fanout_sim::link::{LinkDirection, LinkModel, PacketLoss};

/// The three scales this harness is specified against.
const SCALES: [usize; 3] = [64, 256, 1024];

/// A realistic-but-not-pathological fleet: mixed link speeds, 3% loss,
/// noisy latency, and an uplink that is the real bottleneck on copy.
fn realistic(hosts: usize, seed: u64) -> DagSimConfig {
    DagSimConfig {
        seed,
        hosts,
        closure_bytes: 100 * 1024 * 1024,
        builder_bandwidth: 2 * 1024 * 1024 * 1024,
        schedule: StageSchedule::production(16),
        cost: StageCost::default(),
        network: DagNetwork {
            bandwidth: BandwidthDistribution::Bimodal {
                slow: 10 * 1024 * 1024,
                fast: 1024 * 1024 * 1024,
                fast_fraction: 0.3,
            },
            uplinks: Some(UplinkDistribution::Bimodal {
                slow: 20 * 1024 * 1024,
                fast: 512 * 1024 * 1024,
                fast_fraction: 0.4,
            }),
            latency: Duration::from_millis(2),
        },
        link: LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.03))
            .with_jitter(0.2)
            .with_direction(LinkDirection::UplinkLimited),
        host_variation: 0.3,
        failures: DagFailures::None,
    }
}

#[test]
fn every_scale_converges_completely_on_a_realistic_fleet() {
    for n in SCALES {
        let sim = DeployDagSim::new(realistic(n, 0x5eed_0000 + n as u64));
        let r = sim.run();
        assert!(
            r.is_success(),
            "n={n} reported failures: {:?}",
            r.failures
        );
        assert_eq!(r.converged_hosts, n, "n={n} left hosts behind");
        for stage in DeployStage::ALL {
            assert_eq!(
                r.converged_at(stage),
                n,
                "n={n} stage {} lost hosts",
                stage.name()
            );
        }
    }
}

#[test]
fn no_host_is_left_behind_in_the_real_executor_at_any_scale() {
    for n in SCALES {
        let sim = DeployDagSim::new(realistic(n, 0x5eed_1000 + n as u64));
        let d = sim.validate();
        assert!(d.is_success(), "n={n} real DAG failed: {:?}", d.failed);
        // Every host × every stage completed. This is the "no host
        // left behind" check against the real executor, not the
        // virtual scheduler.
        assert_eq!(
            d.completed.len(),
            n * DeployStage::ALL.len(),
            "n={n} real DAG completed {} of {}",
            d.completed.len(),
            n * DeployStage::ALL.len()
        );
        assert_eq!(d.cancelled.len(), 0, "n={n} cancelled work");
        assert_eq!(d.skipped.len(), 0, "n={n} skipped work");
    }
}

#[test]
fn reports_are_reproducible_at_every_scale() {
    for n in SCALES {
        let seed = 0xABCD_0000 + n as u64;
        let a = DeployDagSim::new(realistic(n, seed)).run();
        let b = DeployDagSim::new(realistic(n, seed)).run();
        assert_eq!(a, b, "n={n} was not reproducible from its seed");
        // The per-stage numbers are the thing downstream reports
        // consume, so pin them individually too.
        for (x, y) in a.stages.iter().zip(b.stages.iter()) {
            assert_eq!(x.makespan, y.makespan, "n={n} {}", x.stage.name());
            assert_eq!(x.total_work, y.total_work, "n={n} {}", x.stage.name());
            assert_eq!(
                x.converged_hosts, y.converged_hosts,
                "n={n} {}",
                x.stage.name()
            );
        }
    }
}

#[test]
fn ordering_is_respected_and_never_reported_by_the_executor() {
    for n in SCALES {
        for limit in [1usize, 4, 16] {
            let mut cfg = realistic(n, 0x1111_2222);
            cfg.schedule = StageSchedule::production(limit);
            let sim = DeployDagSim::new(cfg);
            let d = sim.validate();
            for (id, err) in &d.failed {
                assert!(
                    !err.contains("ordering violated"),
                    "n={n} limit={limit}: {id} reported {err}"
                );
            }
        }
    }
}

#[test]
fn per_stage_timing_is_monotone_in_the_dependency_chain() {
    // Because stage k+1 of a host cannot start before stage k of that
    // host finished, the DAG makespan is at least the sum of the two
    // longest per-host chain segments. A cheaper, exact check: the
    // whole DAG cannot be shorter than the serial eval stage, which is
    // the only limit-1 stage.
    for n in SCALES {
        let r = DeployDagSim::new(realistic(n, 0x9999_0000 + n as u64)).run();
        let eval = r.stage(DeployStage::Eval);
        assert!(
            r.wall_time >= eval.makespan,
            "n={n}: wall {:?} < serial eval {:?}",
            r.wall_time,
            eval.makespan
        );
        // Eval is serial, so its makespan is the sum of every host's
        // eval cost — nothing overlaps.
        assert!(eval.parallel_efficiency >= 1.0 - 1e-9);
        assert_eq!(eval.peak_concurrency, 1, "n={n} eval was not serial");
    }
}

#[test]
fn copy_is_the_dominant_stage_at_scale() {
    // 100 MB over a fabric with loss and a contended uplink should cost
    // far more than a local eval or a short activation. This is the
    // claim the whole harness exists to make measurable.
    for n in SCALES {
        let r = DeployDagSim::new(realistic(n, 0x7777_0000 + n as u64)).run();
        let copy = r.stage(DeployStage::Copy);
        for other in [DeployStage::Eval, DeployStage::Build, DeployStage::Activate] {
            let o = r.stage(other);
            assert!(
                copy.total_work > o.total_work,
                "n={n}: copy {:?} not above {} {:?}",
                copy.total_work,
                other.name(),
                o.total_work
            );
        }
    }
}

#[test]
fn widening_parallelism_shortens_the_deploy_when_the_link_is_not_the_bottleneck() {
    // Plenty of uplink ⇒ each copy stream gets its own edge bandwidth ⇒
    // more workers buys real wall time. Eval and activate are capped, so
    // the win has to come from build/health/copy.
    for n in SCALES {
        let fat = |limit: usize| {
            let mut c = realistic(n, 0x4242_0000 + n as u64);
            c.schedule = StageSchedule::production(limit);
            c.network.uplinks = Some(UplinkDistribution::Uniform(100 * 1024 * 1024 * 1024));
            DeployDagSim::new(c).run()
        };
        let narrow = fat(2);
        let wide = fat(32);
        assert!(
            wide.wall_time < narrow.wall_time,
            "n={n}: 32 workers ({:?}) not faster than 2 ({:?}) on an uncongested link",
            wide.wall_time,
            narrow.wall_time
        );
        assert_eq!(wide.converged_hosts, narrow.converged_hosts);
    }
}

#[test]
fn a_saturated_source_uplink_caps_what_parallelism_can_buy() {
    // The counterweight to the test above, and the reason the model
    // tracks per-node uplink capacity at all. When every copy stream
    // leaves through one saturated link, total fabric throughput is
    // fixed no matter how many workers we point at it. The harness must
    // not let a worker-count increase masquerade as a deploy speedup —
    // if it did, capacity planning would be reading fiction.
    for n in SCALES {
        let thin = |limit: usize| {
            let mut c = realistic(n, 0x4343_0000 + n as u64);
            c.schedule = StageSchedule::production(limit);
            // 25 MB/s shared by every concurrent copy stream.
            c.network.uplinks = Some(UplinkDistribution::Uniform(25 * 1024 * 1024));
            DeployDagSim::new(c).run()
        };
        let narrow = thin(2);
        let wide = thin(32);
        let gain = 1.0 - (wide.wall_time.as_secs_f64() / narrow.wall_time.as_secs_f64());
        let gain_pct = gain * 100.0;
        assert!(
            gain < 0.20,
            "n={n}: 8x the copy workers bought {gain_pct:.1}% wall time on a saturated link"
        );
        // Convergence is unaffected either way.
        assert_eq!(wide.converged_hosts, narrow.converged_hosts);
        // And the copy stage is genuinely the constraint in both runs.
        for r in [&narrow, &wide] {
            let copy = r.stage(DeployStage::Copy);
            assert_eq!(
                copy.peak_concurrency,
                copy.limit,
                "n={n}: copy never used its slots"
            );
        }
    }
}

#[test]
fn a_failed_host_does_not_disturb_the_rest_of_the_fleet() {
    // 3% of hosts fail at build. Everything else must still complete,
    // and the surviving count must be exactly right — this is the
    // isolation property that `ErrorPolicy::ContinueIndependent` is
    // supposed to give us.
    for n in SCALES {
        let mut cfg = realistic(n, 0x3131_0000 + n as u64);
        let every = 32.max(n / 16);
        cfg.failures = DagFailures::EveryNthHost {
            stage: DeployStage::Build,
            every,
            reason: "nix build: out of memory".to_string(),
        };
        let expected_failures = n.div_ceil(every);

        let sim = DeployDagSim::new(cfg);
        let r = sim.run();
        assert_eq!(
            r.failures.len(),
            expected_failures,
            "n={n} every={every}: wrong failure count"
        );
        assert_eq!(
            r.converged_at(DeployStage::Build),
            n - expected_failures
        );
        assert_eq!(r.converged_at(DeployStage::Activate), n - expected_failures);
        assert_eq!(r.converged_hosts, n - expected_failures);
        // Upstream of the failure, nothing is lost.
        assert_eq!(r.converged_at(DeployStage::Eval), n);

        let d = sim.validate();
        assert_eq!(d.failed.len(), expected_failures);
        assert_eq!(
            d.cancelled.len(),
            expected_failures * 2,
            "n={n}: copy+activate should cancel for every failed host"
        );
        let activate_prefix = "activate:";
        let activated = d
            .completed
            .iter()
            .filter(|id| id.0.starts_with(activate_prefix))
            .count();
        assert_eq!(
            activated,
            n - expected_failures,
            "n={n}: real executor activated a host whose build failed"
        );
    }
}

#[test]
fn loss_inflation_shows_up_in_copy_timing_at_every_scale() {
    for n in SCALES {
        let clean = {
            let mut c = realistic(n, 0x5151_0000 + n as u64);
            c.link = LinkModel::new().with_direction(LinkDirection::UplinkLimited);
            DeployDagSim::new(c).run()
        };
        let lossy = DeployDagSim::new(realistic(n, 0x5151_0000 + n as u64)).run();
        assert!(
            lossy.stage(DeployStage::Copy).total_work > clean.stage(DeployStage::Copy).total_work,
            "n={n}: 3% loss did not inflate copy work"
        );
        // Convergence is a link property, not a timing one.
        assert_eq!(lossy.converged_hosts, clean.converged_hosts);
    }
}

#[test]
fn asymmetric_uplinks_are_actually_charged() {
    // With 32 builders pushing concurrently, the source uplink is the
    // binding constraint. Cranking it down must inflate copy cost.
    for n in SCALES {
        let mut fat = realistic(n, 0x6161_0000 + n as u64);
        fat.network.uplinks = Some(UplinkDistribution::Uniform(10 * 1024 * 1024));
        let mut thin = fat.clone();
        thin.network.uplinks = Some(UplinkDistribution::Uniform(1024 * 1024));
        let (a, b) = (
            DeployDagSim::new(fat).run(),
            DeployDagSim::new(thin).run(),
        );
        assert!(
            b.stage(DeployStage::Copy).total_work > a.stage(DeployStage::Copy).total_work,
            "n={n}: narrowing the uplink did not slow copy"
        );
    }
}

#[test]
fn jitter_changes_costs_but_never_convergence() {
    for n in SCALES {
        let mut c = realistic(n, 0x7171_0000 + n as u64);
        c.host_variation = 0.0;
        c.link = c.link.with_jitter(0.0);
        let steady = DeployDagSim::new(c.clone()).run();
        c.link = c.link.with_jitter(0.5);
        let noisy = DeployDagSim::new(c).run();
        assert_eq!(steady.converged_hosts, noisy.converged_hosts);
        assert_ne!(
            steady.stage(DeployStage::Copy).total_work,
            noisy.stage(DeployStage::Copy).total_work,
            "n={n}: 50% jitter had no effect on copy cost"
        );
    }
}
