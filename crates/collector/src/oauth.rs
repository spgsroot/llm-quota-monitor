use crate::auth_types::{AuthError, Credentials, LoginPrompt, Provider};
use crate::net;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use quota_common::ProviderStatus;
use rand::{rngs::OsRng, RngCore};
use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    time::Instant,
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const CLAUDE_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_TOKEN: &str = "https://api.anthropic.com/v1/oauth/token";
const CLAUDE_REDIRECT: &str = "http://localhost:54545/callback";
const XAI_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_TOKEN: &str = "https://auth.x.ai/oauth2/token";
const TOKEN_LIMIT: usize = 64 * 1024;
const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN: &str = "https://auth.openai.com/oauth/token";
const KIMI_CLIENT: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_DEVICE: &str = "https://auth.kimi.com/api/oauth/device_authorization";
const KIMI_TOKEN: &str = "https://auth.kimi.com/api/oauth/token";

struct CodeFlow {
    provider: Provider,
    client_id: &'static str,
    authorize: &'static str,
    redirect: &'static str,
    host: &'static str,
    port: u16,
    path: &'static str,
    scopes: &'static str,
}
const CLAUDE_FLOW: CodeFlow = CodeFlow {
    provider: Provider::Claude, client_id: CLAUDE_CLIENT,
    authorize: "https://claude.ai/oauth/authorize", redirect: CLAUDE_REDIRECT,
    host: "localhost:54545", port: 54545, path: "/callback",
    scopes: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
};
const CODEX_FLOW: CodeFlow = CodeFlow {
    provider: Provider::Codex,
    client_id: CODEX_CLIENT,
    authorize: "https://auth.openai.com/oauth/authorize",
    redirect: "http://localhost:1455/auth/callback",
    host: "localhost:1455",
    port: 1455,
    path: "/auth/callback",
    scopes: "openid profile email offline_access",
};
#[derive(Serialize)]
struct ClaudeExchange<'a> {
    grant_type: &'static str,
    client_id: &'static str,
    code: &'a str,
    redirect_uri: &'static str,
    code_verifier: &'a str,
    state: &'a str,
}
#[derive(Serialize)]
struct ClaudeRefresh<'a> {
    grant_type: &'static str,
    client_id: &'static str,
    refresh_token: &'a str,
}

fn jwt_claims<T: DeserializeOwned>(access: &str) -> Result<T, AuthError> {
    let mut parts = access.split('.');
    let _header = parts.next();
    let payload = parts.next();
    let signature = parts.next();
    if signature.is_none() || parts.next().is_some() {
        return Err(AuthError::new("Invalid OAuth access token claims"));
    }
    let payload = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(payload.unwrap_or(""))
            .map_err(|_| AuthError::new("Invalid OAuth access token claims"))?,
    );
    serde_json::from_slice(&payload)
        .map_err(|_| AuthError::new("Invalid OAuth access token claims"))
}

pub fn codex_account_id(access: &str) -> Result<Zeroizing<String>, AuthError> {
    #[derive(Deserialize)]
    struct Identity {
        #[serde(rename = "https://api.openai.com/auth")]
        auth: Account,
    }
    #[derive(Deserialize)]
    struct Account {
        chatgpt_account_id: String,
    }
    // The HTTPS token issuer is trusted; decoded claims only route this token's quota request.
    let identity: Identity = jwt_claims(access)?;
    let id = Zeroizing::new(identity.auth.chatgpt_account_id);
    if id.is_empty() || id.len() > 128 {
        return Err(AuthError::new(
            "Codex token has no valid ChatGPT account id",
        ));
    }
    Ok(id)
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct TokenReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    token_type: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

fn token(mut reply: TokenReply, previous: Option<&Credentials>) -> Result<Credentials, AuthError> {
    if reply.error.as_deref() == Some("invalid_grant") {
        return Err(AuthError {
            status: ProviderStatus::AuthRequired,
            message: "Authorization expired or was revoked; sign in again",
            revoked: true,
        });
    }
    if reply.error.is_some() {
        return Err(AuthError::new("Provider rejected the OAuth token request"));
    }
    if reply
        .token_type
        .as_ref()
        .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
    {
        return Err(AuthError::new("Unsupported OAuth token type"));
    }
    let access = reply
        .access_token
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::new("OAuth response has no access token"))?;
    if reply.refresh_token.as_ref().is_some_and(String::is_empty)
        || (reply.refresh_token.is_none()
            && previous.is_none_or(|old| old.refresh_token.is_empty()))
    {
        return Err(AuthError::new("OAuth response has no valid refresh token"));
    }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let expires_at = if let Some(seconds) = reply.expires_in {
        let seconds = i64::try_from(seconds)
            .ok()
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| AuthError::new("Invalid OAuth token expiry"))?;
        now.checked_add(seconds)
            .ok_or_else(|| AuthError::new("Invalid OAuth token expiry"))?
    } else {
        // Scheduling only, never authentication/identity: the token was received directly over pinned HTTPS.
        #[derive(Deserialize)]
        struct Expiry {
            exp: i64,
        }
        let expiry: Expiry = jwt_claims(access)?;
        if expiry.exp <= now {
            return Err(AuthError::new("OAuth returned an already expired token"));
        }
        expiry.exp
    };
    let access_token = reply.access_token.take().expect("validated access token");
    let refresh_token = reply.refresh_token.take().unwrap_or_else(|| {
        previous
            .expect("validated previous refresh token")
            .refresh_token
            .clone()
    });
    Ok(Credentials {
        access_token,
        refresh_token,
        expires_at,
        device_id: previous.and_then(|old| old.device_id.clone()),
    })
}

