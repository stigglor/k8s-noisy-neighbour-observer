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

use crate::attribution::{Cause, Wait};
use crate::workload::Entity;

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
    pub runqueue_wait: Arc<RunqueueWait>,
    pub map_entries: Family<MapLabels, Gauge>,
    pub map_bytes: Family<MapLabels, Gauge>,
    pub dropped_samples: Family<MapLabels, Counter>,
    pub series: Gauge,
    pub series_budget: Gauge,
    pub overflow_workloads: Gauge,
    pub unresolved_cgroups: Gauge,
}

impl Metrics {
    pub fn new(bounds: &[Duration]) -> Self {
        Self {
            runqueue_latency: Arc::new(RunqueueLatency {
                bounds: bounds.iter().map(Duration::as_secs_f64).collect(),
                totals: Mutex::default(),
            }),
            runqueue_wait: Arc::default(),
            map_entries: Family::default(),
            map_bytes: Family::default(),
            dropped_samples: Family::default(),
            series: Gauge::default(),
            series_budget: Gauge::default(),
            overflow_workloads: Gauge::default(),
            unresolved_cgroups: Gauge::default(),
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
    registry.register_collector(Box::new(metrics.runqueue_wait.clone()));
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
        "Samples the kernel lost because a BPF map was full",
        metrics.dropped_samples.clone(),
    );
    registry.register(
        "noisy_neighbour_series",
        "Per entity series the agent exports, the reserved _system and _overflow included",
        metrics.series.clone(),
    );
    registry.register(
        "noisy_neighbour_series_budget",
        "The most per entity series the agent exports",
        metrics.series_budget.clone(),
    );
    registry.register(
        "noisy_neighbour_overflow_workloads",
        "Entities whose samples go to _overflow because they did not fit in the series budget",
        metrics.overflow_workloads.clone(),
    );
    registry.register(
        "noisy_neighbour_unresolved_cgroups",
        "Cgroups with samples that wait for the pod watch to name their pod",
        metrics.unresolved_cgroups.clone(),
    );
    registry
}

/// The run queue histogram of each entity. The kernel bins each sample, so the collector
/// encodes the totals as they are. One lock covers every entity, so a scrape sees one read.
#[derive(Debug)]
pub struct RunqueueLatency {
    /// Upper bounds in seconds, without `+Inf`.
    bounds: Vec<f64>,
    totals: Mutex<Vec<(Entity, RunqHist)>>,
}

impl RunqueueLatency {
    pub fn set(&self, totals: Vec<(Entity, RunqHist)>) {
        *self.totals.lock().unwrap_or_else(PoisonError::into_inner) = totals;
    }
}

impl Collector for RunqueueLatency {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let totals = self.totals.lock().unwrap_or_else(PoisonError::into_inner);
        let mut metric = encoder.encode_descriptor(
            "noisy_neighbour_runqueue_latency",
            "Time runnable tasks wait in the run queue before they run.",
            Some(&Unit::Seconds),
            MetricType::Histogram,
        )?;
        for (entity, hist) in totals.iter() {
            let overflow = hist.bins[self.bounds.len()];
            let buckets: Vec<(f64, u64)> = self
                .bounds
                .iter()
                .copied()
                .zip(hist.bins)
                .chain([(f64::MAX, overflow)])
                .collect();
            let count = buckets.iter().map(|(_, count)| count).sum();
            let sum = hist.sum_ns as f64 / 1e9;
            metric
                .encode_family(&entity.labels())?
                .encode_histogram::<NoLabelSet>(sum, count, &buckets, None)?;
        }
        Ok(())
    }
}

/// The run queue wait of each entity by cause and by culprit workload, in seconds. The victim
/// labels match the histogram, so the series join.
#[derive(Debug, Default)]
pub struct RunqueueWait {
    totals: Mutex<Vec<(Entity, Wait)>>,
}

impl RunqueueWait {
    pub fn set(&self, totals: Vec<(Entity, Wait)>) {
        *self.totals.lock().unwrap_or_else(PoisonError::into_inner) = totals;
    }
}

impl Collector for RunqueueWait {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let totals = self.totals.lock().unwrap_or_else(PoisonError::into_inner);
        let mut metric = encoder.encode_descriptor(
            "noisy_neighbour_runqueue_wait_by_cause",
            "Run queue wait of each entity by the cause of the wait.",
            Some(&Unit::Seconds),
            MetricType::Counter,
        )?;
        for (entity, wait) in totals.iter() {
            for (cause, ns) in Cause::ALL.iter().zip(wait.by_cause) {
                let mut labels = entity.labels();
                labels.push(("cause", cause.label()));
                encode_seconds(&mut metric, &labels, ns)?;
            }
        }
        let mut metric = encoder.encode_descriptor(
            "noisy_neighbour_runqueue_wait_by_culprit",
            "Run queue wait of each entity by culprit workload, the rest in _other. No idle wait.",
            Some(&Unit::Seconds),
            MetricType::Counter,
        )?;
        for (entity, wait) in totals.iter() {
            for (culprit, ns) in &wait.by_culprit {
                let mut labels = entity.labels();
                labels.extend([
                    ("culprit_namespace", culprit.namespace.as_str()),
                    ("culprit_workload_kind", culprit.workload_kind.as_str()),
                    ("culprit_workload", culprit.workload.as_str()),
                ]);
                encode_seconds(&mut metric, &labels, *ns)?;
            }
        }
        Ok(())
    }
}

