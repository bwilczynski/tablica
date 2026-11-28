//! Finite label mappings for device-sent strings. Anything outside the
//! allowlist collapses to `other`, so label cardinality stays bounded.

/// The reset reason that is a normal wake, not a fault; never counted.
pub const DEEP_SLEEP: &str = "ESP_RST_DEEPSLEEP";

/// ESP-IDF `esp_reset_reason_t` names.
pub const RESET_REASONS: &[&str] = &[
    "ESP_RST_UNKNOWN",
    "ESP_RST_POWERON",
    "ESP_RST_EXT",
    "ESP_RST_SW",
    "ESP_RST_PANIC",
    "ESP_RST_INT_WDT",
    "ESP_RST_TASK_WDT",
    "ESP_RST_WDT",
    DEEP_SLEEP,
    "ESP_RST_BROWNOUT",
    "ESP_RST_SDIO",
    "ESP_RST_USB",
    "ESP_RST_JTAG",
    "ESP_RST_EFUSE",
    "ESP_RST_PWR_GLITCH",
    "ESP_RST_CPU_LOCKUP",
];

/// Known TRMNL firmware model names.
pub const MODELS: &[&str] = &["og", "x", "xiao_epaper_display"];

const OTHER: &str = "other";

/// The allowlisted reset reason, else `other`.
pub fn reset_reason(raw: &str) -> &'static str {
    RESET_REASONS
        .iter()
        .copied()
        .find(|r| *r == raw)
        .unwrap_or(OTHER)
}

/// The label for a counted reset: `None` when absent or deep sleep.
pub fn counted_reset(raw: Option<&str>) -> Option<&'static str> {
    let raw = raw?;
    if raw == DEEP_SLEEP {
        return None;
    }
    Some(reset_reason(raw))
}

/// Every value `counted_reset` can return.
pub fn counted_reset_labels() -> impl Iterator<Item = &'static str> {
    RESET_REASONS
        .iter()
        .copied()
        .filter(|r| *r != DEEP_SLEEP)
        .chain(std::iter::once(OTHER))
}

/// The allowlisted model, else `other`; `None` when absent.
pub fn model(raw: Option<&str>) -> Option<&'static str> {
    let raw = raw?;
    Some(MODELS.iter().copied().find(|m| *m == raw).unwrap_or(OTHER))
}

/// The trimmed firmware version if it is `D{1,2}.D{1,2}.D{1,2}`, else
/// `other`; `None` when absent.
pub fn fw_version(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    let ok = {
        let parts: Vec<&str> = raw.split('.').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| (1..=2).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
    };
    Some(if ok {
        raw.to_string()
    } else {
        OTHER.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_reasons_map() {
        assert_eq!(reset_reason("ESP_RST_BROWNOUT"), "ESP_RST_BROWNOUT");
        assert_eq!(reset_reason("garbage"), "other");
        assert_eq!(counted_reset(Some(DEEP_SLEEP)), None);
        assert_eq!(counted_reset(None), None);
        assert_eq!(counted_reset(Some("ESP_RST_SW")), Some("ESP_RST_SW"));
    }

    #[test]
    fn counted_labels_cover_mapping() {
        let labels: Vec<&str> = counted_reset_labels().collect();
        assert!(labels.contains(&"ESP_RST_SW"));
        assert!(labels.contains(&"ESP_RST_EXT"));
        assert!(labels.contains(&"other"));
        assert!(!labels.contains(&DEEP_SLEEP));
        assert_eq!(labels.len(), RESET_REASONS.len());
    }

    #[test]
    fn labels_sanitise_garbage() {
        assert_eq!(model(Some("x\"\n")), Some("other"));
        assert_eq!(model(Some(&"a".repeat(200))), Some("other"));
        assert_eq!(fw_version(Some("1.2.3\nevil")), Some("other".to_string()));
        assert_eq!(fw_version(Some("1.6.2")), Some("1.6.2".to_string()));
        assert_eq!(fw_version(Some("100.0.0")), Some("other".to_string()));
        assert_eq!(
            model(Some("xiao_epaper_display")),
            Some("xiao_epaper_display")
        );
    }
}
