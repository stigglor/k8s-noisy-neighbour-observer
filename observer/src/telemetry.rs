//! Structured JSON logs. Respects `RUST_LOG` and defaults to `info`.

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Event fields sit at the top level of each JSON line and span data is omitted, so a log
/// pipeline indexes a flat record.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}
