//! `tablica serve`: prepare the dashboards, bind, start pollers, serve.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::Loaded;
use crate::dashboard::{Active, Dashboard, PrepareContext};
use crate::device_api::{self, AppState, Clock, ImageStore};
use crate::render::{Renderer, TemplateSource};

pub struct ServeOptions {
    pub templates: TemplateSource,
    /// How long shutdown waits for the pollers to stop.
    pub stop_timeout: Duration,
}

/// Prepares every dashboard and builds the renderer and the app state. Touches
/// neither the network nor any listener. Returns the state and the prepared
/// dashboards, in entry order, so the caller decides when to start their pollers.
pub fn prepare_state(
    loaded: Loaded,
    templates: TemplateSource,
    clock: Clock,
) -> anyhow::Result<(AppState, Vec<Arc<dyn Dashboard>>)> {
    let cfg = Arc::new(loaded.config);
    let ctx = PrepareContext {
        timezone: cfg.timezone,
        data_dir: &cfg.data_dir,
    };
    let mut active = Active::new();
    let mut ordered = Vec::new();
    for parsed in loaded.dashboards {
        let dashboard = parsed
            .parsed
            .prepare(&ctx)
            .with_context(|| format!("preparing the {} dashboard", parsed.name))?;
        ordered.push(dashboard.clone());
        active.insert(parsed.name, dashboard);
    }
    let renderer = Arc::new(Renderer::new(templates, &active).context("building the renderer")?);
    let state = AppState {
        cfg,
        dashboards: Arc::new(active),
        renderer,
        images: Arc::new(ImageStore::new()),
        clock,
    };
    Ok((state, ordered))
}

