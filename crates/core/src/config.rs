//! Config loading: one TOML file plus secrets resolved from files or environment.
//! Core parses the globals and devices; each dashboard parses its own section.

use std::collections::{HashMap, HashSet};
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
    #[error("device `{device}` is configured more than once")]
    DuplicateDevice { device: String },
    #[error("devices `{first}` and `{second}` have the same MAC address {mac}")]
    DuplicateMac {
        mac: String,
        first: String,
        second: String,
    },
    #[error("dashboard name `{name}` {reason}")]
    InvalidDashboardName { name: String, reason: &'static str },
    #[error("`metrics_listen` {addr} must differ from `listen`")]
    MetricsListenClash { addr: SocketAddr },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub timezone: Tz,
    pub base_url: String,
    pub listen: SocketAddr,
    /// Where the Prometheus `/metrics` listener binds; `None` disables it.
    pub metrics_listen: Option<SocketAddr>,
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
    pub diagnostics_button: bool,
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
            .field("diagnostics_button", &self.diagnostics_button)
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
    metrics_listen: Option<SocketAddr>,
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
    #[serde(default)]
    diagnostics_button: bool,
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
                diagnostics_button: d.diagnostics_button,
                name: d.name,
                dashboard: d.dashboard,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;

    let mut seen = HashSet::new();
    for device in &devices {
        if !seen.insert(device.name.as_str()) {
            return Err(ConfigError::DuplicateDevice {
                device: device.name.clone(),
            });
        }
    }
    // History is keyed by MAC, and lookups by MAC take the first match.
    let mut macs = HashMap::new();
    for device in &devices {
        if let Some(first) = macs.insert(device.mac.as_str(), device.name.as_str()) {
            return Err(ConfigError::DuplicateMac {
                mac: device.mac.clone(),
                first: first.to_owned(),
                second: device.name.clone(),
            });
        }
    }

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

    let listen = raw
        .listen
        .unwrap_or_else(|| "0.0.0.0:8080".parse().expect("valid default"));
    if raw.metrics_listen == Some(listen) {
        return Err(ConfigError::MetricsListenClash { addr: listen });
    }

    Ok(Loaded {
        config: Config {
            timezone: raw.timezone,
            base_url: raw.base_url,
            listen,
            metrics_listen: raw.metrics_listen,
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use super::*;
    use crate::dashboard::{Dashboard, ParseResult, PrepareContext};

    const UNSET_ENV: &str = "TABLICA_CORE_TEST_SURELY_UNSET";

    struct Stub;

    impl ParsedDashboard for Stub {
        fn prepare(self: Box<Self>, _: &PrepareContext) -> anyhow::Result<Arc<dyn Dashboard>> {
            unreachable!("config tests never prepare")
        }
    }

    /// Active whenever its section is present.
    fn parse_alpha(section: Option<&toml::Value>, _: &Secrets) -> ParseResult {
        Ok(section.map(|_| Box::new(Stub) as Box<dyn ParsedDashboard>))
    }

    /// Never active.
    fn parse_bravo(_: Option<&toml::Value>, _: &Secrets) -> ParseResult {
        Ok(None)
    }

    fn entries() -> Vec<DashboardEntry> {
        vec![
            DashboardEntry {
                name: "alpha",
                parse: parse_alpha,
            },
            DashboardEntry {
                name: "bravo",
                parse: parse_bravo,
            },
        ]
    }

    const MINIMAL: &str = "timezone = \"Europe/Warsaw\"\nbase_url = \"http://x\"\n[alpha]\n";

    /// `MINIMAL` with extra top-level keys placed before its first table.
    fn top_level(keys: &str) -> String {
        format!("{keys}\n{MINIMAL}")
    }

    fn device(extra: &str) -> String {
        format!(
            "{MINIMAL}\n[[device]]\nname = \"shed\"\nmac = \"aa-bb-cc-dd-ee-01\"\n\
             api_key_secret = \"shed_key\"\ndashboard = \"alpha\"\n{extra}"
        )
    }

    /// A secrets dir holding `shed_key`, and the result of loading `toml` against it.
    fn load_str(toml: &str) -> (Result<Loaded, ConfigError>, tempfile::TempDir) {
        load_with(toml, &entries())
    }

    fn load_with(
        toml: &str,
        entries: &[DashboardEntry],
    ) -> (Result<Loaded, ConfigError>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("shed_key"), " shed-secret \n").unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, toml).unwrap();
        (load(&path, dir.path(), entries), dir)
    }

    fn loaded(toml: &str) -> Loaded {
        load_str(toml).0.unwrap_or_else(|e| panic!("{e}"))
    }

    fn err(toml: &str) -> ConfigError {
        load_str(toml).0.err().expect("load should fail")
    }

    #[test]
    fn invalid_entry_names_are_rejected_through_load() {
        fn entry(name: &'static str) -> DashboardEntry {
            DashboardEntry {
                name,
                parse: parse_bravo,
            }
        }
        for bad in [
            vec![entry("abcd")],
            vec![entry("Alpha")],
            vec![entry("device")],
            vec![entry("alpha"), entry("alpha")],
        ] {
            let (result, _dir) = load_with(MINIMAL, &bad);
            assert!(
                matches!(result, Err(ConfigError::InvalidDashboardName { .. })),
                "{:?}",
                bad.iter().map(|e| e.name).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn duplicate_device_names_are_rejected() {
        let toml = device("").replace("name = \"shed\"", "name = \"kitchen\"");
        let second = "\n[[device]]\nname = \"kitchen\"\nmac = \"aa-bb-cc-dd-ee-02\"\n\
                      api_key_secret = \"shed_key\"\ndashboard = \"alpha\"\n";
        let e = err(&format!("{toml}{second}"));
        assert!(
            matches!(&e, ConfigError::DuplicateDevice { device } if device == "kitchen"),
            "{e}"
        );
    }

    #[test]
    fn duplicate_macs_are_rejected() {
        // Spelled differently, the same MAC once normalized.
        let second = "\n[[device]]\nname = \"porch\"\nmac = \"AA:BB:CC:DD:EE:01\"\n\
                      api_key_secret = \"shed_key\"\ndashboard = \"alpha\"\n";
        let e = err(&device(second));
        assert!(
            matches!(&e, ConfigError::DuplicateMac { mac, first, second }
                if mac == "AA:BB:CC:DD:EE:01" && first == "shed" && second == "porch"),
            "{e}"
        );
        assert_eq!(
            e.to_string(),
            "devices `shed` and `porch` have the same MAC address AA:BB:CC:DD:EE:01"
        );
    }

    #[test]
    fn device_on_unregistered_dashboard_is_rejected() {
        let toml = device("").replace("\"alpha\"", "\"gamma\"");
        let e = err(&toml);
        assert!(
            matches!(&e, ConfigError::UnknownDashboard { device, dashboard }
                if device == "shed" && dashboard == "gamma"),
            "{e}"
        );
    }

    #[test]
    fn device_on_inactive_dashboard_is_rejected() {
        let e = err(&device("").replace("\"alpha\"", "\"bravo\""));
        assert!(
            matches!(&e, ConfigError::UnknownDashboard { dashboard, .. } if dashboard == "bravo"),
            "{e}"
        );
    }

    #[test]
    fn only_active_dashboards_are_loaded() {
        let l = loaded(MINIMAL);
        let names: Vec<_> = l.dashboards.iter().map(|d| d.name).collect();
        assert_eq!(names, ["alpha"]);
    }

    #[test]
    fn secret_file_is_trimmed_and_wins_over_env() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("k"), "  from-file\n").unwrap();
        let s = Secrets::new(dir.path().to_owned());
        // PATH is always set; the file must still win.
        let got = s.resolve("owner", "key", Some("k"), Some("PATH")).unwrap();
        assert_eq!(got, "from-file");
    }

    #[test]
    fn missing_file_falls_back_to_trimmed_env() {
        let s = Secrets::new(PathBuf::from("/nonexistent"));
        let got = s
            .resolve("owner", "key", Some("absent"), Some("PATH"))
            .unwrap();
        assert_eq!(got, std::env::var("PATH").unwrap().trim());
    }

    #[test]
    fn unreadable_file_does_not_fall_back_to_env() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("k")).unwrap();
        let s = Secrets::new(dir.path().to_owned());
        let e = s
            .resolve("device `shed`", "api_key", Some("k"), Some("PATH"))
            .unwrap_err();
        assert!(matches!(e, ConfigError::SecretIo { .. }), "{e}");
        assert!(e.to_string().contains("device `shed`"), "{e}");
    }

    #[test]
    fn secret_errors_name_their_owner() {
        let s = Secrets::new(PathBuf::from("/nonexistent"));
        let cases = [
            (Some("absent"), None),
            (Some("absent"), Some(UNSET_ENV)),
            (None, Some(UNSET_ENV)),
            (None, None),
        ];
        for (file, env) in cases {
            let e = s.resolve("kid `Ann`", "login", file, env).unwrap_err();
            assert!(e.to_string().starts_with("kid `Ann`: "), "{e}");
        }
        assert!(matches!(
            s.resolve("o", "k", Some("absent"), Some(UNSET_ENV)),
            Err(ConfigError::MissingSecret { .. })
        ));
        assert!(matches!(
            s.resolve("o", "k", None, Some(UNSET_ENV)),
            Err(ConfigError::UnsetEnv { .. })
        ));
        assert!(matches!(
            s.resolve("o", "k", None, None),
            Err(ConfigError::UnsetSecret { .. })
        ));
    }

    #[test]
    fn secrets_count_every_lookup() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("k"), "v").unwrap();
        let s = Secrets::new(dir.path().to_owned());
        assert_eq!(s.reads(), 0);
        s.resolve("o", "k", Some("k"), None).unwrap();
        let _ = s.resolve("o", "k", Some("absent"), None);
        assert_eq!(s.reads(), 2);
    }

    #[test]
    fn loader_reads_the_device_key_once_and_trims_it() {
        let l = loaded(&device(""));
        assert_eq!(l.config.devices[0].api_key, "shed-secret");
        assert_eq!(l.secrets.reads(), 1);
    }

    #[test]
    fn mac_is_normalized_or_rejected() {
        assert_eq!(
            loaded(&device("")).config.devices[0].mac,
            "AA:BB:CC:DD:EE:01"
        );
        let e = err(&device("").replace("aa-bb-cc-dd-ee-01", "not-a-mac"));
        assert!(
            matches!(&e, ConfigError::InvalidMac { device, .. } if device == "shed"),
            "{e}"
        );
    }

    #[test]
    fn metrics_listen_absent_is_none() {
        assert_eq!(loaded(MINIMAL).config.metrics_listen, None);
    }

    #[test]
    fn metrics_listen_parsed() {
        let c = loaded(&top_level("metrics_listen = \"0.0.0.0:9101\"")).config;
        assert_eq!(c.metrics_listen, Some("0.0.0.0:9101".parse().unwrap()));
    }

    #[test]
    fn metrics_listen_equal_to_listen_rejected() {
        let e = err(&top_level(
            "listen = \"0.0.0.0:8080\"\nmetrics_listen = \"0.0.0.0:8080\"",
        ));
        assert!(matches!(e, ConfigError::MetricsListenClash { .. }), "{e}");
    }

    #[test]
    fn defaults_apply() {
        let c = loaded(MINIMAL).config;
        assert_eq!(c.listen, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.data_dir, Path::new("/data"));
        assert_eq!(c.schedule.wake, NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        assert_eq!(c.schedule.sleep, NaiveTime::from_hms_opt(22, 0, 0).unwrap());
        assert_eq!(c.schedule.refresh_minutes, 15);
        assert!(c.devices.is_empty());
        assert_eq!(
            loaded(&device("")).config.devices[0].bit_depth,
            BitDepth::Two
        );
    }

    #[test]
    fn bit_depth_one_two_and_invalid() {
        let depth =
            |v: u8| loaded(&device(&format!("bit_depth = {v}"))).config.devices[0].bit_depth;
        assert_eq!(depth(1), BitDepth::One);
        assert_eq!(depth(2), BitDepth::Two);
        let e = err(&device("bit_depth = 3"));
        assert!(
            matches!(&e, ConfigError::InvalidBitDepth { value: 3, device } if device == "shed"),
            "{e}"
        );
    }

    #[test]
    fn device_debug_redacts_the_api_key() {
        let l = loaded(&device(""));
        let dev = &l.config.devices[0];
        for dump in [
            format!("{dev:?}"),
            format!("{dev:#?}"),
            format!("{:?}", l.config),
        ] {
            assert!(!dump.contains("shed-secret"), "{dump}");
            assert!(dump.contains(REDACTED), "{dump}");
            assert!(dump.contains("shed"), "{dump}");
        }
    }
}
