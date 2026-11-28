//! Builds the diagnostics screen's template context from a device's history
//! and its dashboard's sections. Every value is pre-formatted here; the
//! template only lays them out.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use serde::Serialize;

use super::battery::{self, Estimate, Power};
use super::derive::{self, rssi_status};
use super::{Row, Section, Status, format_age};
use crate::config::Device;
use crate::dashboard::View;
use crate::history::{DeviceHistory, DeviceLog, Reading};
use crate::render::DIAG_TEMPLATE;

/// Section lines shown in total: each kept section's heading and its rows.
/// Rows that do not fit are "+N more".
pub const MAX_ROWS: usize = 10;
/// Longest row note, in characters, ellipsis included.
pub const NOTE_MAX: usize = 60;
/// Longest device log message, in characters, ellipsis included.
pub const LOG_MESSAGE_MAX: usize = 120;

const NONE_IN_90_DAYS: &str = "none in 90 days";
/// Longer battery estimates read as ">90 days": a slope that flat is noise.
const DAYS_LEFT_MAX: f64 = 90.0;

pub struct DiagInput<'a> {
    pub device: &'a Device,
    /// `Err` when history could not be read: the screen says "unavailable".
    pub history: anyhow::Result<DeviceHistory>,
    pub sections: Vec<Section>,
    pub at: DateTime<Utc>,
    /// Whether `at` was asked for, rather than the real now.
    pub at_given: bool,
    pub tz: Tz,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PowerView {
    /// `"battery"`, `"no_battery"` or `"no_data"`.
    pub kind: &'static str,
    pub volts: Option<String>,
    pub percent: Option<String>,
    pub estimate: Option<String>,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WifiView {
    pub rssi: String,
    pub label: &'static str,
    pub band: Option<String>,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WakeView {
    pub last: Option<String>,
    pub median: Option<String>,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProblemView {
    pub reason: String,
    pub when: String,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogView {
    pub level: Option<String>,
    pub message: String,
    pub when: String,
}

/// A found entry, or the text shown in its place.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Found<T> {
    Some(T),
    None(&'static str),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Footer {
    pub fw: Option<String>,
    pub model: Option<String>,
    pub tablica: &'static str,
    pub checked_in: Option<String>,
    pub next_wake: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiagView {
    pub device: String,
    pub power: PowerView,
    pub wifi: Option<WifiView>,
    pub wake: WakeView,
    pub problem: Found<ProblemView>,
    pub last_log: Found<LogView>,
    pub sections: Vec<Section>,
    /// `"+N more"` when the sections had more rows than fit.
    pub more: Option<String>,
    pub footer: Footer,
    pub unavailable: bool,
    pub no_checkin: bool,
}

pub fn diag_view(input: DiagInput<'_>) -> DiagView {
    diag_view_with_reading(input, None)
}

fn diag_view_with_reading(input: DiagInput<'_>, live: Option<&Reading>) -> DiagView {
    let DiagInput {
        device,
        history,
        sections,
        at,
        at_given,
        tz,
    } = input;
    let (history, unavailable) = match history {
        Ok(h) => (h, false),
        Err(_) => (DeviceHistory::default(), true),
    };
    let latest = live.or(history.latest.as_ref());
    let (sections, more) = budget(sections, at_given);

    DiagView {
        device: device.name.clone(),
        power: power_view(battery::power(
            battery::latest_from(latest),
            &battery::samples_from(&history.readings),
        )),
        wifi: latest.and_then(wifi_view),
        wake: wake_view(derive::wake(latest, &history.readings, at)),
        problem: match &history.last_problem {
            Some(r) => Found::Some(problem_view(r, at, tz)),
            None => Found::None(NONE_IN_90_DAYS),
        },
        last_log: match &history.last_log {
            Some(l) => Found::Some(log_view(l, at, tz)),
            None => Found::None(NONE_IN_90_DAYS),
        },
        sections,
        more,
        footer: footer(latest, at, tz),
        unavailable,
        no_checkin: latest.is_none(),
    }
}

/// Keeps at most `MAX_ROWS` lines across all sections, in order, a kept
/// section's heading costing one line, and shortens notes; returns the
/// "+N more" text for the rows left out. A section with no rows (or no room
/// for one) is left out.
fn budget(sections: Vec<Section>, at_given: bool) -> (Vec<Section>, Option<String>) {
    let mut left = MAX_ROWS;
    let mut hidden = 0;
    let mut kept = Vec::new();
    for section in sections {
        let mut rows = section.rows;
        let room = left.saturating_sub(1);
        if rows.len() > room {
            hidden += rows.len() - room;
            rows.truncate(room);
        }
        if rows.is_empty() {
            continue;
        }
        left -= 1 + rows.len();
        let rows = rows
            .into_iter()
            .map(|row| Row {
                note: row.note.map(|n| shorten(n, NOTE_MAX)),
                ..row
            })
            .collect();
        let title = if at_given {
            format!("{} (now)", section.title)
        } else {
            section.title
        };
        kept.push(Section { title, rows });
    }
    (kept, (hidden > 0).then(|| format!("+{hidden} more")))
}

fn power_view(power: Power) -> PowerView {
    match power {
        Power::NoData => PowerView {
            kind: "no_data",
            volts: None,
            percent: None,
            estimate: None,
            status: Status::Unknown,
        },
        Power::NoBattery => PowerView {
            kind: "no_battery",
            volts: None,
            percent: None,
            estimate: None,
            status: Status::Ok,
        },
        Power::Battery {
            volts,
            percent,
            estimate: e,
        } => PowerView {
            kind: "battery",
            volts: Some(format!("{volts:.2} V")),
            percent: Some(format!("{percent:.0} %")),
            estimate: Some(estimate(&e)),
            status: if percent < 10.0 {
                Status::Bad
            } else if percent < 25.0 {
                Status::Warn
            } else {
                Status::Ok
            },
        },
    }
}

fn wifi_view(latest: &Reading) -> Option<WifiView> {
    let rssi = latest.rssi?;
    let (label, status) = rssi_status(rssi);
    Some(WifiView {
        rssi: format!("{rssi} dBm"),
        label,
        band: latest.wifi_band.clone(),
        status,
    })
}

fn seconds(ms: i64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

fn wake_view(wake: derive::Wake) -> WakeView {
    WakeView {
        last: wake.last_ms.map(seconds),
        median: wake.median_24h_ms.map(seconds),
        status: wake.status,
    }
}

fn problem_view(r: &Reading, at: DateTime<Utc>, tz: Tz) -> ProblemView {
    let reason = r.reset_reason.as_deref().unwrap_or("unknown");
    ProblemView {
        reason: reason.strip_prefix("ESP_RST_").unwrap_or(reason).to_owned(),
        when: clock(r.at, at, tz),
        status: derive::reset_status(reason),
    }
}

fn log_view(l: &DeviceLog, at: DateTime<Utc>, tz: Tz) -> LogView {
    let message = l.message.clone().unwrap_or_else(|| l.raw.clone());
    LogView {
        level: l.level.clone(),
        message: shorten(message, LOG_MESSAGE_MAX),
        when: clock(l.received_at, at, tz),
    }
}

fn footer(latest: Option<&Reading>, at: DateTime<Utc>, tz: Tz) -> Footer {
    let fw = latest.and_then(|r| {
        let version = r.fw_version.as_deref()?;
        Some(match r.fw_commit.as_deref() {
            Some(commit) => format!("{version} ({commit})"),
            None => version.to_owned(),
        })
    });
    let checked_in = latest.map(|r| {
        let age = format_age(at - r.at);
        let age = if age == "just now" {
            age
        } else {
            format!("{age} ago")
        };
        format!("{age} · {}", clock(r.at, at, tz))
    });
    let next_wake = latest
        .and_then(|r| Some(r.at + Duration::seconds(r.next_refresh?)))
        .map_or_else(|| "unknown".to_owned(), |t| clock(t, at, tz));
    Footer {
        fw,
        model: latest.and_then(|r| r.model.clone()),
        tablica: env!("CARGO_PKG_VERSION"),
        checked_in,
        next_wake,
    }
}

fn estimate(e: &Estimate) -> String {
    match *e {
        Estimate::Collecting { hours } => format!("collecting data ({hours} h)"),
        Estimate::DaysLeft(d) if d < 0.5 => "<1 day".to_owned(),
        Estimate::DaysLeft(d) if d < 1.5 => "~1 day".to_owned(),
        Estimate::DaysLeft(d) if d > DAYS_LEFT_MAX => format!(">{DAYS_LEFT_MAX:.0} days"),
        Estimate::DaysLeft(d) => format!("~{:.0} days", d.round()),
    }
}

/// Cuts `s` to at most `max` characters, the last one an ellipsis when cut.
fn shorten(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// `HH:MM` in `tz`, prefixed `dd.mm ` when not on `at`'s local day.
fn clock(t: DateTime<Utc>, at: DateTime<Utc>, tz: Tz) -> String {
    let local = t.with_timezone(&tz);
    if local.date_naive() == at.with_timezone(&tz).date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%d.%m %H:%M").to_string()
    }
}

pub fn build_live(input: DiagInput<'_>, reading: &Reading) -> View {
    as_view(diag_view_with_reading(input, Some(reading)))
}

pub fn build(input: DiagInput<'_>) -> View {
    as_view(diag_view(input))
}

fn as_view(view: DiagView) -> View {
    View {
        template: DIAG_TEMPLATE,
        context: minijinja::Value::from_serialize(view),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BitDepth;
    use chrono::TimeZone;

    fn device() -> Device {
        Device {
            name: "kitchen".into(),
            mac: "AA:BB:CC:DD:EE:FF".into(),
            api_key: "k".into(),
            dashboard: "family".into(),
            bit_depth: BitDepth::Two,
            diagnostics_button: false,
        }
    }

    /// 2026-11-27 07:13 Warsaw.
    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 27, 6, 13, 0).unwrap()
    }

    fn sources(n: usize, title: &str) -> Section {
        Section {
            title: title.into(),
            rows: (0..n)
                .map(|i| Row {
                    label: format!("source {i}"),
                    value: "5 min".into(),
                    status: Status::Ok,
                    note: None,
                })
                .collect(),
        }
    }

    /// A battery device: four days of steady drain every 15 min, a brownout two
    /// days ago, one error log, last check-in 12 minutes before `at`.
    fn battery_history() -> DeviceHistory {
        let latest_at = at() - Duration::minutes(12);
        let readings: Vec<Reading> = (0..=4 * 96)
            .rev()
            .map(|i| {
                let when = latest_at - Duration::minutes(15 * i);
                Reading {
                    device: "AA:BB:CC:DD:EE:FF".into(),
                    at: when,
                    battery_v: Some(4.00 - 0.10 * (4 * 96 - i) as f64 / (4.0 * 96.0)),
                    rssi: Some(-84),
                    wifi_band: Some("2.4 GHz".into()),
                    wake_ms: Some(4100 + (i % 3) * 3900),
                    reset_reason: Some("ESP_RST_DEEPSLEEP".into()),
                    refresh_rate: Some(900),
                    next_refresh: Some(900),
                    fw_version: Some("1.8.19".into()),
                    fw_commit: Some("abc1234".into()),
                    model: Some("og".into()),
                    ..Reading::default()
                }
            })
            .collect();
        let latest = readings.last().cloned().unwrap();
        let brownout = Reading {
            reset_reason: Some("ESP_RST_BROWNOUT".into()),
            ..readings[readings.len() - 1 - 2 * 96].clone()
        };
        DeviceHistory {
            latest: Some(latest),
            readings,
            last_problem: Some(brownout),
            last_log: Some(DeviceLog {
                device: "AA:BB:CC:DD:EE:FF".into(),
                received_at: at() - Duration::minutes(15),
                created_at: None,
                level: Some("error".into()),
                message: Some("display refresh failed: timeout".into()),
                wake_reason: None,
                raw: "{}".into(),
            }),
        }
    }

    fn input<'a>(
        device: &'a Device,
        history: anyhow::Result<DeviceHistory>,
        sections: Vec<Section>,
    ) -> DiagInput<'a> {
        DiagInput {
            device,
            history,
            sections,
            at: at(),
            at_given: false,
            tz: chrono_tz::Europe::Warsaw,
        }
    }

    /// Fills every budget: full rows with max-length notes, a max-length log
    /// message, the "+N more" row.
    fn dense_input(device: &Device) -> DiagInput<'_> {
        let mut history = battery_history();
        if let Some(log) = history.last_log.as_mut() {
            log.message = Some("Wwwwwwwwww ".repeat(100));
        }
        let noted = |title: &str| Section {
            title: title.into(),
            rows: (0..8)
                .map(|i| Row {
                    label: format!("Timetable {i} Wwwwwww"),
                    value: "47 h".into(),
                    status: [Status::Ok, Status::Warn, Status::Bad, Status::Unknown][i % 4],
                    note: Some("W".repeat(500)),
                })
                .collect(),
        };
        DiagInput {
            at_given: true,
            ..input(device, Ok(history), vec![noted("Sources"), noted("Other")])
        }
    }

    #[test]
    fn battery_device_context() {
        let d = device();
        let v = diag_view(input(
            &d,
            Ok(battery_history()),
            vec![sources(3, "Sources")],
        ));
        assert_eq!(v.device, "kitchen");
        assert!(!v.unavailable && !v.no_checkin);

        assert_eq!(v.power.kind, "battery");
        assert_eq!(v.power.volts.as_deref(), Some("3.90 V"));
        assert_eq!(v.power.percent.as_deref(), Some("65 %"));
        let estimate = v.power.estimate.clone().unwrap();
        assert!(
            estimate.starts_with('~') && estimate.ends_with(" days"),
            "{estimate}"
        );

        let wifi = v.wifi.clone().unwrap();
        assert_eq!(wifi.rssi, "-84 dBm");
        assert_eq!((wifi.label, wifi.status), ("poor", Status::Bad));
        assert_eq!(wifi.band.as_deref(), Some("2.4 GHz"));

        assert_eq!(v.wake.last.as_deref(), Some("4.1 s"));
        assert_eq!(v.wake.median.as_deref(), Some("8.0 s"));

        match &v.problem {
            Found::Some(p) => {
                assert_eq!(p.reason, "BROWNOUT");
                assert_eq!(p.status, Status::Bad);
                assert_eq!(p.when, "25.11 07:01");
            }
            other => panic!("{other:?}"),
        }
        match &v.last_log {
            Found::Some(l) => {
                assert_eq!(l.message, "display refresh failed: timeout");
                assert_eq!(l.level.as_deref(), Some("error"));
                assert_eq!(l.when, "06:58");
            }
            other => panic!("{other:?}"),
        }

        assert_eq!(v.sections[0].title, "Sources");
        assert_eq!(v.more, None);
        assert_eq!(v.footer.fw.as_deref(), Some("1.8.19 (abc1234)"));
        assert_eq!(v.footer.model.as_deref(), Some("og"));
        assert_eq!(v.footer.tablica, env!("CARGO_PKG_VERSION"));
        assert_eq!(v.footer.checked_in.as_deref(), Some("12 min ago · 07:01"));
        // latest.at (07:01) + next_refresh (900 s).
        assert_eq!(v.footer.next_wake, "07:16");

        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["problem"]["status"], "bad");
        assert_eq!(json["power"]["kind"], "battery");
    }

    #[test]
    fn no_checkin_yet() {
        let d = device();
        let v = diag_view(input(&d, Ok(DeviceHistory::default()), Vec::new()));
        assert!(v.no_checkin && !v.unavailable);
        assert_eq!(v.power.kind, "no_data");
        assert_eq!(v.power.status, Status::Unknown);
        assert_eq!(v.wifi, None);
        assert_eq!(v.wake.last, None);
        assert_eq!(v.problem, Found::None("none in 90 days"));
        assert_eq!(v.last_log, Found::None("none in 90 days"));
        assert_eq!(v.footer.next_wake, "unknown");
        assert_eq!(v.footer.checked_in, None);
        assert_eq!(v.footer.fw, None);
    }

    #[test]
    fn no_battery_and_collecting() {
        let d = device();
        let mut h = battery_history();
        h.latest.as_mut().unwrap().battery_v = Some(0.0);
        let v = diag_view(input(&d, Ok(h), Vec::new()));
        assert_eq!(v.power.kind, "no_battery");
        assert_eq!(v.power.percent, None);

        let mut h = battery_history();
        h.readings.drain(..h.readings.len() - 120);
        let v = diag_view(input(&d, Ok(h), Vec::new()));
        assert_eq!(v.power.estimate.as_deref(), Some("collecting data (29 h)"));
    }

    #[test]
    fn history_unavailable() {
        let d = device();
        let v = diag_view(input(
            &d,
            Err(anyhow::anyhow!("locked")),
            vec![sources(2, "Sources")],
        ));
        assert!(v.unavailable);
        assert_eq!(v.sections, vec![sources(2, "Sources")]);
        assert_eq!(v.footer.next_wake, "unknown");
    }

    #[test]
    fn at_marks_sections_as_now() {
        let d = device();
        let mut i = input(
            &d,
            Ok(DeviceHistory::default()),
            vec![sources(1, "Sources")],
        );
        i.at_given = true;
        assert_eq!(diag_view(i).sections[0].title, "Sources (now)");
    }

    #[test]
    fn layout_budget() {
        let d = device();
        let v = diag_view(input(
            &d,
            Ok(DeviceHistory::default()),
            vec![sources(5, "A"), sources(8, "B"), sources(3, "C")],
        ));
        // A: heading + 5 rows; B: heading + 3 of its 8; C: no room left.
        assert_eq!(lines(&v), MAX_ROWS);
        assert_eq!(v.sections.len(), 2);
        assert_eq!(v.sections[1].rows.len(), 3);
        assert_eq!(v.more.as_deref(), Some("+8 more"));

        let v = diag_view(input(
            &d,
            Ok(DeviceHistory::default()),
            vec![sources(8, "A"), sources(8, "B")],
        ));
        // One line left after A is a heading with no room for a row: B is dropped.
        assert_eq!(lines(&v), 9);
        assert_eq!(v.sections.len(), 1);
        assert_eq!(v.more.as_deref(), Some("+8 more"));

        let v = diag_view(input(
            &d,
            Ok(DeviceHistory::default()),
            vec![
                sources(0, "Empty"),
                sources(2, "A"),
                sources(0, "Also empty"),
            ],
        ));
        assert_eq!(v.sections.len(), 1);
        assert_eq!(v.sections[0].title, "A");
        assert_eq!(v.more, None);

        let v = diag_view(dense_input(&d));
        let note = v.sections[0].rows[0].note.clone().unwrap();
        assert_eq!(note.chars().count(), 60);
        assert!(note.ends_with('…'));
        match &v.last_log {
            Found::Some(l) => {
                assert_eq!(l.message.chars().count(), 120);
                assert!(l.message.ends_with('…'));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Section headings and rows shown: what the row budget counts.
    fn lines(v: &DiagView) -> usize {
        v.sections.iter().map(|s| 1 + s.rows.len()).sum()
    }

    /// Ten sections of one row each.
    fn single_row_sections() -> Vec<Section> {
        (0..10)
            .map(|i| sources(1, &format!("Section {i}")))
            .collect()
    }

    #[test]
    fn headings_count_against_the_budget() {
        let d = device();
        let v = diag_view(input(
            &d,
            Ok(DeviceHistory::default()),
            single_row_sections(),
        ));
        assert_eq!(lines(&v), MAX_ROWS);
        assert_eq!(v.sections.len(), 5);
        assert_eq!(v.more.as_deref(), Some("+5 more"));
    }

    #[test]
    fn estimate_and_clock_formats() {
        assert_eq!(estimate(&Estimate::DaysLeft(41.4)), "~41 days");
        assert_eq!(estimate(&Estimate::DaysLeft(1.2)), "~1 day");
        assert_eq!(estimate(&Estimate::DaysLeft(0.4)), "<1 day");
        assert_eq!(
            estimate(&Estimate::Collecting { hours: 30 }),
            "collecting data (30 h)"
        );
        assert_eq!(shorten("short".into(), 60), "short");
        assert_eq!(shorten("é".repeat(70), 60).chars().count(), 60);
        assert_eq!(shorten("abc".into(), 0), "");
        assert_eq!(shorten("abc".into(), 1), "…");
        assert_eq!(estimate(&Estimate::DaysLeft(90.0)), "~90 days");
        assert_eq!(estimate(&Estimate::DaysLeft(90.4)), ">90 days");
        assert_eq!(estimate(&Estimate::DaysLeft(1234.0)), ">90 days");
        assert_eq!(estimate(&Estimate::DaysLeft(f64::INFINITY)), ">90 days");
        let tz = chrono_tz::Europe::Warsaw;
        // 23:30 UTC on the 26th is 00:30 on the 27th in Warsaw: same local day.
        assert_eq!(
            clock(
                Utc.with_ymd_and_hms(2026, 11, 26, 23, 30, 0).unwrap(),
                at(),
                tz
            ),
            "00:30"
        );
        assert_eq!(
            clock(
                Utc.with_ymd_and_hms(2026, 11, 26, 22, 30, 0).unwrap(),
                at(),
                tz
            ),
            "26.11 23:30"
        );
    }

    #[test]
    fn diag_renders_both_depths_under_limit() {
        use crate::render::{MAX_PNG_BYTES, Renderer, TemplateSource};
        let r = Renderer::new(TemplateSource::Embedded, &Default::default()).unwrap();
        let d = device();
        let views = [
            (
                "battery",
                build(input(
                    &d,
                    Ok(battery_history()),
                    vec![sources(3, "Sources")],
                )),
            ),
            ("dense", build(dense_input(&d))),
            (
                "single-row sections",
                build(input(&d, Ok(battery_history()), single_row_sections())),
            ),
            (
                "empty",
                build(input(&d, Ok(DeviceHistory::default()), Vec::new())),
            ),
            (
                "unavailable",
                build(input(&d, Err(anyhow::anyhow!("x")), Vec::new())),
            ),
        ];
        for (name, view) in &views {
            for (depth, bits) in [
                (BitDepth::One, png::BitDepth::One),
                (BitDepth::Two, png::BitDepth::Two),
            ] {
                let png = r.render_diag(view, depth).unwrap();
                assert!(png.len() <= MAX_PNG_BYTES, "{name}: {}", png.len());
                let dec = png::Decoder::new(std::io::Cursor::new(png))
                    .read_info()
                    .unwrap();
                let info = dec.info();
                assert_eq!((info.width, info.height, info.bit_depth), (800, 480, bits));
            }
        }
    }
}
