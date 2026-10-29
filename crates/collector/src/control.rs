use crate::auth_types::{CodexSource, LoginPrompt, Provider};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub enum Command {
    Login(Provider),
    CancelLogin,
    SubmitCode(zeroize::Zeroizing<String>),
    Logout(Provider),
    SetCodexSource(CodexSource),
    Refresh,
    Shutdown,
}

#[derive(Clone, Default)]
pub struct ProviderView {
    pub connected: bool,
    pub auth_status: String,
    pub quota_status: String,
    pub quota_error: bool,
}

pub struct UiState {
    pub providers: [ProviderView; 4],
    pub api_status: String,
    pub codex_url: Option<String>,
    pub codex_source: CodexSource,
    pub notice: String,
    pub busy: Option<Provider>,
    pub manual_code: bool,
    pub prompt: Option<LoginPrompt>,
    pub service_running: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            providers: Provider::ALL.map(|_| ProviderView::default()),
            api_status: "Запуск локального HTTP-сервиса…".into(),
            codex_url: None,
            codex_source: CodexSource::default(),
            notice: String::new(),
            busy: None,
            manual_code: false,
            prompt: None,
            service_running: false,
        }
    }
}

pub type SharedUi = Arc<Mutex<UiState>>;
pub struct UiBridge {
    pub state: SharedUi,
    pub commands: mpsc::Receiver<Command>,
    pub notify: Arc<dyn Fn() + Send + Sync>,
}
