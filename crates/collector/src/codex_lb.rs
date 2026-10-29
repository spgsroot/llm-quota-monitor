use std::{env, time::Duration};

use quota_common::{ProviderQuota, ProviderStatus, QuotaWindow, STALE_AFTER_SECONDS};
use reqwest::{
    header::{HeaderMap, HeaderValue, COOKIE},
    Client, Url,
};
use serde::{de::DeserializeOwned, Deserialize};
use time::OffsetDateTime;

const MAX_SOURCE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct SourceError {
    pub status: ProviderStatus,
    pub message: &'static str,
}

impl SourceError {
    fn invalid(message: &'static str) -> Self {
        Self {
            status: ProviderStatus::Error,
            message,
        }
    }
}

pub struct CodexLb {
    base: Url,
    client: Client,
}

impl CodexLb {
    pub fn dashboard_url(&self) -> &str {
        self.base.as_str()
    }

    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let base = Url::parse(
            &env::var("CODEX_LB_URL").unwrap_or_else(|_| "http://127.0.0.1:2455".into()),
        )
        .map_err(|_| "Invalid CODEX_LB_URL")?;
        let loopback = base.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if base.scheme() != "http"
            || !loopback
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path() != "/"
        {
            return Err("CODEX_LB_URL must be an HTTP loopback origin without credentials, path, query or fragment".into());
        }
        let mut headers = HeaderMap::new();
        if let Ok(cookie) = env::var("CODEX_LB_COOKIE") {
            let mut cookie =
                HeaderValue::from_str(&cookie).map_err(|_| "Invalid CODEX_LB_COOKIE")?;
            cookie.set_sensitive(true);
            headers.insert(COOKIE, cookie);
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .default_headers(headers)
            .build()?;
        Ok(Self { base, client })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, SourceError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| SourceError::invalid("Invalid codex-lb endpoint"))?;
        let mut response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|_| SourceError::invalid("Cannot reach codex-lb"))?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(SourceError {
                status: ProviderStatus::AuthRequired,
                message: "codex-lb dashboard authentication required",
            });
        }
        if !response.status().is_success() {
            return Err(SourceError::invalid(
                "codex-lb quota endpoint returned an HTTP error",
            ));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_SOURCE_BYTES as u64)
        {
            return Err(SourceError::invalid("codex-lb response is too large"));
        }
        let mut body = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(8192)
                .min(MAX_SOURCE_BYTES as u64) as usize,
        );
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| SourceError::invalid("Cannot read codex-lb response"))?
        {
            if body.len() + chunk.len() > MAX_SOURCE_BYTES {
                return Err(SourceError::invalid("codex-lb response is too large"));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map_err(|_| SourceError::invalid("Invalid codex-lb quota JSON"))
    }

    pub async fn collect(&self) -> Result<ProviderQuota, SourceError> {
        let summary: UsageSummary = self.get("/api/usage/summary").await?;
        let overview: Freshness = self.get("/api/dashboard/overview?timeframe=1d").await?;
        normalize(summary, overview.last_sync_at, OffsetDateTime::now_utc())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageSummary {
    primary_window: UsageWindow,
    secondary_window: Option<UsageWindow>,
    monthly_window: Option<UsageWindow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageWindow {
    remaining_percent: f64,
    capacity_credits: f64,
    #[serde(default, with = "time::serde::rfc3339::option")]
    reset_at: Option<OffsetDateTime>,
    window_minutes: Option<u64>,
}

#[derive(Deserialize)]
struct Freshness {
    #[serde(default, rename = "lastSyncAt", with = "time::serde::rfc3339::option")]
    last_sync_at: Option<OffsetDateTime>,
}

fn normalize(
    summary: UsageSummary,
    last_sync_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Result<ProviderQuota, SourceError> {
    let source_age_seconds = last_sync_at
        .map(|sync| {
            let seconds = (now - sync).whole_seconds();
            if seconds < -5 {
                return Err(SourceError::invalid(
                    "codex-lb source timestamp is in the future",
                ));
            }
            Ok(seconds.max(0) as u64)
        })
        .transpose()?;
    let mut windows = Vec::with_capacity(3);
    for (fallback, window) in [
        ("primary", Some(summary.primary_window)),
        ("secondary", summary.secondary_window),
        ("monthly", summary.monthly_window),
    ] {
        let Some(window) = window else {
            continue;
        };
        if !window.remaining_percent.is_finite()
            || !(0.0..=100.0).contains(&window.remaining_percent)
            || !window.capacity_credits.is_finite()
            || window.capacity_credits < 0.0
        {
            return Err(SourceError::invalid(
                "codex-lb reported invalid quota values",
            ));
        }
        let name = match window.window_minutes {
            Some(0) => {
                return Err(SourceError::invalid(
                    "codex-lb reported an invalid window duration",
                ))
            }
            Some(minutes) if minutes % 1440 == 0 => format!("{}d", minutes / 1440),
            Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
            Some(minutes) => format!("{minutes}m"),
            None => fallback.to_owned(),
        };
        windows.push(QuotaWindow {
            name,
            remaining_percent: (window.capacity_credits > 0.0)
                .then_some(window.remaining_percent.floor() as u8),
            resets_at: window.reset_at,
        });
    }
    let known = windows
        .iter()
        .any(|window| window.remaining_percent.is_some());
    let reset_passed = windows.iter().any(|window| {
        window.remaining_percent.is_some() && window.resets_at.is_some_and(|reset| reset <= now)
    });
    let stale = source_age_seconds.is_none_or(|age| age >= STALE_AFTER_SECONDS) || reset_passed;
    let message = if !known {
        "codex-lb pool has no known quota capacity"
    } else if reset_passed {
        "codex-lb pool totals; reset timestamp has passed. Awaiting source update."
    } else if source_age_seconds.is_none() {
        "codex-lb pool totals; source freshness is unknown."
    } else if stale {
        "codex-lb pool totals; source data is older than 15 minutes."
    } else {
        "codex-lb pool totals; remaining percentages are rounded down."
    };
    Ok(ProviderQuota {
        status: if !known {
            ProviderStatus::Unavailable
        } else if stale {
            ProviderStatus::Stale
        } else {
            ProviderStatus::Ok
        },
        source: "codex-lb pool".into(),
        source_age_seconds,
        message: Some(message.into()),
        windows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }
    fn window(percent: f64, capacity: f64, minutes: Option<u64>) -> UsageWindow {
        UsageWindow {
            remaining_percent: percent,
            capacity_credits: capacity,
            reset_at: Some(now() + time::Duration::hours(1)),
            window_minutes: minutes,
        }
    }
    fn summary() -> UsageSummary {
        UsageSummary {
            primary_window: window(97.99, 225.0, Some(300)),
            secondary_window: Some(window(95.04, 8694.0, Some(10080))),
            monthly_window: Some(window(0.0, 1134.0, Some(43200))),
        }
    }
    #[test]
    fn uses_pool_percentages_and_actual_durations_without_averaging_accounts() {
        let quota = normalize(summary(), Some(now()), now()).unwrap();
        assert_eq!(quota.status, ProviderStatus::Ok);
        assert_eq!(
            quota
                .windows
                .iter()
                .map(|w| (w.name.as_str(), w.remaining_percent))
                .collect::<Vec<_>>(),
            vec![("5h", Some(97)), ("7d", Some(95)), ("30d", Some(0))]
        );
    }
    #[test]
    fn zero_capacity_is_unknown_but_zero_remaining_is_real() {
        let mut input = summary();
        input.primary_window.capacity_credits = 0.0;
        let quota = normalize(input, Some(now()), now()).unwrap();
        assert_eq!(quota.windows[0].remaining_percent, None);
        assert_eq!(quota.windows[2].remaining_percent, Some(0));
    }
    #[test]
    fn expired_reset_is_not_rewritten_or_treated_as_fresh() {
        let mut input = summary();
        let reset = now() - time::Duration::days(30);
        input.primary_window.reset_at = Some(reset);
        let quota = normalize(input, Some(now()), now()).unwrap();
        assert_eq!(quota.status, ProviderStatus::Stale);
        assert_eq!(quota.windows[0].resets_at, Some(reset));
        assert_eq!(quota.windows[0].remaining_percent, Some(97));
    }
    #[test]
    fn rejects_invalid_percent_capacity_timestamp_and_duration() {
        for percent in [-1.0, 101.0, f64::NAN] {
            let mut input = summary();
            input.primary_window.remaining_percent = percent;
            assert!(normalize(input, Some(now()), now()).is_err());
        }
        let mut input = summary();
        input.primary_window.capacity_credits = -1.0;
        assert!(normalize(input, Some(now()), now()).is_err());
        let mut input = summary();
        input.primary_window.window_minutes = Some(0);
        assert!(normalize(input, Some(now()), now()).is_err());
        assert!(normalize(summary(), Some(now() + time::Duration::hours(1)), now()).is_err());
    }
    #[test]
    fn missing_freshness_is_stale_and_missing_duration_is_not_invented() {
        let mut input = summary();
        input.primary_window.window_minutes = None;
        input.secondary_window = None;
        input.monthly_window = None;
        let quota = normalize(input, None, now()).unwrap();
        assert_eq!(quota.status, ProviderStatus::Stale);
        assert_eq!(quota.windows.len(), 1);
        assert_eq!(quota.windows[0].name, "primary");
        assert_eq!(quota.source_age_seconds, None);
    }
}
