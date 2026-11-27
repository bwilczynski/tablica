//! The SQL side of history: opening (with corruption recovery), the schema,
//! inserts, pruning and the per-device snapshot.

use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, bail};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, Row, TransactionBehavior};
use tracing::{debug, warn};

use super::{
    CORRUPT_KEEP, DeviceHistory, DeviceLog, MAX_LOGS_PER_DEVICE, MAX_READINGS_PER_DEVICE,
    READ_DEADLINE, RETENTION_DAYS, Reading,
};

pub(crate) const DB_FILE: &str = "history.db";
const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 1;
const CORRUPT_PREFIX: &str = "history.db.corrupt-";
const COMPANIONS: [&str; 3] = ["", "-wal", "-shm"];

/// Why opening failed: only a confirmed corruption is worth recovering from.
enum OpenError {
    Corrupt(anyhow::Error),
    Other(anyhow::Error),
}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
                OpenError::Corrupt(e.into())
            }
            _ => OpenError::Other(e.into()),
        }
    }
}

/// The writer's connection.
pub(crate) struct Store {
    conn: Connection,
    /// Test hook: closing waits on this first, standing in for a slow close.
    #[cfg(test)]
    pub(crate) close_gate: Option<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl Drop for Store {
    fn drop(&mut self) {
        if let Some(gate) = self.close_gate.take() {
            let _ = gate.recv();
        }
    }
}

impl Store {
    /// Opens `data_dir/history.db`, moving a corrupt database aside once.
    /// `now` names the corrupt set.
    pub(crate) fn open(data_dir: &Path, now: DateTime<Utc>) -> anyhow::Result<Store> {
        fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
        let path = data_dir.join(DB_FILE);
        let conn = match open_conn(&path) {
            Ok(conn) => conn,
            Err(OpenError::Corrupt(e)) => {
                let set = move_aside(data_dir, now.timestamp())?;
                warn!("history database is corrupt ({e:#}); moved it aside as {set}");
                open_conn(&path).map_err(|e| match e {
                    OpenError::Corrupt(e) | OpenError::Other(e) => e,
                })?
            }
            Err(OpenError::Other(e)) => return Err(e),
        };
        Ok(Store {
            conn,
            #[cfg(test)]
            close_gate: None,
        })
    }

    /// Inserts `r`, then drops that device's readings past the per-device cap,
    /// in one transaction.
    pub(crate) fn insert_reading(&mut self, r: &Reading) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.prepare_cached(
            "INSERT INTO readings (device, at, battery_v, rssi, wifi_band, wake_ms, \
             reset_reason, update_source, image_cached, refresh_rate, next_refresh, \
             fw_version, fw_commit, model, panel_rev, render_ms, image_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )?
        .execute(rusqlite::params![
            r.device,
            r.at.timestamp(),
            r.battery_v,
            r.rssi,
            r.wifi_band,
            r.wake_ms,
            r.reset_reason,
            r.update_source,
            r.image_cached,
            r.refresh_rate,
            r.next_refresh,
            r.fw_version,
            r.fw_commit,
            r.model,
            r.panel_rev,
            r.render_ms,
            r.image_bytes,
        ])?;
        // Walks the (device, at) index from the newest row; normally finds nothing.
        tx.prepare_cached(
            "DELETE FROM readings WHERE id IN (\
               SELECT id FROM readings WHERE device = ?1 \
               ORDER BY at DESC, id DESC LIMIT -1 OFFSET ?2)",
        )?
        .execute(rusqlite::params![r.device, MAX_READINGS_PER_DEVICE])?;
        tx.commit()
    }