fn random_value() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let value = URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    value
}

fn authorization_url(flow: &CodeFlow, state: &str, verifier: &str) -> Url {
    let mut url = Url::parse(flow.authorize).expect("constant OAuth URL");
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    url.query_pairs_mut().extend_pairs([
        ("client_id", flow.client_id),
        ("response_type", "code"),
        ("redirect_uri", flow.redirect),
        ("scope", flow.scopes),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state),
    ]);
    match flow.provider {
        Provider::Claude => {
            url.query_pairs_mut().append_pair("code", "true");
        }
        Provider::Codex => {
            url.query_pairs_mut().extend_pairs([
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "llm-quota-monitor"),
            ]);
        }
        Provider::Grok | Provider::Kimi => unreachable!("provider uses device authorization"),
    }
    url
}

fn callback_code(
    flow: &CodeFlow,
    input: &str,
    state: &str,
    manual: bool,
) -> Result<Zeroizing<String>, AuthError> {
    let input = input.trim();
    if input.is_empty() || input.len() > 8192 {
        return Err(AuthError::new("Invalid OAuth authorization code"));
    }
    if input.starts_with("http:") || input.starts_with("https:") {
        let url = Url::parse(input).map_err(|_| AuthError::new("Invalid OAuth callback URL"))?;
        if url.scheme() != "http"
            || url.host_str() != Some("localhost")
            || url.port() != Some(flow.port)
            || url.path() != flow.path
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(AuthError::new("Unexpected OAuth callback target"));
        }
        let mut code = None;
        let mut received_state = None;
        let mut denied = false;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "state" => {
                    if received_state.is_some() {
                        return Err(AuthError::new("Duplicate OAuth state"));
                    }
                    received_state = Some(value.into_owned());
                }
                "code" => {
                    if code.is_some() {
                        return Err(AuthError::new("Duplicate OAuth code"));
                    }
                    code = Some(Zeroizing::new(value.into_owned()));
                }
                "error" => denied = true,
                _ => {}
            }
        }
        if received_state.as_deref() != Some(state) {
            return Err(AuthError::new("OAuth callback state mismatch"));
        }
        if denied {
            return Err(AuthError {
                status: ProviderStatus::AuthRequired,
                message: "Authorization was denied in the browser",
                revoked: false,
            });
        }
        return code
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AuthError::new("OAuth callback has no authorization code"));
    }
    if !manual {
        return Err(AuthError::new("Invalid OAuth callback"));
    }
    if let Some((code, echo)) = input.split_once('#') {
        if echo != state || code.is_empty() {
            return Err(AuthError::new("OAuth authorization state mismatch"));
        }
        Ok(Zeroizing::new(code.to_owned()))
    } else {
        // Explicit user-pasted code is still bound to our private PKCE verifier at the token endpoint.
        if input.contains(char::is_whitespace) {
            return Err(AuthError::new("Invalid pasted OAuth code"));
        }
        Ok(Zeroizing::new(input.to_owned()))
    }
}

