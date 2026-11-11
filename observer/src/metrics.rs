//! The Prometheus registry. Every series here counts against the cardinality budget in
//! `CLAUDE.md`, so add a label only after that review.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabels {
    version: String,
    git_sha: String,
}

pub fn registry() -> Registry {
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
    registry
}