    /// All rows or none. Only the newest `MAX_LOGS_PER_DEVICE` entries of a batch
    /// are kept, and each device in it is cut back to that cap, by
    /// `(received_at, id)`, in the same transaction.
    pub(crate) fn insert_logs(&mut self, logs: &[DeviceLog]) -> rusqlite::Result<()> {
        let cap = usize::try_from(MAX_LOGS_PER_DEVICE).unwrap_or(usize::MAX);
        let logs = &logs[logs.len().saturating_sub(cap)..];
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO device_logs \
                 (device, received_at, created_at, level, message, wake_reason, raw) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for l in logs {
                stmt.execute(rusqlite::params![
                    l.device,
                    l.received_at.timestamp(),
                    l.created_at.map(|t| t.timestamp()),
                    l.level,
                    l.message,
                    l.wake_reason,
                    l.raw,
                ])?;
            }
            let mut devices: Vec<&str> = logs.iter().map(|l| l.device.as_str()).collect();
            devices.sort_unstable();
            devices.dedup();
            let mut trim = tx.prepare_cached(
                "DELETE FROM device_logs WHERE id IN (\
                   SELECT id FROM device_logs WHERE device = ?1 \
                   ORDER BY received_at DESC, id DESC LIMIT -1 OFFSET ?2)",
            )?;
            for device in devices {
                trim.execute(rusqlite::params![device, MAX_LOGS_PER_DEVICE])?;
            }
        }
        tx.commit()
    }

    /// Deletes rows past retention and rows past the per-device caps, commits,
    /// then truncates the WAL. A failed checkpoint never undoes the prune.
    pub(crate) fn prune(&mut self, now: DateTime<Utc>) -> rusqlite::Result<()> {
        let cutoff = (now - TimeDelta::days(RETENTION_DAYS)).timestamp();
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM readings WHERE at < ?1", [cutoff])?;
        tx.execute("DELETE FROM device_logs WHERE received_at < ?1", [cutoff])?;
        tx.execute(
            "DELETE FROM device_logs WHERE id IN (\
               SELECT id FROM (\
                 SELECT id, ROW_NUMBER() OVER (\
                   PARTITION BY device ORDER BY received_at DESC, id DESC) AS n \
                 FROM device_logs) \
               WHERE n > ?1)",
            [MAX_LOGS_PER_DEVICE],
        )?;
        tx.execute(
            "DELETE FROM readings WHERE id IN (\
               SELECT id FROM (\
                 SELECT id, ROW_NUMBER() OVER (\
                   PARTITION BY device ORDER BY at DESC, id DESC) AS n \
                 FROM readings) \
               WHERE n > ?1)",
            [MAX_READINGS_PER_DEVICE],
        )?;
        tx.commit()?;
        let checkpoint = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            });
        match checkpoint {
            Ok(0) => {}
            Ok(_) => debug!("history WAL checkpoint was busy"),
            Err(e) => debug!("history WAL checkpoint failed: {e}"),
        }
        Ok(())
    }
}

fn open_conn(path: &Path) -> Result<Connection, OpenError> {
    // Check an existing file on a read-only connection first: closing a
    // read-write one would recover (and delete) a `-wal`/`-shm` we may need to
    // move aside, and a database we refuse must stay byte-for-byte as found.
    let probed = path.exists().then(|| {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        check(&Connection::open_with_flags(path, flags)?)
    });
    let probed = match probed {
        Some(Ok(version)) => Some(version),
        Some(Err(OpenError::Corrupt(e))) => return Err(OpenError::Corrupt(e)),
        // A read-only open can fail where a read-write one does not (a WAL
        // without its `-shm`), so check again on the real connection.
        Some(Err(OpenError::Other(_))) | None => None,
    };
    let conn = Connection::open(path)?;
    let version = match probed {
        Some(version) => version,
        None => check(&conn)?,
    };
    if version > SCHEMA_VERSION {
        return Err(OpenError::Other(anyhow::anyhow!(
            "history database has schema version {version}; this build knows {SCHEMA_VERSION}"
        )));
    }
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(OpenError::Other(anyhow::anyhow!(
            "history database refused WAL mode ({mode})"
        )));
    }
    conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
    conn.execute_batch(SCHEMA)?;
    if version == 0 {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(conn)
}

