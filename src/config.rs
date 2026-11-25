//! Config loading: one TOML file plus secrets resolved from files or environment.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use chrono::NaiveTime;
use chrono_tz::Tz;
use serde::Deserialize;

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
}

#[derive(Debug, Clone)]
pub struct Config {
    pub timezone: Tz,
    pub base_url: String,
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub schedule: Schedule,
    pub devices: Vec<Device>,
    pub family: FamilyConfig,
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
    pub dashboard: DashboardKind,
    pub bit_depth: BitDepth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DashboardKind {
    Family,
    Homelab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitDepth {
    One,
    Two,
}

#[derive(Clone)]
pub struct FamilyConfig {
    pub switch_at: NaiveTime,
    pub calendar_days: u32,
    pub ics_url: String,
    pub kids: Vec<KidConfig>,
    pub weather: WeatherConfig,
}

#[derive(Clone)]
pub struct KidConfig {
    pub label: String,
    pub librus_login: String,
    pub librus_password: String,
}

// Debug is written by hand for every type holding a secret, so `{:?}` (and any
// `tracing` field or panic message that uses it) never prints one.
const REDACTED: &str = "<redacted>";

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

impl std::fmt::Debug for FamilyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FamilyConfig")
            .field("switch_at", &self.switch_at)
            .field("calendar_days", &self.calendar_days)
            .field("ics_url", &REDACTED)
            .field("kids", &self.kids)
            .field("weather", &self.weather)
            .finish()
    }
}

impl std::fmt::Debug for KidConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KidConfig")
            .field("label", &self.label)
            .field("librus_login", &REDACTED)
            .field("librus_password", &REDACTED)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct WeatherConfig {
    pub latitude: f64,
    pub longitude: f64,
}

// ---- raw (serde) shapes mirroring the TOML ----

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
    family: RawFamily,
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
    dashboard: DashboardKind,
    bit_depth: Option<u8>,
}

#[derive(Deserialize)]
struct RawFamily {
    switch_at: NaiveTime,
    calendar_days: u32,
    calendar: RawCalendar,
    #[serde(default, rename = "kid")]
    kids: Vec<RawKid>,
    weather: WeatherConfig,
}

#[derive(Deserialize)]
struct RawCalendar {
    ics_url_secret: Option<String>,
    ics_url_env: Option<String>,
}

#[derive(Deserialize)]
struct RawKid {
    label: String,
    librus_login_secret: Option<String>,
    librus_login_env: Option<String>,
    librus_password_secret: Option<String>,
    librus_password_env: Option<String>,
}

