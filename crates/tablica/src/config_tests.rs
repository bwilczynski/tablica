//! Config-loading tests that need the real family and homelab entries.

use std::fs;
use std::path::Path;
use tablica_core::config::{BitDepth, Config, ConfigError, Loaded, Secrets, load};
use tablica_core::dashboard::{DashboardEntry, ParseResult};

const SAMPLE: &str = include_str!("../../../config.sample.toml");

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

fn entries() -> Vec<DashboardEntry> {
    vec![
        crate::family::dashboard::entry(),
        crate::dashboards::homelab::entry(),
    ]
}

fn load_with(
    toml: &str,
    secrets: &Path,
    entries: &[DashboardEntry],
) -> Result<Loaded, ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, toml).unwrap();
    load(&path, secrets, entries)
}

fn load_str(toml: &str, secrets: &Path) -> Result<Config, ConfigError> {
    load_with(toml, secrets, &entries()).map(|l| l.config)
}

fn secrets() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_secrets(dir.path());
    dir
}

fn names(loaded: &Loaded) -> Vec<&'static str> {
    loaded.dashboards.iter().map(|d| d.name).collect()
}

/// Globals and a valid `[family]` section, no devices.
const MINIMAL: &str = r#"
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

const DEVICE_KEY: &str = r#"
mac = "AA:BB:CC:DD:EE:01"
api_key_secret = "trmnl_kitchen_key"
"#;

#[test]
fn devices_debug_never_contains_secrets() {
    let dir = tempfile::tempdir().unwrap();
    write_secrets(dir.path());
    let secrets = [
        ("trmnl_kitchen_key", "SECRETKEY-kitchen"),
        ("trmnl_office_key", "SECRETKEY-office"),
    ];
    for (name, value) in secrets {
        fs::write(dir.path().join(name), value).unwrap();
    }
    let c = load_str(SAMPLE, dir.path()).unwrap();
    assert_eq!(c.devices[0].api_key, "SECRETKEY-kitchen");
    let dumps = [
        format!("{c:?}"),
        format!("{c:#?}"),
        format!("{:?}", c.devices),
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
fn sample_config_loads() {
    let s = secrets();
    let loaded = load_with(SAMPLE, s.path(), &entries()).unwrap();
    assert_eq!(names(&loaded), ["family", "homelab"]);
    let c = &loaded.config;
    assert_eq!(c.timezone, chrono_tz::Europe::Warsaw);
    assert_eq!(c.devices.len(), 2);
    let family = &c.devices[0];
    assert_eq!(family.name, "kitchen");
    assert_eq!(family.dashboard, "family");
    assert_eq!(family.bit_depth, BitDepth::Two);
    assert_eq!(family.api_key, "kitchen-key");
    assert_eq!(c.devices[1].dashboard, "homelab");
}

#[test]
fn missing_family_is_an_error_with_zero_devices() {
    let s = secrets();
    let toml = "timezone = \"Europe/Warsaw\"\nbase_url = \"http://x\"\n";
    let err = load_str(toml, s.path()).unwrap_err();
    assert!(
        matches!(&err, ConfigError::Dashboard { section, .. } if section == "family"),
        "{err}"
    );
}

#[test]
fn malformed_family_is_an_error_with_zero_devices() {
    let s = secrets();
    let toml = MINIMAL.replace("switch_at = \"16:00\"", "switch_at = 5");
    let err = load_str(&toml, s.path()).unwrap_err();
    assert!(
        matches!(&err, ConfigError::Dashboard { section, message }
            if section == "family" && message.contains("switch_at")),
        "{err}"
    );
}

#[test]
fn device_referencing_unregistered_dashboard_is_rejected() {
    let s = secrets();
    let toml =
        format!("{MINIMAL}\n[[device]]\nname = \"shed\"\ndashboard = \"garden\"\n{DEVICE_KEY}");
    let err = load_str(&toml, s.path()).unwrap_err();
    assert!(
        matches!(&err, ConfigError::UnknownDashboard { device, dashboard }
            if device == "shed" && dashboard == "garden"),
        "{err}"
    );
}

#[test]
fn device_referencing_inactive_dashboard_is_rejected() {
    fn inactive(_: Option<&toml::Value>, _: &Secrets) -> ParseResult {
        Ok(None)
    }
    let s = secrets();
    let mut entries = entries();
    entries.push(DashboardEntry {
        name: "stubby",
        parse: inactive,
    });
    let toml =
        format!("{MINIMAL}\n[[device]]\nname = \"shed\"\ndashboard = \"stubby\"\n{DEVICE_KEY}");
    let err = load_with(&toml, s.path(), &entries).err().unwrap();
    assert!(
        matches!(&err, ConfigError::UnknownDashboard { dashboard, .. } if dashboard == "stubby"),
        "{err}"
    );
}

#[test]
fn invalid_entry_names_are_rejected() {
    let s = secrets();
    let mut entries = entries();
    entries.push(crate::dashboards::homelab::entry());
    let err = load_with(MINIMAL, s.path(), &entries).err().unwrap();
    assert!(
        matches!(err, ConfigError::InvalidDashboardName { .. }),
        "{err}"
    );
}

#[test]
fn unknown_top_level_section_is_ignored() {
    let s = secrets();
    let toml = format!("{MINIMAL}\n[weather_station]\nx = 1\n");
    assert!(load_str(&toml, s.path()).is_ok());
}

#[test]
fn homelab_section_with_keys_is_ignored() {
    let s = secrets();
    let toml = format!("{MINIMAL}\n[homelab]\ntoken = \"x\"\n");
    let loaded = load_with(&toml, s.path(), &entries()).unwrap();
    assert!(names(&loaded).contains(&"homelab"));
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
    let c = load_str(MINIMAL, s.path()).unwrap();
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
fn unreadable_secret_surfaces_io_error() {
    let s = secrets();
    // A directory in place of the file: exists, but cannot be read as a file.
    fs::remove_file(s.path().join("trmnl_kitchen_key")).unwrap();
    fs::create_dir(s.path().join("trmnl_kitchen_key")).unwrap();
    let err = load_str(SAMPLE, s.path()).unwrap_err();
    assert!(matches!(err, ConfigError::SecretIo { .. }), "{err}");
    assert!(err.to_string().contains("kitchen"), "{err}");
}

#[test]
fn secrets_count_reads() {
    let s = secrets();
    let secrets = Secrets::new(s.path().to_owned());
    assert_eq!(secrets.reads(), 0);
    secrets
        .resolve(
            "device `kitchen`",
            "api_key",
            Some("trmnl_kitchen_key"),
            None,
        )
        .unwrap();
    let _ = secrets.resolve("device `x`", "api_key", Some("absent"), None);
    assert_eq!(secrets.reads(), 2);
}

#[test]
fn loader_reads_each_secret_once() {
    let s = secrets();
    let loaded = load_with(SAMPLE, s.path(), &entries()).unwrap();
    // Two device keys, the calendar URL, and a login and a password per kid.
    assert_eq!(loaded.secrets.reads(), 7);
}
