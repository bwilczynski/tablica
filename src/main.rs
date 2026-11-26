use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use tablica::config::{Config, Loaded};
use tablica::dashboard::DashboardEntry;
use tablica::device_api::{self, AppState, ImageStore};
use tablica::family::FamilyCaches;
use tablica::family::config::FamilyConfig;
use tablica::family::dashboard::ParsedFamily;
use tablica::render::{Renderer, TemplateSource};
use tablica::scheduler::{self, Sources};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
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

/// Loads the config and takes the family dashboard's parsed section out of it.
// Interim: removed once Task 5 serves through `Dashboard` and Task 6 types capture.
fn load(config: &Path, secrets_dir: &Path) -> anyhow::Result<(Config, ParsedFamily)> {
    let Loaded {
        config: cfg,
        dashboards,
        ..
    } = tablica::config::load(config, secrets_dir, &entries())
        .with_context(|| format!("loading {}", config.display()))?;
    let family = dashboards
        .into_iter()
        .find(|d| d.name == "family")
        .context("family dashboard is not active")?;
    let parsed: Box<dyn Any> = family.parsed;
    let parsed = parsed
        .downcast::<ParsedFamily>()
        .map_err(|_| anyhow!("family entry did not parse to ParsedFamily"))?;
    Ok((cfg, *parsed))
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

async fn serve(
    cfg: Config,
    family: ParsedFamily,
    templates: Option<PathBuf>,
) -> anyhow::Result<()> {
    let cfg = Arc::new(cfg);
    let ParsedFamily {
        cfg: family,
        endpoints,
    } = family;
    let family: Arc<FamilyConfig> = Arc::new(family);
    let caches = Arc::new(FamilyCaches::open(&cfg.data_dir, family.kids.len()));
    let source = match templates {
        Some(dir) => TemplateSource::Dir(dir),
        None => TemplateSource::Embedded,
    };
    let renderer = Arc::new(Renderer::new(source).context("building the renderer")?);

    let http = reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building the http client")?;
    let sources = Sources {
        http,
        weather_base: endpoints.weather_base,
        librus: endpoints.librus,
    };
    // Bind first: a bind failure under a restart loop must never trigger Librus logins.
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    let stop = CancellationToken::new();
    let fetchers = scheduler::spawn_all(
        cfg.clone(),
        family.clone(),
        caches.clone(),
        sources,
        stop.clone(),
    );

    let app = device_api::router(AppState {
        cfg: cfg.clone(),
        family,
        caches,
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
            let (cfg, family) = load(&config, &secrets_dir)?;
            serve(cfg, family, templates).await?;
        }
        Command::Capture {
            source:
                CaptureSource::Librus {
                    config,
                    secrets_dir,
                    out,
                },
        } => {
            let (_, family) = load(&config, &secrets_dir)?;
            tablica::sources::librus::capture(&family.cfg, &out).await?;
        }
    }
    Ok(())
}
