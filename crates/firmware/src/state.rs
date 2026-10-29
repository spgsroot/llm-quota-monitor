use std::time::Instant;

use quota_common::QuotaSnapshot;

#[derive(Default)]
pub struct MonitorState {
    pub snapshot: Option<QuotaSnapshot>,
    pub wifi_up: bool,
    pub collector_up: bool,
    pub config_required: bool,
    received_at: Option<Instant>,
}

impl MonitorState {
    pub fn set_wifi(&mut self, up: bool) {
        self.wifi_up = up;
        if !up {
            self.collector_up = false;
        }
    }

    pub fn accept(&mut self, snapshot: QuotaSnapshot, now: Instant) -> Result<(), &'static str> {
        snapshot.validate()?;
        self.snapshot = Some(snapshot);
        self.received_at = Some(now);
        self.collector_up = true;
        Ok(())
    }

    pub fn fault_led_on(&self) -> bool {
        !self.wifi_up || !self.collector_up
    }

    pub fn stale(&self, now: Instant) -> bool {
        self.snapshot
            .as_ref()
            .zip(self.received_at)
            .is_some_and(|(snapshot, received_at)| {
                snapshot.is_stale_after(now.saturating_duration_since(received_at).as_secs())
            })
    }

    pub fn age_seconds(&self, now: Instant) -> u64 {
        self.snapshot
            .as_ref()
            .zip(self.received_at)
            .map_or(0, |(snapshot, received_at)| {
                snapshot
                    .age_seconds
                    .saturating_add(now.saturating_duration_since(received_at).as_secs())
            })
    }

    pub fn stale_providers(&self, now: Instant) -> u8 {
        self.snapshot.as_ref().map_or(0, |snapshot| {
            snapshot
                .providers
                .values()
                .take(8)
                .enumerate()
                .fold(0, |bits, (index, provider)| {
                    bits | (u8::from(
                        provider
                            .is_stale_after(self.age_seconds(now), snapshot.stale_after_seconds),
                    ) << index)
                })
        })
    }

    pub fn status(&self, now: Instant) -> &'static str {
        if self.config_required {
            "CONFIG REQUIRED"
        } else if !self.wifi_up {
            if self.stale(now) {
                "WIFI OFFLINE STALE"
            } else {
                "WIFI OFFLINE"
            }
        } else if !self.collector_up {
            if self.stale(now) {
                "PC OFFLINE STALE"
            } else {
                "PC OFFLINE"
            }
        } else if self.stale(now) {
            "STALE"
        } else {
            "ONLINE"
        }
    }
}
