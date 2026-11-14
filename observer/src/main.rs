//! Noisy neighbour observer entrypoint.

use std::sync::Arc;

use anyhow::Context;
use observer::config::Config;
use observer::metrics::{self, Metrics};
use observer::routes::{self, AppState};
use observer::telemetry;

/// musl's mallocng does not return freed pages to the OS, so allocation churn settles as
/// permanent RSS. Linux only, which is the only platform the agent ships.
#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init_tracing();
    let config = Config::from_env().context("loading process config")?;

    // Attach before the listener binds, so a pod that serves metrics always measures.
    let metrics = Metrics::new(&config.histogram_bounds);
    probes::start(&config, &metrics)
        .await
        .context("starting the eBPF probes")?;

    let state = Arc::new(AppState {
        registry: metrics::registry(&metrics),
    });

    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("binding {}", config.bind_addr))?;
    tracing::info!(addr = %config.bind_addr, git_sha = observer::git_sha(), "listening");

    axum::serve(listener, routes::router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    tracing::info!("shutdown complete");
    Ok(())
}

#[cfg(target_os = "linux")]
mod probes {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Context;
    use observer::bpf::{self, Probes};
    use observer::cardinality::Cardinality;
    use observer::cgroup::Resolver;
    use observer::config::{Config, Granularity};
    use observer::k8s::{self, PodIndex, Watch};
    use observer::metrics::{MapLabels, Metrics, RunqueueLatency, RunqueueWait};
    use observer::runq::Accumulator;
    use observer::workload::Entity;
    use prometheus_client::metrics::counter::Counter;
    use prometheus_client::metrics::family::Family;
    use prometheus_client::metrics::gauge::Gauge;

    /// Loads and attaches the programs, starts the pod watch, then reads the maps on
    /// `NNO_READ_INTERVAL`.
    pub async fn start(config: &Config, metrics: &Metrics) -> anyhow::Result<()> {
        let node = config
            .node_name
            .as_deref()
            .context("NNO_NODE_NAME must name the node of the agent")?;
        let probes = Probes::load(config)?;
        for (name, map) in bpf::MAPS {
            let bytes = probes.map_bytes(name)?;
            metrics
                .map_bytes
                .get_or_create(&MapLabels { map })
                .set(bytes as i64);
        }
        tracing::info!("eBPF programs attached");
        let watch = k8s::watch(node).await?;
        tracing::info!(node, pods = watch.store.len(), "pod watch synced");
        let mut resolver = Resolver::new(&config.cgroup_root);
        resolver.scan()?;
        metrics.series_budget.set(config.series_budget as i64);
        let cardinality = Cardinality::new(config.series_budget, config.series_per_entity());
        let entries = |map| {
            metrics
                .map_entries
                .get_or_create(&MapLabels { map })
                .clone()
        };
        let reader = Reader {
            latency: metrics.runqueue_latency.clone(),
            wait: metrics.runqueue_wait.clone(),
            runq_entries: entries("runq"),
            pairs_entries: entries("pairs"),
            dropped: metrics.dropped_samples.clone(),
            dropped_seen: [0; 2],
            accumulator: Accumulator::new(
                config.granularity,
                cardinality,
                config.culprits_per_victim,
            ),
            resolver,
            watch,
            index: None,
            granularity: config.granularity,
            series: metrics.series.clone(),
            overflow_workloads: metrics.overflow_workloads.clone(),
            unresolved_cgroups: metrics.unresolved_cgroups.clone(),
        };
        let interval = config.read_interval;
        // The map reads are blocking syscalls, so they run on their own thread and never stall
        // the runtime that serves /metrics.
        std::thread::Builder::new()
            .name("map-reader".into())
            .spawn(move || reader.run(probes, interval))
            .context("starting the map reader thread")?;
        Ok(())
    }

    struct Reader {
        latency: Arc<RunqueueLatency>,
        wait: Arc<RunqueueWait>,
        runq_entries: Gauge,
        pairs_entries: Gauge,
        dropped: Family<MapLabels, Counter>,
        dropped_seen: [u64; 2],
        accumulator: Accumulator,
        resolver: Resolver,
        watch: Watch,
        /// The pod index, the live entities, and the watch generation they came from.
        index: Option<(u64, PodIndex, HashSet<Entity>)>,
        granularity: Granularity,
        series: Gauge,
        overflow_workloads: Gauge,
        unresolved_cgroups: Gauge,
    }

    impl Reader {
        /// Owns `probes` for the life of the process, because the programs detach when it drops.
        fn run(mut self, mut probes: Probes, interval: Duration) {
            loop {
                std::thread::sleep(interval);
                if let Err(error) = self.read_once(&mut probes) {
                    tracing::error!(error = format!("{error:#}"), "reading the BPF maps failed");
                }
                self.latency.set(self.accumulator.snapshot());
                self.wait.set(self.accumulator.wait_snapshot());
                let cardinality = self.accumulator.cardinality();
                self.series.set(cardinality.series() as i64);
                self.overflow_workloads.set(cardinality.overflowed() as i64);
                self.unresolved_cgroups
                    .set(self.accumulator.unresolved() as i64);
            }
        }

        fn read_once(&mut self, probes: &mut Probes) -> anyhow::Result<()> {
            let runq = probes.read_runq()?;
            self.runq_entries.set(runq.len() as i64);
            let pairs = probes.read_pairs()?;
            self.pairs_entries.set(pairs.len() as i64);
            let pair_cgroups = pairs.keys().flat_map(|p| [p.victim_cg, p.culprit_cg]);
            if let Err(error) = self
                .resolver
                .refresh(runq.keys().copied().chain(pair_cgroups))
            {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "scanning the cgroup tree failed"
                );
            }
            let generation = self.watch.generation();
            if self
                .index
                .as_ref()
                .is_none_or(|(seen, _, _)| *seen != generation)
            {
                let pods = self.watch.store.state();
                let index = PodIndex::new(self.granularity, pods.iter().map(|pod| pod.as_ref()));
                let live = index.live();
                self.index = Some((generation, index, live));
            }
            let Some((_, index, live)) = &self.index else {
                return Ok(());
            };
            let resolver = &self.resolver;
            let resolve = |cgroup_id| index.owner(resolver.get(cgroup_id));
            let idle = self.accumulator.update(runq, resolve, live);
            probes.delete_runq(&idle)?;
            let idle = self.accumulator.update_pairs(pairs, resolve, live);
            probes.delete_pairs(&idle)?;
            for ((map, dropped), seen) in probes
                .read_dropped()?
                .into_iter()
                .zip(&mut self.dropped_seen)
            {
                self.dropped
                    .get_or_create(&MapLabels { map })
                    .inc_by(dropped.saturating_sub(*seen));
                *seen = dropped;
            }
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod probes {
    use observer::config::Config;
    use observer::metrics::Metrics;

    pub async fn start(_: &Config, _: &Metrics) -> anyhow::Result<()> {
        anyhow::bail!("the eBPF probes need Linux")
    }
}

async fn shutdown_signal() {
    wait_for_signal().await;
    tracing::info!("shutdown signal received, draining");
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
