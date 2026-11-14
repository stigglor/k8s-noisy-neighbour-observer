# Noisy Neighbour Observer

A Kubernetes DaemonSet agent that finds noisy neighbours. It measures the run queue latency of each workload with eBPF and names the workloads that caused the wait. For the design see [`CLAUDE.md`](./CLAUDE.md).

## Status

The agent measures run queue latency with eBPF and exports one histogram for each workload on the node. It also exports the wait of each workload by cause and by culprit workload. A dashboard comes in a later change.

## Usage

Install the eBPF toolchain once (see [`AGENTS.md`](./AGENTS.md)):

```
make ebpf-toolchain
```

Run the agent as root on a Linux host, with a kubeconfig or in a cluster:

```
NNO_NODE_NAME=$(hostname) make run
```

Build the container image:

```
make docker-build
```

Deploy the agent to a cluster with the Helm chart in [`helm/noisy-neighbour-observer`](./helm/noisy-neighbour-observer/README.md). Its README lists the node requirements and the supported environments.

The agent reads these environment variables:

| Variable | Default | Use |
|---|---|---|
| `NNO_BIND_ADDR` | `[::]:9400` | Listen address. `[::]` also serves IPv4 when `net.ipv6.bindv6only` is 0, the Linux default. |
| `NNO_READ_INTERVAL` | `5s` | Time between two reads of the BPF maps. Units: `ns`, `us`, `ms`, `s`. |
| `NNO_HISTOGRAM_BOUNDS` | `10us,100us,500us,1ms,5ms,10ms,100ms,1s` | Bucket bounds of the latency histogram. At most 12, strictly increasing. |
| `NNO_NODE_NAME` | none, required | The node of the agent. The agent watches only the pods on it. In a pod, set it from `spec.nodeName` with the downward API. |
| `NNO_CGROUP_ROOT` | `/sys/fs/cgroup` | The cgroup v2 tree of the host. In a pod, mount the host `/sys/fs/cgroup` read only and point this at it, for example `/host/sys/fs/cgroup`. |
| `NNO_GRANULARITY` | `workload` | The labels of an entity: `workload` (`namespace`, `workload_kind`, `workload`), `pod` (adds `pod`) or `container` (adds `pod` and `container`). |
| `NNO_SERIES_BUDGET` | `1500` | The most per entity series. Each entity costs the bounds plus 8 series, and one more for each culprit series. `_system` and `_overflow` always hold theirs, so the budget must fit 2 x (bounds + 8). |
| `NNO_CULPRITS_PER_VICTIM` | `3` | Culprit workloads with their own series for each entity, at least 1. The other culprits sum into `culprit_workload="_other"`. |
| `NNO_MAX_CGROUPS` | `1024` | Cgroups the kernel tracks at a time. Each one costs 8 bytes plus 112 bytes per CPU. When the map is full, new cgroups lose samples, which `noisy_neighbour_dropped_samples_total{map="runq"}` counts. |
| `NNO_MAX_PAIRS` | `16384` | Victim and culprit cgroup pairs the kernel tracks at a time. Each one costs 16 bytes plus 8 bytes per CPU. When the map is full, new pairs lose samples, which `noisy_neighbour_dropped_samples_total{map="pairs"}` counts. |

Set `RUST_LOG` to change the log level. The default is `info`. Logs are JSON.

## Endpoints

| Path | Use |
|---|---|
| `/metrics` | Prometheus metrics, see below. |
| `/healthz` | Liveness. Always returns 200. |
| `/version` | The git sha of the build, as JSON. |

## Metrics

| Metric | Type | Labels | Use |
|---|---|---|---|
| `noisy_neighbour_runqueue_latency_seconds` | histogram | `namespace`, `workload_kind`, `workload` (plus `pod`, `container` by granularity) | Time runnable tasks of each entity wait in the run queue. `sum()` over the labels gives the node. |
| `noisy_neighbour_runqueue_wait_by_cause_seconds_total` | counter | the entity labels, `cause` | Wait of each entity by cause: `same_workload`, `other_workload`, `system` or `idle`. `other_workload` is the noisy neighbour signal. |
| `noisy_neighbour_runqueue_wait_by_culprit_seconds_total` | counter | the entity labels, `culprit_namespace`, `culprit_workload_kind`, `culprit_workload` | Wait of each entity by culprit workload. At most `NNO_CULPRITS_PER_VICTIM` culprits, plus `_other` for the rest. |
| `noisy_neighbour_map_entries` | gauge | `map` | Entries in the cgroup map (`runq`) and the pair map (`pairs`) at the last read |
| `noisy_neighbour_map_bytes` | gauge | `map` | Memory the kernel preallocates for each BPF map |
| `noisy_neighbour_dropped_samples_total` | counter | `map` | Samples lost because the cgroup map (`runq`) or the pair map (`pairs`) was full |
| `noisy_neighbour_series` | gauge | none | Per entity series the agent exports, `_system` and `_overflow` included |
| `noisy_neighbour_series_budget` | gauge | none | The value of `NNO_SERIES_BUDGET` |
| `noisy_neighbour_overflow_workloads` | gauge | none | Entities in `_overflow`, because they did not fit in the budget |
| `noisy_neighbour_unresolved_cgroups` | gauge | none | Cgroups with samples that wait for the pod watch. Near 0 is healthy. |
| `noisy_neighbour_build_info` | gauge | `version`, `git_sha` | Build metadata, always 1 |

The wait series carry the same entity labels as the histogram, so a query can divide one by the other. The 4 cause series of an entity add up to its histogram `_sum`, less the samples that `noisy_neighbour_dropped_samples_total` counts. The culprit series add up to the same sum less the `idle` cause. The culprit is always a workload, whatever the granularity.

`cause="idle"` is wait that ends on an idle CPU, so no workload took the CPU. A high idle share means CFS throttle or wakeup latency on an idle CPU, not a neighbour. A wait is task time, so a culprit with many threads collects more wait. Compare the culprit shares of one entity. For the rules see "Culprit attribution" in [`CLAUDE.md`](./CLAUDE.md).

Synthetic entities:

- `_system` collects every cgroup outside a pod: the host, the runtime and the kubelet. It is also a culprit.
- `_overflow` collects every entity that did not fit in the budget, for its whole life on the node.
- At `container` granularity, `container="_pod"` collects a pod cgroup that no container owns, such as CRI-O `conmon` or the pod sandbox.
- `culprit_workload="_other"` collects the culprits of an entity that have no series of their own.

With the default bounds, each entity costs 16 series: 11 histogram, 4 cause and 1 `_other` series. Each culprit series adds 1, so an entity costs at most 19. The agent exports 12 other series. So a node with N entities exports between 12 + 16 x (N + 2) and 12 + 19 x (N + 2) series, at most 1512 with the default budget.

## RBAC

The agent needs `get`, `list` and `watch` on `pods` across all namespaces. It watches only the pods of its node, through a field selector on `spec.nodeName`. At startup it waits up to 60s for the first pod list, and exits if the list does not come or holds no pods.

## Scrape config

Set `honor_labels: true` on the scrape of the agent. Service discovery adds `namespace`, `pod` and `container` target labels for the agent pod, which would otherwise rename the workload labels to `exported_namespace` and the like.

## Requirements

- Linux 6.1 or newer, with BTF and cgroup v2.
- Root with `CAP_BPF` and `CAP_PERFMON`. `CAP_BPF` alone does not let the agent load the programs. The default seccomp profile of the container runtime is enough.
- EKS and GKE Standard are the target environments.
- On macOS, OrbStack is the supported container runtime. Run `make preflight` to check an engine.
