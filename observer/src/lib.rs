//! Noisy neighbour observer. The binary in `main.rs` wires these modules together, and the
//! integration tests drive them directly.

pub mod attribution;
#[cfg(target_os = "linux")]
pub mod bpf;
pub mod cardinality;
pub mod cgroup;
pub mod config;
pub mod k8s;
pub mod metrics;
pub mod offsets;
pub mod routes;
pub mod runq;
pub mod telemetry;
pub mod workload;

/// The commit served at `/version` and labelled on `noisy_neighbour_build_info`.
pub fn git_sha() -> &'static str {
    option_env!("NNO_GIT_SHA").unwrap_or("unknown")
}
