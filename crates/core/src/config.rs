//! Config loading: one TOML file plus secrets resolved from files or environment.
//! Core parses the globals and devices; each dashboard parses its own section.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::NaiveTime;
use chrono_tz::Tz;
use serde::Deserialize;

use crate::dashboard::{DashboardEntry, ParsedDashboard, validate_names};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{owner}: secret `{key}` not found: no file {file} and no usable environment variable")]
    MissingSecret {
        owner: String,
        key: String,
        file: String,
    },
    #[error("{owner}: secret `{key}` is not configured: set `{key}_secret` or `{key}_env`")]
    UnsetSecret { owner: String, key: String },
    #[error("{owner}: environment variable {var} for `{key}` is not set")]
    UnsetEnv {
        owner: String,
        key: String,
        var: String,
    },
    #[error("{owner}: cannot read secret file {path}: {source}")]
    SecretIo {
        owner: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("device `{device}`: invalid bit_depth {value} (expected 1 or 2)")]
    InvalidBitDepth { device: String, value: u8 },
    #[error("device `{device}`: invalid MAC address `{mac}`")]
    InvalidMac { device: String, mac: String },
    #[error("[{section}]: {message}")]
    Dashboard { section: String, message: String },
    #[error("device `{device}`: unknown or inactive dashboard `{dashboard}`")]
    UnknownDashboard { device: String, dashboard: String },
    #[error("dashboard name `{name}` {reason}")]
    InvalidDashboardName { name: String, reason: &'static str },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub timezone: Tz,
    pub base_url: String,
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub schedule: Schedule,
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub wake: NaiveTime,
    pub sleep: NaiveTime,
    pub refresh_minutes: u32,
}

#[derive(Clone)]
pub struct Device {
    pub name: String,
    /// Uppercase, colon-separated.
    pub mac: String,
    pub api_key: String,
    /// Name of an active dashboard entry.
    pub dashboard: String,
    pub bit_depth: BitDepth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitDepth {
    One,
    Two,
}

/// A dashboard's parsed (not prepared) config, under its entry's name.
pub struct ParsedEntry {
    pub name: &'static str,
    pub parsed: Box<dyn ParsedDashboard>,
}

/// Everything [`load`] produces.
pub struct Loaded {
    pub config: Config,
    /// Active dashboards, in entry order.
    pub dashboards: Vec<ParsedEntry>,
    pub secrets: Secrets,
}

// Debug is written by hand for every type holding a secret, so `{:?}` (and any
// `tracing` field or panic message that uses it) never prints one.
pub const REDACTED: &str = "<redacted>";

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .field("mac", &self.mac)
            .field("api_key", &REDACTED)
            .field("dashboard", &self.dashboard)
            .field("bit_depth", &self.bit_depth)
            .finish()
    }
}

// ---- raw (serde) shapes mirroring the TOML ----

/// Core's part of the file; dashboard sections and unknown keys are ignored here.
#[derive(Deserialize)]
struct RawConfig {
    timezone: Tz,
    base_url: String,
    listen: Option<SocketAddr>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    schedule: RawSchedule,
    #[serde(default, rename = "device")]
    devices: Vec<RawDevice>,
}

#[derive(Deserialize)]
#[serde(default)]
struct RawSchedule {
    wake: NaiveTime,
    sleep: NaiveTime,
    refresh_minutes: u32,
}

impl Default for RawSchedule {
    fn default() -> Self {
        Self {
            wake: NaiveTime::from_hms_opt(6, 0, 0).expect("valid time"),
            sleep: NaiveTime::from_hms_opt(22, 0, 0).expect("valid time"),
            refresh_minutes: 15,
        }
    }
}

#[derive(Deserialize)]
struct RawDevice {
    name: String,
    mac: String,
    api_key_secret: Option<String>,
    api_key_env: Option<String>,
    dashboard: String,
    bit_depth: Option<u8>,
}

/// Resolves secrets from files under one directory or from the environment,
/// counting every lookup.
pub struct Secrets {
    dir: PathBuf,
    reads: AtomicUsize,
}

impl Secrets {
    pub fn new(dir: PathBuf) -> Secrets {
        Secrets {
            dir,
            reads: AtomicUsize::new(0),
        }
    }

