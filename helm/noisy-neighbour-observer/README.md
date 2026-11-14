# noisy-neighbour-observer Helm chart

Deploys the noisy neighbour observer as a DaemonSet. Each agent measures the run queue latency of the workloads on its node with eBPF, names the workloads that cause the wait, and serves the result as Prometheus metrics on port 9400. For the metrics and the agent settings see the [repo README](../../README.md).

The chart renders no Service. Prometheus scrapes each agent pod directly.

## Node requirements

- Linux 6.1 or newer, with BTF (`/sys/kernel/btf/vmlinux`).
- cgroup v2 only.
- containerd 1.6.20 or newer (or CRI-O 1.33 or newer). Older runtimes block `bpf()` in the default seccomp profile.
- Kernel lockdown off or in integrity mode. Confidentiality mode blocks the kernel memory reads of the programs.

## Supported environments

| Environment | Status |
|---|---|
| EKS on AL2023 (managed node groups and Karpenter) | Supported |
| EKS on Bottlerocket | Supported |
| GKE Standard on COS | Supported |
| GKE Standard on Ubuntu | Supported |
| EKS Auto Mode | Supported after validation |
| EKS Fargate | Unsupported: no DaemonSets |
| EKS on AL2 | Unsupported: kernel 5.10, cgroup v1, end of life |
| GKE Autopilot | Unsupported: it blocks `BPF`, `PERFMON` and the cgroup hostPath without an allowlist from Google |
| GKE Sandbox pods | Unsupported: gVisor hides them from the host kernel |

Both amd64 and arm64 nodes are supported. One image covers both, so a cluster with mixed pools needs no values for each pool.

## Install

```bash
helm install noisy-neighbour-observer ./helm/noisy-neighbour-observer \
  --namespace noisy-neighbour-observer --create-namespace \
  --set image.repository=<registry>/noisy-neighbour-observer \
  --set image.tag=<tag>
```

