use crate::state::MonitorState;
use core::fmt::{self, Write};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    text::Text,
};
use quota_common::{ProviderQuota, ProviderStatus, QuotaWindow};
use std::time::Instant;

fn provider_label(key: &str) -> &str {
    match key {
        "claude" => "Claude",
        "codex" => "Codex",
        "grok" => "Grok",
        "kimi" => "Kimi",
        other => other,
    }
}

/// None keeps the overview visible; a future encoder can select a provider and window page.
#[derive(Clone, Copy, Default)]
pub struct Selection {
    pub provider: Option<usize>,
    pub window_page: usize,
}

struct LineText {
    text: heapless::String<26>,
    columns: usize,
}

impl fmt::Write for LineText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for ch in text.chars() {
            if self.text.len() >= self.columns {
                break;
            }
            let _ = self.text.push(if ch.is_ascii() { ch } else { '?' });
        }
        Ok(())
    }
}

fn line<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    x: i32,
    y: i32,
    columns: usize,
    color: Rgb565,
    args: fmt::Arguments<'_>,
) -> Result<(), D::Error> {
    let mut text = LineText {
        text: heapless::String::new(),
        columns: columns.min(26),
    };
    let _ = text.write_fmt(args);
    Text::new(
        &text.text,
        Point::new(x, y),
        MonoTextStyle::new(&FONT_6X10, color),
    )
    .draw(display)?;
    Ok(())
}

fn status(provider: &ProviderQuota, state: &MonitorState, now: Instant) -> &'static str {
    match provider.status {
        ProviderStatus::Unavailable => "N/A",
        ProviderStatus::AuthRequired => "AUTH",
        ProviderStatus::Error => "ERROR",
        ProviderStatus::Stale => "STALE",
        ProviderStatus::Ok
            if provider.is_stale_after(
                state.age_seconds(now),
                state.snapshot.as_ref().unwrap().stale_after_seconds,
            ) =>
        {
            "STALE"
        }
        ProviderStatus::Ok => "OK",
    }
}

fn amount<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    x: i32,
    y: i32,
    window: &QuotaWindow,
    color: Rgb565,
) -> Result<(), D::Error> {
    match window.remaining_percent {
        Some(percent) => line(
            display,
            x,
            y,
            8,
            color,
            format_args!("{:.3} {}%", window.name, percent),
        ),
        None => line(
            display,
            x,
            y,
            8,
            color,
            format_args!("{:.3} --", window.name),
        ),
    }
}

fn reset_summary<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    y: i32,
    provider: &ProviderQuota,
    status: &str,
    color: Rgb565,
) -> Result<(), D::Error> {
    let mut text = LineText {
        text: heapless::String::new(),
        columns: 26,
    };
    let _ = write!(text, "{status}");
    for window in provider.windows.iter().take(2) {
        if let Some(reset) = window.resets_at {
            let reset = reset.to_offset(time::UtcOffset::UTC);
            let _ = write!(text, " {:02}:{:02}Z", reset.hour(), reset.minute());
        } else {
            let _ = write!(text, " --:--");
        }
    }
    if provider.windows.len() > 2 {
        let _ = write!(text, " +{}", provider.windows.len() - 2);
    }
    Text::new(
        &text.text,
        Point::new(2, y),
        MonoTextStyle::new(&FONT_6X10, color),
    )
    .draw(display)?;
    Ok(())
}

fn overview<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    state: &MonitorState,
    now: Instant,
) -> Result<(), D::Error> {
    let Some(snapshot) = state.snapshot.as_ref() else {
        line(
            display,
            2,
            32,
            26,
            Rgb565::YELLOW,
            format_args!("NO QUOTA DATA"),
        )?;
        return Ok(());
    };
    let more = snapshot.providers.len().saturating_sub(4);
    line(
        display,
        2,
        12,
        26,
        Rgb565::WHITE,
        format_args!("LLM QUOTAS"),
    )?;
    if more > 0 {
        line(display, 80, 12, 5, Rgb565::WHITE, format_args!("+{more}"))?;
    }
    if snapshot.providers.is_empty() {
        line(
            display,
            2,
            32,
            26,
            Rgb565::YELLOW,
            format_args!("NO PROVIDERS"),
        )?;
    }
    for (row, (key, provider)) in snapshot.providers.iter().take(4).enumerate() {
        let y = 26 + row as i32 * 24;
        line(
            display,
            2,
            y,
            8,
            Rgb565::WHITE,
            format_args!("{}", provider_label(key)),
        )?;
        let status = status(provider, state, now);
        let color = if status == "OK" {
            Rgb565::GREEN
        } else {
            Rgb565::YELLOW
        };
        for (column, window) in provider.windows.iter().take(2).enumerate() {
            amount(display, 52 + column as i32 * 54, y, window, color)?;
        }
        if provider.windows.is_empty() {
            line(display, 52, y, 8, color, format_args!("--"))?;
        }
        reset_summary(display, y + 12, provider, status, color)?;
    }
    Ok(())
}

