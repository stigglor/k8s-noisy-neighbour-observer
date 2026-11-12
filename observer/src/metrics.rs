//! The Prometheus registry. Every series here counts against the cardinality budget in
//! `CLAUDE.md`, so add a label only after that review.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use observer_common::RunqHist;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeLabelSet, NoLabelSet};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::MetricType;
use prometheus_client::registry::{Registry, Unit};

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabels {
    version: String,
    git_sha: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MapLabels {
    pub map: &'static str,
}

pub struct Metrics {
    pub runqueue_latency: Arc<RunqueueLatency>,
    pub map_entries: Family<MapLabels, Gauge>,
    pub map_bytes: Family<MapLabels, Gauge>,
    pub dropped_samples: Counter,
}

impl Metrics {
    pub fn new(bounds: &[Duration]) -> Self {
        Self {
            runqueue_latency: Arc::new(RunqueueLatency {
                bounds: bounds.iter().map(Duration::as_secs_f64).collect(),
                totals: Mutex::default(),
            }),
            map_entries: Family::default(),
            map_bytes: Family::default(),
            dropped_samples: Counter::default(),
        }
    }
}

pub fn registry(metrics: &Metrics) -> Registry {
    let mut registry = Registry::default();
    let build_info = Family::<BuildLabels, Gauge>::default();
    registry.register(
        "noisy_neighbour_build_info",
        "Build metadata of the running agent, always 1",
        build_info.clone(),
    );
    build_info
        .get_or_create(&BuildLabels {
            version: env!("CARGO_PKG_VERSION").to_string(),
            git_sha: crate::git_sha().to_string(),
        })
        .set(1);
    registry.register_collector(Box::new(metrics.runqueue_latency.clone()));
    registry.register(
        "noisy_neighbour_map_entries",
        "Entries in each BPF map at the last read",
        metrics.map_entries.clone(),
    );
    registry.register(
        "noisy_neighbour_map_bytes",
        "Memory the kernel preallocates for each BPF map",
        metrics.map_bytes.clone(),
    );
    registry.register(
        "noisy_neighbour_dropped_samples",
        "Run queue samples the kernel lost because the cgroup map was full",
        metrics.dropped_samples.clone(),
    );
    registry
}

/// The node run queue histogram. The kernel bins each sample, so the collector encodes the
/// totals as they are.
#[derive(Debug)]
pub struct RunqueueLatency {
    /// Upper bounds in seconds, without `+Inf`.
    bounds: Vec<f64>,
    totals: Mutex<RunqHist>,
}

impl RunqueueLatency {
    pub fn set(&self, totals: RunqHist) {
        *self.totals.lock().unwrap_or_else(PoisonError::into_inner) = totals;
    }
}

impl Collector for RunqueueLatency {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let totals = *self.totals.lock().unwrap_or_else(PoisonError::into_inner);
        let overflow = totals.bins[self.bounds.len()];
        let buckets: Vec<(f64, u64)> = self
            .bounds
            .iter()
            .copied()
            .zip(totals.bins)
            .chain([(f64::MAX, overflow)])
            .collect();
        let count = buckets.iter().map(|(_, count)| count).sum();
        let sum = totals.sum_ns as f64 / 1e9;
        encoder
            .encode_descriptor(
                "noisy_neighbour_runqueue_latency",
                "Time runnable tasks wait in the run queue before they run.",
                Some(&Unit::Seconds),
                MetricType::Histogram,
            )?
            .encode_histogram::<NoLabelSet>(sum, count, &buckets, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus_client::encoding::text::encode;

    #[test]
    fn histogram_encodes_cumulative_buckets() {
        let metrics = Metrics::new(&[Duration::from_micros(10), Duration::from_millis(1)]);
        let mut totals = RunqHist {
            sum_ns: 1_500_000_000,
            ..Default::default()
        };
        totals.bins[..3].copy_from_slice(&[4, 2, 1]);
        metrics.runqueue_latency.set(totals);

        let mut body = String::new();
        encode(&mut body, &registry(&metrics)).unwrap();
        let name = "noisy_neighbour_runqueue_latency_seconds";
        for line in [
            format!("# TYPE {name} histogram"),
            format!("{name}_sum 1.5"),
            format!("{name}_count 7"),
            format!("{name}_bucket{{le=\"0.00001\"}} 4"),
            format!("{name}_bucket{{le=\"0.001\"}} 6"),
            format!("{name}_bucket{{le=\"+Inf\"}} 7"),
        ] {
            assert!(
                body.lines().any(|l| l == line),
                "missing {line:?} in\n{body}"
            );
        }
    }
}
