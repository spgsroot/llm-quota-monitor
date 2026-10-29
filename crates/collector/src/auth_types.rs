use quota_common::ProviderStatus;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
    Grok,
    Kimi,
}

impl Provider {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Kimi => "kimi",
        }
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Grok => "Grok",
            Self::Kimi => "Kimi",
        }
    }
    pub const fn index(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
            Self::Grok => 2,
            Self::Kimi => 3,
        }
    }
    pub const ALL: [Self; 4] = [Self::Claude, Self::Codex, Self::Grok, Self::Kimi];

    pub const fn quota_source(self) -> &'static str {
        match self {
            Self::Claude => "Claude OAuth usage",
            Self::Codex => "Codex OAuth usage",
            Self::Grok => "Grok OAuth billing",
            Self::Kimi => "Kimi Code OAuth usage",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodexSource {
    #[serde(rename = "oauth")]
    OAuth,
    #[default]
    #[serde(rename = "codex_lb")]
    CodexLb,
}

impl CodexSource {
    pub const fn quota_source(self) -> &'static str {
        match self {
            Self::OAuth => Provider::Codex.quota_source(),
            Self::CodexLb => "codex-lb pool",
        }
    }
}

// No Debug/Clone: credentials must not enter logs or UI snapshots.
#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug)]
pub struct AuthError {
    pub status: ProviderStatus,
    pub message: &'static str,
    pub revoked: bool,
}

impl AuthError {
    pub const fn new(message: &'static str) -> Self {
        Self {
            status: ProviderStatus::Error,
            message,
            revoked: false,
        }
    }
}
impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for AuthError {}

// Browser-only authorization URL and human instructions. Never serialized to the LAN API.
pub struct LoginPrompt {
    pub provider: Provider,
    pub url: String,
    pub instructions: String,
    pub manual_code: bool,
}
