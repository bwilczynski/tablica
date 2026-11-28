//! `tablica serve`: prepare the dashboards, bind, seed metrics, start pollers, serve.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use tokio::sync::{Semaphore, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::{Device, Loaded};
use crate::dashboard::{Active, Dashboard, PrepareContext};
use crate::device_api::{self, AppState, Clock, ImageStore};
use crate::history::History;
use crate::metrics::{self, Recorder, ScrapeContext};
use crate::render::{Renderer, TemplateSource};

pub struct ServeOptions {
    pub templates: TemplateSource,
    /// How long shutdown waits for the pollers to stop.
    pub stop_timeout: Duration,
}

/// Prepares every dashboard and builds the renderer and the app state. Also
/// creates `data_dir`, opens `data_dir/history.db` and starts the history writer
/// thread; a history that cannot be opened is disabled, never an error. Touches
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
    let history = Arc::new(History::open(&cfg.data_dir, clock.clone()));
    let device_dashboards: Vec<(String, String)> = cfg
        .devices
        .iter()
        .map(|d| (d.name.clone(), d.dashboard.clone()))
        .collect();
    let recorder = Recorder::new(&device_dashboards);
    let state = AppState {
        cfg,
        dashboards: Arc::new(active),
        renderer,
        images: Arc::new(ImageStore::new()),
        history,
        diag_reads: Arc::new(Semaphore::new(1)),
        recorder,
        clock,
    };
    Ok((state, ordered))
}

/// Seeds each configured device's metrics state from history rows strictly
/// before `t0`'s second. Blocking. A history that cannot be read leaves the
/// state empty and logs a warning; seeding never touches counters.
pub fn seed_metrics(history: &History, recorder: &Recorder, devices: &[Device], t0: DateTime<Utc>) {
    let macs: Vec<String> = devices.iter().map(|d| d.mac.clone()).collect();
    match history.metrics_seed(&macs, t0) {
        Ok(seeds) => {
            for (mac, seed) in seeds {
                if let Some(device) = devices.iter().find(|d| d.mac == mac) {
                    recorder.seed(&device.name, seed);
                }
            }
        }
        Err(e) => warn!("metrics seed unavailable: {e:#}"),
    }
}

async fn bind(addr: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))
}

/// Prepares the dashboards (see [`prepare_state`]), calls `shutdown` to install
/// the signal handlers, binds `listen` and, when set, `metrics_listen`, and only
/// then seeds the metrics from history and starts the pollers, so a failure
/// before serving never reaches the network. Serves both listeners until the
/// future `shutdown` returned resolves, then cancels the pollers and waits for
/// them up to `stop_timeout`.
pub async fn run<F>(
    loaded: Loaded,
    opts: ServeOptions,
    shutdown: impl FnOnce() -> anyhow::Result<F>,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    run_with(loaded, opts, shutdown, None).await
}

