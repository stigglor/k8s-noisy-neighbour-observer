//! HTTP surface: `/metrics`, the liveness probe and `/version`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use serde_json::{json, Value};

pub struct AppState {
    pub registry: Registry,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .with_state(state)
}

const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut body = String::new();
    match encode(&mut body, &state.registry) {
        Ok(()) => (StatusCode::OK, [(header::CONTENT_TYPE, OPENMETRICS)], body),
        Err(error) => {
            tracing::error!(%error, "failed to encode metrics");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                String::new(),
            )
        }
    }
}

/// Liveness, the process is up.
async fn healthz() -> &'static str {
    "ok"
}

/// The commit the image was built from, so a deploy can tell which build serves.
async fn version() -> Json<Value> {
    Json(json!({ "git_sha": crate::git_sha() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tower::ServiceExt;

    fn state() -> Arc<AppState> {
        let metrics = Metrics::new(&[Duration::from_millis(1)]);
        Arc::new(AppState {
            registry: crate::metrics::registry(&metrics),
        })
    }

    async fn get_path(state: &Arc<AppState>, path: &str) -> (StatusCode, String) {
        let response = router(state.clone())
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (status, body) = get_path(&state(), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn metrics_exposes_build_info() {
        let (status, body) = get_path(&state(), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("noisy_neighbour_build_info{"), "{body}");
        assert!(body.contains(concat!("version=\"", env!("CARGO_PKG_VERSION"), "\"")));
        assert!(body.contains("git_sha=\""));
        assert!(body.contains("} 1\n"));
    }

    #[tokio::test]
    async fn version_reports_git_sha() {
        let (status, body) = get_path(&state(), "/version").await;
        assert_eq!(status, StatusCode::OK);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["git_sha"], crate::git_sha());
    }
}
