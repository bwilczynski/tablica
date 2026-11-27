-- History schema, version 1 (PRAGMA user_version). Idempotent.

CREATE TABLE IF NOT EXISTS readings (
  id            INTEGER PRIMARY KEY,
  device        TEXT    NOT NULL CHECK (device <> ''),  -- canonical MAC
  at            INTEGER NOT NULL,   -- unix seconds, server clock
  battery_v     REAL,               -- NULL if header missing/unparseable; 0.0 kept as sent
  rssi          INTEGER,
  wifi_band     TEXT,
  wake_ms       INTEGER,
  reset_reason  TEXT,
  update_source TEXT,
  image_cached  INTEGER,            -- 0/1/NULL
  refresh_rate  INTEGER,            -- header: seconds the device last slept
  next_refresh  INTEGER,            -- seconds tablica answered with
  fw_version    TEXT,
  fw_commit     TEXT,
  model         TEXT,
  panel_rev     TEXT,
  render_ms     INTEGER,            -- NULL when the render failed
  image_bytes   INTEGER
);
CREATE INDEX IF NOT EXISTS readings_device_at ON readings(device, at);

CREATE TABLE IF NOT EXISTS device_logs (
  id          INTEGER PRIMARY KEY,
  device      TEXT    NOT NULL CHECK (device <> ''),    -- canonical MAC
  received_at INTEGER NOT NULL,     -- unix seconds, server clock
  created_at  INTEGER,              -- device's own timestamp, if parseable
  level       TEXT,
  message     TEXT,                 -- truncated to 1 KiB
  wake_reason TEXT,
  raw         TEXT    NOT NULL      -- the entry as JSON, truncated to 4 KiB
);
CREATE INDEX IF NOT EXISTS device_logs_device_at ON device_logs(device, received_at);