async fn callback_request(
    flow: &CodeFlow,
    mut stream: TcpStream,
    state: &str,
) -> Result<Option<Zeroizing<String>>, AuthError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut request = Zeroizing::new(Vec::with_capacity(2048));
    let mut chunk = [0_u8; 1024];
    loop {
        let count = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| AuthError::new("OAuth callback read timed out"))?
            .map_err(|_| AuthError::new("Cannot read OAuth callback"))?;
        if count == 0 {
            return Ok(None);
        }
        if request.len() + count > 12 * 1024 {
            return Ok(None);
        }
        request.extend_from_slice(&chunk[..count]);
        chunk.zeroize();
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break;
        }
    }
    let request_text = std::str::from_utf8(&request)
        .map_err(|_| AuthError::new("Invalid OAuth callback encoding"))?;
    let mut lines = request_text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next();
    let target = parts.next().unwrap_or("");
    let host = lines.find_map(|line| {
        line.split_once(':')
            .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim())
    });
    let result = if method == Some("GET")
        && target
            .split_once('?')
            .is_some_and(|(path, _)| path == flow.path)
        && host == Some(flow.host)
    {
        callback_code(
            flow,
            &Zeroizing::new(format!("http://{}{target}", flow.host)),
            state,
            false,
        )
        .map(Some)
    } else {
        Ok(None)
    };
    let accepted = matches!(result, Ok(Some(_)))
        || result
            .as_ref()
            .err()
            .is_some_and(|error| error.status == ProviderStatus::AuthRequired);
    let reply = if accepted {
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\nOAuth callback received. Return to LLM Quota Monitor."
    } else {
        "HTTP/1.1 400 Bad Request\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nInvalid OAuth callback."
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.write_all(reply.as_bytes())).await;
    result
}

async fn accept_optional(listener: &Option<TcpListener>) -> std::io::Result<TcpStream> {
    match listener {
        Some(listener) => listener.accept().await.map(|(stream, _)| stream),
        None => std::future::pending().await,
    }
}

async fn code_login(
    flow: &CodeFlow,
    client: &Client,
    prompts: mpsc::Sender<LoginPrompt>,
    mut manual: oneshot::Receiver<Zeroizing<String>>,
) -> Result<Credentials, AuthError> {
    let ipv4 = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, flow.port))
        .await
        .map_err(|_| {
            AuthError::new(match flow.provider {
                Provider::Codex => {
                    "Codex OAuth callback port 1455 is busy; close the other sign-in and retry"
                }
                _ => "Claude OAuth callback port 54545 is busy; close the other sign-in and retry",
            })
        })?;
    let ipv6 = match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, flow.port)).await {
        Ok(listener) => Some(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrNotAvailable => None,
        Err(_) => {
            return Err(AuthError::new(
                "IPv6 OAuth callback port is unavailable; close the other sign-in and retry",
            ))
        }
    };
    let verifier = Zeroizing::new(random_value());
    let state = Zeroizing::new(random_value());
    prompts.send(LoginPrompt { provider: flow.provider, url: authorization_url(flow,&state,&verifier).into(),
        instructions: format!("Завершите вход {} в браузере. Если возврат не сработал, вставьте полный callback URL или выданный код ниже.",flow.provider.name()), manual_code: true })
        .await.map_err(|_| AuthError::new("Authorization was cancelled"))?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let code = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Err(AuthError::new("OAuth sign-in timed out; start again")),
            entered = &mut manual => {
                let entered = entered.map_err(|_| AuthError::new("Authorization was cancelled"))?;
                break callback_code(flow,&entered,&state,true)?;
            }
            accepted = ipv4.accept() => {
                let (stream,_) = accepted.map_err(|_| AuthError::new("OAuth callback listener failed"))?;
                match callback_request(flow,stream,&state).await {Ok(Some(code))=>break code,Err(error) if error.status==ProviderStatus::AuthRequired=>return Err(error),_=>{}}
            }
            accepted = accept_optional(&ipv6) => {
                let stream = accepted.map_err(|_| AuthError::new("IPv6 OAuth callback listener failed"))?;
                match callback_request(flow,stream,&state).await {Ok(Some(code))=>break code,Err(error) if error.status==ProviderStatus::AuthRequired=>return Err(error),_=>{}}
            }
        }
    };
    let request = match flow.provider {
        Provider::Claude => client.post(CLAUDE_TOKEN).json(&ClaudeExchange {
            grant_type: "authorization_code",
            client_id: CLAUDE_CLIENT,
            code: &code,
            redirect_uri: flow.redirect,
            code_verifier: &verifier,
            state: &state,
        }),
        Provider::Codex => client.post(CODEX_TOKEN).form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT),
            ("code", code.as_str()),
            ("redirect_uri", flow.redirect),
            ("code_verifier", verifier.as_str()),
        ]),
        Provider::Grok | Provider::Kimi => unreachable!("provider uses device authorization"),
    };
    let response = request
        .send()
        .await
        .map_err(|_| AuthError::new("OAuth token exchange request failed"))?;
    let (status, reply): (_, TokenReply) = net::json(response, TOKEN_LIMIT).await?;
    if !status.is_success() && reply.error.is_none() {
        return Err(AuthError::new("OAuth token exchange was rejected"));
    }
    token(reply, None)
}

