use crate::{
    auth_types::{AuthError, Provider},
    net,
};
use quota_common::{ProviderQuota, ProviderStatus, QuotaWindow};
use reqwest::Client;
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const BILLING: &str = "https://cli-chat-proxy.grok.com/v1/billing";
const LIMIT: usize = 1024 * 1024;
fn number(value: &Value) -> Option<f64> {
    let value = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))?;
    value.is_finite().then_some(value)
}
fn remaining(value: &Value) -> Result<Option<u8>, AuthError> {
    if value.is_null() {
        return Ok(None);
    }
    let used = number(value)
        .filter(|value| (0.0..=100.0).contains(value))
        .ok_or_else(|| AuthError::new("Provider returned an invalid subscription percentage"))?;
    Ok(Some((100.0 - used).floor() as u8))
}
fn timestamp(value: &Value) -> Result<Option<OffsetDateTime>, AuthError> {
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .ok_or_else(|| AuthError::new("Provider returned an invalid reset timestamp"))?;
    OffsetDateTime::parse(text, &Rfc3339)
        .map(Some)
        .map_err(|_| AuthError::new("Provider returned an invalid reset timestamp"))
}
fn quota(
    source: &str,
    windows: Vec<QuotaWindow>,
    now: OffsetDateTime,
    period_not_started: bool,
) -> ProviderQuota {
    let known = windows
        .iter()
        .any(|window| window.remaining_percent.is_some());
    let stale = period_not_started
        || windows
            .iter()
            .any(|window| window.resets_at.is_some_and(|reset| reset <= now));
    ProviderQuota {
        source: source.into(),
        source_age_seconds: Some(0),
        windows,
        status: if !known {
            ProviderStatus::Unavailable
        } else if stale {
            ProviderStatus::Stale
        } else {
            ProviderStatus::Ok
        },
        message: if !known {
            Some("The provider did not report an included subscription percentage.".into())
        } else if stale {
            Some("The reported billing period is not current; waiting for the provider to update it.".into())
        } else {
            None
        },
    }
}
fn claude(raw: &Value, now: OffsetDateTime) -> Result<ProviderQuota, AuthError> {
    let object = raw
        .as_object()
        .ok_or_else(|| AuthError::new("Invalid Claude usage response"))?;
    let mut windows = Vec::with_capacity(8);
    let mut recognized = false;
    for (key, name) in [
        ("five_hour", "5h"),
        ("seven_day", "7d"),
        ("seven_day_sonnet", "7d Sonnet"),
        ("seven_day_opus", "7d Opus"),
        ("seven_day_oauth_apps", "7d Apps"),
        ("seven_day_cowork", "7d Cowork"),
    ] {
        let Some(value) = object.get(key) else {
            continue;
        };
        recognized = true;
        if value.is_null() {
            continue;
        }
        if !value.is_object() {
            return Err(AuthError::new("Invalid Claude subscription window"));
        }
        windows.push(QuotaWindow {
            name: name.into(),
            remaining_percent: remaining(&value["utilization"])?,
            resets_at: timestamp(&value["resets_at"])?,
        });
    }
    if let Some(limits) = object.get("limits").filter(|value| !value.is_null()) {
        recognized = true;
        let limits = limits
            .as_array()
            .ok_or_else(|| AuthError::new("Invalid Claude limits array"))?;
        for entry in limits {
            let name = match entry["kind"].as_str() {
                Some("session") => "5h".to_owned(),
                Some("weekly_all") => "7d".to_owned(),
                Some("weekly_scoped") => {
                    let model = entry["scope"]["model"]["display_name"]
                        .as_str()
                        .filter(|model| !model.trim().is_empty())
                        .ok_or_else(|| AuthError::new("Claude scoped quota has no model label"))?;
                    let model: String = model
                        .chars()
                        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '.'))
                        .take(24)
                        .collect();
                    if model.is_empty() {
                        return Err(AuthError::new(
                            "Claude scoped quota has an invalid model label",
                        ));
                    }
                    format!("7d {model}")
                }
                _ => continue,
            };
            let window = QuotaWindow {
                name,
                remaining_percent: remaining(&entry["percent"])?,
                resets_at: timestamp(&entry["resets_at"])?,
            };
            // is_active indicates the binding cap, not whether a reported quota exists.
            if let Some(existing) = windows
                .iter_mut()
                .find(|existing| existing.name == window.name)
            {
                if existing.remaining_percent.is_none() {
                    *existing = window;
                }
            } else {
                if windows.len() == 8 {
                    return Err(AuthError::new("Claude returned more subscription windows than the quota contract supports"));
                }
                windows.push(window);
            }
        }
    }
    if !recognized {
        return Err(AuthError {
            status: ProviderStatus::Unavailable,
            message: "Claude usage response contains no supported subscription windows",
            revoked: false,
        });
    }
    // The shared weekly cap also makes an apparently unused session window unavailable.
    // Do not copy the weekly reset into an inactive session whose reset is unknown.
    if windows
        .iter()
        .any(|window| window.name == "7d" && window.remaining_percent == Some(0))
    {
        if let Some(session) = windows.iter_mut().find(|window| window.name == "5h") {
            session.remaining_percent = Some(0);
        }
    }
    Ok(quota("Claude OAuth usage", windows, now, false))
}
struct Period {
    start: OffsetDateTime,
    end: OffsetDateTime,
}
fn period(start: &Value, end: &Value) -> Result<Period, AuthError> {
    let start =
        timestamp(start)?.ok_or_else(|| AuthError::new("Billing period has no start timestamp"))?;
    let end =
        timestamp(end)?.ok_or_else(|| AuthError::new("Billing period has no end timestamp"))?;
    if end <= start {
        return Err(AuthError::new(
            "Provider returned an inverted billing period",
        ));
    }
    Ok(Period { start, end })
}
fn weekly(raw: &Value) -> Result<Option<(QuotaWindow, Period)>, AuthError> {
    let config = &raw["config"];
    if config["currentPeriod"].is_null() {
        return Ok(None);
    }
    let value = &config["currentPeriod"];
    let Some(kind) = value["type"].as_str() else {
        return Err(AuthError::new("Grok billing period has no type"));
    };
    if !kind.to_ascii_uppercase().contains("WEEK") {
        return Ok(None);
    }
    let period = period(&value["start"], &value["end"])?;
    Ok(Some((
        QuotaWindow {
            name: "7d".into(),
            remaining_percent: remaining(&config["creditUsagePercent"])?,
            resets_at: Some(period.end),
        },
        period,
    )))
}
fn monthly(raw: &Value) -> Result<Option<(QuotaWindow, Period)>, AuthError> {
    let config = &raw["config"];
    if config["monthlyLimit"].is_null() {
        return Ok(None);
    }
    let limit = number(&config["monthlyLimit"]["val"])
        .filter(|limit| *limit >= 0.0)
        .ok_or_else(|| AuthError::new("Invalid Grok included quota limit"))?;
    if limit == 0.0 {
        return Ok(None);
    }
    let period = period(&config["billingPeriodStart"], &config["billingPeriodEnd"])?;
    let used = if config["used"].is_null() {
        None
    } else {
        Some(
            number(&config["used"]["val"])
                .filter(|used| *used >= 0.0)
                .ok_or_else(|| AuthError::new("Invalid Grok included quota usage"))?,
        )
    };
    let percent = used.map(|used| ((1.0 - used / limit).clamp(0.0, 1.0) * 100.0).floor() as u8);
    Ok(Some((
        QuotaWindow {
            name: "1mo".into(),
            remaining_percent: percent,
            resets_at: Some(period.end),
        },
        period,
    )))
}
fn grok(
    credits: &Value,
    default: Option<&Value>,
    now: OffsetDateTime,
) -> Result<ProviderQuota, AuthError> {
    let mut windows = Vec::with_capacity(2);
    let mut not_started = false;
    if let Some((window, period)) = weekly(credits)? {
        not_started |= period.start > now;
        windows.push(window);
    }
    if let Some(raw) = default {
        if let Some((window, period)) = monthly(raw)? {
            not_started |= period.start > now;
            windows.push(window);
        }
    }
    Ok(quota("Grok OAuth billing", windows, now, not_started))
}
fn codex_seconds(value: &Value) -> Result<Option<i64>, AuthError> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .map(Some)
        .ok_or_else(|| AuthError::new("Codex returned an invalid quota duration or timestamp"))
}
fn codex_reset(window: &Value, now: OffsetDateTime) -> Result<Option<OffsetDateTime>, AuthError> {
    if let Some(timestamp) = codex_seconds(&window["reset_at"])? {
        return OffsetDateTime::from_unix_timestamp(timestamp)
            .map(Some)
            .map_err(|_| AuthError::new("Codex returned an invalid reset timestamp"));
    }
    if let Some(seconds) = codex_seconds(&window["reset_after_seconds"])? {
        return now
            .checked_add(time::Duration::seconds(seconds))
            .map(Some)
            .ok_or_else(|| AuthError::new("Codex reset timestamp is out of range"));
    }
    Ok(None)
}
fn codex_window(
    raw: &Value,
    key: &str,
    prefix: &str,
    now: OffsetDateTime,
) -> Result<Option<QuotaWindow>, AuthError> {
    if raw.is_null() {
        return Ok(None);
    }
    if !raw.is_object() {
        return Err(AuthError::new("Codex returned an invalid quota window"));
    }
    let duration = codex_seconds(&raw["limit_window_seconds"])?;
    let label = match duration {
        Some(seconds) if seconds > 0 => {
            if seconds % 86400 == 0 {
                format!("{}d", seconds / 86400)
            } else if seconds % 3600 == 0 {
                format!("{}h", seconds / 3600)
            } else if seconds % 60 == 0 {
                format!("{}m", seconds / 60)
            } else {
                format!("{seconds}s")
            }
        }
        Some(_) => return Err(AuthError::new("Codex quota duration must be positive")),
        None => key.into(),
    };
    let name = if prefix.is_empty() {
        label
    } else {
        format!("{prefix} {label}")
    };
    if name.len() > 32 {
        return Err(AuthError::new(
            "Codex quota window label exceeds the contract limit",
        ));
    }
    Ok(Some(QuotaWindow {
        name,
        remaining_percent: remaining(&raw["used_percent"])?,
        resets_at: codex_reset(raw, now)?,
    }))
}
fn codex(raw: &Value, now: OffsetDateTime) -> Result<ProviderQuota, AuthError> {
    if ![
        "plan_type",
        "rate_limit",
        "additional_rate_limits",
        "spend_control",
        "credits",
    ]
    .iter()
    .any(|key| raw.get(*key).is_some())
    {
        return Err(AuthError::new(
            "Codex returned an unrecognized quota schema",
        ));
    }
    let mut windows = Vec::with_capacity(8);
    for (key, label) in [
        ("primary_window", "primary"),
        ("secondary_window", "secondary"),
    ] {
        if let Some(window) = codex_window(&raw["rate_limit"][key], label, "", now)? {
            windows.push(window);
        }
    }
    if let Some(additional) = raw
        .get("additional_rate_limits")
        .filter(|value| !value.is_null())
    {
        let additional = additional
            .as_array()
            .ok_or_else(|| AuthError::new("Invalid Codex additional rate limits"))?;
        for (index, limit) in additional.iter().enumerate() {
            let label = limit["limit_name"]
                .as_str()
                .or_else(|| limit["metered_feature"].as_str());
            let label = label
                .map(|label| {
                    label
                        .chars()
                        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '.'))
                        .take(16)
                        .collect::<String>()
                })
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| format!("feature{}", index + 1));
            for (key, fallback) in [
                ("primary_window", "primary"),
                ("secondary_window", "secondary"),
            ] {
                if let Some(window) =
                    codex_window(&limit["rate_limit"][key], fallback, &label, now)?
                {
                    if windows.len() == 8 {
                        return Err(AuthError::new(
                            "Codex returned more quota windows than the contract supports",
                        ));
                    }
                    windows.push(window);
                }
            }
        }
    }
    let cap = &raw["spend_control"]["individual_limit"];
    if !cap.is_null() {
        if !cap.is_object() {
            return Err(AuthError::new("Invalid Codex per-seat credit cap"));
        }
        let capacity = number(&cap["limit"]).filter(|value| *value > 0.0);
        let used = number(&cap["used"]).filter(|value| *value >= 0.0);
        let percent = if let (Some(capacity), Some(used)) = (capacity, used) {
            Some(((1.0 - used / capacity).clamp(0.0, 1.0) * 100.0).floor() as u8)
        } else if !cap["used_percent"].is_null() {
            remaining(&cap["used_percent"])?
        } else if let (Some(capacity), Some(left)) = (
            capacity,
            number(&cap["remaining"]).filter(|value| *value >= 0.0),
        ) {
            Some((left / capacity * 100.0).clamp(0.0, 100.0).floor() as u8)
        } else {
            None
        };
        if windows.len() == 8 {
            return Err(AuthError::new(
                "Codex returned more quota windows than the contract supports",
            ));
        }
        windows.push(QuotaWindow {
            name: "seat cap".into(),
            remaining_percent: percent,
            resets_at: codex_reset(cap, now)?,
        });
    }
    // Purchased overage-credit balances are not percentages of the included subscription.
    Ok(quota(Provider::Codex.quota_source(), windows, now, false))
}
fn kimi_reset(raw: &Value, now: OffsetDateTime) -> Result<Option<OffsetDateTime>, AuthError> {
    for key in ["reset_time", "resetTime", "reset_at", "resetAt"] {
        let value = &raw[key];
        if value.is_null() {
            continue;
        }
        if value.is_string() {
            return timestamp(value);
        }
        let seconds = value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            .ok_or_else(|| AuthError::new("Kimi returned an invalid reset timestamp"))?;
        let seconds = if seconds > 1_000_000_000_000 {
            seconds / 1000
        } else {
            seconds
        };
        return OffsetDateTime::from_unix_timestamp(seconds)
            .map(Some)
            .map_err(|_| AuthError::new("Kimi reset timestamp is out of range"));
    }
    for key in ["reset_in", "resetIn", "ttl"] {
        let value = &raw[key];
        if value.is_null() {
            continue;
        }
        let seconds = number(value)
            .filter(|seconds| *seconds > 0.0 && *seconds <= i64::MAX as f64)
            .ok_or_else(|| AuthError::new("Kimi returned an invalid reset interval"))?
            as i64;
        return now
            .checked_add(time::Duration::seconds(seconds))
            .map(Some)
            .ok_or_else(|| AuthError::new("Kimi reset timestamp is out of range"));
    }
    Ok(None)
}

