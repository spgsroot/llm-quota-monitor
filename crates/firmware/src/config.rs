pub struct NetworkConfig {
    pub ssid: &'static str,
    pub password: &'static str,
    pub collector_url: &'static str,
}

impl NetworkConfig {
    pub fn load() -> Result<Self, &'static str> {
        Self::parse(
            option_env!("WIFI_SSID").unwrap_or(""),
            option_env!("WIFI_PASSWORD").unwrap_or(""),
            option_env!("COLLECTOR_URL").unwrap_or(""),
        )
    }

    pub fn parse(
        ssid: &'static str,
        password: &'static str,
        collector_url: &'static str,
    ) -> Result<Self, &'static str> {
        if ssid.is_empty() || ssid.len() > 32 {
            return Err("Set WIFI_SSID (1..32 bytes) before building");
        }
        if !password.is_empty() && !(8..=63).contains(&password.len()) {
            return Err("WIFI_PASSWORD must be empty for open Wi-Fi or 8..63 bytes for WPA2");
        }
        // An explicit LAN IP avoids DNS dependency; credentials never belong in this URL.
        let (authority, path) = collector_url
            .strip_prefix("http://")
            .and_then(|url| url.split_once('/'))
            .ok_or("Set COLLECTOR_URL to http://<LAN-IP>:8765/api/quotas")?;
        if authority.parse::<std::net::SocketAddr>().is_err() || path != "api/quotas" {
            return Err(
                "COLLECTOR_URL requires an IP:port and /api/quotas; no credentials or query string",
            );
        }
        Ok(Self {
            ssid,
            password,
            collector_url,
        })
    }
}
