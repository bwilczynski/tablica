//! Prometheus metrics: recording, seeding from history, and scrape-time encoding.

mod collector;
pub mod devices;
pub mod labels;
pub mod process;
mod recorder;

use prometheus_client::registry::Registry;

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