fn kimi_percent(raw: &Value) -> Result<Option<u8>, AuthError> {
    if !raw["used_ratio"].is_null() {
        let used = number(&raw["used_ratio"])
            .filter(|used| (0.0..=1.0).contains(used))
            .ok_or_else(|| AuthError::new("Kimi returned an invalid used ratio"))?;
        return Ok(Some(((1.0 - used) * 100.0).floor() as u8));
    }
    let limit = number(&raw["limit"])
        .filter(|limit| *limit >= 0.0)
        .unwrap_or(0.0);
    let remaining = if raw["remaining"].is_null() {
        None
    } else {
        Some(
            number(&raw["remaining"])
                .filter(|remaining| *remaining >= 0.0)
                .ok_or_else(|| AuthError::new("Kimi returned invalid remaining quota"))?,
        )
    };
    let used = if raw["used"].is_null() {
        None
    } else {
        Some(
            number(&raw["used"])
                .filter(|used| *used >= 0.0)
                .ok_or_else(|| AuthError::new("Kimi returned invalid used quota"))?,
        )
    };
    if limit <= 0.0 {
        return Ok(None);
    }
    let fraction = remaining
        .map(|remaining| remaining / limit)
        .or_else(|| used.map(|used| 1.0 - used / limit));
    Ok(fraction.map(|fraction| (fraction.clamp(0.0, 1.0) * 100.0).floor() as u8))
}

