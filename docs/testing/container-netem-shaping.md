# Container-tier network shaping: mapping a sim `NetworkProfile` onto `tc netem`

Status: implemented. Shape API: `crates/consortium-test-harness/src/netem.rs` + `DockerCluster::{apply_netem, clear_netem}`.
Verified by: `cargo test -p consortium --features docker-tests --test docker_integration`.

[ADR 0001](../adr/0001-hybrid-test-framework.md) §7.1 names this as "the single highest
fidelity-per-day item in the program", because it is what makes a sim↔container comparison a
*fabrication* comparison rather than a loopback one. This file records what the two models
actually are, which parts of one map onto the other, and which parts do not — the part that is
easy to get wrong and expensive to discover late.

## 1. The two models

**Sim tier** — `NetworkProfile` (`crates/consortium-nix/src/cascade.rs`):

| Field | Type | Meaning |
| --- | --- | --- |
| `bandwidth` | `HashMap<(src, tgt), u64>` | bytes/sec, per **directed edge** |
| `latency` | `HashMap<(src, tgt), Duration>` | one-way, per **directed edge** |
| `partitions` | `HashSet<(src, tgt)>` | directed edges that pass nothing |
| `nodes` | `HashMap<NodeId, NodeSpec>` | per-node `uplink` / `downlink` bytes/sec, for contention |

`effective_bandwidth(src, tgt, src_out, tgt_in, default)` is
`min(edge_bw, src.uplink / src_out, tgt.downlink / tgt_in)` — a *contention* model. A node
without a `NodeSpec` has no such term, and the math degenerates to the per-edge number.