/// Resolve a secret. A secret file wins when it exists; a missing file falls back to
/// the named environment variable, any other I/O error is reported. Every resolved
/// value is trimmed. `owner` names the device or kid for error messages.
fn resolve_secret(
    owner: &str,
    key: &str,
    file: Option<&str>,
    env: Option<&str>,
    secrets_dir: &Path,
) -> Result<String, ConfigError> {
    if let Some(name) = file {
        let path = secrets_dir.join(name);
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

pub fn load(path: &Path, secrets_dir: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let raw: RawConfig = toml::from_str(&text)?;

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
                api_key: resolve_secret(
                    &owner,
                    "api_key",
                    d.api_key_secret.as_deref(),
                    d.api_key_env.as_deref(),
                    secrets_dir,
                )?,
                mac: normalize_mac(&d.name, &d.mac)?,
                bit_depth,
                name: d.name,
                dashboard: d.dashboard,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;

    let kids = raw
        .family
        .kids
        .into_iter()
        .enumerate()
        .map(|(i, k)| {
            let owner = format!("kid {} (`{}`)", i + 1, k.label);
            Ok(KidConfig {
                librus_login: resolve_secret(
                    &owner,
                    "librus_login",
                    k.librus_login_secret.as_deref(),
                    k.librus_login_env.as_deref(),
                    secrets_dir,
                )?,
                librus_password: resolve_secret(
                    &owner,
                    "librus_password",
                    k.librus_password_secret.as_deref(),
                    k.librus_password_env.as_deref(),
                    secrets_dir,
                )?,
                label: k.label,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;

    let cal = raw.family.calendar;
    let ics_url = resolve_secret(
        "family.calendar",
        "ics_url",
        cal.ics_url_secret.as_deref(),
        cal.ics_url_env.as_deref(),
        secrets_dir,
    )?;
    let ics_url = match ics_url.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => ics_url,
    };

    Ok(Config {
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
        family: FamilyConfig {
            switch_at: raw.family.switch_at,
            calendar_days: raw.family.calendar_days,
            ics_url,
            kids,
            weather: raw.family.weather,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    const SAMPLE: &str = include_str!("../config.sample.toml");

    fn write_secrets(dir: &Path) {
        for (name, value) in [
            ("trmnl_kitchen_key", "kitchen-key\n"),
            ("trmnl_office_key", "office-key\n"),
            ("icloud_ics_url", "webcal://example.com/cal.ics\n"),
            ("librus_kid1_login", "kid1\n"),
            ("librus_kid1_password", "pw1\n"),
            ("librus_kid2_login", "kid2\n"),
            ("librus_kid2_password", "pw2\n"),
        ] {
            fs::write(dir.join(name), value).unwrap();
        }
    }

    fn load_str(toml: &str, secrets: &Path) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, toml).unwrap();
        load(&path, secrets)
    }

    fn secrets() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_secrets(dir.path());
        dir
    }

    #[test]
    fn debug_output_never_contains_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = [
            ("trmnl_kitchen_key", "SECRETKEY-kitchen"),
            ("trmnl_office_key", "SECRETKEY-office"),
            ("icloud_ics_url", "webcal://SECRETICS.example.com/cal.ics"),
            ("librus_kid1_login", "SECRETLOGIN1"),
            ("librus_kid1_password", "SECRETPASS1"),
            ("librus_kid2_login", "SECRETLOGIN2"),
            ("librus_kid2_password", "SECRETPASS2"),
        ];
        for (name, value) in secrets {
            fs::write(dir.path().join(name), value).unwrap();
        }
        let c = load_str(SAMPLE, dir.path()).unwrap();
        let dumps = [
            format!("{c:?}"),
            format!("{c:#?}"),
            format!("{:?}", c.devices),
            format!("{:?}", c.family),
        ];
        for dump in &dumps {
            for (_, value) in secrets {
                assert!(!dump.contains(value), "{value} leaked in {dump}");
            }
        }
        assert!(dumps[0].contains("<redacted>"));
        assert!(dumps[0].contains(&c.devices[0].name));
    }

    #[test]
    fn loads_sample_config() {
        let s = secrets();
        let c = load_str(SAMPLE, s.path()).unwrap();
        assert_eq!(c.timezone, chrono_tz::Europe::Warsaw);
        assert_eq!(c.devices.len(), 2);
        let family = &c.devices[0];
        assert_eq!(family.name, "kitchen");
        assert_eq!(family.dashboard, DashboardKind::Family);
        assert_eq!(family.bit_depth, BitDepth::Two);
        assert_eq!(family.api_key, "kitchen-key");
        assert_eq!(c.devices[1].dashboard, DashboardKind::Homelab);
        assert_eq!(
            c.family.switch_at,
            chrono::NaiveTime::from_hms_opt(16, 0, 0).unwrap()
        );
        assert_eq!(c.family.calendar_days, 7);
        assert_eq!(c.family.kids.len(), 2);
        assert_eq!(c.family.kids[1].librus_login, "kid2");
    }

    #[test]
    fn secret_file_wins_and_is_trimmed() {
        let s = secrets();
        fs::write(s.path().join("librus_kid1_password"), "s3cret\n").unwrap();
        let c = load_str(SAMPLE, s.path()).unwrap();
        assert_eq!(c.family.kids[0].librus_password, "s3cret");
    }

    #[test]
    fn env_form_reads_environment() {
        let s = secrets();
        // SAFETY: the variable name is unique to this test, so no other thread touches it.
        unsafe { std::env::set_var("TABLICA_TEST_PW", "  from-env \n") };
        let toml = SAMPLE.replace(
            "librus_password_secret = \"librus_kid1_password\"",
            "librus_password_env = \"TABLICA_TEST_PW\"",
        );
        let c = load_str(&toml, s.path()).unwrap();
        assert_eq!(c.family.kids[0].librus_password, "from-env");
    }

    #[test]
    fn missing_secret_is_error_naming_key() {
        let s = secrets();
        fs::remove_file(s.path().join("librus_kid1_password")).unwrap();
        let err = load_str(SAMPLE, s.path()).unwrap_err();
        assert!(err.to_string().contains("librus_kid1_password"), "{err}");
    }

    #[test]
    fn mac_is_normalized() {
        let s = secrets();
        let toml = SAMPLE.replace("AA:BB:CC:DD:EE:01", "aa-bb-cc-dd-ee-01");
        let c = load_str(&toml, s.path()).unwrap();
        assert_eq!(c.devices[0].mac, "AA:BB:CC:DD:EE:01");

        let bad = SAMPLE.replace("AA:BB:CC:DD:EE:01", "not-a-mac");
        assert!(load_str(&bad, s.path()).is_err());
    }

    #[test]
    fn defaults_apply() {
        let s = secrets();
        let toml = r#"
timezone = "Europe/Warsaw"
base_url = "http://x"

[family]
switch_at = "16:00"
calendar_days = 7

[family.calendar]
ics_url_secret = "icloud_ics_url"

[family.weather]
latitude = 1.0
longitude = 2.0
"#;
        let c = load_str(toml, s.path()).unwrap();
        assert_eq!(c.listen, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.data_dir, Path::new("/data"));
        assert_eq!(
            c.schedule.wake,
            chrono::NaiveTime::from_hms_opt(6, 0, 0).unwrap()
        );
        assert_eq!(
            c.schedule.sleep,
            chrono::NaiveTime::from_hms_opt(22, 0, 0).unwrap()
        );
        assert_eq!(c.schedule.refresh_minutes, 15);
        assert_eq!(c.family.ics_url, "https://example.com/cal.ics");
        assert!(c.devices.is_empty());
    }

    #[test]
    fn bit_depth_one_is_honoured() {
        let s = secrets();
        let toml = SAMPLE.replace("bit_depth = 2 ", "bit_depth = 1 ");
        let c = load_str(&toml, s.path()).unwrap();
        assert_eq!(c.devices[0].bit_depth, BitDepth::One);
    }

    #[test]
    fn invalid_bit_depth_is_error() {
        let s = secrets();
        let toml = SAMPLE.replace("bit_depth = 2 ", "bit_depth = 4 ");
        let err = load_str(&toml, s.path()).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidBitDepth { value: 4, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("kitchen"), "{err}");
    }

    #[test]
    fn unset_env_var_error_names_it_and_owner() {
        let s = secrets();
        let toml = SAMPLE.replace(
            "librus_password_secret = \"librus_kid1_password\"",
            "librus_password_env = \"TABLICA_TEST_UNSET_VAR\"",
        );
        let err = load_str(&toml, s.path()).unwrap_err().to_string();
        assert!(err.contains("TABLICA_TEST_UNSET_VAR"), "{err}");
        assert!(err.contains("Kid 1"), "{err}");
    }

    #[test]
    fn unreadable_secret_surfaces_io_error() {
        let s = secrets();
        // A directory in place of the file: exists, but cannot be read as a file.
        fs::remove_file(s.path().join("trmnl_kitchen_key")).unwrap();
        fs::create_dir(s.path().join("trmnl_kitchen_key")).unwrap();
        let err = load_str(SAMPLE, s.path()).unwrap_err();
        assert!(matches!(err, ConfigError::SecretIo { .. }), "{err}");
        assert!(err.to_string().contains("kitchen"), "{err}");
    }
}
