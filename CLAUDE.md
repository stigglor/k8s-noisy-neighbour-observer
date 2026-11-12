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

## Kernel struct offsets

The programs read five kernel struct members: `task_struct.pid`, `task_struct.cgroups`, `css_set.dfl_cgrp`, `cgroup.kn` and `kernfs_node.id`.

- aya-ebpf has no CO-RE for Rust (aya#349). Offsets from `aya-tool` bindings are fixed to the build kernel.
- The agent reads the offsets from `/sys/kernel/btf/vmlinux` at startup with `btf-rs`. It descends into anonymous members.
- The agent passes the offsets to the programs as a global in `.rodata`.
- The programs dereference BTF pointers directly, with no `bpf_probe_read_kernel`. The verifier accepts this because aya freezes `.rodata`, so the offsets are constants.
- A missing member, or a member that is not byte aligned, stops the agent at startup.

## Cardinality rules

Cardinality is a core goal. Each agent pod must keep a small, bounded and stable set of series.

- The default label is the workload, not the pod. Pod names change on each rollout and would create new series.
- Histograms use few buckets. The kernel bins each sample against the configured bounds, so the bucket count is fixed at load.
- Label sets are closed. Never export a pod UID, container id, cgroup id, pid, comm, IP or node label.
- Each victim keeps at most K culprit workloads. Other culprits sum into one `_other` series.
- A hard series budget applies to each agent pod. A workload that does not fit goes to `_overflow` for its whole life on the node, so counters stay monotonic.
- Histograms are classic, not native. The backend is VictoriaMetrics, which cannot scrape native histograms and converts them to bucket series on ingestion.
- Target labels are stable. The scrape config relabels the node name to `node` and drops `pod` and `instance`.
- The scrape must set `honor_labels: true`. Kubernetes service discovery adds `namespace`, `pod` and `container` target labels for the agent pod, which would otherwise rename the workload labels to `exported_*`.
- cAdvisor already exports the CFS throttle counters. The agent does not duplicate them.

A metric or label that changes the series count needs a cardinality review in its PR.

## Workload identity

The agent names the entity of each cgroup from the cgroup path and a watch on the pods of its node. The controller owner reference of a pod gives its workload, with no extra API watches.

| Controller owner | `workload_kind` | `workload` |
|---|---|---|
| ReplicaSet, pod label `pod-template-hash` | `Deployment` | ReplicaSet name without the `-<hash>` suffix |
| ReplicaSet, pod label `rollouts-pod-template-hash` | `Rollout` | ReplicaSet name without the `-<hash>` suffix |
| ReplicaSet with no hash suffix | `ReplicaSet` | ReplicaSet name |
| StatefulSet, DaemonSet | the owner kind | owner name |
| Job with a `-<8 or more digits>` suffix | `CronJob` | Job name without the suffix |
| Other Job | `Job` | Job name |
| Node (static pod) | `StaticPod` | pod name without the `-<node>` suffix |
| None | `Pod` | pod name |
| Any other kind | the owner kind | owner name |

- The kubelet names the cgroup of a static pod after its config hash. The mirror pod carries that hash in the `kubernetes.io/config.mirror` annotation.
- Every cgroup outside a pod goes to the `_system` entity, which never goes away.
- A cgroup that a complete walk of the tree cannot find also goes to `_system`. It is gone or outside the mounted tree, so it can never resolve. After a walk with a read error, the agent waits for the next walk.
- A cgroup in a pod that the watch does not know yet keeps its samples in a buffer. When the pod resolves, the agent adds the buffer to the entity.
- The agent drops the buffer of a cgroup that stays unresolved for 10 reads.
- At `container` granularity, a cgroup of a known pod with no container status (the pod sandbox) goes to `container="_pod"` after the 10 reads. A new container gets its status well within that time.
- The agent waits up to 60s for the first pod list at startup and exits if it does not come or holds no pods. So a missing RBAC rule or a wrong `NNO_NODE_NAME` stops the pod, not the samples.
- An unresolved cgroup never gets a temporary label. A later move to the real entity would make the temporary counter fall.
- An entity that does not fit in the series budget goes to `_overflow`. It stays there until it leaves the node, because a move would make a counter fall.
- An entity leaves the node when all of its pods completed or left, and no cgroup of it is in the map. The agent drops its series one read later and frees its budget. An entity that comes back starts again from zero.

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
