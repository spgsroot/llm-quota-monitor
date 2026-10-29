pub mod auth_types;
mod codex_lb;
pub mod control;
#[cfg(windows)]
pub mod desktop;
mod net;
mod oauth;
mod subscriptions;
mod vault;

use auth_types::{AuthError, CodexSource, Credentials, Provider};
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use control::{Command, SharedUi, UiBridge};
use quota_common::{
    ProviderQuota, ProviderStatus, QuotaSnapshot, MAX_RESPONSE_BYTES, STALE_AFTER_SECONDS,
};
use std::{
    collections::{BTreeMap, VecDeque},
    env,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
};
use vault::Vault;
use zeroize::Zeroizing;

fn unavailable(source: &str, status: ProviderStatus, message: &str) -> ProviderQuota {
    ProviderQuota {
        status,
        source: source.into(),
        source_age_seconds: None,
        message: Some(message.into()),
        windows: Vec::new(),
    }
}
struct Cache {
    snapshot: QuotaSnapshot,
    refreshed_at: Instant,
}
impl Cache {
    fn new(now: Instant, wall: OffsetDateTime, codex_source: CodexSource) -> Self {
        Self {
            refreshed_at: now,
            snapshot: QuotaSnapshot {
                updated_at: wall,
                age_seconds: 0,
                stale_after_seconds: STALE_AFTER_SECONDS,
                providers: BTreeMap::from([
                    (
                        "claude".into(),
                        unavailable(
                            "Claude OAuth usage",
                            ProviderStatus::AuthRequired,
                            "Sign in to Claude from the Windows application.",
                        ),
                    ),
                    (
                        "codex".into(),
                        unavailable(
                            codex_source.quota_source(),
                            ProviderStatus::Unavailable,
                            "Waiting for quota data from the selected Codex source.",
                        ),
                    ),
                    (
                        "grok".into(),
                        unavailable(
                            "Grok OAuth billing",
                            ProviderStatus::AuthRequired,
                            "Sign in to Grok from the Windows application.",
                        ),
                    ),
                    (
                        "kimi".into(),
                        unavailable(
                            "Kimi Code OAuth usage",
                            ProviderStatus::AuthRequired,
                            "Sign in to Kimi from the Windows application.",
                        ),
                    ),
                ]),
            },
        }
    }
    fn switch_codex(&mut self, source: CodexSource, now: Instant, wall: OffsetDateTime) {
        self.publish(
            [(
                "codex",
                Ok(unavailable(
                    source.quota_source(),
                    ProviderStatus::Unavailable,
                    "Codex source changed; waiting for its first quota response.",
                )),
            )],
            now,
            wall,
        );
    }
    fn publish(
        &mut self,
        results: impl IntoIterator<Item = (&'static str, Result<ProviderQuota, AuthError>)>,
        now: Instant,
        wall: OffsetDateTime,
    ) {
        let elapsed = now.saturating_duration_since(self.refreshed_at).as_secs();
        for provider in self.snapshot.providers.values_mut() {
            provider.source_age_seconds = provider
                .source_age_seconds
                .map(|age| age.saturating_add(elapsed));
        }
        for (key, result) in results {
            match result {
                Ok(provider) => {
                    self.snapshot.providers.insert(key.into(), provider);
                }
                Err(error) => {
                    let provider = self
                        .snapshot
                        .providers
                        .get_mut(key)
                        .expect("fixed provider key");
                    provider.status = error.status;
                    provider.message = Some(if provider.windows.is_empty() {
                        error.message.into()
                    } else {
                        format!("{}; showing last known quota values.", error.message)
                    });
                }
            }
        }
        self.snapshot.updated_at = wall;
        self.snapshot.age_seconds = 0;
        self.refreshed_at = now;
    }
    fn encode_at(&mut self, now: Instant) -> Result<Vec<u8>, serde_json::Error> {
        self.snapshot.age_seconds = now.saturating_duration_since(self.refreshed_at).as_secs();
        serde_json::to_vec(&self.snapshot)
    }
}
type AppState = Arc<Mutex<Cache>>;
async fn quotas(State(state): State<AppState>) -> Response {
    let result = state.lock().await.encode_at(Instant::now());
    match result {
        Ok(body) if body.len() <= MAX_RESPONSE_BYTES => (
            [
                ("content-type", "application/json"),
                ("cache-control", "no-store"),
            ],
            body,
        )
            .into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("cache-control", "no-store")],
            "Cannot encode quota snapshot",
        )
            .into_response(),
    }
}
async fn dashboard() -> impl IntoResponse {
    (
        [
            ("cache-control", "no-store"),
            ("x-content-type-options", "nosniff"),
        ],
        Html(include_str!("../web/index.html")),
    )
}
async fn javascript() -> impl IntoResponse {
    (
        [
            ("content-type", "text/javascript; charset=utf-8"),
            ("cache-control", "no-store"),
            ("x-content-type-options", "nosniff"),
        ],
        include_str!("../web/dashboard.js"),
    )
}

