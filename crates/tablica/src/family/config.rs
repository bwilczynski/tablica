//! The `[family]` config section.

use chrono::NaiveTime;
use serde::Deserialize;

use tablica_core::config::{ConfigError, REDACTED, Secrets};

const SECTION: &str = "family";

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

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct WeatherConfig {
    pub latitude: f64,
    pub longitude: f64,
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

// ---- raw (serde) shapes mirroring the TOML ----

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

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Dashboard {
        section: SECTION.to_owned(),
        message: message.into(),
    }
}

/// Parses the `[family]` section, which is required, and resolves its secrets.
pub fn parse_config(
    section: Option<&toml::Value>,
    secrets: &Secrets,
) -> Result<FamilyConfig, ConfigError> {
    let section = section.ok_or_else(|| invalid("missing [family] section"))?;
    let raw: RawFamily = section
        .clone()
        .try_into()
        // Display, unlike `message()`, names the offending key path.
        .map_err(|e: toml::de::Error| invalid(e.to_string().trim().replace('\n', " ")))?;

    let kids = raw
        .kids
        .into_iter()
        .enumerate()
        .map(|(i, k)| {
            let owner = format!("kid {} (`{}`)", i + 1, k.label);
            Ok(KidConfig {
                librus_login: secrets.resolve(
                    &owner,
                    "librus_login",
                    k.librus_login_secret.as_deref(),
                    k.librus_login_env.as_deref(),
                )?,
                librus_password: secrets.resolve(
                    &owner,
                    "librus_password",
                    k.librus_password_secret.as_deref(),
                    k.librus_password_env.as_deref(),
                )?,
                label: k.label,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;

    let cal = raw.calendar;
    let ics_url = secrets.resolve(
        "family.calendar",
        "ics_url",
        cal.ics_url_secret.as_deref(),
        cal.ics_url_env.as_deref(),
    )?;
    let ics_url = match ics_url.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => ics_url,
    };

    Ok(FamilyConfig {
        switch_at: raw.switch_at,
        calendar_days: raw.calendar_days,
        ics_url,
        kids,
        weather: raw.weather,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tablica_core::config::Secrets;

    const SAMPLE: &str = include_str!("../../../../config.sample.toml");

    fn write_secrets(dir: &Path) {
        for (name, value) in [
            ("icloud_ics_url", "webcal://example.com/cal.ics\n"),
            ("librus_kid1_login", "kid1\n"),
            ("librus_kid1_password", "pw1\n"),
            ("librus_kid2_login", "kid2\n"),
            ("librus_kid2_password", "pw2\n"),
        ] {
            fs::write(dir.join(name), value).unwrap();
        }
    }

    fn secrets() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_secrets(dir.path());
        dir
    }

    /// Parses the `[family]` section of a whole config file.
    fn parse_str(toml: &str, secrets_dir: &Path) -> Result<FamilyConfig, ConfigError> {
        let table: toml::Table = toml::from_str(toml).unwrap();
        parse_config(table.get("family"), &Secrets::new(secrets_dir.to_owned()))
    }

    #[test]
    fn family_debug_never_contains_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = [
            ("icloud_ics_url", "webcal://SECRETICS.example.com/cal.ics"),
            ("librus_kid1_login", "SECRETLOGIN1"),
            ("librus_kid1_password", "SECRETPASS1"),
            ("librus_kid2_login", "SECRETLOGIN2"),
            ("librus_kid2_password", "SECRETPASS2"),
        ];
        for (name, value) in secrets {
            fs::write(dir.path().join(name), value).unwrap();
        }
        let c = parse_str(SAMPLE, dir.path()).unwrap();
        assert!(c.ics_url.contains("SECRETICS"), "{}", c.ics_url);
        let dumps = [format!("{c:?}"), format!("{c:#?}"), format!("{:?}", c.kids)];
        for dump in &dumps {
            for marker in [
                "SECRETICS",
                "SECRETLOGIN1",
                "SECRETPASS1",
                "SECRETLOGIN2",
                "SECRETPASS2",
            ] {
                assert!(!dump.contains(marker), "{marker} leaked in {dump}");
            }
        }
        assert!(dumps[0].contains("<redacted>"));
        assert!(dumps[0].contains("Kid 1"));
    }

    #[test]
    fn loads_sample_family() {
        let s = secrets();
        let c = parse_str(SAMPLE, s.path()).unwrap();
        assert_eq!(
            c.switch_at,
            chrono::NaiveTime::from_hms_opt(16, 0, 0).unwrap()
        );
        assert_eq!(c.calendar_days, 7);
        assert_eq!(c.kids.len(), 2);
        assert_eq!(c.kids[1].librus_login, "kid2");
        assert_eq!(
            c.weather,
            WeatherConfig {
                latitude: 52.0,
                longitude: 21.0
            }
        );
    }

    #[test]
    fn webcal_is_rewritten_to_https() {
        let s = secrets();
        let c = parse_str(SAMPLE, s.path()).unwrap();
        assert_eq!(c.ics_url, "https://example.com/cal.ics");
    }

    #[test]
    fn missing_section_is_an_error() {
        let s = secrets();
        let err = parse_config(None, &Secrets::new(s.path().to_owned())).unwrap_err();
        assert!(
            matches!(&err, ConfigError::Dashboard { section, .. } if section == "family"),
            "{err}"
        );
    }

    #[test]
    fn malformed_section_error_names_the_field() {
        let s = secrets();
        let toml = SAMPLE.replace("latitude = 52.0", "latitude = \"a\"");
        let err = parse_str(&toml, s.path()).unwrap_err();
        assert!(
            matches!(&err, ConfigError::Dashboard { section, message }
                if section == "family" && message.contains("weather.latitude")),
            "{err}"
        );
    }

    #[test]
    fn secret_file_wins_and_is_trimmed() {
        let s = secrets();
        fs::write(s.path().join("librus_kid1_password"), "s3cret\n").unwrap();
        let c = parse_str(SAMPLE, s.path()).unwrap();
        assert_eq!(c.kids[0].librus_password, "s3cret");
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
        let c = parse_str(&toml, s.path()).unwrap();
        assert_eq!(c.kids[0].librus_password, "from-env");
    }

    #[test]
    fn missing_secret_is_error_naming_key() {
        let s = secrets();
        fs::remove_file(s.path().join("librus_kid1_password")).unwrap();
        let err = parse_str(SAMPLE, s.path()).unwrap_err();
        assert!(err.to_string().contains("librus_kid1_password"), "{err}");
    }

    #[test]
    fn unset_env_var_error_names_it_and_owner() {
        let s = secrets();
        let toml = SAMPLE.replace(
            "librus_password_secret = \"librus_kid1_password\"",
            "librus_password_env = \"TABLICA_TEST_UNSET_VAR\"",
        );
        let err = parse_str(&toml, s.path()).unwrap_err().to_string();
        assert!(err.contains("TABLICA_TEST_UNSET_VAR"), "{err}");
        assert!(err.contains("Kid 1"), "{err}");
    }

    #[test]
    fn each_secret_is_read_once() {
        let s = secrets();
        let table: toml::Table = toml::from_str(SAMPLE).unwrap();
        let secrets = Secrets::new(s.path().to_owned());
        parse_config(table.get("family"), &secrets).unwrap();
        // The calendar URL plus a login and a password per kid.
        assert_eq!(secrets.reads(), 5);
    }
}
