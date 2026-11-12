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
    probes::start(&config, &metrics).context("starting the eBPF probes")?;

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
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Context;
    use observer::bpf::{self, Probes};
    use observer::config::Config;
    use observer::metrics::{MapLabels, Metrics, RunqueueLatency};
    use observer::runq::Accumulator;
    use prometheus_client::metrics::counter::Counter;
    use prometheus_client::metrics::gauge::Gauge;

    /// Loads and attaches the programs, then reads their maps on `NNO_READ_INTERVAL`.
    pub fn start(config: &Config, metrics: &Metrics) -> anyhow::Result<()> {
        let probes = Probes::load(config)?;
        for (name, map) in bpf::MAPS {
            let bytes = probes.map_bytes(name)?;
            metrics
                .map_bytes
                .get_or_create(&MapLabels { map })
                .set(bytes as i64);
        }
        tracing::info!("eBPF programs attached");
        let reader = Reader {
            latency: metrics.runqueue_latency.clone(),
            runq_entries: metrics
                .map_entries
                .get_or_create(&MapLabels { map: "runq" })
                .clone(),
            dropped: metrics.dropped_samples.clone(),
            dropped_seen: 0,
            accumulator: Accumulator::default(),
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
        runq_entries: Gauge,
        dropped: Counter,
        dropped_seen: u64,
        accumulator: Accumulator,
    }

    impl Reader {
        /// Owns `probes` for the life of the process, because the programs detach when it drops.
        fn run(mut self, mut probes: Probes, interval: Duration) {
            loop {
                std::thread::sleep(interval);
                if let Err(error) = self.read_once(&mut probes) {
                    tracing::error!(error = format!("{error:#}"), "reading the BPF maps failed");
                }
                self.latency.set(self.accumulator.node());
            }
        }

        fn read_once(&mut self, probes: &mut Probes) -> anyhow::Result<()> {
            let runq = probes.read_runq()?;
            self.runq_entries.set(runq.len() as i64);
            let idle = self.accumulator.update(runq);
            probes.delete_runq(&idle)?;
            let dropped = probes.read_dropped()?;
            self.dropped
                .inc_by(dropped.saturating_sub(self.dropped_seen));
            self.dropped_seen = dropped;
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod probes {
    use observer::config::Config;
    use observer::metrics::Metrics;

    pub fn start(_: &Config, _: &Metrics) -> anyhow::Result<()> {
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
