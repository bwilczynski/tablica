//! Device telemetry and log history, kept in SQLite.
//!
//! One writer thread owns the only write connection and is fed by a bounded,
//! lossy queue: producers never wait. Reads open their own read-only
//! connection. History is best-effort: when the database cannot be opened it is
//! "unavailable" and everything recorded is dropped.

mod logs;
mod store;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use chrono::{DateTime, TimeDelta, Utc};
use tracing::{error, warn};

use crate::device_api::Clock;
use crate::metrics::MetricsSeed;
pub use logs::parse_log_body;
use store::Store;

pub const QUEUE_CAPACITY: usize = 256;
pub const RETENTION_DAYS: i64 = 90;
pub const MAX_LOGS_PER_DEVICE: i64 = 2000;
/// About 90 days at ~220 readings a day.
pub const MAX_READINGS_PER_DEVICE: i64 = 20_000;
pub const HEADER_MAX: usize = 64;
pub const MESSAGE_MAX: usize = 1024;
pub const RAW_MAX: usize = 4096;
/// The largest `/api/log` body that is parsed; a larger one is stored as one
/// "body too large" row.
pub const LOG_BODY_LIMIT: usize = 1_048_576;
/// The most `/api/log` body bytes waiting in the queue at once.
pub const QUEUE_BYTES_MAX: usize = 8 * 1_048_576;
pub const CORRUPT_KEEP: usize = 3;
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
pub const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
pub const READ_DEADLINE: Duration = Duration::from_secs(2);

/// How often an idle writer wakes to check whether a prune is due.
const IDLE_WAKE: Duration = Duration::from_secs(60);
/// The least time between two "queue full" warnings.
const DROP_WARN_EVERY: Duration = Duration::from_secs(60);

/// One `/api/display` request's telemetry.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct Reading {
    pub device: String, // canonical MAC
    pub at: DateTime<Utc>,
    pub battery_v: Option<f64>,
    pub rssi: Option<i64>,
    pub wifi_band: Option<String>,
    pub wake_ms: Option<i64>,
    pub reset_reason: Option<String>,
    pub update_source: Option<String>,
    pub image_cached: Option<bool>,
    pub refresh_rate: Option<i64>,
    pub next_refresh: Option<i64>,
    pub fw_version: Option<String>,
    pub fw_commit: Option<String>,
    pub model: Option<String>,
    pub panel_rev: Option<String>,
    pub render_ms: Option<i64>,
    pub image_bytes: Option<i64>,
}

/// One entry of a `/api/log` body.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeviceLog {
    pub device: String,
    pub received_at: DateTime<Utc>,
    pub created_at: Option<DateTime<Utc>>,
    pub level: Option<String>,
    pub message: Option<String>,
    pub wake_reason: Option<String>,
    pub raw: String,
}

/// What the diagnostics screen reads for one device as of `at`, from one read transaction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeviceHistory {
    /// Newest with `at <= at`, any age.
    pub latest: Option<Reading>,
    /// `at` in `(at - 7d, at]`, oldest first (trend data).
    pub readings: Vec<Reading>,
    /// Newest with `reset_reason` not `ESP_RST_DEEPSLEEP`, `at <= at`.
    pub last_problem: Option<Reading>,
    /// Newest by `(received_at, id)` with `received_at <= at`.
    pub last_log: Option<DeviceLog>,
}

enum Op {
    Reading(Box<Reading>),
    LogBody {
        device: String,
        received_at: DateTime<Utc>,
        body: Vec<u8>,
    },
    LogTooLarge {
        device: String,
        received_at: DateTime<Utc>,
        len: usize,
    },
    #[cfg(test)]
    Prune(DateTime<Utc>),
    Flush(SyncSender<()>),
    #[cfg(test)]
    Stall(Receiver<()>),
    /// Makes closing the store wait until the sender sends or is dropped.
    #[cfg(test)]
    StallClose(Receiver<()>),
}

/// The handle to the history database: cheap to record into, never blocking.
pub struct History {
    data_dir: Option<PathBuf>,
    sender: RwLock<Option<SyncSender<Op>>>,
    done: Mutex<Option<Receiver<()>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    dropped: AtomicU64,
    /// Bytes of log bodies queued and not yet taken by the writer.
    queued_bytes: Arc<AtomicUsize>,
    last_drop_warn: Mutex<Option<Instant>>,
    /// Set once the writer is found gone before shutdown, so that is logged once.
    writer_gone: AtomicBool,
    /// Test hook: a snapshot waits on this first, standing in for a stalled read.
    #[cfg(test)]
    read_gate: Mutex<Option<Receiver<()>>>,
}

