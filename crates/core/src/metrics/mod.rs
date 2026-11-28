//! Prometheus metrics: recording, seeding from history, and scrape-time encoding.

pub mod devices;
pub mod labels;
mod recorder;

pub use devices::{DeviceState, DeviceStore, MetricsSeed};
pub use recorder::{HttpOutcome, Recorder, RenderKind, SourceMetrics};
