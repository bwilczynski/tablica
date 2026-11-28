//! Prometheus metrics: recording, seeding from history, and scrape-time encoding.

pub mod devices;
pub mod labels;

pub use devices::{DeviceState, DeviceStore, MetricsSeed};