fn kimi_duration(raw: &Value) -> Result<Option<String>, AuthError> {
    if raw.is_null() {
        return Ok(None);
    }
    let Some(duration) = number(&raw["duration"]) else {
        return Ok(None);
    };
    let unit = raw["timeUnit"]
        .as_str()
        .ok_or_else(|| AuthError::new("Kimi quota window has no time unit"))?
        .to_ascii_uppercase();
    let multiplier = if unit.contains("MINUTE") {
        60.0
    } else if unit.contains("HOUR") {
        3600.0
    } else if unit.contains("DAY") {
        86400.0
    } else if unit.contains("WEEK") {
        604800.0
    } else if unit.contains("SECOND") {
        1.0
    } else {
        return Err(AuthError::new("Kimi returned an unknown quota time unit"));
    };
    let seconds = duration * multiplier;
    if !seconds.is_finite() || seconds <= 0.0 || seconds > u64::MAX as f64 {
        return Err(AuthError::new("Kimi returned an invalid quota duration"));
    }
    let seconds = seconds as u64;
    let label = if seconds.is_multiple_of(86400) {
        format!("{}d", seconds / 86400)
    } else if seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    };
    Ok(Some(label))
}

fn kimi(raw: &Value, now: OffsetDateTime) -> Result<ProviderQuota, AuthError> {
    if !raw.is_object() {
        return Err(AuthError::new("Invalid Kimi usage response"));
    }
    fn push(windows: &mut Vec<QuotaWindow>, window: QuotaWindow) -> Result<(), AuthError> {
        if let Some(existing) = windows
            .iter_mut()
            .find(|existing| existing.name == window.name)
        {
            if existing.remaining_percent.is_none() {
                *existing = window;
            }
        } else {
            if windows.len() == 8 {
                return Err(AuthError::new(
                    "Kimi returned more quota windows than the contract supports",
                ));
            }
            windows.push(window);
        }
        Ok(())
    }
    let recognized = ["usage", "limits", "usages", "totalQuota"]
        .iter()
        .any(|key| !raw[*key].is_null());
    if !recognized {
        return Err(AuthError::new(
            "Kimi usage response contains no supported subscription windows",
        ));
    }
    let mut windows = Vec::with_capacity(4);
    for (key, name) in [("usage", "7d"), ("totalQuota", "1mo total")] {
        let value = &raw[key];
        if value.is_object() {
            push(
                &mut windows,
                QuotaWindow {
                    name: name.into(),
                    remaining_percent: kimi_percent(value)?,
                    resets_at: kimi_reset(value, now)?,
                },
            )?;
        }
    }
    let mut has_5h = false;
    if let Some(limits) = raw["limits"].as_array() {
        for (index, entry) in limits.iter().enumerate() {
            let detail = if entry["detail"].is_object() {
                &entry["detail"]
            } else {
                entry
            };
            let name =
                kimi_duration(&entry["window"])?.unwrap_or_else(|| format!("limit {}", index + 1));
            has_5h |= name == "5h";
            push(
                &mut windows,
                QuotaWindow {
                    name,
                    remaining_percent: kimi_percent(detail)?,
                    resets_at: kimi_reset(detail, now)?,
                },
            )?;
        }
    }
    for (key, name) in [
        ("limit_5h", "5h"),
        ("limit_7d", "7d"),
        ("limit_month_total", "1mo total"),
        ("limit_month_code", "1mo code"),
    ] {
        let value = &raw["usages"][key];
        if value.is_object() && (key != "limit_5h" || !has_5h) {
            push(
                &mut windows,
                QuotaWindow {
                    name: name.into(),
                    remaining_percent: kimi_percent(value)?,
                    resets_at: kimi_reset(value, now)?,
                },
            )?;
        }
    }
    Ok(quota(Provider::Kimi.quota_source(), windows, now, false))
}