Then set up the scrape (see [Scrape](#scrape)).

## Required inputs

| Value | Why |
|---|---|
| `image.repository` | No default. No image is published yet. |

## Node coverage

The defaults place one agent on each Linux node in every node pool, with no values for each pool.

- `tolerations` holds one `operator: Exists` entry, so the agent tolerates every taint (GPU, spot, dedicated, `CriticalAddonsOnly`).
- `nodeSelector` is empty.
- The pod always has one required node affinity term. It excludes nodes that are not Linux, EKS Fargate nodes (`eks.amazonaws.com/compute-type=fargate`) and virtual-kubelet nodes (`type=virtual-kubelet`).
- `extraNodeAffinityExpressions` adds expressions to that term. The chart rejects `affinity.nodeAffinity`, because it would replace the defaults.
- `updateStrategy` updates 10% of the nodes at a time.

## Priority class

The default `priorityClassName` is `system-node-critical`. The scheduler preempts other pods to place the agent on a full node, and the kubelet evicts the agent last.

- Upstream Kubernetes admits the critical classes in any namespace since 1.17.
- GKE admits them outside `kube-system` only in a namespace with a ResourceQuota whose scope selector matches the class. Without it, the DaemonSet creates no pods: `insufficient quota to match these scopes`.
- The chart renders that quota in the release namespace (`priorityClassQuota.create`). It skips the quota in `kube-system` and for other classes.
- To install without the quota, set `priorityClassQuota.create=false` and install in `kube-system`, or set `priorityClassName` to another class.

## Security

The agent runs as root with `CAP_BPF` and `CAP_PERFMON`, not as a privileged container. Kubernetes gives added capabilities no effect for a non-root user.

- `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, all other capabilities dropped and the `RuntimeDefault` seccomp profile.
- No `hostPID` and no host network.
- The host `/sys/fs/cgroup` mounts read only at `/host/sys/fs/cgroup`. The cgroup namespace of the container hides the other pods.

Toggles for a node that blocks the agent with the defaults. All are off by default and are safe for the whole cluster:

| Value | Use |
|---|---|
| `seLinux.type` | SELinux type of the container. On Bottlerocket, set `spc_t` (Bottlerocket maps it to `control_t`) only if the default label blocks the BTF or hostPath reads. A node without SELinux ignores it. |
| `appArmor.unconfined` | Runs the agent without an AppArmor profile, for COS and GKE Ubuntu. A node without AppArmor ignores it. |
| `extraCapabilities` | Capabilities to add, for example `SYS_ADMIN` as a fallback. |
| `privileged` | A privileged container, as the last fallback. The chart rejects it together with `seLinux.type`, because `privileged` overrides `seLinuxOptions` (bottlerocket#3791). |

## Resources

- The CPU request is 50m. There is no CPU limit, because a CFS throttle would add run queue wait to the agent that measures it.
- The memory limit must fit the BPF maps, because the kernel charges them to the pod.
- The cgroup map takes `config.maxCgroups x (8 + 112 x CPUs)` bytes: about 22 MB at 1024 cgroups on a 192 vCPU node.
- The pair map takes `config.maxPairs x (16 + 8 x CPUs)` bytes: about 25 MB at 16384 pairs on a 192 vCPU node.
- The task map takes about 1.3 MB on any node.
- With the defaults, the maps take about 49 MB on a 192 vCPU node, and about 6 MB on a 16 vCPU node. The process takes about 30 MB.
- The 160Mi limit holds the maps of a 192 vCPU node and the process. The 64Mi request covers a node of up to about 128 vCPU.
- The agent exports the size of each map as `noisy_neighbour_map_bytes`.
- Raise `resources.limits.memory` when you raise `config.maxCgroups` or `config.maxPairs`.

## Probes

The agent binds its listener only after the programs attach and the first pod list arrives. So `/healthz` answers only on an agent that measures, and it serves as the startup, liveness and readiness probe.

- The first pod list can take up to 60s. After that, the agent exits.
- The startup probe allows 90s, so the agent exits before the kubelet kills it.
- With no RBAC for pods, the agent logs `no pod list after 60s` and exits.

## Scrape

Prometheus must scrape each agent pod with `honor_labels: true`. Kubernetes service discovery adds `namespace`, `pod` and `container` target labels for the agent pod. Without `honor_labels`, they rename the workload labels of the agent to `exported_namespace` and the like.

Set the target labels from the node name, so the series stay the same when the agent pods restart:

- `node` and `instance` from `__meta_kubernetes_pod_node_name`.
- No `pod` or `container` label. These name the agent pod, not the workloads it measures.

Two ways to scrape:

- `podMonitor.enabled=true` renders a `monitoring.coreos.com/v1` PodMonitor with these settings. It needs the Prometheus Operator CRDs. The VictoriaMetrics operator converts PodMonitors to VMPodScrapes.
- `prometheusAnnotations=true` adds `prometheus.io/scrape`, `prometheus.io/port` and `prometheus.io/path` to the pods. The scrape config that reads them must set `honor_labels: true` and the relabels above.

## Choose a granularity

`config.granularity` sets the labels on each series. A coarse granularity keeps the series count low. A fine granularity shows which replica or container waits.

| Granularity | Labels | Series per agent pod | Use |
|---|---|---|---|
| `workload` (default) | `namespace`, `workload_kind`, `workload` | 16 to 19 for each workload on the node | Day to day. Replicas on one node share a series, and a rollout keeps the same series. |
| `pod` | adds `pod` | 16 to 19 for each pod | Find the replica that waits, for example the one replica next to a noisy pod. |
| `container` | adds `pod` and `container` | 16 to 19 for each container, plus 16 to 19 for the `_pod` entity of each pod | Find the container that waits, for example an application against its sidecar. |

- Each entity has 11 histogram series, 4 cause series and 1 `_other` culprit series. Each culprit with its own series adds 1, up to `config.culpritsPerVictim` (default 3).
- The counts assume the default 8 histogram bounds. Each extra bound adds one series to each entity.
- The culprit labels are always at workload level, so the granularity does not change the culprit count.
- The `pod` and `container` labels change on each rollout, so each rollout creates new series. The old series end one read after their pod leaves the node.
- At `container` granularity, `container="_pod"` collects the pod sandbox and the other pod cgroups that no container owns.
- `config.seriesBudget` caps the per entity series of each agent pod at 1500 by default. An entity that does not fit goes to `_overflow`, and `noisy_neighbour_overflow_workloads` counts these entities.
- Size the budget for the granularity you choose. At `container`, a pod with two containers costs 48 to 57 series, so 1500 covers about 26 to 31 such pods on a node.
- Culprit series take budget too. When the budget is full, a new culprit goes to `_other`, and the entity keeps its own series.
- Start with `workload`. To investigate a node, set `pod` or `container` on the release, then set `workload` again when you finish.

## Values

| Value | Default | Notes |
|---|---|---|
| `image.repository` | `""` | Required. |
| `image.tag` | `""` | Empty means the chart `appVersion`. |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | |
| `serviceAccount.create` / `.name` / `.annotations` | `true` / `""` / `{}` | |
| `rbac.create` | `true` | ClusterRole with `get`, `list` and `watch` on pods. With `false`, grant them yourself. |
| `config.granularity` | `workload` | `workload`, `pod` or `container`. `pod` and `container` create new series on each rollout. |
| `config.histogramBounds` | `[10us, 100us, 500us, 1ms, 5ms, 10ms, 100ms, 1s]` | At most 12, strictly increasing. Units: `ns`, `us`, `ms`, `s`. The agent checks the count and order at startup. |
| `config.seriesBudget` | `1500` | Must be at least 2 x (bounds + 8). The agent checks this at startup. |
| `config.culpritsPerVictim` | `3` | Culprit workloads with their own series for each entity, at least 1. The agent checks this at startup. |
| `config.readInterval` | `5s` | Above zero. |
| `config.maxCgroups` | `1024` | Raise the memory limit with it. |
| `config.maxPairs` | `16384` | Victim and culprit cgroup pairs. Raise the memory limit with it. |
| `config.logLevel` | `info` | `RUST_LOG`. |
| `extraEnv` | `[]` | Extra env entries for the agent. |
| `port` | `9400` | Port of `/metrics`, `/healthz` and `/version`. |
| `bindHost` | `[::]` | Serves IPv6 and, on Linux defaults, IPv4 too. Do not narrow it to `0.0.0.0`. |
| `privileged` | `false` | See [Security](#security). |
| `extraCapabilities` | `[]` | See [Security](#security). |
| `seLinux.type` | `""` | See [Security](#security). |
| `appArmor.unconfined` | `false` | See [Security](#security). |
| `podSecurityContext` | `{}` | |
| `priorityClassName` | `system-node-critical` | See [Priority class](#priority-class). |
| `priorityClassQuota.create` | `true` | See [Priority class](#priority-class). |
| `resources` | 50m CPU, 64Mi memory request, 160Mi memory limit | See [Resources](#resources). |
| `startupProbe` / `livenessProbe` / `readinessProbe` | `/healthz` | See [Probes](#probes). |
| `terminationGracePeriodSeconds` | `10` | |
| `updateStrategy` | `RollingUpdate`, `maxUnavailable: 10%` | |
| `podMonitor.enabled` / `.interval` / `.labels` | `false` / `""` / `{}` | See [Scrape](#scrape). |
| `prometheusAnnotations` | `false` | See [Scrape](#scrape). |
| `podAnnotations` / `podLabels` | `{}` | |
| `nodeSelector` | `{}` | |
| `tolerations` | `[{operator: Exists}]` | |
| `extraNodeAffinityExpressions` | `[]` | See [Node coverage](#node-coverage). |
| `affinity` | `{}` | Other affinity kinds. `nodeAffinity` is rejected. |

## Versions

- `Chart.yaml` `version` is the chart version (SemVer). Bump it on each change to templates or values. `make helm-version-check` enforces it.
- `Chart.yaml` `appVersion` is the image tag that the chart deploys when `image.tag` is empty.

## Test

- `make helm-validate` lints the chart and validates each rendered scenario with kubeconform.
- `make kind-up kind-test kind-down` installs the chart in a two node kind cluster with a tainted worker and runs the e2e assertions.
