# Grafana dashboard

`noisy-neighbour.json` answers two questions in order:

1. Does a workload wait for CPU?
2. If it does, is a neighbour the cause, and which one?

## Read it from top to bottom

Each row narrows the question of the row above it.

| Row | Answer |
|---|---|
| Start here | The worst p99 wait in each cluster, the number of workloads above 1 ms on a node, and the share of all wait that neighbours cause. These panels do not count the synthetic `_system` and `_overflow` entities. |
| Where | The workloads and the nodes, ranked by their worst p99 wait in the time range. Click a name to select it. The link keeps the time range. |
| Victim | The wait of the selected workloads: a heatmap of the buckets, the mean, and the p50 and p99. |
| Who | The share of the wait that each culprit workload causes, and the share of each cause. |
| Rule out | The throttled and idle share of the agent, the node CPU pressure, and p99 wait against the neighbour share. A text panel gives the verdict. |
| Trust | The health of each agent: series against the budget, overflow workloads, unresolved cgroups, dropped samples and versions. A gap here means that the rows above are not complete. |

- Amber starts at 100 µs and red at 1 ms. A task on an idle CPU waits a few µs. A task on a busy CPU waits for ms.
- The histogram has 8 buckets by default, so the p50 and p99 are coarse. Use them to rank and to see a trend.
- The colour of each cause is fixed: `other_workload` red, `same_workload` amber, `system` blue, `idle` light grey and `throttled` taupe. `_other` and `_overflow` are grey.

## Datasource requirements

- Install the VictoriaMetrics plugin (`victoriametrics-metrics-datasource`) in Grafana. The dashboard does not work with a datasource of the Prometheus type.
- Name each VictoriaMetrics datasource `vm-cluster-<region>`. The `datasource` variable lists the datasources of the plugin with a name that matches the regex `/^vm-cluster-/`.
- To use other names, edit the `regex` field of the `datasource` variable in `noisy-neighbour.json`, then import the file again. An empty regex lists every datasource of the plugin.
- Select one datasource at a time. The rows drill down within one datasource.
- Set the external label `kubernetes_cluster_name` on the series of each cluster. See [Scrape requirements](#scrape-requirements).

## Import

The file is the source. UI edits do not survive the next import, so change this file and import it again.

1. In Grafana, open **Dashboards**, **New**, **Import**.
2. Paste the contents of `noisy-neighbour.json`.
3. Click **Import**.

The `uid` is `noisy-neighbour`, so a new import replaces the dashboard and does not add a second one. Grafana can also provision the file from disk, as `make kind-dashboard` does.

## Variables

| Variable | Source |
|---|---|
| `datasource` | Every datasource of the VictoriaMetrics plugin with a name that matches `/^vm-cluster-/`. You select one. |
| `cluster` | `label_values` of the `kubernetes_cluster_name` label. All matches series with no `kubernetes_cluster_name` label too. |
| `namespace` | `label_values` of `noisy_neighbour_runqueue_latency_seconds_count`, within the selected clusters. |
| `workload` | The same, within the selected namespaces. |
| `node` | The same, within the selected workloads. |

- Each query variable reloads when the time range changes.
- All is the regex `.*`, not a list of every value.
- Victim, Who and Rule out show the sum of the selected workloads. Select one workload to read them.

## Scrape requirements

- Set `honor_labels: true` on the scrape of the agent. Without it, the workload labels become `exported_namespace` and the like. See the [chart README](../../helm/noisy-neighbour-observer/README.md#scrape).
- Set the `node` label from the node name on each agent target.
- Add the external label `kubernetes_cluster_name` to the series of each cluster, as a remote write external label or a target label. Without it, the dashboard still works, but it cannot tell the clusters apart.

The dashboard needs no cAdvisor series. It needs one source that the agent does not export:

- node exporter, for `node_pressure_cpu_waiting_seconds_total` (pressure stall information, PSI). Enable the pressure collector of node exporter. The series needs the `node` label.

Without it, the Node CPU pressure panel shows no data. The other panels need only the agent.

## Check it locally

`make kind-up kind-dashboard` installs VictoriaMetrics, node exporter and Grafana in the kind cluster. Grafana installs the VictoriaMetrics plugin at version 1.0.1 from grafana.com at start, so the cluster needs internet access. The datasource is `vm-cluster-kind` and the external label is `kubernetes_cluster_name=nno-local`. The check runs each panel query against VictoriaMetrics, then through Grafana and the plugin. It checks that each query returns data for the `spin` workload, and that `stress` is the top culprit of `spin`.

Format the file with `make dashboard-fmt`. `make dashboard-lint` runs the pinned `dashboard-linter` with `--strict`, and checks that the file is in `jq -S` form. `.lint` lists the rules that the linter skips, with the reason for each.