/// `PRAGMA quick_check`, then the schema version.
fn check(conn: &Connection) -> Result<i64, OpenError> {
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(OpenError::Corrupt(anyhow::anyhow!("quick_check: {check}")));
    }
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Renames `history.db{,-wal,-shm}` to `history.db.corrupt-<secs>-<seq>{,-wal,-shm}`
/// and keeps only the newest `CORRUPT_KEEP` sets. Returns the new set's name.
fn move_aside(dir: &Path, secs: i64) -> anyhow::Result<String> {
    let mut sets = corrupt_sets(dir)?;
    let seq = sets
        .iter()
        .filter(|(s, _)| *s == secs)
        .map(|(_, q)| q + 1)
        .max()
        .unwrap_or(0);
    let set = format!("{CORRUPT_PREFIX}{secs}-{seq}");
    for suffix in COMPANIONS {
        let from = dir.join(format!("{DB_FILE}{suffix}"));
        if from.exists() {
            let to = dir.join(format!("{set}{suffix}"));
            fs::rename(&from, &to).with_context(|| format!("moving aside {}", from.display()))?;
        }
    }
    sets.push((secs, seq));
    sets.sort_unstable();
    sets.dedup();
    let old = sets.len().saturating_sub(CORRUPT_KEEP);
    for (s, q) in &sets[..old] {
        for suffix in COMPANIONS {
            let file = dir.join(format!("{CORRUPT_PREFIX}{s}-{q}{suffix}"));
            if let Err(e) = fs::remove_file(&file)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                warn!("cannot delete old corrupt history {}: {e}", file.display());
            }
        }
    }
    Ok(set)
}

/// `(secs, seq)` of every corrupt set in `dir`, unordered, possibly repeated.
fn corrupt_sets(dir: &Path) -> anyhow::Result<Vec<(i64, u64)>> {
    let mut sets = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let name = entry?.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(CORRUPT_PREFIX)) else {
            continue;
        };
        let rest = rest
            .strip_suffix("-wal")
            .or_else(|| rest.strip_suffix("-shm"))
            .unwrap_or(rest);
        if let Some((s, q)) = rest.split_once('-')
            && let (Ok(s), Ok(q)) = (s.parse(), q.parse())
        {
            sets.push((s, q));
        }
    }
    Ok(sets)
}

const READING_COLUMNS: &str = "device, at, battery_v, rssi, wifi_band, wake_ms, reset_reason, \
     update_source, image_cached, refresh_rate, next_refresh, fw_version, fw_commit, model, \
     panel_rev, render_ms, image_bytes";

fn timestamp(secs: i64) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::from_timestamp(secs, 0)
        .ok_or_else(|| rusqlite::Error::IntegralValueOutOfRange(0, secs))
}

fn reading_row(r: &Row) -> rusqlite::Result<Reading> {
    Ok(Reading {
        device: r.get(0)?,
        at: timestamp(r.get(1)?)?,
        battery_v: r.get(2)?,
        rssi: r.get(3)?,
        wifi_band: r.get(4)?,
        wake_ms: r.get(5)?,
        reset_reason: r.get(6)?,
        update_source: r.get(7)?,
        image_cached: r.get(8)?,
        refresh_rate: r.get(9)?,
        next_refresh: r.get(10)?,
        fw_version: r.get(11)?,
        fw_commit: r.get(12)?,
        model: r.get(13)?,
        panel_rev: r.get(14)?,
        render_ms: r.get(15)?,
        image_bytes: r.get(16)?,
    })
}

fn log_row(r: &Row) -> rusqlite::Result<DeviceLog> {
    Ok(DeviceLog {
        device: r.get(0)?,
        received_at: timestamp(r.get(1)?)?,
        created_at: r.get::<_, Option<i64>>(2)?.map(timestamp).transpose()?,
        level: r.get(3)?,
        message: r.get(4)?,
        wake_reason: r.get(5)?,
        raw: r.get(6)?,
    })
}