#[derive(Deserialize)]
struct DeviceReply {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval: Option<u64>,
    expires_in: Option<u64>,
}

fn verification_url(url: &str, provider: Provider) -> Result<(), AuthError> {
    let url = Url::parse(url).map_err(|_| AuthError::new("Invalid OAuth verification URL"))?;
    let host = url.host_str().unwrap_or("");
    let domain = match provider {
        Provider::Grok => "x.ai",
        Provider::Kimi => "kimi.com",
        _ => return Err(AuthError::new("Provider does not use device authorization")),
    };
    if url.scheme() != "https"
        || !(host == domain || host.ends_with(&format!(".{domain}")))
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AuthError::new("Unexpected OAuth verification origin"));
    }
    Ok(())
}

fn kimi_headers(device_id: &str) -> Result<reqwest::header::HeaderMap, AuthError> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in [
        ("x-msh-platform", "llm-quota-monitor"),
        ("x-msh-version", env!("CARGO_PKG_VERSION")),
        ("x-msh-device-name", "Windows quota monitor"),
        ("x-msh-device-model", std::env::consts::ARCH),
        ("x-msh-os-version", "windows"),
        ("x-msh-device-id", device_id),
    ] {
        headers.insert(
            reqwest::header::HeaderName::from_static(name),
            reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| AuthError::new("Invalid Kimi device identity"))?,
        );
    }
    Ok(headers)
}

async fn device_login(
    provider: Provider,
    client: &Client,
    prompts: mpsc::Sender<LoginPrompt>,
) -> Result<Credentials, AuthError> {
    let device_id = (provider == Provider::Kimi).then(random_value);
    let mut request = client.post(match provider {
        Provider::Grok => "https://auth.x.ai/oauth2/device/code",
        Provider::Kimi => KIMI_DEVICE,
        _ => return Err(AuthError::new("Provider does not use device authorization")),
    });
    if provider == Provider::Kimi {
        request = request
            .headers(kimi_headers(device_id.as_deref().expect("Kimi device id"))?)
            .form(&[("client_id", KIMI_CLIENT)]);
    } else {
        request = request.form(&[
            ("client_id", XAI_CLIENT),
            (
                "scope",
                "openid profile email offline_access grok-cli:access api:access",
            ),
        ]);
    }
    let response = request.send().await.map_err(|_| {
        AuthError::new(match provider {
            Provider::Kimi => "Kimi device authorization request failed",
            _ => "xAI device authorization request failed",
        })
    })?;
    if !response.status().is_success() {
        return Err(AuthError::new(match provider {
            Provider::Kimi => "Kimi device authorization was rejected",
            _ => "xAI device authorization was rejected",
        }));
    }
    let (_, mut device): (_, DeviceReply) = net::json(response, TOKEN_LIMIT).await?;
    let device_code = Zeroizing::new(std::mem::take(&mut device.device_code));
    let expires_in = device
        .expires_in
        .filter(|seconds| *seconds > 0 && *seconds <= 3600);
    if device_code.is_empty() || device.user_code.is_empty() || expires_in.is_none() {
        return Err(AuthError::new(
            "Provider returned invalid device authorization fields",
        ));
    }
    let url = device
        .verification_uri_complete
        .take()
        .unwrap_or(device.verification_uri);
    verification_url(&url, provider)?;
    let mut interval = device.interval.unwrap_or(5);
    if interval == 0 || interval > 300 {
        return Err(AuthError::new(
            "Provider returned an invalid polling interval",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(expires_in.expect("validated expiry"));
    prompts
        .send(LoginPrompt {
            provider,
            url,
            instructions: format!(
                "Подтвердите вход {} в браузере. Код: {}",
                provider.name(),
                device.user_code
            ),
            manual_code: false,
        })
        .await
        .map_err(|_| AuthError::new("Authorization was cancelled"))?;
    loop {
        let delay = interval.min(deadline.saturating_duration_since(Instant::now()).as_secs());
        tokio::time::sleep(Duration::from_secs(delay)).await;
        if Instant::now() >= deadline {
            return Err(AuthError::new("Device code expired; start sign-in again"));
        }
        let client_id = match provider {
            Provider::Grok => XAI_CLIENT,
            Provider::Kimi => KIMI_CLIENT,
            _ => unreachable!("validated device provider"),
        };
        let mut request = client.post(match provider {
            Provider::Grok => XAI_TOKEN,
            Provider::Kimi => KIMI_TOKEN,
            _ => unreachable!("validated device provider"),
        });
        if provider == Provider::Kimi {
            request = request.headers(kimi_headers(device_id.as_deref().expect("Kimi device id"))?);
        }
        let response = request
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", device_code.as_str()),
            ])
            .send()
            .await
            .map_err(|_| AuthError::new("OAuth device token polling failed"))?;
        let (status, reply): (_, TokenReply) = net::json(response, TOKEN_LIMIT).await?;
        match reply.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval = reply
                    .interval
                    .filter(|seconds| *seconds > 0 && *seconds <= 300)
                    .unwrap_or_else(|| interval.saturating_add(5));
                continue;
            }
            Some("access_denied") => {
                return Err(AuthError::new("Device authorization was denied"));
            }
            Some("expired_token") => {
                return Err(AuthError::new("Device code expired; start sign-in again"));
            }
            _ => {}
        }
        if !status.is_success() && reply.error.is_none() {
            return Err(AuthError::new("OAuth token exchange was rejected"));
        }
        let mut credentials = token(reply, None)?;
        credentials.device_id = device_id;
        return Ok(credentials);
    }
}

