# Noisy Neighbour Observer

A Kubernetes DaemonSet agent that finds noisy neighbours. It measures the run queue latency of each pod with eBPF and names the workload that caused each preemption. For the design see [`CLAUDE.md`](./CLAUDE.md).

## Status

The agent measures run queue latency with eBPF and exports one histogram for the whole node. The pod and workload labels, the culprit attribution and the Helm chart come in later changes.

## Usage

Install the eBPF toolchain once (see [`AGENTS.md`](./AGENTS.md)):

```
make ebpf-toolchain
```

Run the agent as root on a Linux host:

```
make run
```

Build the container image:

```
make docker-build
```

The agent reads these environment variables:

| Variable | Default | Use |
|---|---|---|
| `NNO_BIND_ADDR` | `[::]:9400` | Listen address. `[::]` also serves IPv4 when `net.ipv6.bindv6only` is 0, the Linux default. |
| `NNO_READ_INTERVAL` | `5s` | Time between two reads of the BPF maps. Units: `ns`, `us`, `ms`, `s`. |
| `NNO_HISTOGRAM_BOUNDS` | `10us,100us,500us,1ms,5ms,10ms,100ms,1s` | Bucket bounds of the latency histogram. At most 12, strictly increasing. |
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
| `noisy_neighbour_runqueue_latency_seconds` | histogram | none | Time runnable tasks on the node wait in the run queue |
| `noisy_neighbour_map_entries` | gauge | `map` | Entries in the cgroup map (`runq`) at the last read |
| `noisy_neighbour_map_bytes` | gauge | `map` | Memory the kernel preallocates for each BPF map |
| `noisy_neighbour_dropped_samples_total` | counter | none | Samples lost because the cgroup map was full |
| `noisy_neighbour_build_info` | gauge | `version`, `git_sha` | Build metadata, always 1 |

With the default bounds, the agent exports 16 series.

## Requirements

- Linux 6.1 or newer, with BTF and cgroup v2.
- Root with `CAP_BPF` and `CAP_PERFMON`. `CAP_BPF` alone does not let the agent load the programs. The default seccomp profile of the container runtime is enough.
- EKS and GKE Standard are the target environments.
- On macOS, OrbStack is the supported container runtime. Run `make preflight` to check an engine.
