use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const POLL_SECONDS: u64 = 30;
pub const STALE_AFTER_SECONDS: u64 = 900;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuotaSnapshot {
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Age of the provider cache at response time, not time since the last HTTP request.
    pub age_seconds: u64,
    pub stale_after_seconds: u64,
    pub providers: BTreeMap<String, ProviderQuota>,
}

impl QuotaSnapshot {
    pub fn is_stale_after(&self, elapsed_seconds: u64) -> bool {
        let elapsed = self.age_seconds.saturating_add(elapsed_seconds);
        elapsed >= self.stale_after_seconds
            || self
                .providers
                .values()
                .any(|provider| provider.is_stale_after(elapsed, self.stale_after_seconds))
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.stale_after_seconds == 0 {
            return Err("stale_after_seconds must be positive");
        }
        if self.providers.len() > 8 {
            return Err("too many providers");
        }
        for (name, provider) in &self.providers {
            if name.is_empty() || name.len() > 32 {
                return Err("invalid provider name");
            }
            if provider.source.is_empty() || provider.source.len() > 64 {
                return Err("invalid quota source");
            }
            if provider
                .message
                .as_ref()
                .is_some_and(|message| message.len() > 256)
            {
                return Err("quota message is too long");
            }
            if provider.windows.len() > 8 {
                return Err("too many quota windows");
            }
            for window in &provider.windows {
                if window.name.is_empty() || window.name.len() > 32 {
                    return Err("invalid window name");
                }
                if window.remaining_percent.is_some_and(|value| value > 100) {
                    return Err("remaining_percent must be between 0 and 100");
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderQuota {
    pub status: ProviderStatus,
    pub source: String,
    /// Age of upstream quota data at snapshot creation, not at the latest HTTP poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_age_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub windows: Vec<QuotaWindow>,
}

impl ProviderQuota {
    pub fn is_stale_after(&self, elapsed_seconds: u64, stale_after_seconds: u64) -> bool {
        self.status == ProviderStatus::Stale
            || (matches!(
                self.status,
                ProviderStatus::Error | ProviderStatus::AuthRequired | ProviderStatus::Unavailable
            ) && self
                .windows
                .iter()
                .any(|window| window.remaining_percent.is_some()))
            || self
                .source_age_seconds
                .is_some_and(|age| age.saturating_add(elapsed_seconds) >= stale_after_seconds)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Ok,
    Stale,
    Unavailable,
    AuthRequired,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuotaWindow {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<u8>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub resets_at: Option<OffsetDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> QuotaSnapshot {
        serde_json::from_str(
            r#"{
            "updated_at":"2026-10-28T11:40:00Z",
            "age_seconds":600, "stale_after_seconds":900,
            "providers":{
                "claude":{"status":"ok","source":"fixture","source_age_seconds":0,"windows":[
                    {"name":"5h","remaining_percent":72,"resets_at":"2026-10-28T14:00:00Z"},
                    {"name":"7d"}
                ]},
                "grok":{"status":"unavailable","source":"unconfigured","windows":[]}
            }
        }"#,
        )
        .unwrap()
    }

    #[test]
    fn cached_age_survives_a_successful_http_poll() {
        let data = snapshot();
        assert!(!data.is_stale_after(299));
        assert!(data.is_stale_after(300));
        assert!(data.is_stale_after(u64::MAX));
    }

    #[test]
    fn source_age_is_not_reset_by_collector_refresh() {
        let mut data = snapshot();
        data.age_seconds = 0;
        data.providers.get_mut("claude").unwrap().source_age_seconds = Some(899);
        assert!(!data.is_stale_after(0));
        assert!(data.is_stale_after(1));
        data.providers.get_mut("claude").unwrap().source_age_seconds = Some(0);
        data.providers.get_mut("claude").unwrap().status = ProviderStatus::Stale;
        assert!(data.is_stale_after(0));
        data.providers.get_mut("claude").unwrap().status = ProviderStatus::Error;
        assert!(data.is_stale_after(0));
    }

    #[test]
    fn unknown_percent_is_not_zero_and_optional_reset_is_preserved() {
        let data = snapshot();
        data.validate().unwrap();
        let windows = &data.providers["claude"].windows;
        assert_eq!(windows[0].remaining_percent, Some(72));
        assert_eq!(windows[0].resets_at.unwrap().hour(), 14);
        assert_eq!(windows[1].remaining_percent, None);
        assert_eq!(windows[1].resets_at, None);
        assert_eq!(data.providers["grok"].status, ProviderStatus::Unavailable);
        let wire = serde_json::to_value(&data).unwrap();
        assert!(wire["providers"]["claude"]["windows"][1]
            .get("remaining_percent")
            .is_none());
    }

    #[test]
    fn rejects_out_of_range_quota_and_zero_freshness_window() {
        let mut data = snapshot();
        data.providers.get_mut("claude").unwrap().windows[0].remaining_percent = Some(101);
        assert_eq!(
            data.validate(),
            Err("remaining_percent must be between 0 and 100")
        );
        data.providers.get_mut("claude").unwrap().windows[0].remaining_percent = Some(100);
        data.stale_after_seconds = 0;
        assert_eq!(data.validate(), Err("stale_after_seconds must be positive"));
    }

    #[test]
    fn rejects_invalid_timestamp_instead_of_making_it_fresh() {
        let data = serde_json::to_string(&snapshot()).unwrap();
        let invalid = data.replace("2026-10-28T11:40:00Z", "not-a-timestamp");
        assert!(serde_json::from_str::<QuotaSnapshot>(&invalid).is_err());
    }
}