**Container tier** — one root `netem` qdisc per node, on that container's only interface
(`eth0`, inside the container's own network namespace).

That asymmetry is the whole story: **the sim's model is per-edge, and a netem qdisc is
per-node.** One qdisc covers every peer that node talks to, aggregated.

## 2. What maps, exactly

| Sim | netem knob | Conversion | Exact? |
| --- | --- | --- | --- |
| `latency[(a, b)]` | `delay` on **a**'s `eth0` | `delay_from_duration` | **yes**, per direction |
| `NodeSpec.uplink` | `rate` on that node's `eth0` | `rate_from_bytes_per_sec` | **yes** |
| `NodeSpec.downlink` of b | `rate` on **a**'s `eth0` | `rate_from_bytes_per_sec` | **yes** — it is a's uplink |
| `bandwidth[(a, b)]` | `rate` on a's `eth0` | `rate_from_bytes_per_sec` | **only** if a has one peer |
| `partitions` | `loss 100%` on the sender's `eth0` | — | **yes**, for that node's whole egress |
| — | `loss` | — | no sim-side loss model exists |

Two conversion notes, both of which have already cost a day:

- **`tc` rate units are bits per second.** The sim stores bytes/sec. `rate_from_bytes_per_sec`
  multiplies by 8; a figure passed through unconverted is an **8x** error that every command
  accepts silently and that shows up only as a container tier that is slower than the sim
  predicted. `tc` also has no bytes unit, so there is no way to be right by accident.
- **Use the largest exact unit.** `tc` parses `1mbit` as 1 000 000 bits/sec = 125 000 byte/sec.

Delay is a clean per-direction mapping: `netem` delays a packet on egress, the sim's
`latency[(a, b)]` is the one-way cost of a→b, so a's `delay` *is* `latency[(a, b)]`. A round
trip over a link with shaping on both ends is `latency[(a,b)] + latency[(b,a)]`; with shaping on
one end it is that one term.

## 3. What does not map, and what to do instead

- **Per-edge bandwidth with a multi-peer sender.** A single qdisc cannot express
  `bandwidth[(a, b)] != bandwidth[(a, c)]`. Options, in increasing order of work: give every node
  a `NodeSpec` and make the scenario topology one where per-edge and per-node agree (star, or
  disjoint pairs); accept the aggregation and say so in the divergence report; or build
  per-destination classification (`prio` bands + `u32` filters, or netem on per-destination
  IFB devices). This issue builds the first option and deliberately not the third.
- **Contention.** `effective_bandwidth` divides a node's uplink by its fan-out for that round.
  A static qdisc cannot know the round's fan-out, so shaping a node with a per-node uplink cap
  reproduces `NodeSpec.uplink`, not `effective_bandwidth`. For a calibration scenario, cap by
  `NodeSpec` and let the sim's contention term be one of the things being calibrated — that is
  the interesting part, and a static qdisc that reproduced it would be assuming the answer.
- **In-node loss.** There is no sim-side loss model, so leave `NetemProfile::loss` unset unless
  the scenario is deliberately testing something the sim cannot express. A divergence caused by
  loss is not a simulator error.
- **A partition is not a node kill.** `loss 100%` is the analogue of `partitions[(a, b)]` for
  everything leaving a. `ip link set dev eth0 down` is closer to the sim's `KillNodeAtRound` for
  that node. Pick deliberately; they are different scenarios.

## 4. Three environment requirements, and how each one fails silently

1. **`tc` must exist in the image** — `iproute2` in `tests/docker/Dockerfile.ssh-node`.
   Without it every call is `command not found`.
2. **The node needs `CAP_NET_ADMIN`** — granted to every generated service by `plan_compose`.
   Docker's default capability set omits it, so an unprivileged container's `tc qdisc` fails
   `EPERM` *even as root*. This is the failure mode with the worst signature: the cluster starts,
   the lane runs, and nothing is ever shaped.
3. **Shaping is egress-only.** Traffic arriving at a node is untouched, and the kernel default
   qdisc on a container's veth is `noqueue`, not `pfifo_fast` — so teardown asserts the
   *absence* of `netem`, not the presence of a particular default.

A fourth, quieter one: **`docker exec`'s output does not reliably traverse the container's
`eth0`.** The exec attach stream is not the same path as a connection to the node's published
port, so do not measure imposed bandwidth or delay through `docker exec`. Use the real SSH path —
the tests below do, and that is why their numbers mean anything.

## 5. Asserting that shaping happened

`tc qdisc replace dev eth0 root netem` with no netem options **exits 0 and shapes nothing**. A
harness that checks exit codes reports a shaped cluster that is unshaped. `NetemProfile::qdisc_args`
therefore rejects an empty profile, and `apply_netem` propagates that rather than running the
command anyway.

But rejecting the empty profile is not enough, because requirement 2 above can fail with a
non-empty profile. The gate has to be the **observed** network:

- a delay of X must appear in a measured round trip;
- a rate cap must appear in a timed transfer;
- teardown must make the node measurably fast again.

`crates/consortium/tests/docker_integration.rs` does this over the host's SSH path to the node's
published port (ingress unshaped, egress shaped, so an imposed delay is paid exactly once, in the
response). It uses `login-01` — present in the small topology and used by no other test — so a
leaked qdisc cannot perturb the rest of the lane, and a `Drop` guard clears shaping even when an
assertion panics. Teardown is idempotent, because the common case is a node no test shaped, and
a teardown that failed there would either fail every such test or teach everyone to ignore it.

When a `rate` is set, also set `limit`. `netem`'s default queue is 1000 packets; at a few Mbit/s
that is under a couple of megabytes of buffer, and a larger transfer overruns it and netem
**drops** the overflow. The transfer still completes — on TCP retransmits — so the measurement is
loss recovery rather than the rate that was asked for.

## 6. Worked example: one reference scenario

The shape a [CON-99](/CON/issues/CON-99) calibration should start from: a `controller` hub with
four `compute` workers (the names `DockerCluster` actually generates), symmetric 100 MiB/s links,
20 ms one-way, one dropped edge.

```rust
use std::time::Duration;
use consortium_test_harness::{delay_from_duration, rate_from_bytes_per_sec, NetemProfile, DEFAULT_IFACE};

// Per-edge and per-node agree here only because the topology is a star: every
// worker has exactly one peer. That is the precondition for a single qdisc per
// node to be a faithful representation, and it should be stated in the report
// rather than assumed.
const LINK_BYTES_PER_SEC: u64 = 100 * 1024 * 1024;
const LINK_LATENCY: Duration = Duration::from_millis(20);

// Same conversion the sim uses; 100 MiB/s is 838.8mbit, not 104.8mbit.
let profile = NetemProfile::with_delay(delay_from_duration(LINK_LATENCY))
    .rate(rate_from_bytes_per_sec(LINK_BYTES_PER_SEC))
    .limit("100000");

for node in ["compute-01", "compute-02", "compute-03", "compute-04"] {
    cluster.apply_netem(node, &profile, DEFAULT_IFACE)?;
}
// The dropped edge: everything leaving compute-03 stops, which is what
// `partitions` means. `loss 100%` on its own is a valid non-empty profile.
cluster.apply_netem(
    "compute-03",
    &NetemProfile::default().loss("100%"),
    DEFAULT_IFACE,
)?;
// ... run the scenario, collect per-round completion times ...

for node in cluster.node_names() {
    cluster.clear_netem(&node, DEFAULT_IFACE)?;
}
```

The scenario is then expressible in both tiers from one set of numbers, and the divergence report
compares per-round completion times under a network both tiers were told about. That is the
difference between a calibration and a loopback measurement.