/// [`run`], reporting the bound `(listen, metrics_listen)` addresses on
/// `on_bound` right after both binds.
async fn run_with<F>(
    loaded: Loaded,
    opts: ServeOptions,
    shutdown: impl FnOnce() -> anyhow::Result<F>,
    on_bound: Option<oneshot::Sender<(SocketAddr, Option<SocketAddr>)>>,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (state, dashboards) = prepare_state(loaded, opts.templates, Arc::new(chrono::Utc::now))?;
    let cfg = state.cfg.clone();
    let history = state.history.clone();
    // Only after preparing: until here a signal keeps its default exit behaviour,
    // even while a cache read blocks.
    let shutdown = shutdown()?;
    // Seeding reads only rows before this second; every live check-in is at or after it.
    let t0 = (state.clock)();

    // Bind first: a bind failure under a restart loop must never start pollers.
    let listener = bind(cfg.listen).await?;
    let metrics_listener = match cfg.metrics_listen {
        Some(addr) => Some(bind(addr).await?),
        None => None,
    };
    let main_addr = listener.local_addr().context("reading the bound address")?;
    let metrics_addr = metrics_listener
        .as_ref()
        .map(tokio::net::TcpListener::local_addr)
        .transpose()
        .context("reading the bound metrics address")?;
    if let Some(tx) = on_bound {
        let _ = tx.send((main_addr, metrics_addr));
    }

    {
        let history = state.history.clone();
        let recorder = state.recorder.clone();
        let devices = cfg.devices.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("metrics-seed".into())
            .spawn(move || seed_metrics(&history, &recorder, &devices, t0))
        {
            warn!("metrics seed unavailable: {e}");
        }
    }
    let stop = CancellationToken::new();
    let recorder = state.recorder.clone();
    let pollers: Vec<_> = dashboards
        .into_iter()
        .flat_map(|d| d.spawn_pollers(stop.clone(), recorder.clone()))
        .collect();

    let registry = metrics_listener.as_ref().map(|_| {
        Arc::new(metrics::build_registry(ScrapeContext {
            recorder: state.recorder.clone(),
            dashboards: state.dashboards.clone(),
            history: state.history.clone(),
            data_dir: cfg.data_dir.clone(),
            clock: state.clock.clone(),
        }))
    });
    let app = device_api::router(state);
    let names: Vec<&str> = cfg.devices.iter().map(|d| d.name.as_str()).collect();
    info!(devices = %names.join(", "), "listening on {main_addr}");
    if let Some(addr) = metrics_addr {
        info!("serving metrics on {addr}");
    }

    // The shutdown future is awaited once; both listeners stop on the token.
    let signal = {
        let stop = stop.clone();
        tokio::spawn(async move {
            shutdown.await;
            stop.cancel();
        })
    };
    // Connect info gives the access log the peer address when no proxy sets
    // `X-Forwarded-For`.
    let app = app.into_make_service_with_connect_info::<SocketAddr>();
    let main = {
        let stop = stop.clone();
        async move {
            let served = axum::serve(listener, app)
                .with_graceful_shutdown(stop.clone().cancelled_owned())
                .await
                .context("serving");
            // A listener that ends on its own takes the other one down with it.
            stop.cancel();
            served
        }
    };
    let metrics_served = {
        let stop = stop.clone();
        async move {
            let (Some(listener), Some(registry)) = (metrics_listener, registry) else {
                return Ok(());
            };
            let served = axum::serve(listener, metrics::router(registry))
                .with_graceful_shutdown(stop.clone().cancelled_owned())
                .await
                .context("serving metrics");
            stop.cancel();
            served
        }
    };
    let (served, metrics_served) = tokio::join!(main, metrics_served);
    signal.abort();

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
    // Flush what is queued and stop the writer; early returns above rely on `Drop`.
    if let Err(e) = tokio::task::spawn_blocking(move || history.shutdown()).await {
        warn!("history shutdown failed: {e}");
    }
    served?;
    metrics_served?;
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

        fn spawn_pollers(
            self: Arc<Self>,
            shutdown: CancellationToken,
            _recorder: crate::metrics::Recorder,
        ) -> Vec<JoinHandle<()>> {
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
                metrics_listen: None,
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

    const KITCHEN_MAC: &str = "AA:BB:CC:DD:EE:01";
    const WAIT: Duration = Duration::from_secs(5);

    fn kitchen() -> crate::config::Device {
        crate::config::Device {
            name: "kitchen".into(),
            mac: KITCHEN_MAC.into(),
            api_key: "key".into(),
            dashboard: "stubs".into(),
            bit_depth: crate::config::BitDepth::One,
        }
    }

    /// Writes `readings` into `data_dir/history.db` and closes it.
    fn write_history(data_dir: &std::path::Path, readings: Vec<crate::history::Reading>) {
        let history = History::open(data_dir, Arc::new(Utc::now));
        for r in readings {
            history.record_reading(r);
        }
        history.shutdown();
    }

    /// One HTTP/1.1 GET over a raw TCP stream: the lowercased head and the body.
    async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(WAIT, async {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let req = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let text = String::from_utf8(buf).unwrap();
            let (head, body) = text.split_once("\r\n\r\n").unwrap();
            let status = head.split(' ').nth(1).unwrap().parse().unwrap();
            (status, head.to_lowercase(), body.to_owned())
        })
        .await
        .expect("request timed out")
    }

    fn sample(body: &str, series: &str) -> Option<f64> {
        body.lines()
            .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
            .map(|v| v.split(' ').next().unwrap().parse().unwrap())
    }

    type Bound = (std::net::SocketAddr, Option<std::net::SocketAddr>);

    fn local() -> std::net::SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    #[tokio::test]
    async fn both_listeners_serve_then_stop() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let (mut loaded, dir) = loaded(local(), counting(&spawned));
        loaded.config.metrics_listen = Some(local());
        loaded.config.devices = vec![kitchen()];
        let seen = Utc::now() - chrono::TimeDelta::seconds(10);
        write_history(
            dir.path(),
            vec![crate::history::Reading {
                device: KITCHEN_MAC.into(),
                at: seen,
                battery_v: Some(4.0),
                ..Default::default()
            }],
        );

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let install = move || {
            Ok(async move {
                let _ = stopped.await;
            })
        };
        let (bound_tx, bound_rx) = tokio::sync::oneshot::channel::<Bound>();
        let server = tokio::spawn(run_with(
            loaded,
            opts(Duration::from_secs(1)),
            install,
            Some(bound_tx),
        ));
        let (main, metrics) = tokio::time::timeout(WAIT, bound_rx)
            .await
            .expect("never bound")
            .expect("bound sender dropped");
        let metrics = metrics.expect("no metrics listener");

        let series = r#"tablica_device_last_seen_timestamp_seconds{device="kitchen"}"#;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let (head, body) = loop {
            let (status, head, body) = http_get(metrics, "/metrics").await;
            assert_eq!(status, 200, "{head}");
            if sample(&body, series) == Some(seen.timestamp() as f64) {
                break (head, body);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "seed never arrived: {body}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(
            head.contains(&format!(
                "content-type: {}",
                crate::metrics::CONTENT_TYPE.to_lowercase()
            )),
            "{head}"
        );
        assert!(body.trim_end().ends_with("# EOF"), "{body}");

        assert_eq!(http_get(metrics, "/healthz").await.0, 404);
        assert_eq!(http_get(main, "/metrics").await.0, 404);
        assert_eq!(http_get(main, "/healthz").await.0, 200);

        stop.send(()).unwrap();
        tokio::time::timeout(WAIT, server)
            .await
            .expect("run did not stop")
            .unwrap()
            .expect("run failed");
        assert!(tokio::net::TcpStream::connect(main).await.is_err());
        assert!(tokio::net::TcpStream::connect(metrics).await.is_err());
    }

    #[tokio::test]
    async fn second_bind_failure_starts_no_pollers() {
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let spawned = Arc::new(AtomicUsize::new(0));
        let (mut loaded, _dir) = loaded(local(), counting(&spawned));
        loaded.config.metrics_listen = Some(taken.local_addr().unwrap());
        let err = tokio::time::timeout(
            WAIT,
            run_with(loaded, opts(Duration::from_secs(1)), || Ok(async {}), None),
        )
        .await
        .expect("run did not return")
        .unwrap_err();
        assert!(format!("{err:#}").contains("binding"), "{err:#}");
        assert_eq!(spawned.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn no_metrics_listen_binds_only_main() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let (loaded, _dir) = loaded(local(), counting(&spawned));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let install = move || {
            Ok(async move {
                let _ = stopped.await;
            })
        };
        let (bound_tx, bound_rx) = tokio::sync::oneshot::channel::<Bound>();
        let server = tokio::spawn(run_with(
            loaded,
            opts(Duration::from_secs(1)),
            install,
            Some(bound_tx),
        ));
        let (main, metrics) = tokio::time::timeout(WAIT, bound_rx)
            .await
            .expect("never bound")
            .expect("bound sender dropped");
        assert_eq!(metrics, None);
        assert_eq!(http_get(main, "/healthz").await.0, 200);
        assert_eq!(http_get(main, "/metrics").await.0, 404);
        stop.send(()).unwrap();
        tokio::time::timeout(WAIT, server)
            .await
            .expect("run did not stop")
            .unwrap()
            .expect("run failed");
    }

    #[test]
    fn seed_metrics_fields() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let at = |secs: i64| DateTime::from_timestamp(t0.timestamp() - secs, 0).unwrap();
        let row = |secs: i64| crate::history::Reading {
            device: KITCHEN_MAC.into(),
            at: at(secs),
            ..Default::default()
        };
        write_history(
            dir.path(),
            vec![
                crate::history::Reading {
                    wake_ms: Some(1200),
                    ..row(50)
                },
                crate::history::Reading {
                    image_bytes: Some(4321),
                    ..row(40)
                },
                crate::history::Reading {
                    next_refresh: Some(900),
                    ..row(30)
                },
                crate::history::Reading {
                    reset_reason: Some("ESP_RST_PANIC".into()),
                    ..row(20)
                },
                crate::history::Reading {
                    battery_v: Some(3.9),
                    ..row(10)
                },
            ],
        );
        let history = History::open(dir.path(), Arc::new(Utc::now));
        let recorder = Recorder::new(&[("kitchen".into(), "stubs".into())]);

        seed_metrics(&history, &recorder, &[kitchen()], t0);

        let st = recorder.devices().get("kitchen").expect("kitchen seeded");
        assert_eq!(st.last_seen, Some(at(10)));
        assert_eq!(st.last_wake, Some((at(50), 1200)));
        assert_eq!(st.last_image_bytes, Some((at(40), 4321)));
        assert_eq!(st.last_reply, Some((at(30), 900)));
        assert_eq!(st.last_reset, Some(at(20)));
        assert_eq!(
            st.samples.iter().copied().collect::<Vec<_>>(),
            vec![(at(10), 3.9)]
        );

        let mut registry = prometheus_client::registry::Registry::default();
        recorder.register(&mut registry);
        let body = crate::metrics::encode(&registry).unwrap();
        let resets: Vec<&str> = body
            .lines()
            .filter(|l| l.starts_with("tablica_device_resets_total{"))
            .collect();
        assert!(!resets.is_empty(), "{body}");
        for line in resets {
            assert!(line.ends_with(" 0"), "{line}");
        }
        history.shutdown();
    }
}