impl History {
    /// Opens `data_dir/history.db` and starts the writer. Never fails: on any
    /// error it logs and returns a disabled handle.
    pub fn open(data_dir: &Path, clock: Clock) -> History {
        let store = match Store::open(data_dir, clock()) {
            Ok(store) => store,
            Err(e) => {
                error!("history unavailable: {e:#}");
                return History::disabled();
            }
        };
        let (sender, queue) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (done_tx, done) = mpsc::sync_channel(1);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let taken = queued_bytes.clone();
        let spawned = std::thread::Builder::new()
            .name("history-writer".into())
            .spawn(move || run_writer(store, queue, clock, done_tx, &taken));
        match spawned {
            Ok(join) => History {
                data_dir: Some(data_dir.to_owned()),
                sender: RwLock::new(Some(sender)),
                done: Mutex::new(Some(done)),
                join: Mutex::new(Some(join)),
                dropped: AtomicU64::new(0),
                queued_bytes,
                last_drop_warn: Mutex::new(None),
                writer_gone: AtomicBool::new(false),
                #[cfg(test)]
                read_gate: Mutex::new(None),
            },
            Err(e) => {
                error!("history unavailable: cannot start the writer: {e}");
                History::disabled()
            }
        }
    }

    /// A handle that drops everything recorded and has nothing to read.
    pub fn disabled() -> History {
        History {
            data_dir: None,
            sender: RwLock::new(None),
            done: Mutex::new(None),
            join: Mutex::new(None),
            dropped: AtomicU64::new(0),
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            last_drop_warn: Mutex::new(None),
            writer_gone: AtomicBool::new(false),
            #[cfg(test)]
            read_gate: Mutex::new(None),
        }
    }

    pub fn is_available(&self) -> bool {
        self.data_dir.is_some()
    }

    pub fn record_reading(&self, mut r: Reading) {
        for field in [
            &mut r.wifi_band,
            &mut r.reset_reason,
            &mut r.update_source,
            &mut r.fw_version,
            &mut r.fw_commit,
            &mut r.model,
            &mut r.panel_rev,
        ] {
            *field = field.take().map(|s| truncate(s, HEADER_MAX));
        }
        self.send(Op::Reading(Box::new(r)));
    }