pub async fn login(
    provider: Provider,
    client: Client,
    prompts: mpsc::Sender<LoginPrompt>,
    manual_code: oneshot::Receiver<Zeroizing<String>>,
) -> Result<Credentials, AuthError> {
    match provider {
        Provider::Claude => code_login(&CLAUDE_FLOW, &client, prompts, manual_code).await,
        Provider::Codex => code_login(&CODEX_FLOW, &client, prompts, manual_code).await,
        Provider::Grok | Provider::Kimi => {
            drop(manual_code);
            device_login(provider, &client, prompts).await
        }
    }
}

pub async fn refresh(
    provider: Provider,
    client: &Client,
    credentials: &Credentials,
) -> Result<Credentials, AuthError> {
    let request = match provider {
        Provider::Claude => client
            .post(CLAUDE_TOKEN)
            .header("anthropic-beta", "oauth-2025-04-20")
            .json(&ClaudeRefresh {
                grant_type: "refresh_token",
                client_id: CLAUDE_CLIENT,
                refresh_token: &credentials.refresh_token,
            }),
        Provider::Codex => client.post(CODEX_TOKEN).form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CODEX_CLIENT),
            ("refresh_token", credentials.refresh_token.as_str()),
        ]),
        Provider::Grok => client.post(XAI_TOKEN).form(&[
            ("grant_type", "refresh_token"),
            ("client_id", XAI_CLIENT),
            ("refresh_token", credentials.refresh_token.as_str()),
        ]),
        Provider::Kimi => {
            let device_id = credentials
                .device_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AuthError::new("Kimi authorization has no device identity"))?;
            client
                .post(KIMI_TOKEN)
                .headers(kimi_headers(device_id)?)
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", KIMI_CLIENT),
                    ("refresh_token", credentials.refresh_token.as_str()),
                ])
        }
    };
    let response = request
        .send()
        .await
        .map_err(|_| AuthError::new("OAuth refresh request failed"))?;
    let (status, reply): (_, TokenReply) = net::json(response, TOKEN_LIMIT).await?;
    if !status.is_success() && reply.error.is_none() {
        return Err(AuthError::new("OAuth refresh was rejected"));
    }
    token(reply, Some(credentials))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_requires_exact_target_and_state_and_rejects_duplicates() {
        assert_eq!(
            callback_code(
                &CLAUDE_FLOW,
                "http://localhost:54545/callback?code=abc&state=nonce",
                "nonce",
                false
            )
            .unwrap()
            .as_str(),
            "abc"
        );
        for input in [
            "http://localhost:54545/callback?code=abc&state=other",
            "http://example.com:54545/callback?code=abc&state=nonce",
            "http://localhost:54545/other?code=abc&state=nonce",
            "http://localhost:54545/callback?code=abc&state=nonce&state=nonce",
            "http://localhost:54545/callback?code=abc&code=other&state=nonce",
        ] {
            assert!(callback_code(&CLAUDE_FLOW, input, "nonce", false).is_err());
        }
        assert!(callback_code(&CLAUDE_FLOW, "abc#wrong", "nonce", true).is_err());
        assert_eq!(
            callback_code(&CLAUDE_FLOW, "abc#nonce", "nonce", true)
                .unwrap()
                .as_str(),
            "abc"
        );
    }
    #[test]
    fn codex_callback_is_bound_to_exact_provider_target_and_denial_finishes_the_flow() {
        let url = "http://localhost:1455/auth/callback?code=codex-code&state=nonce";
        assert_eq!(
            callback_code(&CODEX_FLOW, url, "nonce", false)
                .unwrap()
                .as_str(),
            "codex-code"
        );
        assert!(callback_code(&CLAUDE_FLOW, url, "nonce", false).is_err());
        assert!(callback_code(&CODEX_FLOW, url, "different", false).is_err());
        let error = callback_code(
            &CODEX_FLOW,
            "http://localhost:1455/auth/callback?error=access_denied&state=nonce",
            "nonce",
            false,
        )
        .err()
        .unwrap();
        assert_eq!(error.status, ProviderStatus::AuthRequired);
    }
    #[test]
    fn pkce_matches_rfc7636_vector_and_url_contains_no_verifier() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let url = authorization_url(&CLAUDE_FLOW, "nonce", verifier);
        let params = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            params["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(params["redirect_uri"], CLAUDE_REDIRECT);
        assert!(!url.as_str().contains(verifier));
        assert!(verification_url("https://accounts.x.ai/device", Provider::Grok).is_ok());
        assert!(
            verification_url("https://accounts.x.ai.evil.example/device", Provider::Grok).is_err()
        );
        assert!(verification_url("http://auth.x.ai/device", Provider::Grok).is_err());
        assert!(verification_url("https://www.kimi.com/code/device", Provider::Kimi).is_ok());
        assert!(
            verification_url("https://www.kimi.com.evil.example/device", Provider::Kimi).is_err()
        );
    }
    #[test]
    fn token_rotation_retains_old_refresh_only_when_omitted_and_revocation_is_explicit() {
        let old = Credentials {
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            expires_at: 1,
            device_id: Some("stable-device".into()),
        };
        let reply: TokenReply =
            serde_json::from_str(r#"{"access_token":"new-access","expires_in":3600}"#).unwrap();
        let rotated = token(reply, Some(&old)).unwrap();
        assert_eq!(rotated.refresh_token, "old-refresh");
        assert_eq!(rotated.device_id.as_deref(), Some("stable-device"));
        let reply: TokenReply = serde_json::from_str(
            r#"{"access_token":"new-access","refresh_token":"rotated","expires_in":3600}"#,
        )
        .unwrap();
        assert_eq!(token(reply, Some(&old)).unwrap().refresh_token, "rotated");
        let reply: TokenReply = serde_json::from_str(r#"{"error":"invalid_grant"}"#).unwrap();
        assert!(token(reply, Some(&old)).err().unwrap().revoked);
        let reply: TokenReply = serde_json::from_str(r#"{"access_token":"new-access"}"#).unwrap();
        assert!(token(reply, Some(&old)).is_err());
    }
    #[test]
    fn jwt_expiry_is_used_only_when_reported_and_empty_rotation_is_rejected() {
        let expiry = OffsetDateTime::now_utc().unix_timestamp() + 3600;
        let payload = URL_SAFE_NO_PAD.encode(format!("{{\"exp\":{expiry}}}"));
        let reply:TokenReply=serde_json::from_value(serde_json::json!({"access_token":format!("header.{payload}.signature"),"refresh_token":"refresh"})).unwrap();
        assert_eq!(token(reply, None).unwrap().expires_at, expiry);
        let reply: TokenReply = serde_json::from_value(
            serde_json::json!({"access_token":"opaque","refresh_token":"","expires_in":3600}),
        )
        .unwrap();
        let previous = Credentials {
            access_token: "old".into(),
            refresh_token: "previous".into(),
            expires_at: 1,
            device_id: None,
        };
        assert!(token(reply, Some(&previous)).is_err());
    }
}
