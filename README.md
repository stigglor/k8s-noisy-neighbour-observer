# Noisy Neighbour Observer

A Kubernetes DaemonSet agent that finds noisy neighbours. It measures the run queue latency of each pod with eBPF and names the workload that caused each preemption. For the design see [`CLAUDE.md`](./CLAUDE.md).

## Status

The agent measures run queue latency with eBPF and exports one histogram for each workload on the node. The culprit attribution comes in a later change.

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
| `NNO_SERIES_BUDGET` | `1500` | The most per entity series. Each entity costs the bounds plus 3 series. `_system` and `_overflow` always hold theirs, so the budget must fit them. |
| `NNO_MAX_CGROUPS` | `1024` | Cgroups the kernel tracks at a time. Each one costs 8 bytes plus 112 bytes per CPU. When the map is full, new cgroups lose samples, which `noisy_neighbour_dropped_samples_total` counts. |

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
| `noisy_neighbour_map_entries` | gauge | `map` | Entries in the cgroup map (`runq`) at the last read |
| `noisy_neighbour_map_bytes` | gauge | `map` | Memory the kernel preallocates for each BPF map |
| `noisy_neighbour_dropped_samples_total` | counter | none | Samples lost because the cgroup map was full |
| `noisy_neighbour_series` | gauge | none | Per entity series the agent exports, `_system` and `_overflow` included |
| `noisy_neighbour_series_budget` | gauge | none | The value of `NNO_SERIES_BUDGET` |
| `noisy_neighbour_overflow_workloads` | gauge | none | Entities in `_overflow`, because they did not fit in the budget |
| `noisy_neighbour_unresolved_cgroups` | gauge | none | Cgroups with samples that wait for the pod watch. Near 0 is healthy. |
| `noisy_neighbour_build_info` | gauge | `version`, `git_sha` | Build metadata, always 1 |

Synthetic entities:

- `_system` collects every cgroup outside a pod: the host, the runtime and the kubelet.
- `_overflow` collects every entity that did not fit in the budget, for its whole life on the node.
- At `container` granularity, `container="_pod"` collects a pod cgroup that no container owns, such as CRI-O `conmon` or the pod sandbox.

With the default bounds, each entity costs 11 series. The agent exports 9 other series, so a node with N entities exports 9 + 11 x (N + 2) series, at most 1509 with the default budget.

## RBAC

The agent needs `get`, `list` and `watch` on `pods` across all namespaces. It watches only the pods of its node, through a field selector on `spec.nodeName`. At startup it waits up to 60s for the first pod list, and exits if the list does not come or holds no pods.

## Scrape config

Set `honor_labels: true` on the scrape of the agent. Service discovery adds `namespace`, `pod` and `container` target labels for the agent pod, which would otherwise rename the workload labels to `exported_namespace` and the like.

## Requirements

- Linux 6.1 or newer, with BTF and cgroup v2.
- Root with `CAP_BPF` and `CAP_PERFMON`. `CAP_BPF` alone does not let the agent load the programs. The default seccomp profile of the container runtime is enough.
- EKS and GKE Standard are the target environments.
- On macOS, OrbStack is the supported container runtime. Run `make preflight` to check an engine.