    /// Queues a whole `/api/log` body of at most `LOG_BODY_LIMIT` bytes; a
    /// larger one (whatever it holds) queues a "body too large" marker instead,
    /// stored as one row without parsing. An empty (or whitespace-only) body
    /// within the limit stores nothing. A body that would take the queued
    /// bytes past `QUEUE_BYTES_MAX` is dropped like one meeting a full queue.
    pub fn record_log_body(&self, device: &str, received_at: DateTime<Utc>, body: &[u8]) {
        if !self.is_available() {
            return;
        }
        let len = body.len();
        if len > LOG_BODY_LIMIT {
            self.send(Op::LogTooLarge {
                device: device.to_owned(),
                received_at,
                len,
            });
            return;
        }
        if body.trim_ascii().is_empty() {
            return;
        }
        let device = device.to_owned();
        let reserved = self
            .queued_bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |q| {
                q.checked_add(len).filter(|&n| n <= QUEUE_BYTES_MAX)
            });
        if reserved.is_err() {
            self.note_dropped();
            return;
        }
        let op = Op::LogBody {
            device,
            received_at,
            body: body.to_vec(),
        };
        if !self.send(op) {
            self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
        }
    }

    /// Blocks until everything queued before this call has been handled.
    pub fn flush(&self) {
        let Some(sender) = self.sender().clone() else {
            return;
        };
        let (ack, acked) = mpsc::sync_channel(1);
        if sender.send(Op::Flush(ack)).is_ok() {
            // An error means the writer is gone, which also ends the wait.
            let _ = acked.recv();
        }
    }

    pub fn shutdown(&self) {
        self.shutdown_within(SHUTDOWN_WAIT);
    }

    /// Closes the queue and waits up to `limit` for the writer to drain it.
    /// Returns true when the writer finished (or there was none to wait for);
    /// on timeout the writer is left running detached. A second call is a no-op.
    pub fn shutdown_within(&self, limit: Duration) -> bool {
        self.sender
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(done) = self
            .done
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return true;
        };
        let join = self
            .join
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match done.recv_timeout(limit) {
            // Disconnected: the writer ended without signalling (it panicked).
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(Err(_)) = join.map(JoinHandle::join) {
                    error!("history writer panicked");
                }
                true
            }
            Err(RecvTimeoutError::Timeout) => {
                warn!("history writer did not finish within {limit:?}; leaving it behind");
                false
            }
        }
    }

    /// Reads one device's history as of `at`. Blocking; bounded by `READ_DEADLINE`.
    pub fn snapshot(&self, device: &str, at: DateTime<Utc>) -> anyhow::Result<DeviceHistory> {
        #[cfg(test)]
        if let Some(gate) = self
            .read_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = gate.recv();
        }
        let dir = self
            .data_dir
            .as_deref()
            .ok_or_else(|| anyhow!("history unavailable"))?;
        store::snapshot(dir, device, at)
    }

    /// Reads the metrics seed for each of `macs`, in order, from rows strictly
    /// before `t0`'s second. Blocking; bounded by `READ_DEADLINE`.
    pub fn metrics_seed(
        &self,
        macs: &[String],
        t0: DateTime<Utc>,
    ) -> anyhow::Result<Vec<(String, MetricsSeed)>> {
        let dir = self
            .data_dir
            .as_deref()
            .ok_or_else(|| anyhow!("history unavailable"))?;
        store::metrics_seed(dir, macs, t0)
    }

    /// Items dropped because the queue (or its byte budget) was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn sender(&self) -> std::sync::RwLockReadGuard<'_, Option<SyncSender<Op>>> {
        self.sender.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `op` without waiting; false when it was not queued.
    fn send(&self, op: Op) -> bool {
        let Some(sent) = self.sender().as_ref().map(|s| s.try_send(op)) else {
            return false;
        };
        match sent {
            Ok(()) => return true,
            // The sender is taken on shutdown, so this means the writer died.
            Err(TrySendError::Disconnected(_)) => {
                if !self.writer_gone.swap(true, Ordering::Relaxed) {
                    error!("history writer stopped; history is no longer recorded");
                }
            }
            Err(TrySendError::Full(_)) => self.note_dropped(),
        }
        false
    }

    /// Counts an item dropped for want of room, warning at most once a minute.
    fn note_dropped(&self) {
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = self
            .last_drop_warn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if last.is_none_or(|t| t.elapsed() >= DROP_WARN_EVERY) {
            *last = Some(Instant::now());
            warn!("history queue full; {total} items dropped so far");
        }
    }

    /// Blocks the next snapshot until the returned sender sends or is dropped.
    #[cfg(test)]
    pub(crate) fn stall_next_read(&self) -> SyncSender<()> {
        let (release, wait) = mpsc::sync_channel(0);
        *self
            .read_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(wait);
        release
    }

    /// Blocks the writer until the returned sender sends or is dropped.
    #[cfg(test)]
    fn stall(&self) -> SyncSender<()> {
        let (release, wait) = mpsc::sync_channel(0);
        let sender = self.sender().clone().expect("history is available");
        sender.send(Op::Stall(wait)).unwrap();
        release
    }
}

impl Drop for History {
    /// Closes the queue without waiting; the writer drains it and exits.
    fn drop(&mut self) {
        self.sender
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// Cuts `s` to at most `max` bytes, at a char boundary.
pub(crate) fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let end = s.floor_char_boundary(max);
        s.truncate(end);
    }
    s
}