fn encode_seconds(
    metric: &mut prometheus_client::encoding::MetricEncoder,
    labels: &Vec<(&str, &str)>,
    ns: u64,
) -> Result<(), std::fmt::Error> {
    metric
        .encode_family(labels)?
        .encode_counter::<NoLabelSet, _, u64>(&(ns as f64 / 1e9), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Granularity;
    use crate::workload::Workload;
    use prometheus_client::encoding::text::encode;

    #[test]
    fn histogram_encodes_cumulative_buckets_per_entity() {
        let metrics = Metrics::new(&[Duration::from_micros(10), Duration::from_millis(1)]);
        let mut totals = RunqHist {
            sum_ns: 1_500_000_000,
            ..Default::default()
        };
        totals.bins[..3].copy_from_slice(&[4, 2, 1]);
        let system = Entity::system(Granularity::Workload);
        let web = Workload {
            kind: "Deployment".into(),
            name: "web".into(),
        };
        let web = Entity::new(Granularity::Pod, "shop", &web, "web-1", "");
        metrics
            .runqueue_latency
            .set(vec![(system, totals), (web, RunqHist::default())]);

        let mut body = String::new();
        encode(&mut body, &registry(&metrics)).unwrap();
        let name = "noisy_neighbour_runqueue_latency_seconds";
        let system = r#"namespace="_system",workload_kind="_system",workload="_system""#;
        let web = r#"namespace="shop",workload_kind="Deployment",workload="web",pod="web-1""#;
        for line in [
            format!("# TYPE {name} histogram"),
            format!("{name}_sum{{{system}}} 1.5"),
            format!("{name}_count{{{system}}} 7"),
            format!("{name}_bucket{{le=\"0.00001\",{system}}} 4"),
            format!("{name}_bucket{{le=\"0.001\",{system}}} 6"),
            format!("{name}_bucket{{le=\"+Inf\",{system}}} 7"),
            format!("{name}_count{{{web}}} 0"),
        ] {
            assert!(
                body.lines().any(|l| l == line),
                "missing {line:?} in\n{body}"
            );
        }
    }

    #[test]
    fn wait_encodes_seconds_by_cause_and_culprit() {
        let metrics = Metrics::new(&[Duration::from_millis(1)]);
        let stress = Workload {
            kind: "Deployment".into(),
            name: "stress".into(),
        };
        let stress = Entity::new(Granularity::Workload, "e2e", &stress, "", "");
        let spin = Workload {
            kind: "Deployment".into(),
            name: "spin".into(),
        };
        let spin = Entity::new(Granularity::Pod, "e2e", &spin, "spin-1", "");
        let wait = Wait {
            by_cause: [0, 2_500_000_000, 1_000, 3_000_000_000],
            by_culprit: vec![(stress, 2_500_000_000), (Entity::other(), 1_000)],
        };
        metrics.runqueue_wait.set(vec![(spin, wait)]);

        let mut body = String::new();
        encode(&mut body, &registry(&metrics)).unwrap();
        let victim = r#"namespace="e2e",workload_kind="Deployment",workload="spin",pod="spin-1""#;
        let cause = "noisy_neighbour_runqueue_wait_by_cause_seconds_total";
        let culprit = "noisy_neighbour_runqueue_wait_by_culprit_seconds_total";
        for line in [
            "# TYPE noisy_neighbour_runqueue_wait_by_cause_seconds counter".to_string(),
            format!("{cause}{{{victim},cause=\"same_workload\"}} 0.0"),
            format!("{cause}{{{victim},cause=\"other_workload\"}} 2.5"),
            format!("{cause}{{{victim},cause=\"system\"}} 0.000001"),
            format!("{cause}{{{victim},cause=\"idle\"}} 3.0"),
            format!(
                "{culprit}{{{victim},culprit_namespace=\"e2e\",\
                 culprit_workload_kind=\"Deployment\",culprit_workload=\"stress\"}} 2.5"
            ),
            format!(
                "{culprit}{{{victim},culprit_namespace=\"_other\",\
                 culprit_workload_kind=\"_other\",culprit_workload=\"_other\"}} 0.000001"
            ),
        ] {
            assert!(
                body.lines().any(|l| l == line),
                "missing {line:?} in\n{body}"
            );
        }
    }
}