struct Session {
    credentials: Option<Credentials>,
    blocked: bool,
    unsaved: bool,
    auth_status: &'static str,
}
impl Session {
    fn load(vault: &Vault, provider: Provider) -> Self {
        match vault.load(provider) {
            Ok(credentials) => {
                let auth_status = if credentials.is_some() {
                    "Вход сохранён; проверяем доступ к квотам"
                } else {
                    "Вход не выполнен"
                };
                Self {
                    credentials,
                    blocked: false,
                    unsaved: false,
                    auth_status,
                }
            }
            Err(error) => Self {
                credentials: None,
                blocked: true,
                unsaved: false,
                auth_status: error.message,
            },
        }
    }
    async fn refresh(
        &mut self,
        vault: &Vault,
        provider: Provider,
        client: &reqwest::Client,
    ) -> Result<(), AuthError> {
        let previous = self.credentials.as_ref().ok_or(AuthError {
            status: ProviderStatus::AuthRequired,
            message: "Sign in from the Windows application to read subscription quota",
            revoked: false,
        })?;
        match oauth::refresh(provider, client, previous).await {
            Ok(next) => {
                // Persist a rotating refresh token before any subsequent token request. Keep it in RAM if storage fails.
                self.credentials = Some(next);
                self.unsaved = true;
                self.persist(vault, provider)?;
                self.auth_status = "OAuth выполнен; токены сохранены в Windows";
                Ok(())
            }
            Err(error) => {
                if error.revoked {
                    self.blocked = true;
                    self.auth_status = error.message;
                }
                Err(error)
            }
        }
    }
    fn persist(&mut self, vault: &Vault, provider: Provider) -> Result<(), AuthError> {
        if self.unsaved {
            let credentials = self
                .credentials
                .as_ref()
                .expect("unsaved credentials exist");
            if let Err(error) = vault.save(provider, credentials) {
                self.auth_status = "Ошибка DPAPI: обновление токенов приостановлено";
                return Err(error);
            }
            self.unsaved = false;
        }
        Ok(())
    }
    async fn collect(
        &mut self,
        vault: &Vault,
        provider: Provider,
        client: &reqwest::Client,
    ) -> Result<ProviderQuota, AuthError> {
        if self.blocked {
            return Err(AuthError {
                status: ProviderStatus::AuthRequired,
                message: self.auth_status,
                revoked: false,
            });
        }
        self.persist(vault, provider)?;
        let credentials = self.credentials.as_ref().ok_or(AuthError {
            status: ProviderStatus::AuthRequired,
            message: "Sign in from the Windows application to read subscription quota",
            revoked: false,
        })?;
        if credentials.expires_at <= OffsetDateTime::now_utc().unix_timestamp() + 60 {
            self.refresh(vault, provider, client).await?;
        }
        let access = &self
            .credentials
            .as_ref()
            .expect("authorized session")
            .access_token;
        let mut result = subscriptions::fetch(client, provider, access).await;
        if result
            .as_ref()
            .err()
            .is_some_and(|error| error.status == ProviderStatus::AuthRequired)
        {
            self.refresh(vault, provider, client).await?;
            result = subscriptions::fetch(
                client,
                provider,
                &self
                    .credentials
                    .as_ref()
                    .expect("refreshed session")
                    .access_token,
            )
            .await;
        }
        self.auth_status = match &result {
            Ok(_) => "OAuth выполнен; токены сохранены в Windows",
            Err(error) if error.status == ProviderStatus::Unavailable => {
                "OAuth сохранён; провайдер запретил чтение квот"
            }
            Err(error) if error.status == ProviderStatus::AuthRequired => {
                "Провайдер отклонил вход; авторизуйтесь заново"
            }
            Err(_) => "OAuth сохранён; ошибка запроса квот",
        };
        result
    }
}
type LoginTask = JoinHandle<Result<Credentials, AuthError>>;
struct ActiveLogin {
    provider: Provider,
    task: LoginTask,
    manual: Option<oneshot::Sender<Zeroizing<String>>>,
}
#[derive(Clone)]
enum CodexBackend {
    OAuth,
    CodexLb(Arc<codex_lb::CodexLb>),
}
impl CodexBackend {
    fn load(kind: CodexSource) -> Result<Self, Box<dyn std::error::Error>> {
        match kind {
            CodexSource::OAuth => Ok(Self::OAuth),
            CodexSource::CodexLb => Ok(Self::CodexLb(Arc::new(codex_lb::CodexLb::from_env()?))),
        }
    }
    fn kind(&self) -> CodexSource {
        match self {
            Self::OAuth => CodexSource::OAuth,
            Self::CodexLb(_) => CodexSource::CodexLb,
        }
    }
    fn dashboard_url(&self) -> Option<&str> {
        match self {
            Self::OAuth => None,
            Self::CodexLb(source) => Some(source.dashboard_url()),
        }
    }
    async fn collect(
        &self,
        session: &mut Session,
        vault: &Vault,
        client: &reqwest::Client,
    ) -> Result<ProviderQuota, AuthError> {
        match self {
            Self::OAuth => session.collect(vault, Provider::Codex, client).await,
            Self::CodexLb(source) => source.collect().await.map_err(|error| AuthError {
                status: error.status,
                message: error.message,
                revoked: false,
            }),
        }
    }
}
const ALL_PROVIDER_MASK: u8 = 0b1111;
type ProviderUpdate = (&'static str, Result<ProviderQuota, AuthError>);
type CycleResult = ([Session; 4], [Option<ProviderUpdate>; 4]);
async fn collect(
    mut sessions: [Session; 4],
    vault: Arc<Vault>,
    source: CodexBackend,
    client: reqwest::Client,
    selected: u8,
) -> CycleResult {
    let [claude, codex, grok, kimi] = &mut sessions;
    let (first, second, third, fourth) = tokio::join!(
        async {
            if selected & 1 != 0 {
                Some((
                    "claude",
                    claude.collect(&vault, Provider::Claude, &client).await,
                ))
            } else {
                None
            }
        },
        async {
            if selected & 2 != 0 {
                Some(("codex", source.collect(codex, &vault, &client).await))
            } else {
                None
            }
        },
        async {
            if selected & 4 != 0 {
                Some(("grok", grok.collect(&vault, Provider::Grok, &client).await))
            } else {
                None
            }
        },
        async {
            if selected & 8 != 0 {
                Some(("kimi", kimi.collect(&vault, Provider::Kimi, &client).await))
            } else {
                None
            }
        },
    );
    (sessions, [first, second, third, fourth])
}
fn ui_update(bridge: &Option<UiBridge>, update: impl FnOnce(&mut control::UiState)) {
    if let Some(bridge) = bridge {
        let mut state = bridge
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        update(&mut state);
        drop(state);
        (bridge.notify)();
    }
}
fn sync_views(ui: &SharedUi, sessions: &[Session; 4], cache: &Cache, codex_source: CodexSource) {
    let mut state = ui.lock().unwrap_or_else(|poison| poison.into_inner());
    for (slot, key) in ["claude", "codex", "grok", "kimi"].iter().enumerate() {
        let quota = &cache.snapshot.providers[*key];
        let status = if quota.is_stale_after(cache.snapshot.age_seconds, STALE_AFTER_SECONDS) {
            "STALE".into()
        } else {
            format!("{:?}", quota.status)
        };
        let mut text = status;
        for window in quota.windows.iter().take(2) {
            use std::fmt::Write;
            let _ = write!(
                text,
                " · {} {}",
                window.name,
                window
                    .remaining_percent
                    .map_or_else(|| "N/A".into(), |percent| format!("{percent}%"))
            );
        }
        if quota.windows.len() > 2 {
            use std::fmt::Write;
            let _ = write!(text, " · +{} окон", quota.windows.len() - 2);
        }
        if !quota.windows.is_empty() {
            use std::fmt::Write;
            let _ = write!(text, "\r\nСброс UTC:");
            for (index, window) in quota.windows.iter().take(2).enumerate() {
                if index > 0 {
                    let _ = write!(text, " ·");
                }
                if let Some(reset) = window.resets_at {
                    let _ = write!(
                        text,
                        " {} {:02}.{:02} {:02}:{:02}",
                        window.name,
                        reset.day(),
                        u8::from(reset.month()),
                        reset.hour(),
                        reset.minute()
                    );
                } else {
                    let _ = write!(text, " {} ?", window.name);
                }
            }
        }
        if let Some(message) = &quota.message {
            use std::fmt::Write;
            let _ = write!(text, "\r\n{message}");
        }
        state.providers[slot].quota_status = text;
        state.providers[slot].quota_error = quota.status == ProviderStatus::Error;
    }
    for provider in Provider::ALL {
        let index = provider.index();
        state.providers[index].connected =
            sessions[index].credentials.is_some() && !sessions[index].blocked;
        state.providers[index].auth_status = sessions[index].auth_status.into();
    }
    if codex_source == CodexSource::CodexLb {
        state.providers[1].connected = matches!(
            cache.snapshot.providers["codex"].status,
            ProviderStatus::Ok | ProviderStatus::Stale
        );
        state.providers[1].auth_status = "Источник: CodexLB, read-only агрегат пула".into();
    }
}
async fn next_command(bridge: &mut Option<UiBridge>) -> Option<Command> {
    match bridge {
        Some(bridge) => bridge.commands.recv().await,
        None => std::future::pending().await,
    }
}

/// One engine for the native app and explicit headless mode. No OAuth control route is exposed on the LAN.
pub async fn run(mut bridge: Option<UiBridge>) -> Result<(), Box<dyn std::error::Error>> {
    let bind: SocketAddr = env::var("QUOTA_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8765".into())
        .parse()
        .map_err(|_| "Invalid QUOTA_BIND (expected IP:port)")?;
    let refresh_seconds: u64 = env::var("QUOTA_REFRESH_SECONDS")
        .unwrap_or_else(|_| "60".into())
        .parse()
        .map_err(|_| "Invalid QUOTA_REFRESH_SECONDS")?;
    if refresh_seconds == 0 {
        return Err("QUOTA_REFRESH_SECONDS must be positive".into());
    }
    let vault = Arc::new(Vault::open()?);
    let _instance = vault.acquire_instance()?;
    let codex_source = match env::var("CODEX_SOURCE") {
        Ok(value) => match value.as_str() {
            "oauth" => CodexSource::OAuth,
            "codex_lb" => CodexSource::CodexLb,
            _ => return Err("CODEX_SOURCE must be oauth or codex_lb".into()),
        },
        Err(env::VarError::NotPresent) => vault.codex_source()?,
        Err(_) => return Err("Invalid CODEX_SOURCE".into()),
    };
    let mut source = CodexBackend::load(codex_source)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .user_agent("llm-quota-monitor/0.1")
        .build()?;
    let mut sessions = Some(Provider::ALL.map(|provider| Session::load(&vault, provider)));
    let listener=tokio::net::TcpListener::bind(bind).await.map_err(|error|format!("Cannot listen on {bind}: {error}. Close the previous collector before using this port."))?;
    let address = listener.local_addr()?;
    let state = Arc::new(Mutex::new(Cache::new(
        Instant::now(),
        OffsetDateTime::now_utc(),
        source.kind(),
    )));
    let app = Router::new()
        .route("/", get(dashboard))
        .route("/dashboard.js", get(javascript))
        .route("/api/quotas", get(quotas))
        .with_state(Arc::clone(&state));
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    println!("LLM Quota Monitor: native subscription OAuth + read-only codex-lb.");
    println!("Listening on http://{address} (provider refresh: {refresh_seconds}s).");
    println!(
        "Unauthenticated HTTP: trusted LAN only. No credentials or OAuth control in the LAN API."
    );
    ui_update(&bridge, |ui| {
        ui.service_running = true;
        ui.codex_url = source.dashboard_url().map(str::to_owned);
        ui.codex_source = source.kind();
        ui.api_status =
            format!("HTTP: {address} · только доверенная LAN · обновление {refresh_seconds} с");
    });
    if let (Some(bridge), Some(sessions)) = (&bridge, &sessions) {
        let cache = state.lock().await;
        sync_views(&bridge.state, sessions, &cache, source.kind());
        (bridge.notify)();
    }
    let (prompt_tx, mut prompts) = mpsc::channel(2);
    let mut cycle: Option<JoinHandle<CycleResult>> = None;
    let mut login: Option<ActiveLogin> = None;
    let mut deferred = VecDeque::with_capacity(Provider::ALL.len());
    let headless = bridge.is_none();
    let mut refresh_requested = ALL_PROVIDER_MASK;
    let mut shutting_down = false;
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(refresh_seconds),
        Duration::from_secs(refresh_seconds),
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if let (Some(bridge), Some(sessions)) = (&bridge, &sessions) {
            let cache = state.lock().await;
            sync_views(&bridge.state, sessions, &cache, source.kind());
            (bridge.notify)();
        }
        if cycle.is_none() && login.is_none() {
            if shutting_down {
                break;
            }
            if let Some(command) = deferred.pop_front() {
                match command {
                    Command::Login(provider) => {
                        let (manual_tx, manual_rx) = oneshot::channel();
                        login = Some(ActiveLogin {
                            provider,
                            task: tokio::spawn(oauth::login(
                                provider,
                                client.clone(),
                                prompt_tx.clone(),
                                manual_rx,
                            )),
                            manual: Some(manual_tx),
                        });
                        ui_update(&bridge, |ui| {
                            ui.busy = Some(provider);
                            ui.notice = format!("Начинаем OAuth {}…", provider.name());
                            ui.manual_code = false;
                        });
                    }
                    Command::Logout(provider) => match vault.remove(provider) {
                        Ok(()) => {
                            let index = provider.index();
                            sessions.as_mut().expect("idle sessions")[index] = Session {
                                credentials: None,
                                blocked: false,
                                unsaved: false,
                                auth_status: "Вход не выполнен",
                            };
                            if provider != Provider::Codex || source.kind() == CodexSource::OAuth {
                                state.lock().await.publish([(provider.key(),Ok(unavailable(provider.quota_source(),ProviderStatus::AuthRequired,"Signed out locally; sign in to read subscription quota.")))],Instant::now(),OffsetDateTime::now_utc());
                            }
                            ui_update(&bridge, |ui| {
                                ui.notice = format!(
                                    "{}: локальная OAuth авторизация удалена с этого ПК.",
                                    provider.name()
                                )
                            });
                        }
                        Err(error) => ui_update(&bridge, |ui| ui.notice = error.message.into()),
                    },
                    Command::SetCodexSource(kind) => {
                        if kind != source.kind() {
                            match CodexBackend::load(kind).and_then(|next| {
                                vault.save_codex_source(kind)?;
                                Ok(next)
                            }) {
                                Ok(next) => {
                                    source = next;
                                    state.lock().await.switch_codex(
                                        kind,
                                        Instant::now(),
                                        OffsetDateTime::now_utc(),
                                    );
                                    refresh_requested |= 1 << Provider::Codex.index();
                                    ui_update(&bridge, |ui| {
                                        ui.codex_source = kind;
                                        ui.codex_url = source.dashboard_url().map(str::to_owned);
                                        ui.notice="Источник Codex сохранён. Получаем его собственные квоты…".into();
                                    });
                                }
                                Err(error) => {
                                    ui_update(&bridge, |ui| ui.notice = error.to_string())
                                }
                            }
                        }
                    }
                    _ => unreachable!("only configuration and authorization commands are deferred"),
                }
                if login.is_none() {
                    continue;
                }
            } else if refresh_requested != 0 {
                cycle = Some(tokio::spawn(collect(
                    sessions.take().expect("idle sessions"),
                    Arc::clone(&vault),
                    source.clone(),
                    client.clone(),
                    std::mem::take(&mut refresh_requested),
                )));
            }
        }
        tokio::select! {
            result=&mut server=>{result??;return Err("HTTP service stopped unexpectedly".into());}
            signal=tokio::signal::ctrl_c(),if headless&&!shutting_down=>{signal.map_err(|_|"Cannot install the console shutdown handler")?;shutting_down=true;if let Some(active)=login.take(){active.task.abort();}deferred.clear();}
            _=interval.tick(),if !shutting_down=>{refresh_requested|=ALL_PROVIDER_MASK;}
            prompt=prompts.recv(),if login.is_some()=>{if let Some(prompt)=prompt{ui_update(&bridge,|ui|{ui.manual_code=prompt.manual_code;ui.notice=prompt.instructions.clone();ui.prompt=Some(prompt);});}}
            result=async{cycle.as_mut().expect("active cycle").await},if cycle.is_some()=>{
                let (returned,results)=result.map_err(|_|"Provider update worker stopped unexpectedly")?;cycle=None;sessions=Some(returned);
                state.lock().await.publish(results.into_iter().flatten(),Instant::now(),OffsetDateTime::now_utc());
                ui_update(&bridge,|ui|{if ui.busy.is_none(){ui.notice="Проверка квот завершена. Результат - в строках провайдеров.".into();}});
            }
            result=async{(&mut login.as_mut().expect("active login").task).await},if login.is_some()=>{
                let provider=login.take().expect("completed login").provider;
                let result=result.map_err(|_|"OAuth worker stopped unexpectedly")?;
                match result{
                    Ok(credentials)=>{
                        let index=provider.index();
                        let session=&mut sessions.as_mut().expect("login does not overlap refresh")[index];
                        *session=Session{credentials:Some(credentials),blocked:false,unsaved:true,auth_status:"OAuth получен; сохраняем в Windows"};
                        match session.persist(&vault,provider){
                            Ok(())=>{session.auth_status="OAuth выполнен; токены сохранены в Windows";ui_update(&bridge,|ui|ui.notice=format!("{}: вход завершён. Проверяем доступ к подписочным квотам…",provider.name()));}
                            Err(error)=>ui_update(&bridge,|ui|ui.notice=format!("OAuth получен, но {}",error.message)),
                        }
                        if provider!=Provider::Codex || source.kind()==CodexSource::OAuth{
                            state.lock().await.publish([(provider.key(),Ok(unavailable(provider.quota_source(),ProviderStatus::Unavailable,"Signed in; waiting for the first subscription quota response.")))],Instant::now(),OffsetDateTime::now_utc());
                        }
                        refresh_requested|=1<<provider.index();
                    }
                    Err(error)=>ui_update(&bridge,|ui|ui.notice=error.message.into()),
                }
                ui_update(&bridge,|ui|{ui.busy=None;ui.manual_code=false;ui.prompt=None;});
            }
            command=next_command(&mut bridge),if !shutting_down=>{
                match command.unwrap_or(Command::Shutdown){
                    Command::Login(provider)=>{
                        if login.is_some()||deferred.iter().any(|command|matches!(command,Command::Login(_))){ui_update(&bridge,|ui|ui.notice="Сначала завершите или отмените текущий вход.".into());}
                        else{deferred.push_back(Command::Login(provider));ui_update(&bridge,|ui|{ui.busy=Some(provider);ui.notice="Ожидание завершения текущего запроса перед входом…".into();});}
                    }
                    Command::CancelLogin=>{
                        if let Some(active)=login.take(){active.task.abort();}
                        deferred.retain(|command|!matches!(command,Command::Login(_)));
                        while prompts.try_recv().is_ok(){}
                        ui_update(&bridge,|ui|{ui.busy=None;ui.manual_code=false;ui.prompt=None;ui.notice="Вход отменён. Ранее сохранённая авторизация не изменена.".into();});
                    }
                    Command::SubmitCode(code)=>{
                        if let Some(sender)=login.as_mut().and_then(|active|active.manual.take()){
                            let _=sender.send(code);
                            ui_update(&bridge,|ui|{ui.manual_code=false;ui.notice="Проверяем код авторизации…".into();});
                        }
                    }
                    Command::Logout(provider)=>{
                        if login.as_ref().is_some_and(|active|active.provider==provider){login.take().unwrap().task.abort();ui_update(&bridge,|ui|{ui.busy=None;ui.manual_code=false;ui.prompt=None;});}
                        deferred.retain(|command|!matches!(command,Command::Login(active) if *active==provider));
                        if !deferred.iter().any(|command|matches!(command,Command::Logout(active) if *active==provider)){deferred.push_back(Command::Logout(provider));}
                        ui_update(&bridge,|ui|ui.notice="Выход после завершения текущего запроса…".into());
                    }
                    Command::SetCodexSource(kind)=>{
                        deferred.retain(|command|!matches!(command,Command::SetCodexSource(_)));
                        deferred.push_back(Command::SetCodexSource(kind));
                        ui_update(&bridge,|ui|ui.notice="Смена источника Codex после завершения текущего запроса…".into());
                    }
                    Command::Refresh=>{refresh_requested|=ALL_PROVIDER_MASK;ui_update(&bridge,|ui|ui.notice="Обновление квот запрошено; HTTP и остальные окна доступны.".into());}
                    Command::Shutdown=>{shutting_down=true;deferred.clear();if let Some(active)=login.take(){active.task.abort();}ui_update(&bridge,|ui|{ui.busy=None;ui.manual_code=false;ui.prompt=None;ui.notice="Завершаем запрос и сохраняем токены перед выходом…".into();});}
                }
            }
        }
    }
    let _ = shutdown_tx.send(());
    // Idle LAN keep-alive clients must not keep the tray process alive indefinitely.
    match tokio::time::timeout(Duration::from_secs(2), &mut server).await {
        Ok(result) => result??,
        Err(_) => server.abort(),
    };
    ui_update(&bridge, |ui| {
        ui.service_running = false;
        ui.api_status = "HTTP-сервис остановлен".into();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_common::QuotaWindow;
    #[test]
    fn independent_provider_failures_preserve_values_and_age_when_another_source_succeeds() {
        let start = Instant::now();
        let wall = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let mut cache = Cache::new(start, wall, CodexSource::CodexLb);
        cache.publish(
            [(
                "claude",
                Ok(ProviderQuota {
                    status: ProviderStatus::Ok,
                    source: "Claude OAuth usage".into(),
                    source_age_seconds: Some(20),
                    message: None,
                    windows: vec![QuotaWindow {
                        name: "5h".into(),
                        remaining_percent: Some(73),
                        resets_at: None,
                    }],
                }),
            )],
            start,
            wall,
        );
        let error = AuthError {
            status: ProviderStatus::AuthRequired,
            message: "Authorization revoked",
            revoked: true,
        };
        cache.publish(
            [
                ("claude", Err(error)),
                (
                    "codex",
                    Ok(unavailable(
                        "codex-lb pool",
                        ProviderStatus::Error,
                        "offline",
                    )),
                ),
            ],
            start + Duration::from_secs(40),
            wall + time::Duration::seconds(40),
        );
        cache.publish(
            [(
                "codex",
                Ok(unavailable(
                    "codex-lb pool",
                    ProviderStatus::Error,
                    "offline",
                )),
            )],
            start + Duration::from_secs(70),
            wall + time::Duration::seconds(70),
        );
        let read: QuotaSnapshot =
            serde_json::from_slice(&cache.encode_at(start + Duration::from_secs(75)).unwrap())
                .unwrap();
        assert_eq!(read.age_seconds, 5);
        assert_eq!(read.providers["claude"].source_age_seconds, Some(90));
        assert_eq!(
            read.providers["claude"].windows[0].remaining_percent,
            Some(73)
        );
        assert_eq!(
            read.providers["claude"].status,
            ProviderStatus::AuthRequired
        );
        assert!(read.is_stale_after(0));
        read.validate().unwrap();
    }
    #[test]
    fn changing_codex_source_clears_other_accounts_quota_and_never_falls_back_on_auth_error() {
        let start = Instant::now();
        let wall = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let mut cache = Cache::new(start, wall, CodexSource::CodexLb);
        cache.publish(
            [(
                "codex",
                Ok(ProviderQuota {
                    status: ProviderStatus::Ok,
                    source: "codex-lb pool".into(),
                    source_age_seconds: Some(0),
                    message: None,
                    windows: vec![QuotaWindow {
                        name: "5h".into(),
                        remaining_percent: Some(81),
                        resets_at: None,
                    }],
                }),
            )],
            start,
            wall,
        );
        cache.switch_codex(CodexSource::OAuth, start + Duration::from_secs(1), wall);
        assert!(cache.snapshot.providers["codex"].windows.is_empty());
        cache.publish(
            [(
                "codex",
                Err(AuthError {
                    status: ProviderStatus::AuthRequired,
                    message: "Sign in required",
                    revoked: false,
                }),
            )],
            start + Duration::from_secs(2),
            wall,
        );
        assert_eq!(
            cache.snapshot.providers["codex"].source,
            Provider::Codex.quota_source()
        );
        assert_eq!(
            cache.snapshot.providers["codex"].status,
            ProviderStatus::AuthRequired
        );
        assert!(cache.snapshot.providers["codex"].windows.is_empty());
    }
}