fn run_writer(
    mut store: Store,
    queue: Receiver<Op>,
    clock: Clock,
    done: SyncSender<()>,
    queued_bytes: &AtomicUsize,
) {
    let prune_every = TimeDelta::from_std(PRUNE_EVERY).unwrap_or(TimeDelta::days(1));
    let prune = |store: &mut Store, now: DateTime<Utc>| {
        if let Err(e) = store.prune(now) {
            warn!("history prune failed: {e}");
        }
        now + prune_every
    };
    let mut next_prune = prune(&mut store, clock());
    loop {
        match queue.recv_timeout(IDLE_WAKE) {
            Ok(op) => {
                if let Op::LogBody { body, .. } = &op {
                    queued_bytes.fetch_sub(body.len(), Ordering::Relaxed);
                }
                handle(&mut store, op);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let now = clock();
        if now >= next_prune {
            next_prune = prune(&mut store, now);
        }
    }
    // Close the database (its final checkpoint and sync) before signalling, so
    // that work stays inside the caller's timed wait rather than its join.
    drop(store);
    let _ = done.send(());
}

fn handle(store: &mut Store, op: Op) {
    match op {
        Op::Reading(r) => {
            if let Err(e) = store.insert_reading(&r) {
                warn!(device = r.device, "history: cannot store reading: {e}");
            }
        }
        Op::LogBody {
            device,
            received_at,
            body,
        } => {
            let rows = parse_log_body(&device, received_at, &body);
            if let Err(e) = store.insert_logs(&rows) {
                warn!(device, "history: cannot store device log: {e}");
            }
        }
        Op::LogTooLarge {
            device,
            received_at,
            len,
        } => {
            let row = DeviceLog {
                device,
                received_at,
                created_at: None,
                level: None,
                message: None,
                wake_reason: None,
                raw: format!("body too large ({len} bytes)"),
            };
            if let Err(e) = store.insert_logs(std::slice::from_ref(&row)) {
                warn!(device = row.device, "history: cannot store device log: {e}");
            }
        }
        #[cfg(test)]
        Op::Prune(now) => {
            if let Err(e) = store.prune(now) {
                warn!("history prune failed: {e}");
            }
        }
        Op::Flush(ack) => {
            let _ = ack.send(());
        }
        #[cfg(test)]
        Op::Stall(wait) => {
            let _ = wait.recv();
        }
        #[cfg(test)]
        Op::StallClose(wait) => store.close_gate = Some(wait),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use chrono::{DateTime, TimeDelta, Utc};

    use super::store::Store;
    use super::*;

    const MAC: &str = "AA:BB:CC:DD:EE:FF";
    const OTHER: &str = "11:22:33:44:55:66";

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        t("2026-11-27T12:00:00Z")
    }

    fn fixed(at: DateTime<Utc>) -> Clock {
        Arc::new(move || at)
    }

    fn reading(at: DateTime<Utc>) -> Reading {
        Reading {
            device: MAC.into(),
            at,
            ..Reading::default()
        }
    }

    fn full(at: DateTime<Utc>) -> Reading {
        Reading {
            device: MAC.into(),
            at,
            battery_v: Some(4.12),
            rssi: Some(-61),
            wifi_band: Some("2.4".into()),
            wake_ms: Some(4210),
            reset_reason: Some("ESP_RST_DEEPSLEEP".into()),
            update_source: Some("timer".into()),
            image_cached: Some(false),
            refresh_rate: Some(900),
            next_refresh: Some(1800),
            fw_version: Some("1.6.2".into()),
            fw_commit: Some("abc1234".into()),
            model: Some("og".into()),
            panel_rev: Some("v2".into()),
            render_ms: Some(512),
            image_bytes: Some(23_456),
        }
    }

    fn open_at(dir: &std::path::Path, at: DateTime<Utc>) -> History {
        let h = History::open(dir, fixed(at));
        assert!(h.is_available());
        h
    }

    #[test]
    fn reading_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let r = full(now() - TimeDelta::minutes(5));
        h.record_reading(r.clone());
        h.flush();
        let snap = h.snapshot(MAC, now()).unwrap();
        assert_eq!(snap.latest, Some(r.clone()));
        assert_eq!(snap.readings, vec![r]);
        assert_eq!(snap.last_problem, None);
        assert_eq!(snap.last_log, None);
    }

    #[test]
    fn strings_are_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let at = now() - TimeDelta::minutes(1);
        h.record_reading(Reading {
            fw_version: Some("x".repeat(10_000)),
            model: Some("é".repeat(40)), // 80 bytes; 64 falls on a boundary, 63 would not
            panel_rev: Some(format!("a{}", "é".repeat(40))), // byte 64 is mid-char
            ..reading(at)
        });
        let entry = serde_json::json!({
            "message": "m".repeat(5_000),
            "level": "error",
        });
        let body = serde_json::to_vec(&serde_json::json!({ "logs": [entry] })).unwrap();
        h.record_log_body(MAC, at, &body);
        h.flush();

        let snap = h.snapshot(MAC, now()).unwrap();
        let latest = snap.latest.unwrap();
        assert_eq!(latest.fw_version.unwrap().len(), HEADER_MAX);
        assert_eq!(latest.model.unwrap(), "é".repeat(32));
        assert_eq!(latest.panel_rev.unwrap(), format!("a{}", "é".repeat(31)));
        let log = snap.last_log.unwrap();
        assert_eq!(log.message.unwrap().len(), MESSAGE_MAX);
        assert_eq!(log.raw.len(), RAW_MAX);
        assert_eq!(log.level.as_deref(), Some("error"));
    }

    #[test]
    fn truncate_cuts_on_a_char_boundary() {
        assert_eq!(truncate("aé".into(), 2), "a");
        assert_eq!(truncate("aé".into(), 3), "aé");
        assert_eq!(truncate("short".into(), 64), "short");
    }

    #[test]
    fn snapshot_window() {
        let dir = tempfile::tempdir().unwrap();
        let at = now();
        let h = open_at(dir.path(), at);
        let brownout = Reading {
            reset_reason: Some("ESP_RST_BROWNOUT".into()),
            ..reading(at - TimeDelta::days(30))
        };
        let deepsleep = |when| Reading {
            reset_reason: Some("ESP_RST_DEEPSLEEP".into()),
            ..reading(when)
        };
        h.record_reading(brownout.clone());
        h.record_reading(deepsleep(at - TimeDelta::days(8)));
        h.record_reading(deepsleep(at - TimeDelta::days(1)));
        h.record_reading(deepsleep(at + TimeDelta::hours(1)));
        h.flush();

        let snap = h.snapshot(MAC, at).unwrap();
        assert_eq!(snap.readings, vec![deepsleep(at - TimeDelta::days(1))]);
        assert_eq!(snap.latest, Some(deepsleep(at - TimeDelta::days(1))));
        assert_eq!(snap.last_problem, Some(brownout));
    }

    #[test]
    fn offline_for_eight_days() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let old = reading(now() - TimeDelta::days(8));
        h.record_reading(old.clone());
        h.flush();
        let snap = h.snapshot(MAC, now()).unwrap();
        assert!(snap.readings.is_empty());
        assert_eq!(snap.latest, Some(old));
    }

    #[test]
    fn log_batch() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let at = now() - TimeDelta::minutes(2);
        let body = br#"{"logs":[
            {"message":"one","level":"error","created_at":1795780000},
            {"message":"two","level":"error","created_at":"2026-11-27T11:00:00Z"},
            {"message":"three","level":"fatal","wake_reason":"timer"}
        ]}"#;
        h.record_log_body(MAC, at, body);
        h.flush();

        let rows = parse_log_body(MAC, at, body);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.received_at == at));
        assert_eq!(
            rows[0].created_at,
            DateTime::from_timestamp(1_795_780_000, 0)
        );
        assert_eq!(rows[1].created_at, Some(t("2026-11-27T11:00:00Z")));

        let last = h.snapshot(MAC, now()).unwrap().last_log.unwrap();
        assert_eq!(last.message.as_deref(), Some("three"));
        assert_eq!(last.level.as_deref(), Some("fatal"));
        assert_eq!(last.wake_reason.as_deref(), Some("timer"));
        assert_eq!(last.received_at, at);
        assert_eq!(last.created_at, None);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&last.raw).unwrap(),
            serde_json::json!({"message":"three","level":"fatal","wake_reason":"timer"})
        );
    }

    #[test]
    fn empty_log_bodies_store_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let logs = |h: &History, body: &[u8]| {
            h.record_log_body(MAC, now(), body);
            h.flush();
            count(dir.path(), "SELECT count(*) FROM device_logs")
        };
        assert_eq!(logs(&h, b""), 0);
        assert_eq!(logs(&h, b" \r\n"), 0);
        assert_eq!(logs(&h, br#"{"logs":[]}"#), 0);
        assert_eq!(logs(&h, b"{}"), 1);
        assert_eq!(h.snapshot(MAC, now()).unwrap().last_log.unwrap().raw, "{}");
    }

    #[test]
    fn large_log_body_keeps_its_entries() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let entries: Vec<_> = (0..1_200)
            .map(|n| serde_json::json!({ "message": format!("{n:0>64}"), "level": "info" }))
            .collect();
        let body = serde_json::to_vec(&serde_json::json!({ "logs": entries })).unwrap();
        assert!(body.len() > 100 * 1024, "{} bytes", body.len());
        h.record_log_body(MAC, now(), &body);
        h.flush();
        assert_eq!(count(dir.path(), "SELECT count(*) FROM device_logs"), 1_200);
        let last = h.snapshot(MAC, now()).unwrap().last_log.unwrap();
        assert_eq!(last.message.unwrap(), format!("{:0>64}", 1_199));
    }

    #[test]
    fn oversized_log_body_is_one_too_large_row() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let entry = serde_json::json!({ "message": "x".repeat(LOG_BODY_LIMIT) });
        let body = serde_json::to_vec(&serde_json::json!({ "logs": [entry] })).unwrap();
        h.record_log_body(MAC, now(), &body);
        h.flush();
        assert_eq!(count(dir.path(), "SELECT count(*) FROM device_logs"), 1);
        let last = h.snapshot(MAC, now()).unwrap().last_log.unwrap();
        assert_eq!(last.raw, format!("body too large ({} bytes)", body.len()));
        assert_eq!((last.message, last.level), (None, None));
    }

    #[test]
    fn body_size_is_checked_before_the_whitespace_skip() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let rows = |h: &History, len: usize| {
            h.record_log_body(MAC, now(), &vec![b' '; len]);
            h.flush();
            count(dir.path(), "SELECT count(*) FROM device_logs")
        };
        assert_eq!(rows(&h, LOG_BODY_LIMIT), 0, "at the limit: blank, skipped");
        assert_eq!(rows(&h, LOG_BODY_LIMIT + 1), 1, "over the limit: marked");
        assert_eq!(
            h.snapshot(MAC, now()).unwrap().last_log.unwrap().raw,
            format!("body too large ({} bytes)", LOG_BODY_LIMIT + 1)
        );
    }

    #[test]
    fn queued_log_bytes_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let release = h.stall();
        let body = vec![b'x'; LOG_BODY_LIMIT];
        let fit = QUEUE_BYTES_MAX / LOG_BODY_LIMIT;
        for _ in 0..fit + 1 {
            h.record_log_body(MAC, now(), &body);
        }
        assert_eq!(h.dropped(), 1);
        release.send(()).unwrap();
        h.flush();
        assert_eq!(h.queued_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(
            count(dir.path(), "SELECT count(*) FROM device_logs"),
            fit as i64
        );
        // Room again once the writer has taken the queued bodies.
        h.record_log_body(MAC, now(), &body);
        h.flush();
        assert_eq!(h.dropped(), 1);
    }

    #[test]
    fn non_json_log_body_is_one_raw_row() {
        let rows = parse_log_body(MAC, now(), b"not \xff json");
        assert_eq!(
            rows,
            vec![DeviceLog {
                device: MAC.into(),
                received_at: now(),
                created_at: None,
                level: None,
                message: None,
                wake_reason: None,
                raw: "not \u{fffd} json".into(),
            }]
        );
    }

    fn count(dir: &std::path::Path, sql: &str) -> i64 {
        let conn = rusqlite::Connection::open(dir.join("history.db")).unwrap();
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn log_row(device: &str, received_at: DateTime<Utc>, n: usize) -> DeviceLog {
        DeviceLog {
            device: device.into(),
            received_at,
            created_at: None,
            level: None,
            message: Some(n.to_string()),
            wake_reason: None,
            raw: format!("{{\"n\":{n}}}"),
        }
    }

    #[test]
    fn prune_deletes_only_old_rows() {
        let dir = tempfile::tempdir().unwrap();
        let old = now() - TimeDelta::days(91);
        let kept = now() - TimeDelta::days(89);
        {
            let mut store = Store::open(dir.path(), now()).unwrap();
            store.insert_reading(&reading(old)).unwrap();
            store.insert_reading(&reading(kept)).unwrap();
            store.insert_logs(&[log_row(OTHER, old, 0)]).unwrap();
            store.insert_logs(&[log_row(OTHER, kept, 1)]).unwrap();
            // 2_005 logs for MAC: rows 0..=5 share the oldest second, so the cap
            // (applied on insert here) cuts inside that second and ids break the tie.
            let base = now() - TimeDelta::hours(1);
            let rows: Vec<_> = (0..2_005)
                .map(|n| log_row(MAC, base + TimeDelta::seconds((n as i64 - 5).max(0)), n))
                .collect();
            store.insert_logs(&rows).unwrap();
        }
        // The writer's own start-up prune runs at a time when nothing is old.
        let h = open_at(dir.path(), now() - TimeDelta::days(30));
        h.send(Op::Prune(now())); // queue has room: nothing else is pending
        h.flush();

        let wal = dir.path().join("history.db-wal");
        assert_eq!(fs::metadata(&wal).unwrap().len(), 0, "WAL truncated");
        let d = dir.path();
        assert_eq!(count(d, "SELECT count(*) FROM readings"), 1);
        assert_eq!(count(d, "SELECT min(at) FROM readings"), kept.timestamp());
        assert_eq!(
            count(
                d,
                &format!("SELECT count(*) FROM device_logs WHERE device = '{OTHER}'")
            ),
            1
        );
        assert_eq!(
            count(
                d,
                &format!("SELECT count(*) FROM device_logs WHERE device = '{MAC}'")
            ),
            2_000
        );
        // Rows 0..5 went; row 5, from the same second, is now the oldest.
        assert_eq!(
            count(
                d,
                &format!(
                    "SELECT CAST(message AS INTEGER) FROM device_logs WHERE device = '{MAC}' \
                     ORDER BY received_at, id LIMIT 1"
                )
            ),
            5
        );
    }

    #[test]
    fn writer_prunes_on_start() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = Store::open(dir.path(), now()).unwrap();
            store
                .insert_reading(&reading(now() - TimeDelta::days(91)))
                .unwrap();
        }
        let h = open_at(dir.path(), now());
        h.flush();
        assert_eq!(count(dir.path(), "SELECT count(*) FROM readings"), 0);
    }

    fn corrupt_sets(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("history.db.corrupt-"))
            .collect();
        names.sort();
        names
    }

    fn write_garbage(dir: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            fs::write(dir.join(format!("history.db{suffix}")), vec![0x5a; 4096]).unwrap();
        }
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        write_garbage(dir.path());
        let h = open_at(dir.path(), now());
        h.record_reading(reading(now()));
        h.flush();
        assert_eq!(h.snapshot(MAC, now()).unwrap().latest, Some(reading(now())));
        h.shutdown();
        let secs = now().timestamp();
        assert_eq!(
            corrupt_sets(dir.path()),
            vec![
                format!("history.db.corrupt-{secs}-0"),
                format!("history.db.corrupt-{secs}-0-shm"),
                format!("history.db.corrupt-{secs}-0-wal"),
            ]
        );

        for _ in 0..3 {
            write_garbage(dir.path());
            let h = open_at(dir.path(), now());
            h.shutdown();
        }
        let mut expected = Vec::new();
        for seq in 1..=3 {
            for suffix in ["", "-shm", "-wal"] {
                expected.push(format!("history.db.corrupt-{secs}-{seq}{suffix}"));
            }
        }
        assert_eq!(corrupt_sets(dir.path()), expected);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE future (x); PRAGMA user_version = 2;")
                .unwrap();
        }
        let before = fs::read(&path).unwrap();
        let h = History::open(dir.path(), fixed(now()));
        assert!(!h.is_available());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(corrupt_sets(dir.path()).is_empty());
    }

    #[test]
    fn unusable_path_disables_history() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        let h = History::open(&file, fixed(now()));
        assert!(!h.is_available());
        h.record_reading(reading(now()));
        h.record_log_body(MAC, now(), b"{}");
        h.flush();
        assert!(h.snapshot(MAC, now()).is_err());
        assert!(h.shutdown_within(Duration::from_millis(100)));
    }

    #[test]
    fn full_queue_never_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let release = h.stall();
        let start = Instant::now();
        for _ in 0..300 {
            h.record_reading(reading(now()));
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(h.dropped() >= 44, "dropped {}", h.dropped());
        release.send(()).unwrap();
        h.flush();
        let stored = count(dir.path(), "SELECT count(*) FROM readings");
        assert_eq!(stored as u64 + h.dropped(), 300);
    }

    #[test]
    fn shutdown_drains_queue() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        for n in 0..10 {
            h.record_reading(reading(now() - TimeDelta::minutes(n)));
        }
        h.shutdown();
        h.shutdown(); // a second call is a no-op
        let h = open_at(dir.path(), now());
        assert_eq!(h.snapshot(MAC, now()).unwrap().readings.len(), 10);
    }

    #[test]
    fn closing_the_database_is_inside_the_shutdown_wait() {
        let dir = tempfile::tempdir().unwrap();
        let h = Arc::new(open_at(dir.path(), now()));
        let (release, wait) = mpsc::sync_channel(0);
        h.send(Op::StallClose(wait));
        h.flush();
        let (finished, result) = mpsc::channel();
        let shutting = h.clone();
        std::thread::spawn(move || {
            let _ = finished.send(shutting.shutdown_within(Duration::from_millis(100)));
        });
        let result = result.recv_timeout(Duration::from_secs(2));
        drop(release);
        assert_eq!(
            result,
            Ok(false),
            "a slow close must count against the limit"
        );
    }

    #[test]
    fn shutdown_gives_up_on_a_stalled_writer() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let release = h.stall();
        let start = Instant::now();
        assert!(!h.shutdown_within(Duration::from_millis(100)));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(release);
    }

    fn seed_reading(at: DateTime<Utc>, v: f64) -> Reading {
        Reading {
            battery_v: Some(v),
            ..reading(at)
        }
    }

    #[test]
    fn metrics_seed_reads_last_known_fields() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let t1 = now() - TimeDelta::hours(2);
        let t2 = now() - TimeDelta::hours(1);
        h.record_reading(Reading {
            battery_v: Some(4.0),
            wake_ms: Some(5000),
            image_bytes: Some(100),
            next_refresh: Some(900),
            reset_reason: Some("ESP_RST_BROWNOUT".into()),
            ..reading(t1)
        });
        h.record_reading(Reading {
            battery_v: Some(3.9),
            reset_reason: Some("ESP_RST_DEEPSLEEP".into()),
            ..reading(t2)
        });
        h.flush();
        let seeds = h.metrics_seed(&[MAC.to_owned()], now()).unwrap();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].0, MAC);
        let s = &seeds[0].1;
        assert_eq!(s.latest.as_ref().unwrap().at, t2);
        assert_eq!(s.last_wake.as_ref().unwrap().at, t1);
        assert_eq!(s.last_image.as_ref().unwrap().at, t1);
        assert_eq!(s.last_reply.as_ref().unwrap().at, t1);
        assert_eq!(s.last_reset.as_ref().unwrap().at, t1);
        assert_eq!(s.battery, vec![(t1, 4.0), (t2, 3.9)]);
    }

    #[test]
    fn metrics_seed_respects_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let before = now() - TimeDelta::minutes(5);
        h.record_reading(seed_reading(before, 4.0));
        h.record_reading(seed_reading(now() + TimeDelta::minutes(5), 3.5));
        h.flush();
        let seeds = h.metrics_seed(&[MAC.to_owned()], now()).unwrap();
        let s = &seeds[0].1;
        assert_eq!(s.latest.as_ref().unwrap().at, before);
        assert_eq!(s.battery, vec![(before, 4.0)]);
    }

    #[test]
    fn metrics_seed_excludes_cutoff_second() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let t0 = t("2026-11-27T12:00:00.700Z");
        h.record_reading(seed_reading(t("2026-11-27T12:00:00.200Z"), 3.5));
        h.record_reading(seed_reading(t("2026-11-27T11:59:59Z"), 4.0));
        h.flush();
        let seeds = h.metrics_seed(&[MAC.to_owned()], t0).unwrap();
        let s = &seeds[0].1;
        assert_eq!(s.latest.as_ref().unwrap().at, t("2026-11-27T11:59:59Z"));
        assert_eq!(s.battery, vec![(t("2026-11-27T11:59:59Z"), 4.0)]);
    }

    #[test]
    fn delayed_seed_keeps_live_values() {
        use crate::metrics::DeviceStore;
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let old = now() - TimeDelta::seconds(60);
        h.record_reading(seed_reading(old, 4.0));
        h.flush();
        let t0 = now();
        let live = seed_reading(t0 + TimeDelta::seconds(1), 3.8);
        let ds = DeviceStore::default();
        ds.record(MAC, &live);
        h.record_reading(live.clone());
        h.flush();
        for (mac, seed) in h.metrics_seed(&[MAC.to_owned()], t0).unwrap() {
            ds.seed(&mac, seed);
        }
        let st = ds.get(MAC).unwrap();
        assert_eq!(st.last_seen, Some(live.at));
        assert_eq!(
            st.samples.iter().copied().collect::<Vec<_>>(),
            vec![(old, 4.0), (live.at, 3.8)]
        );
    }

    #[test]
    fn seed_battery_capped_and_deduped() {
        use crate::metrics::devices::MAX_SAMPLES;
        let dir = tempfile::tempdir().unwrap();
        let base = now() - TimeDelta::days(1);
        let mut store = Store::open(dir.path(), now()).unwrap();
        for n in 0..3_000 {
            store
                .insert_reading(&seed_reading(base + TimeDelta::seconds(n), 4.0))
                .unwrap();
        }
        // Duplicates the 10th-newest second (n = 2990), written later.
        store
            .insert_reading(&seed_reading(base + TimeDelta::seconds(2_990), 3.3))
            .unwrap();
        drop(store);
        let h = open_at(dir.path(), now());
        let seeds = h.metrics_seed(&[MAC.to_owned()], now()).unwrap();
        let b = &seeds[0].1.battery;
        assert_eq!(b.len(), MAX_SAMPLES);
        assert!(b.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(b[0].0, base + TimeDelta::seconds(500));
        assert_eq!(b.last().unwrap().0, base + TimeDelta::seconds(2_999));
        let dup = b
            .iter()
            .find(|x| x.0 == base + TimeDelta::seconds(2_990))
            .unwrap();
        assert_eq!(dup.1, 3.3);
        assert_eq!(b.iter().filter(|x| x.1 == 3.3).count(), 1);
    }

    #[test]
    fn metrics_seed_ties_break_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let h = open_at(dir.path(), now());
        let at = now() - TimeDelta::hours(1);
        h.record_reading(seed_reading(at, 4.0));
        h.record_reading(seed_reading(at, 3.7));
        h.flush();
        let seeds = h.metrics_seed(&[MAC.to_owned()], now()).unwrap();
        let s = &seeds[0].1;
        assert_eq!(s.latest.as_ref().unwrap().battery_v, Some(3.7));
        assert_eq!(s.battery, vec![(at, 3.7)]);
    }

    #[test]
    fn metrics_seed_unavailable() {
        let h = History::disabled();
        assert!(h.metrics_seed(&[MAC.to_owned()], now()).is_err());
    }
}