async fn get(
    client: &Client,
    url: &str,
    provider: Provider,
    access: &str,
) -> Result<Value, AuthError> {
    let mut request = client
        .get(url)
        .bearer_auth(access)
        .header("accept", "application/json");
    request = match provider {
        Provider::Claude => request.header("anthropic-beta", "oauth-2025-04-20"),
        Provider::Codex => {
            let account = crate::oauth::codex_account_id(access)?;
            let mut header = reqwest::header::HeaderValue::from_str(&account)
                .map_err(|_| AuthError::new("Invalid Codex ChatGPT account header"))?;
            header.set_sensitive(true);
            request.header("ChatGPT-Account-Id", header)
        }
        Provider::Grok => request.header("X-XAI-Token-Auth", "xai-grok-cli"),
        Provider::Kimi => request,
    };
    let response = request
        .send()
        .await
        .map_err(|_| AuthError::new("Cannot reach the subscription quota endpoint"))?;
    match response.status().as_u16() {
        401 => {
            return Err(AuthError {
                status: ProviderStatus::AuthRequired,
                message: "Subscription endpoint rejected the access token",
                revoked: false,
            })
        }
        403 => {
            return Err(AuthError {
                status: ProviderStatus::Unavailable,
                message:
                    "Signed-in account is not permitted to read this subscription quota endpoint",
                revoked: false,
            })
        }
        429 => return Err(AuthError::new(
            "Subscription quota endpoint is rate limited; waiting for the next scheduled refresh",
        )),
        _ => {}
    }
    if !response.status().is_success() {
        return Err(AuthError::new(
            "Subscription quota endpoint returned an HTTP error",
        ));
    }
    let (_, body): (_, Value) = net::json(response, LIMIT).await?;
    if !body.is_object() {
        return Err(AuthError::new(
            "Subscription endpoint returned an invalid quota schema",
        ));
    }
    Ok(body)
}
pub async fn fetch(
    client: &Client,
    provider: Provider,
    access_token: &str,
) -> Result<ProviderQuota, AuthError> {
    match provider {
        Provider::Claude => claude(
            &get(
                client,
                "https://api.anthropic.com/api/oauth/usage",
                provider,
                access_token,
            )
            .await?,
            OffsetDateTime::now_utc(),
        ),
        Provider::Codex => codex(
            &get(
                client,
                "https://chatgpt.com/backend-api/wham/usage",
                provider,
                access_token,
            )
            .await?,
            OffsetDateTime::now_utc(),
        ),
        Provider::Grok => {
            let credits = get(
                client,
                &format!("{BILLING}?format=credits"),
                provider,
                access_token,
            )
            .await?;
            let weekly = weekly(&credits)?;
            let needs_default = credits["config"]["isUnifiedBillingUser"].as_bool() == Some(true)
                || weekly
                    .as_ref()
                    .is_none_or(|(window, _)| window.remaining_percent.is_none());
            let default = if needs_default {
                Some(get(client, BILLING, provider, access_token).await?)
            } else {
                None
            };
            grok(&credits, default.as_ref(), OffsetDateTime::now_utc())
        }
        Provider::Kimi => kimi(
            &get(
                client,
                "https://api.kimi.com/coding/v1/usages",
                provider,
                access_token,
            )
            .await?,
            OffsetDateTime::now_utc(),
        ),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn now() -> OffsetDateTime {
        OffsetDateTime::parse("2026-10-28T12:00:00Z", &Rfc3339).unwrap()
    }
    #[test]
    fn codex_windows_preserve_real_duration_zero_unknown_and_expired_resets() {
        let raw = json!({"rate_limit":{
            "primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_at":now().unix_timestamp()-10},
            "secondary_window":{"limit_window_seconds":604800,"reset_after_seconds":3600}
        },"additional_rate_limits":[{"limit_name":"Spark","rate_limit":{"primary_window":{"used_percent":12.3,"limit_window_seconds":7200}}}],
        "credits":{"has_credits":true,"balance":300}});
        let parsed = codex(&raw, now()).unwrap();
        assert_eq!(parsed.windows[0].name, "5h");
        assert_eq!(parsed.windows[0].remaining_percent, Some(0));
        assert_eq!(parsed.windows[1].name, "7d");
        assert_eq!(parsed.windows[1].remaining_percent, None);
        assert_eq!(
            parsed.windows[1].resets_at,
            Some(now() + time::Duration::hours(1))
        );
        assert_eq!(parsed.windows[2].name, "Spark 2h");
        assert_eq!(parsed.windows[2].remaining_percent, Some(87));
        assert_eq!(parsed.status, ProviderStatus::Stale);
        let unknown = codex(&json!({"credits":{"unlimited":true,"balance":100}}), now()).unwrap();
        assert_eq!(unknown.status, ProviderStatus::Unavailable);
        assert!(unknown.windows.is_empty());
    }
    #[test]
    fn claude_used_percent_is_inverted_unknown_is_not_zero_and_past_reset_stays_stale() {
        let raw = json!({"five_hour":{"utilization":27.3,"resets_at":"2026-10-28T14:00:00Z"},"seven_day":{"utilization":100,"resets_at":"2026-10-27T12:00:00Z"},"seven_day_sonnet":{"utilization":null,"resets_at":null},"extra_usage":{"utilization":50,"monthly_limit":100}});
        let parsed = claude(&raw, now()).unwrap();
        assert_eq!(parsed.windows[0].remaining_percent, Some(0));
        assert_eq!(parsed.windows[1].remaining_percent, Some(0));
        assert_eq!(
            parsed.windows[0].resets_at,
            Some(now() + time::Duration::hours(2))
        );
        assert_eq!(parsed.windows[2].remaining_percent, None);
        assert_eq!(parsed.status, ProviderStatus::Stale);
        assert_eq!(parsed.windows.len(), 3);
        assert!(claude(&json!({"five_hour":{"utilization":101}}), now()).is_err());
        assert!(claude(&json!({"unexpected":{"utilization":70}}), now()).is_err());
    }
    #[test]
    fn claude_generic_limits_preserve_inactive_model_caps_and_legacy_shared_precedence() {
        let raw = json!({"five_hour":{"utilization":25},"limits":[
            {"kind":"session","percent":50},{"kind":"weekly_all","percent":15},
            {"kind":"weekly_scoped","percent":30,"is_active":false,"scope":{"model":{"display_name":"Sonnet"}}}
        ]});
        let parsed = claude(&raw, now()).unwrap();
        assert_eq!(parsed.windows[0].remaining_percent, Some(75));
        assert_eq!(parsed.windows[1].name, "7d");
        assert_eq!(parsed.windows[1].remaining_percent, Some(85));
        assert_eq!(parsed.windows[2].name, "7d Sonnet");
        assert_eq!(parsed.windows[2].remaining_percent, Some(70));
    }
    #[test]
    fn kimi_preserves_detail_reset_and_supports_ratio_only_plans() {
        let raw = json!({
            "usage":{"limit":"100","used":"20","remaining":"80","resetTime":"2026-11-04T12:00:00Z"},
            "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"limit":"100","used":"100","resetTime":"2026-10-28T17:45:55.083587Z"}}],
            "usages":{"limit_month_total":{"used_ratio":0.25,"reset_time":"2026-11-01T00:00:00Z"}}
        });
        let parsed = kimi(&raw, now()).unwrap();
        assert_eq!(parsed.windows[0].name, "7d");
        assert_eq!(parsed.windows[0].remaining_percent, Some(80));
        assert_eq!(parsed.windows[1].name, "5h");
        assert_eq!(parsed.windows[1].remaining_percent, Some(0));
        assert_eq!(
            parsed.windows[1].resets_at,
            OffsetDateTime::parse("2026-10-28T17:45:55.083587Z", &Rfc3339).ok()
        );
        assert_eq!(parsed.windows[2].name, "1mo total");
        assert_eq!(parsed.windows[2].remaining_percent, Some(75));

        let ratio_only = kimi(
            &json!({"usages":{"limit_5h":{"used_ratio":0.2,"reset_time":"2026-10-28T13:00:00Z"}}}),
            now(),
        )
        .unwrap();
        assert_eq!(ratio_only.windows.len(), 1);
        assert_eq!(ratio_only.windows[0].name, "5h");
        assert_eq!(ratio_only.windows[0].remaining_percent, Some(80));
        assert!(kimi(&json!({"unexpected":{}}), now()).is_err());
    }
    #[test]
    fn grok_missing_usage_never_becomes_full_and_monthly_is_not_weekly_or_on_demand() {
        let credits = json!({"config":{"currentPeriod":{"start":"2026-10-26T00:00:00Z","end":"2026-11-02T00:00:00Z","type":"WEEKLY"},"isUnifiedBillingUser":true}});
        let parsed = grok(&credits, None, now()).unwrap();
        assert_eq!(parsed.status, ProviderStatus::Unavailable);
        assert_eq!(parsed.windows[0].remaining_percent, None);
        let default = json!({"config":{"billingPeriodStart":"2026-10-01T00:00:00Z","billingPeriodEnd":"2026-11-01T00:00:00Z","monthlyLimit":{"val":"1000"},"used":{"val":"120"},"onDemandCap":{"val":100},"onDemandUsed":{"val":100}}});
        let parsed = grok(&credits, Some(&default), now()).unwrap();
        assert_eq!(parsed.windows[1].name, "1mo");
        assert_eq!(parsed.windows[1].remaining_percent, Some(88));
        assert_eq!(parsed.status, ProviderStatus::Ok);
        let mut over = default.clone();
        over["config"]["used"]["val"] = json!(2000);
        assert_eq!(
            grok(&credits, Some(&over), now()).unwrap().windows[1].remaining_percent,
            Some(0)
        );
        let mut zero = default.clone();
        zero["config"]["monthlyLimit"]["val"] = json!(0);
        assert_eq!(grok(&credits, Some(&zero), now()).unwrap().windows.len(), 1);
        let mut missing = default;
        missing["config"].as_object_mut().unwrap().remove("used");
        assert_eq!(
            grok(&credits, Some(&missing), now()).unwrap().windows[1].remaining_percent,
            None
        );
    }
}