/// Prepares the dashboards (see [`prepare_state`]), calls `shutdown` to install
/// the signal handlers, binds `listen`, and only then starts the pollers, so a
/// failure before serving never reaches the network. Serves until the future
/// `shutdown` returned resolves, then cancels the pollers and waits for them up
/// to `stop_timeout`.
pub async fn run<F>(
    loaded: Loaded,
    opts: ServeOptions,
    shutdown: impl FnOnce() -> anyhow::Result<F>,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (state, dashboards) = prepare_state(loaded, opts.templates, Arc::new(chrono::Utc::now))?;
    let cfg = state.cfg.clone();
    // Only after preparing: until here a signal keeps its default exit behaviour,
    // even while a cache read blocks.
    let shutdown = shutdown()?;

    // Bind first: a bind failure under a restart loop must never start pollers.
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    let stop = CancellationToken::new();
    let pollers: Vec<_> = dashboards
        .into_iter()
        .flat_map(|d| d.spawn_pollers(stop.clone()))
        .collect();

    let app = device_api::router(state);
    let names: Vec<&str> = cfg.devices.iter().map(|d| d.name.as_str()).collect();
    info!(devices = %names.join(", "), "listening on {}", cfg.listen);

    let stop_pollers = stop.clone();
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            stop_pollers.cancel();
        })
        .await
        .context("serving");

    stop.cancel();
    let stopped = tokio::time::timeout(opts.stop_timeout, async {
        for handle in pollers {
            let _ = handle.await;
        }
    })
    .await;
    if stopped.is_err() {
        warn!("fetchers did not stop in time");
    }
    served?;
    info!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use chrono::{DateTime, NaiveTime, Utc};
    use chrono_tz::Tz;
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::config::{Config, Loaded, ParsedEntry, Schedule, Secrets};
    use crate::dashboard::{Dashboard, ParsedDashboard, PrepareContext, View};
    use crate::device_api::stub::StubDashboard;
    use crate::render::TemplateSource;

    /// A parsed dashboard whose pollers are counted.
    struct Counting {
        spawned: Arc<AtomicUsize>,
        fail_prepare: bool,
        /// When set, the poller ignores cancellation.
        hang: bool,
    }

    impl ParsedDashboard for Counting {
        fn prepare(self: Box<Self>, _ctx: &PrepareContext) -> anyhow::Result<Arc<dyn Dashboard>> {
            anyhow::ensure!(!self.fail_prepare, "prepare failure requested");
            Ok(Arc::new(CountingDashboard {
                stub: StubDashboard::new("stubs"),
                spawned: self.spawned,
                hang: self.hang,
            }))
        }
    }

    struct CountingDashboard {
        stub: StubDashboard,
        spawned: Arc<AtomicUsize>,
        hang: bool,
    }

    impl Dashboard for CountingDashboard {
        fn load_templates(&self, env: &mut minijinja::Environment<'static>) {
            self.stub.load_templates(env);
        }

        fn spawn_pollers(self: Arc<Self>, shutdown: CancellationToken) -> Vec<JoinHandle<()>> {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            let hang = self.hang;
            vec![tokio::spawn(async move {
                if hang {
                    std::future::pending::<()>().await;
                } else {
                    shutdown.cancelled().await;
                }
            })]
        }

        fn view(&self, now: DateTime<Tz>) -> anyhow::Result<View> {
            self.stub.view(now)
        }

        fn health(&self, now: DateTime<Utc>) -> serde_json::Value {
            self.stub.health(now)
        }
    }

    fn loaded(listen: std::net::SocketAddr, stub: Counting) -> (Loaded, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let loaded = Loaded {
            config: Config {
                timezone: chrono_tz::Europe::Warsaw,
                base_url: "http://x".into(),
                listen,
                data_dir: dir.path().to_owned(),
                schedule: Schedule {
                    wake: NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
                    sleep: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
                    refresh_minutes: 15,
                },
                devices: Vec::new(),
            },
            dashboards: vec![ParsedEntry {
                name: "stubs",
                parsed: Box::new(stub),
            }],
            secrets: Secrets::new(PathBuf::from("/nonexistent")),
        };
        (loaded, dir)
    }

    fn counting(spawned: &Arc<AtomicUsize>) -> Counting {
        Counting {
            spawned: spawned.clone(),
            fail_prepare: false,
            hang: false,
        }
    }

    fn opts(stop_timeout: Duration) -> ServeOptions {
        ServeOptions {
            templates: TemplateSource::Embedded,
            stop_timeout,
        }
    }

    /// A free local port: bound, then released.
    async fn free_addr() -> std::net::SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    }

    #[tokio::test]
    async fn occupied_port_starts_no_pollers() {
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let spawned = Arc::new(AtomicUsize::new(0));
        let (loaded, _dir) = loaded(taken.local_addr().unwrap(), counting(&spawned));
        let err = run(loaded, opts(Duration::from_secs(1)), || Ok(async {}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("binding"), "{err:#}");
        assert_eq!(spawned.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failing_prepare_starts_no_pollers() {
        let addr = free_addr().await;
        let spawned = Arc::new(AtomicUsize::new(0));
        let stub = Counting {
            fail_prepare: true,
            ..counting(&spawned)
        };
        let (loaded, _dir) = loaded(addr, stub);
        let installed = Arc::new(AtomicUsize::new(0));
        let install = {
            let installed = installed.clone();
            move || {
                installed.fetch_add(1, Ordering::SeqCst);
                Ok(async {})
            }
        };
        let err = run(loaded, opts(Duration::from_secs(1)), install)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("prepare failure"), "{err:#}");
        assert_eq!(spawned.load(Ordering::SeqCst), 0);
        // Signal handlers are installed only once preparing succeeded.
        assert_eq!(installed.load(Ordering::SeqCst), 0);
        // The port was never bound, so it is still free.
        tokio::net::TcpListener::bind(addr).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_with_hanging_poller_returns_within_budget() {
        let addr = free_addr().await;
        let spawned = Arc::new(AtomicUsize::new(0));
        let stub = Counting {
            hang: true,
            ..counting(&spawned)
        };
        let (loaded, _dir) = loaded(addr, stub);
        let up = spawned.clone();
        let shutdown = async move {
            // Resolve once the server is up: pollers start only after the bind.
            while up.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(1500),
            run(loaded, opts(Duration::from_secs(1)), move || Ok(shutdown)),
        )
        .await;
        result
            .expect("run did not return within 1.5 s")
            .expect("run failed");
        assert_eq!(spawned.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() >= Duration::from_secs(1));
    }
}
