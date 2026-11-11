# Noisy Neighbour Observer

A Kubernetes DaemonSet agent that finds noisy neighbours. It measures the run queue latency of each pod with eBPF and names the workload that caused each preemption. For the design see [`CLAUDE.md`](./CLAUDE.md).

## Status

The agent is a scaffold. It serves metrics and probes, but it does not measure anything yet. The eBPF probes, the pod labels and the Helm chart come in later changes.

## Usage

Run the agent locally:

```
make run
```

Build the container image:

```
make docker-build
```

The agent reads this environment variable:

| Variable | Default | Use |
|---|---|---|
| `NNO_BIND_ADDR` | `[::]:9400` | Listen address. `[::]` also serves IPv4 when `net.ipv6.bindv6only` is 0, the Linux default. |

Set `RUST_LOG` to change the log level. The default is `info`. Logs are JSON.

## Endpoints

| Path | Use |
|---|---|
| `/metrics` | Prometheus metrics. Today only `noisy_neighbour_build_info`. |
| `/healthz` | Liveness. Always returns 200. |
| `/version` | The git sha of the build, as JSON. |

## Requirements

- Linux 6.1 or newer, with BTF and cgroup v2.
- EKS and GKE Standard are the target environments.
- On macOS, OrbStack is the supported container runtime. Run `make preflight` to check an engine.
