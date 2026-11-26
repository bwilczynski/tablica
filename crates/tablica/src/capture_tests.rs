//! `tablica capture librus` loads the full config, then takes family's kids.

use std::fs;

use tablica_core::config::{ConfigError, Loaded, load};
use tablica_family::family_kids;

const FAMILY: &str = r#"
timezone = "Europe/Warsaw"
base_url = "http://x"

[family]
switch_at = "16:00"
calendar_days = 7

[family.calendar]
ics_url_secret = "icloud_ics_url"

[[family.kid]]
label = "Kid 1"
librus_login_secret = "librus_kid1_login"
librus_password_secret = "librus_kid1_password"

[family.weather]
latitude = 1.0
longitude = 2.0
"#;

const OFFICE: &str = r#"
[[device]]
name = "office"
mac = "AA:BB:CC:DD:EE:02"
api_key_secret = "trmnl_office_key"
dashboard = "homelab"
"#;

fn load_toml(toml: &str) -> Result<Loaded, ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    for (name, value) in [
        ("trmnl_office_key", "office-key"),
        ("icloud_ics_url", "https://example.com/cal.ics"),
        ("librus_kid1_login", "kid1"),
        ("librus_kid1_password", "pw1"),
    ] {
        fs::write(dir.path().join(name), value).unwrap();
    }
    let path = dir.path().join("config.toml");
    fs::write(&path, toml).unwrap();
    let entries = [tablica_family::entry(), tablica_homelab::entry()];
    load(&path, dir.path(), &entries)
}

#[test]
fn capture_rejects_invalid_unrelated_device() {
    let toml = format!(
        "{FAMILY}{}",
        OFFICE.replace("AA:BB:CC:DD:EE:02", "not-a-mac")
    );
    let err = load_toml(&toml).err().expect("bad MAC accepted");
    assert!(matches!(err, ConfigError::InvalidMac { .. }), "{err}");
}

#[test]
fn capture_accepts_zero_devices_with_kids() {
    let kids = family_kids(load_toml(FAMILY).unwrap()).unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].librus_login, "kid1");
}

#[test]
fn capture_reads_each_secret_once() {
    let loaded = load_toml(&format!("{FAMILY}{OFFICE}")).unwrap();
    // The office key, the calendar URL, and the kid's login and password.
    assert_eq!(loaded.secrets.reads(), 4);
    let kids = family_kids(loaded).unwrap();
    assert_eq!(kids[0].librus_password, "pw1");
}
