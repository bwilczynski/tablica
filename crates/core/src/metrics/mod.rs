//! Prometheus metrics: recording, seeding from history, and scrape-time encoding.

mod collector;
pub mod devices;
pub mod labels;
pub mod process;
mod recorder;

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use prometheus_client::registry::Registry;
use tracing::warn;

pub use collector::ScrapeContext;
pub use devices::{DeviceState, DeviceStore, MetricsSeed};
pub use recorder::{HttpOutcome, Recorder, RenderKind, SourceMetrics};

/// The scrape response's content type.
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// The registry a scrape encodes: the recorder's families, the scrape-time
/// collector and the process collector.
pub fn build_registry(ctx: ScrapeContext) -> Registry {
    let mut registry = Registry::default();
    ctx.recorder.register(&mut registry);
    registry.register_collector(Box::new(collector::ScrapeCollector::new(ctx)));
    registry.register_collector(Box::new(process::ProcessCollector));
    registry
}

/// Encodes the whole registry as OpenMetrics text, ending in one `# EOF`.
/// Reads procfs and the filesystem, so callers run it in `spawn_blocking`. On
/// error, fail the scrape; never send a partial body.
pub fn encode(registry: &Registry) -> Result<String, std::fmt::Error> {
    let mut body = String::new();
    prometheus_client::encoding::text::encode(&mut body, registry)?;
    Ok(body)
}

/// The metrics listener's router: `GET /metrics` and nothing else.
pub fn router(registry: Arc<Registry>) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(registry)
}

/// Encodes off the async workers; a failed encode is a 500, never a partial body.
async fn scrape(State(registry): State<Arc<Registry>>) -> Response {
    match tokio::task::spawn_blocking(move || encode(&registry)).await {
        Ok(Ok(body)) => ([(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response(),
        Ok(Err(e)) => {
            warn!("encoding metrics failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            warn!("encoding metrics failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
