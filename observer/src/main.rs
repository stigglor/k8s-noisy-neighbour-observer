//! Noisy neighbour observer entrypoint.

mod config;
mod metrics;
mod routes;
mod telemetry;

use std::sync::Arc;

use anyhow::Context;
use routes::AppState;

/// musl's mallocng does not return freed pages to the OS, so allocation churn settles as
/// permanent RSS. Linux only, which is the only platform the agent ships.
#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// The commit served at `/version` and labelled on `noisy_neighbour_build_info`.
pub fn git_sha() -> &'static str {
    option_env!("NNO_GIT_SHA").unwrap_or("unknown")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init_tracing();
    let config = config::Config::from_env().context("loading process config")?;

    let state = Arc::new(AppState {
        registry: metrics::registry(),
    });

    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("binding {}", config.bind_addr))?;
    tracing::info!(addr = %config.bind_addr, git_sha = git_sha(), "listening");

    axum::serve(listener, routes::router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    tracing::info!("shutdown complete");
    Ok(())
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