/// Reads one device's history as of `at` on a fresh read-only connection,
/// all four queries in one read transaction, giving up after `READ_DEADLINE`.
pub(crate) fn snapshot(
    data_dir: &Path,
    device: &str,
    at: DateTime<Utc>,
) -> anyhow::Result<DeviceHistory> {
    let path = data_dir.join(DB_FILE);
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let mut conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(READ_DEADLINE)?;
    let deadline = Instant::now() + READ_DEADLINE;
    conn.progress_handler(1_000, Some(move || Instant::now() >= deadline))?;

    let at_secs = at.timestamp();
    let since = (at - TimeDelta::days(7)).timestamp();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let latest = tx
        .query_row(
            &format!(
                "SELECT {READING_COLUMNS} FROM readings WHERE device = ?1 AND at <= ?2 \
                 ORDER BY at DESC, id DESC LIMIT 1"
            ),
            rusqlite::params![device, at_secs],
            reading_row,
        )
        .optional()?;
    let readings = tx
        .prepare(&format!(
            "SELECT {READING_COLUMNS} FROM readings WHERE device = ?1 AND at > ?2 AND at <= ?3 \
             ORDER BY at, id"
        ))?
        .query_map(rusqlite::params![device, since, at_secs], reading_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let last_problem = tx
        .query_row(
            &format!(
                "SELECT {READING_COLUMNS} FROM readings WHERE device = ?1 AND at <= ?2 \
                 AND reset_reason IS NOT NULL AND reset_reason <> 'ESP_RST_DEEPSLEEP' \
                 ORDER BY at DESC, id DESC LIMIT 1"
            ),
            rusqlite::params![device, at_secs],
            reading_row,
        )
        .optional()?;
    let last_log = tx
        .query_row(
            "SELECT device, received_at, created_at, level, message, wake_reason, raw \
             FROM device_logs WHERE device = ?1 AND received_at <= ?2 \
             ORDER BY received_at DESC, id DESC LIMIT 1",
            rusqlite::params![device, at_secs],
            log_row,
        )
        .optional()?;
    tx.finish()?;
    drop(conn);
    Ok(DeviceHistory {
        latest,
        readings,
        last_problem,
        last_log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_batch_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut store = Store::open(dir.path(), now).unwrap();
        let row = |device: &str| DeviceLog {
            device: device.into(),
            received_at: now,
            created_at: None,
            level: None,
            message: None,
            wake_reason: None,
            raw: "{}".into(),
        };
        let rows = [row("AA:BB:CC:DD:EE:FF"), row(""), row("AA:BB:CC:DD:EE:FF")];
        assert!(store.insert_logs(&rows).is_err());
        let n: i64 = store
            .conn
            .query_row("SELECT count(*) FROM device_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    const MAC: &str = "AA:BB:CC:DD:EE:FF";
    const OTHER: &str = "11:22:33:44:55:66";

    fn log(device: &str, received_at: DateTime<Utc>, n: i64) -> DeviceLog {
        DeviceLog {
            device: device.into(),
            received_at,
            created_at: None,
            level: None,
            message: Some(n.to_string()),
            wake_reason: None,
            raw: "{}".into(),
        }
    }

    fn reading(device: &str, at: DateTime<Utc>) -> Reading {
        Reading {
            device: device.into(),
            at,
            ..Reading::default()
        }
    }

    fn count(store: &Store, sql: &str, device: &str) -> i64 {
        store.conn.query_row(sql, [device], |r| r.get(0)).unwrap()
    }

    fn logs(store: &Store, device: &str) -> i64 {
        count(
            store,
            "SELECT count(*) FROM device_logs WHERE device = ?1",
            device,
        )
    }

    fn oldest_log(store: &Store, device: &str) -> i64 {
        count(
            store,
            "SELECT CAST(message AS INTEGER) FROM device_logs WHERE device = ?1 \
             ORDER BY received_at, id LIMIT 1",
            device,
        )
    }

    fn readings(store: &Store, device: &str) -> i64 {
        count(
            store,
            "SELECT count(*) FROM readings WHERE device = ?1",
            device,
        )
    }

    /// Inserts `n` readings for `device`, one a second from `from`, bypassing the cap.
    fn seed_readings(store: &Store, device: &str, from: DateTime<Utc>, n: i64) {
        store
            .conn
            .execute(
                "WITH RECURSIVE k(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM k WHERE i + 1 < ?3) \
                 INSERT INTO readings (device, at) SELECT ?1, ?2 + i FROM k",
                rusqlite::params![device, from.timestamp(), n],
            )
            .unwrap();
    }

    #[test]
    fn log_cap_holds_on_insert() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut store = Store::open(dir.path(), now).unwrap();
        let cap = MAX_LOGS_PER_DEVICE;

        // One batch over the cap keeps only its newest (last) entries.
        let batch: Vec<_> = (0..cap + 5).map(|n| log(MAC, now, n)).collect();
        store.insert_logs(&batch).unwrap();
        assert_eq!(logs(&store, MAC), cap);
        assert_eq!(oldest_log(&store, MAC), 5);

        // A later batch pushes the oldest rows out, by (received_at, id).
        let later = now + TimeDelta::seconds(1);
        let batch: Vec<_> = (0..3).map(|n| log(MAC, later, 10_000 + n)).collect();
        store.insert_logs(&batch).unwrap();
        assert_eq!(logs(&store, MAC), cap);
        assert_eq!(oldest_log(&store, MAC), 8);

        // Another device's rows are untouched.
        store.insert_logs(&[log(OTHER, now, 0)]).unwrap();
        assert_eq!(logs(&store, OTHER), 1);
        assert_eq!(logs(&store, MAC), cap);
    }

    #[test]
    fn reading_cap_holds_on_insert() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut store = Store::open(dir.path(), now).unwrap();
        let cap = MAX_READINGS_PER_DEVICE;
        let start = now - TimeDelta::days(30);
        seed_readings(&store, MAC, start, cap);
        store.insert_reading(&reading(OTHER, start)).unwrap();

        store.insert_reading(&reading(MAC, now)).unwrap();
        assert_eq!(readings(&store, MAC), cap);
        let oldest = count(
            &store,
            "SELECT min(at) FROM readings WHERE device = ?1",
            MAC,
        );
        assert_eq!(oldest, start.timestamp() + 1);
        assert_eq!(readings(&store, OTHER), 1);
    }

    #[test]
    fn prune_caps_logs() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut store = Store::open(dir.path(), now).unwrap();
        let cap = MAX_LOGS_PER_DEVICE;
        // Bypassing the insert cap: rows 0..=5 share the oldest second, so the
        // cap cuts inside it and ids break the tie.
        let base = (now - TimeDelta::hours(1)).timestamp();
        store
            .conn
            .execute(
                "WITH RECURSIVE k(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM k WHERE i + 1 < ?3) \
                 INSERT INTO device_logs (device, received_at, message, raw) \
                 SELECT ?1, ?2 + max(i - 5, 0), i, '{}' FROM k",
                rusqlite::params![MAC, base, cap + 5],
            )
            .unwrap();
        store.insert_logs(&[log(OTHER, now, 0)]).unwrap();
        store.prune(now).unwrap();
        assert_eq!(logs(&store, MAC), cap);
        assert_eq!(oldest_log(&store, MAC), 5);
        assert_eq!(logs(&store, OTHER), 1);
    }

    #[test]
    fn prune_caps_readings() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut store = Store::open(dir.path(), now).unwrap();
        let cap = MAX_READINGS_PER_DEVICE;
        let start = now - TimeDelta::days(30);
        seed_readings(&store, MAC, start, cap + 7);
        seed_readings(&store, OTHER, start, 3);
        store.prune(now).unwrap();
        assert_eq!(readings(&store, MAC), cap);
        let oldest = count(
            &store,
            "SELECT min(at) FROM readings WHERE device = ?1",
            MAC,
        );
        assert_eq!(oldest, start.timestamp() + 7);
        assert_eq!(readings(&store, OTHER), 3);
    }
}
