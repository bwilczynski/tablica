use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tablica::family::dashboard::family_kids;
use tablica_core::config::Loaded;
use tablica_core::dashboard::DashboardEntry;
use tablica_core::render::TemplateSource;
use tablica_core::server::ServeOptions;
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// How long shutdown waits for the fetch loops to stop.
const FETCHER_STOP_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Parser)]
#[command(name = "tablica", version, about = "TRMNL e-ink backend")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// Path to the TOML config file.
        #[arg(long)]
        config: PathBuf,
        /// Directory holding secret files.
        #[arg(long, default_value = "/run/secrets")]
        secrets_dir: PathBuf,
        /// Directory holding templates (overrides the built-in ones).
        #[arg(long)]
        templates: Option<PathBuf>,
    },
    /// Record raw responses from an external source as test fixtures.
    Capture {
        #[command(subcommand)]
        source: CaptureSource,
    },
}

#[derive(Subcommand)]
enum CaptureSource {
    /// Log in to Librus once per kid and save the login chain and two timetable weeks.
    Librus {
        /// Path to the TOML config file.
        #[arg(long)]
        config: PathBuf,
        /// Directory holding secret files.
        #[arg(long, default_value = "/run/secrets")]
        secrets_dir: PathBuf,
        /// Output directory (one `kid{N}` subdirectory per kid).
        #[arg(long)]
        out: PathBuf,
    },
}

/// Logs to stdout at `info`, or as `RUST_LOG` says.
fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

fn entries() -> Vec<DashboardEntry> {
    vec![
        tablica::family::dashboard::entry(),
        tablica::dashboards::homelab::entry(),
    ]
}

fn load(config: &Path, secrets_dir: &Path) -> anyhow::Result<Loaded> {
    tablica_core::config::load(config, secrets_dir, &entries())
        .with_context(|| format!("loading {}", config.display()))
}

/// Installs SIGTERM and SIGINT handlers; the returned future resolves on the first.
fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()>> {
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => info!("SIGTERM received, shutting down"),
            _ = int.recv() => info!("SIGINT received, shutting down"),
        }
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    match Cli::parse().command {
        Command::Serve {
            config,
            secrets_dir,
            templates,
        } => {
            let loaded = load(&config, &secrets_dir)?;
            let shutdown = shutdown_signal()?;
            let templates = match templates {
                Some(dir) => TemplateSource::Dir(dir),
                None => TemplateSource::Embedded,
            };
            let opts = ServeOptions {
                templates,
                stop_timeout: FETCHER_STOP_TIMEOUT,
            };
            tablica_core::server::run(loaded, opts, shutdown).await?;
        }
        Command::Capture {
            source:
                CaptureSource::Librus {
                    config,
                    secrets_dir,
                    out,
                },
        } => {
            let kids = family_kids(load(&config, &secrets_dir)?)?;
            tablica::sources::librus::capture(&kids, &out).await?;
        }
    }
    Ok(())
}
