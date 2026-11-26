//! Open-Meteo forecast source.

use chrono::{NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::SourceError;
use crate::config::WeatherConfig;
use crate::text::{Condition, PrecipKind};
use tracing::debug;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Weather {
    pub current_temp_c: f32,
    pub current: Condition,
    pub today: Daily,
    pub tomorrow: Daily,
    pub hourly: Vec<HourlyPrecip>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Daily {
    /// Local calendar day this entry forecasts. Caches written before this field
    /// existed load with the epoch date, which never matches a real day, so the
    /// view omits them until the next refresh (at most 30 minutes).
    #[serde(default = "epoch")]
    pub date: NaiveDate,
    pub max_c: f32,
    pub min_c: f32,
    pub condition: Condition,
}

fn epoch() -> NaiveDate {
    NaiveDate::default()
}

impl Weather {
    /// The daily entry for `date`, if the forecast covers it.
    pub fn day(&self, date: NaiveDate) -> Option<&Daily> {
        [&self.today, &self.tomorrow]
            .into_iter()
            .find(|d| d.date == date)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HourlyPrecip {
    /// Local wall-clock time.
    pub at: NaiveDateTime,
    pub probability: u8,
    pub kind: PrecipKind,
}

/// Maps a WMO weather code to a condition; unknown codes count as cloudy.
pub fn condition_from_wmo(code: u16) -> Condition {
    match code {
        0 => Condition::Clear,
        1 | 2 => Condition::PartlyCloudy,
        3 => Condition::Cloudy,
        45 | 48 => Condition::Fog,
        51..=67 | 80..=82 => Condition::Rain,
        71..=77 | 85 | 86 => Condition::Snow,
        95..=99 => Condition::Storm,
        _ => Condition::Cloudy,
    }
}

fn precip_kind_from_wmo(code: u16) -> PrecipKind {
    match code {
        71..=77 | 85 | 86 => PrecipKind::Snow,
        _ => PrecipKind::Rain,
    }
}

#[derive(Deserialize)]
struct Raw {
    current: RawCurrent,
    daily: RawDaily,
    hourly: RawHourly,
}

#[derive(Deserialize)]
struct RawCurrent {
    temperature_2m: f32,
    weather_code: u16,
}

#[derive(Deserialize)]
struct RawDaily {
    time: Vec<String>,
    weather_code: Vec<u16>,
    temperature_2m_max: Vec<f32>,
    temperature_2m_min: Vec<f32>,
}

#[derive(Deserialize)]
struct RawHourly {
    time: Vec<String>,
    precipitation_probability: Vec<Option<f32>>,
    weather_code: Vec<u16>,
}

/// Parses an Open-Meteo forecast response.
pub fn parse(json: &str) -> Result<Weather, SourceError> {
    let raw: Raw = serde_json::from_str(json).map_err(|e| {
        debug!(
            source = "weather",
            line = e.line(),
            column = e.column(),
            "parse failed"
        );
        SourceError::Parse("forecast JSON does not match the expected shape")
    })?;
    let d = &raw.daily;
    let day = |i: usize| -> Result<Daily, SourceError> {
        match (
            d.time.get(i),
            d.weather_code.get(i),
            d.temperature_2m_max.get(i),
            d.temperature_2m_min.get(i),
        ) {
            (Some(time), Some(&code), Some(&max_c), Some(&min_c)) => Ok(Daily {
                date: NaiveDate::parse_from_str(time, "%Y-%m-%d")
                    .map_err(|_| SourceError::Parse("bad daily date"))?,
                max_c,
                min_c,
                condition: condition_from_wmo(code),
            }),
            _ => Err(SourceError::Parse("missing daily entry")),
        }
    };
    let h = &raw.hourly;
    if h.precipitation_probability.len() != h.time.len() || h.weather_code.len() != h.time.len() {
        return Err(SourceError::Parse("hourly arrays differ in length"));
    }
    let mut hourly = Vec::with_capacity(h.time.len());
    for ((time, prob), &code) in h
        .time
        .iter()
        .zip(&h.precipitation_probability)
        .zip(&h.weather_code)
    {
        // Open-Meteo local times carry no seconds or offset.
        let at = NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H:%M")
            .map_err(|_| SourceError::Parse("bad hourly time"))?;
        hourly.push(HourlyPrecip {
            at,
            probability: prob.unwrap_or(0.0).clamp(0.0, 100.0).round() as u8,
            kind: precip_kind_from_wmo(code),
        });
    }
    Ok(Weather {
        current_temp_c: raw.current.temperature_2m,
        current: condition_from_wmo(raw.current.weather_code),
        today: day(0)?,
        tomorrow: day(1)?,
        hourly,
    })
}

/// Fetches and parses the two-day forecast.
pub async fn fetch(
    client: &reqwest::Client,
    base_url: &str,
    cfg: &WeatherConfig,
    tz: Tz,
) -> Result<Weather, SourceError> {
    let url = format!("{}/v1/forecast", base_url.trim_end_matches('/'));
    let resp = client
        .get(url)
        .query(&[
            ("latitude", cfg.latitude.to_string()),
            ("longitude", cfg.longitude.to_string()),
            ("current", "temperature_2m,weather_code".to_string()),
            (
                "daily",
                "weather_code,temperature_2m_max,temperature_2m_min".to_string(),
            ),
            (
                "hourly",
                "precipitation_probability,weather_code".to_string(),
            ),
            ("timezone", tz.name().to_string()),
            ("forecast_days", "2".to_string()),
        ])
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(SourceError::Status(status.as_u16()));
    }
    parse(&resp.text().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FIXTURE: &str = include_str!("../../tests/fixtures/weather/forecast.json");

    #[test]
    fn wmo_mapping() {
        assert_eq!(condition_from_wmo(0), Condition::Clear);
        assert_eq!(condition_from_wmo(1), Condition::PartlyCloudy);
        assert_eq!(condition_from_wmo(2), Condition::PartlyCloudy);
        assert_eq!(condition_from_wmo(3), Condition::Cloudy);
        assert_eq!(condition_from_wmo(45), Condition::Fog);
        assert_eq!(condition_from_wmo(48), Condition::Fog);
        for c in (51..=67).chain(80..=82) {
            assert_eq!(condition_from_wmo(c), Condition::Rain, "{c}");
        }
        for c in (71..=77).chain([85, 86]) {
            assert_eq!(condition_from_wmo(c), Condition::Snow, "{c}");
        }
        for c in 95..=99 {
            assert_eq!(condition_from_wmo(c), Condition::Storm, "{c}");
        }
        assert_eq!(condition_from_wmo(1234), Condition::Cloudy);
    }

    #[test]
    fn parses_fixture() {
        let w = parse(FIXTURE).unwrap();
        let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let expected = v["current"]["temperature_2m"].as_f64().unwrap() as f32;
        assert_eq!(w.current_temp_c, expected);
        assert_eq!(w.hourly.len(), 48);
        assert!(w.today.max_c >= w.today.min_c);
        assert_eq!(w.today.date, NaiveDate::from_ymd_opt(2026, 11, 25).unwrap());
        assert_eq!(
            w.tomorrow.date,
            NaiveDate::from_ymd_opt(2026, 11, 26).unwrap()
        );
        assert_eq!(w.day(w.tomorrow.date), Some(&w.tomorrow));
        assert_eq!(w.day(NaiveDate::from_ymd_opt(2026, 11, 27).unwrap()), None);
    }

    #[test]
    fn old_cache_without_dates_still_loads_but_matches_no_day() {
        let w = parse(FIXTURE).unwrap();
        let mut v = serde_json::to_value(&w).unwrap();
        v["today"].as_object_mut().unwrap().remove("date");
        v["tomorrow"].as_object_mut().unwrap().remove("date");
        let old: Weather = serde_json::from_value(v).unwrap();
        assert_eq!(old.day(w.today.date), None);
        assert_eq!(old.day(w.tomorrow.date), None);
    }

    #[test]
    fn precip_kind_from_hourly_code() {
        let json = r#"{
            "current": {"temperature_2m": -1.0, "weather_code": 73},
            "daily": {"time": ["2026-01-05", "2026-01-06"], "weather_code": [73, 3], "temperature_2m_max": [0.0, 2.0], "temperature_2m_min": [-3.0, -1.0]},
            "hourly": {"time": ["2026-01-05T08:00", "2026-01-05T09:00"],
                       "precipitation_probability": [80, null],
                       "weather_code": [73, 61]}
        }"#;
        let w = parse(json).unwrap();
        assert_eq!(w.hourly[0].kind, PrecipKind::Snow);
        assert_eq!(w.hourly[0].probability, 80);
        assert_eq!(w.hourly[1].kind, PrecipKind::Rain);
        assert_eq!(w.hourly[1].probability, 0);
        assert_eq!(w.current, Condition::Snow);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(matches!(parse("{}"), Err(SourceError::Parse(_))));
    }

    #[tokio::test]
    async fn fetch_hits_expected_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/forecast"))
            .and(query_param("latitude", "52.23"))
            .and(query_param("longitude", "21.01"))
            .and(query_param("current", "temperature_2m,weather_code"))
            .and(query_param(
                "daily",
                "weather_code,temperature_2m_max,temperature_2m_min",
            ))
            .and(query_param(
                "hourly",
                "precipitation_probability,weather_code",
            ))
            .and(query_param("timezone", "Europe/Warsaw"))
            .and(query_param("forecast_days", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
            .expect(1)
            .mount(&server)
            .await;
        let cfg = WeatherConfig {
            latitude: 52.23,
            longitude: 21.01,
        };
        let w = fetch(
            &reqwest::Client::new(),
            &server.uri(),
            &cfg,
            Tz::Europe__Warsaw,
        )
        .await
        .unwrap();
        assert_eq!(w.hourly.len(), 48);
    }

    #[tokio::test]
    async fn fetch_reports_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let cfg = WeatherConfig {
            latitude: 1.0,
            longitude: 2.0,
        };
        let err = fetch(&reqwest::Client::new(), &server.uri(), &cfg, Tz::UTC)
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Status(503)));
    }
}