fn details<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    state: &MonitorState,
    selection: Selection,
    now: Instant,
) -> Result<(), D::Error> {
    let Some(snapshot) = state.snapshot.as_ref() else {
        return overview(display, state, now);
    };
    let Some((key, provider)) = selection
        .provider
        .and_then(|index| snapshot.providers.iter().nth(index))
    else {
        return overview(display, state, now);
    };
    let label = provider_label(key);
    let pages = provider.windows.len().max(1).div_ceil(2);
    let page = selection.window_page.min(pages - 1);
    let label_status = status(provider, state, now);
    let color = if label_status == "OK" {
        Rgb565::GREEN
    } else {
        Rgb565::YELLOW
    };
    line(
        display,
        2,
        12,
        26,
        Rgb565::WHITE,
        format_args!("{label} {label_status} {}/{}", page + 1, pages),
    )?;
    line(
        display,
        2,
        26,
        26,
        Rgb565::WHITE,
        format_args!("{}", provider.source),
    )?;
    if provider.windows.is_empty() {
        line(display, 2, 53, 26, color, format_args!("No quota data"))?;
    }
    for (row, window) in provider.windows.iter().skip(page * 2).take(2).enumerate() {
        let y = 46 + row as i32 * 29;
        match window.remaining_percent {
            Some(percent) => line(
                display,
                2,
                y,
                26,
                color,
                format_args!("{} {}%", window.name, percent),
            )?,
            None => line(display, 2, y, 26, color, format_args!("{} --", window.name))?,
        }
        if let Some(reset) = window.resets_at {
            let reset = reset.to_offset(time::UtcOffset::UTC);
            line(
                display,
                2,
                y + 12,
                26,
                Rgb565::WHITE,
                format_args!(
                    "RST {:02}-{:02} {:02}:{:02}Z",
                    u8::from(reset.month()),
                    reset.day(),
                    reset.hour(),
                    reset.minute()
                ),
            )?;
        } else {
            line(
                display,
                2,
                y + 12,
                26,
                Rgb565::WHITE,
                format_args!("RST unknown"),
            )?;
        }
    }
    if let Some(message) = &provider.message {
        line(
            display,
            2,
            105,
            26,
            Rgb565::YELLOW,
            format_args!("{message}"),
        )?;
    }
    Ok(())
}

pub fn draw<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    state: &MonitorState,
    selection: Selection,
    now: Instant,
) -> Result<(), D::Error> {
    display.clear(Rgb565::BLACK)?;
    if state.config_required {
        line(
            display,
            2,
            16,
            26,
            Rgb565::WHITE,
            format_args!("LLM QUOTAS"),
        )?;
        line(
            display,
            2,
            45,
            26,
            Rgb565::YELLOW,
            format_args!("Set WIFI_SSID and"),
        )?;
        line(
            display,
            2,
            60,
            26,
            Rgb565::WHITE,
            format_args!("COLLECTOR_URL"),
        )?;
        line(
            display,
            2,
            75,
            26,
            Rgb565::WHITE,
            format_args!("then rebuild"),
        )?;
    } else if selection.provider.is_some() {
        details(display, state, selection, now)?;
    } else {
        overview(display, state, now)?;
    }
    line(
        display,
        2,
        123,
        26,
        if state.fault_led_on() || state.stale(now) {
            Rgb565::YELLOW
        } else {
            Rgb565::GREEN
        },
        format_args!("{}", state.status(now)),
    )?;
    Ok(())
}
