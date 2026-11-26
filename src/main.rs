use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use tablica::config::Loaded;
use tablica::dashboard::{Active, DashboardEntry, PrepareContext};
use tablica::device_api::{self, AppState, ImageStore};
use tablica::family::dashboard::ParsedFamily;
use tablica::render::{Renderer, TemplateSource};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
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
    tablica::config::load(config, secrets_dir, &entries())
        .with_context(|| format!("loading {}", config.display()))
}

/// Takes the family dashboard's parsed section out of the loaded config.
// Interim: Task 6 types capture.
fn parsed_family(loaded: Loaded) -> anyhow::Result<ParsedFamily> {
    let family = loaded
        .dashboards
        .into_iter()
        .find(|d| d.name == "family")
        .context("family dashboard is not active")?;
    let parsed: Box<dyn Any> = family.parsed;
    let parsed = parsed
        .downcast::<ParsedFamily>()
        .map_err(|_| anyhow!("family entry did not parse to ParsedFamily"))?;
    Ok(*parsed)
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

async fn serve(loaded: Loaded, templates: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = Arc::new(loaded.config);
    let ctx = PrepareContext {
        timezone: cfg.timezone,
        data_dir: &cfg.data_dir,
    };
    let mut dashboards = Active::new();
    for parsed in loaded.dashboards {
        let dashboard = parsed
            .parsed
            .prepare(&ctx)
            .with_context(|| format!("preparing the {} dashboard", parsed.name))?;
        dashboards.insert(parsed.name, dashboard);
    }
    let source = match templates {
        Some(dir) => TemplateSource::Dir(dir),
        None => TemplateSource::Embedded,
    };
    let renderer = Arc::new(Renderer::new(source, &dashboards).context("building the renderer")?);

    // Bind first: a bind failure under a restart loop must never trigger Librus logins.
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    let stop = CancellationToken::new();
    let fetchers: Vec<_> = dashboards
        .values()
        .flat_map(|d| d.clone().spawn_pollers(stop.clone()))
        .collect();

    let app = device_api::router(AppState {
        cfg: cfg.clone(),
        dashboards: Arc::new(dashboards),
        renderer,
        images: Arc::new(ImageStore::new()),
        clock: Arc::new(chrono::Utc::now),
    });
    let names: Vec<&str> = cfg.devices.iter().map(|d| d.name.as_str()).collect();
    info!(devices = %names.join(", "), "listening on {}", cfg.listen);

    let shutdown = shutdown_signal()?;
    let stop_fetchers = stop.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            stop_fetchers.cancel();
        })
        .await
        .context("serving")?;

    stop.cancel();
    let stopped = tokio::time::timeout(FETCHER_STOP_TIMEOUT, async {
        for handle in fetchers {
            let _ = handle.await;
        }
    })
    .await;
    if stopped.is_err() {
        warn!("fetchers did not stop in time");
    }
    info!("stopped");
    Ok(())
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
            serve(load(&config, &secrets_dir)?, templates).await?;
        }
        Command::Capture {
            source:
                CaptureSource::Librus {
                    config,
                    secrets_dir,
                    out,
                },
        } => {
            let family = parsed_family(load(&config, &secrets_dir)?)?;
            tablica::sources::librus::capture(&family.cfg, &out).await?;
        }
    }
    Ok(())
}
