# Noisy Neighbour Observer, Design Context

> The "why" behind the project: the problem, the design and the cardinality rules.
> For commands and invariants see [`AGENTS.md`](./AGENTS.md).
> For usage and status see [`README.md`](./README.md).
> Exact code shapes live in the source. This document does not copy them.

## Problem

Pods on a shared node compete for CPU. A pod that is runnable but waits in the kernel run queue gets latency that its own CPU metrics do not explain.

The agent measures the run queue wait of each pod. It also names the workload that took the CPU. Both leave the node as Prometheus metrics.

## Netflix approach

The design follows the Netflix post "Noisy Neighbor Detection with eBPF".

- `sched_wakeup` and `sched_wakeup_new` record the enqueue time of a task.
- `sched_switch` computes `now - enqueue_ts` for the incoming task.
- The cgroup id of a task maps the sample to a container.
- The preemption cause is the noisy neighbour signal. Latency alone misleads, because CFS quota throttling also raises run queue latency.

## Deviations from Netflix

1. **Aggregate in the kernel.** BPF maps hold histograms and counters, and userspace reads them on a timer. There is no ring buffer, no rate limit and no sampling bias.
2. **Re-enqueue preempted tasks.** On `sched_switch`, a `prev` task that is still `TASK_RUNNING` gets a new enqueue time. Without it, the wait after a preemption goes unmeasured.
3. **Classify the cause in userspace.** The kernel records raw victim and culprit cgroup pairs. Userspace knows which cgroups share a pod.

## Cardinality rules

Cardinality is a core goal. Each agent pod must keep a small, bounded and stable set of series.

- The default label is the workload, not the pod. Pod names change on each rollout and would create new series.
- Histograms use few buckets. The kernel keeps fine buckets and the agent folds them into the configured bounds.
- Label sets are closed. Never export a pod UID, container id, cgroup id, pid, comm, IP or node label.
- Each victim keeps at most K culprit workloads. Other culprits sum into one `_other` series.
- A hard series budget applies to each agent pod. A workload that does not fit goes to `_overflow` for its whole life on the node, so counters stay monotonic.
- Histograms are classic, not native. The backend is VictoriaMetrics, which cannot scrape native histograms and converts them to bucket series on ingestion.
- Target labels are stable. The scrape config relabels the node name to `node` and drops `pod` and `instance`.
- cAdvisor already exports the CFS throttle counters. The agent does not duplicate them.

A metric or label that changes the series count needs a cardinality review in its PR.

## Target environments

- Primary: EKS and GKE Standard, on amd64 and arm64.
- Kernel 6.1 or newer, with BTF and cgroup v2 only.
- Unsupported: EKS Fargate, EKS AL2, GKE Autopilot and GKE Sandbox pods.
- The agent runs as root with `CAP_BPF` and `CAP_PERFMON`, not as a privileged container.

## Design decisions

- Aya (pure Rust) for the eBPF programs and the loader.
- Metrics through `prometheus-client`, not OTLP.
- Static musl binary with `tikv-jemallocator`, built with `cargo-zigbuild`.
- A Helm chart deploys the agent as a DaemonSet. An operator adds no value for one stateless agent.