    /// Resolve a secret. A secret file wins when it exists; a missing file falls
    /// back to the named environment variable, any other I/O error is reported.
    /// Every resolved value is trimmed. `owner` names the device or kid for error
    /// messages.
    pub fn resolve(
        &self,
        owner: &str,
        key: &str,
        file: Option<&str>,
        env: Option<&str>,
    ) -> Result<String, ConfigError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if let Some(name) = file {
            let path = self.dir.join(name);
            match std::fs::read_to_string(&path) {
                Ok(raw) => return Ok(raw.trim().to_owned()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ConfigError::SecretIo {
                        owner: owner.to_owned(),
                        path,
                        source,
                    });
                }
            }
        }
        if let Some(var) = env {
            match std::env::var(var) {
                Ok(raw) => return Ok(raw.trim().to_owned()),
                Err(_) if file.is_none() => {
                    return Err(ConfigError::UnsetEnv {
                        owner: owner.to_owned(),
                        key: key.to_owned(),
                        var: var.to_owned(),
                    });
                }
                Err(_) => {}
            }
        }
        match file {
            Some(name) => Err(ConfigError::MissingSecret {
                owner: owner.to_owned(),
                key: key.to_owned(),
                file: name.to_owned(),
            }),
            None => Err(ConfigError::UnsetSecret {
                owner: owner.to_owned(),
                key: key.to_owned(),
            }),
        }
    }

    /// How many times [`Secrets::resolve`] has been called.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

fn normalize_mac(device: &str, raw: &str) -> Result<String, ConfigError> {
    let hex: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, ':' | '-'))
        .collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ConfigError::InvalidMac {
            device: device.to_owned(),
            mac: raw.to_owned(),
        });
    }
    let hex = hex.to_ascii_uppercase();
    Ok(hex
        .as_bytes()
        .chunks(2)
        .map(|p| std::str::from_utf8(p).expect("ascii"))
        .collect::<Vec<_>>()
        .join(":"))
}

/// Reads globals, devices and then each entry's section, touching only the config and secret files.
pub fn load(
    path: &Path,
    secrets_dir: &Path,
    entries: &[DashboardEntry],
) -> Result<Loaded, ConfigError> {
    validate_names(entries)?;
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    // Parsed twice: the typed pass keeps toml's line/column in error messages,
    // the table hands each dashboard its own section.
    let raw: RawConfig = toml::from_str(&text)?;
    let table: toml::Table = toml::from_str(&text)?;
    let secrets = Secrets::new(secrets_dir.to_owned());

    let devices = raw
        .devices
        .into_iter()
        .map(|d| {
            let owner = format!("device `{}`", d.name);
            let bit_depth = match d.bit_depth {
                None | Some(2) => BitDepth::Two,
                Some(1) => BitDepth::One,
                Some(value) => {
                    return Err(ConfigError::InvalidBitDepth {
                        device: d.name,
                        value,
                    });
                }
            };
            Ok(Device {
                api_key: secrets.resolve(
                    &owner,
                    "api_key",
                    d.api_key_secret.as_deref(),
                    d.api_key_env.as_deref(),
                )?,
                mac: normalize_mac(&d.name, &d.mac)?,
                bit_depth,
                name: d.name,
                dashboard: d.dashboard,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;

    let mut dashboards = Vec::new();
    for entry in entries {
        if let Some(parsed) = (entry.parse)(table.get(entry.name), &secrets)? {
            dashboards.push(ParsedEntry {
                name: entry.name,
                parsed,
            });
        }
    }
    for device in &devices {
        if !dashboards.iter().any(|d| d.name == device.dashboard) {
            return Err(ConfigError::UnknownDashboard {
                device: device.name.clone(),
                dashboard: device.dashboard.clone(),
            });
        }
    }

    Ok(Loaded {
        config: Config {
            timezone: raw.timezone,
            base_url: raw.base_url,
            listen: raw
                .listen
                .unwrap_or_else(|| "0.0.0.0:8080".parse().expect("valid default")),
            data_dir: raw.data_dir.unwrap_or_else(|| PathBuf::from("/data")),
            schedule: Schedule {
                wake: raw.schedule.wake,
                sleep: raw.schedule.sleep,
                refresh_minutes: raw.schedule.refresh_minutes,
            },
            devices,
        },
        dashboards,
        secrets,
    })
}
