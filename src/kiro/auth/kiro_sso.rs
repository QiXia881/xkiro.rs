use std::future;
use std::net::{IpAddr, TcpListener};
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::{Client, Proxy, Url, redirect::Policy};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::auth::{oauth_callback, social};
use crate::kiro::model::token_refresh::SocialCreateTokenResponse;
use crate::model::config::{Config, TlsBackend};

const KIRO_SIGN_IN_BASE_URL: &str = "https://app.kiro.dev/signin";
const KIRO_REDIRECT_URI: &str = "http://localhost:3128";
const KIRO_REDIRECT_PORT: u16 = 3128;
const KIRO_REDIRECT_FROM: &str = "KiroIDE";
const KIRO_OAUTH_CALLBACK_PATH: &str = "/oauth/callback";
const KIRO_SOCIAL_TOKEN_URL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token";
const LOGIN_TIMEOUT_SECS: u64 = 10 * 60;
const MICROSOFT_ISSUER_SUFFIXES: &[&str] = &[
    ".microsoftonline.com",
    ".microsoftonline.us",
    ".microsoftonline.cn",
];

#[derive(Debug)]
pub struct StartKiroSsoSession {
    pub sign_in_url: String,
    pub callback_rx: oneshot::Receiver<KiroSsoCapture>,
    pub manual_callback_tx: mpsc::Sender<ManualCallbackRequest>,
    pub server_handle: Option<ServerHandle>,
}

#[derive(Debug)]
pub struct KiroSsoCapture {
    pub kind: KiroSsoCaptureKind,
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    pub token_endpoint: Option<String>,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub scopes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KiroSsoCaptureKind {
    Social,
    ExternalIdp,
}

#[derive(Debug)]
pub struct ManualCallbackRequest {
    pub callback_url: String,
    pub response_tx: oneshot::Sender<ManualCallbackResult>,
}

#[derive(Debug)]
pub enum ManualCallbackResult {
    Pending,
    Redirect(String),
    Submitted,
    Failed(String),
}

pub struct ServerHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl std::fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerHandle").finish_non_exhaustive()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Debug, Clone)]
struct ExternalLeg {
    state: String,
    verifier: String,
    token_endpoint: String,
    issuer_url: String,
    client_id: String,
    scopes: String,
    redirect_uri: String,
}

#[derive(Debug)]
enum CallbackOutcome {
    Pending,
    Redirect(String),
    Captured(KiroSsoCapture),
    Failed(anyhow::Error),
}

pub fn start_login(
    config: &Config,
    proxy: Option<ProxyConfig>,
) -> anyhow::Result<StartKiroSsoSession> {
    start_login_impl(config, proxy, true)
}

pub fn start_login_manual(
    config: &Config,
    proxy: Option<ProxyConfig>,
) -> anyhow::Result<StartKiroSsoSession> {
    start_login_impl(config, proxy, false)
}

fn start_login_impl(
    config: &Config,
    proxy: Option<ProxyConfig>,
    enable_loopback_listener: bool,
) -> anyhow::Result<StartKiroSsoSession> {
    let (verifier, challenge) = social::generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let (callback_tx, callback_rx) = oneshot::channel();
    let (manual_callback_tx, manual_callback_rx) = mpsc::channel(4);

    let sign_in_url = build_kiro_sign_in_url(&state, &challenge)?;

    let server_state = KiroSsoServerState {
        state,
        verifier,
        proxy,
        tls_backend: config.tls_backend,
        leg2: None,
    };
    let server_handle = if enable_loopback_listener {
        match bind_callback_listeners() {
            Ok(listeners) => {
                let (shutdown_tx, shutdown_rx) = oneshot::channel();
                tokio::spawn(async move {
                    run_callback_server(
                        listeners,
                        server_state,
                        callback_tx,
                        manual_callback_rx,
                        shutdown_rx,
                    )
                    .await;
                });
                Some(ServerHandle {
                    shutdown_tx: Some(shutdown_tx),
                })
            }
            Err(error) => {
                tracing::warn!("Kiro SSO 本地回调端口不可用，仅启用手动回调: {}", error);
                tokio::spawn(async move {
                    run_manual_callback_server(server_state, callback_tx, manual_callback_rx).await;
                });
                None
            }
        }
    } else {
        tokio::spawn(async move {
            run_manual_callback_server(server_state, callback_tx, manual_callback_rx).await;
        });
        None
    };

    Ok(StartKiroSsoSession {
        sign_in_url,
        callback_rx,
        manual_callback_tx,
        server_handle,
    })
}

fn build_kiro_sign_in_url(state: &str, challenge: &str) -> anyhow::Result<String> {
    let mut sign_in = Url::parse(KIRO_SIGN_IN_BASE_URL)?;
    sign_in
        .query_pairs_mut()
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("redirect_uri", KIRO_REDIRECT_URI)
        .append_pair("redirect_from", KIRO_REDIRECT_FROM);
    Ok(sign_in.to_string())
}

struct KiroSsoServerState {
    state: String,
    verifier: String,
    proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
    leg2: Option<ExternalLeg>,
}

fn bind_callback_listeners() -> anyhow::Result<Vec<std::net::TcpListener>> {
    if let Ok(bind_host) = std::env::var("KIRO_SSO_CALLBACK_BIND") {
        let listener = bind_callback_listener(&bind_host)?;
        return Ok(vec![listener]);
    }

    let primary = bind_callback_listener("127.0.0.1")?;
    let mut listeners = vec![primary];
    match bind_callback_listener("::1") {
        Ok(listener) => listeners.push(listener),
        Err(error) => tracing::debug!("Kiro SSO IPv6 回调端口不可用，跳过: {}", error),
    }
    Ok(listeners)
}

fn bind_callback_listener(bind_host: &str) -> anyhow::Result<std::net::TcpListener> {
    let listener = TcpListener::bind((bind_host, KIRO_REDIRECT_PORT))
        .with_context(|| format!("无法绑定 Kiro SSO 回调端口 {}", KIRO_REDIRECT_PORT))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn run_callback_server(
    std_listeners: Vec<std::net::TcpListener>,
    mut state: KiroSsoServerState,
    callback_tx: oneshot::Sender<KiroSsoCapture>,
    mut manual_callback_rx: mpsc::Receiver<ManualCallbackRequest>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let mut listeners = Vec::with_capacity(std_listeners.len());
    for std_listener in std_listeners {
        match tokio::net::TcpListener::from_std(std_listener) {
            Ok(listener) => listeners.push(listener),
            Err(error) => tracing::error!("Kiro SSO 回调服务器初始化失败: {}", error),
        }
    }
    if listeners.is_empty() {
        return;
    }
    let mut callback_tx = Some(callback_tx);
    let deadline = tokio::time::sleep(Duration::from_secs(LOGIN_TIMEOUT_SECS));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            result = accept_callback_connection(&listeners) => {
                let (mut stream, _) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::debug!("Kiro SSO 回调 accept 失败: {}", error);
                        break;
                    }
                };

                let mut buf = vec![0u8; 8192];
                let n = match stream.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let Some(path_and_query) = request.lines().next().and_then(parse_request_target) else {
                    let _ = write_response(&mut stream, "HTTP/1.1 404 Not Found", "", None).await;
                    continue;
                };

                match handle_callback(&mut state, path_and_query).await {
                    CallbackOutcome::Pending => {
                        let _ = write_response(&mut stream, "HTTP/1.1 204 No Content", "", None).await;
                    }
                    CallbackOutcome::Redirect(location) => {
                        let _ = write_response(&mut stream, "HTTP/1.1 302 Found", "", Some(&location)).await;
                    }
                    CallbackOutcome::Captured(capture) => {
                        let _ = write_callback_page(&mut stream, true).await;
                        if let Some(tx) = callback_tx.take() {
                            let _ = tx.send(capture);
                        }
                        break;
                    }
                    CallbackOutcome::Failed(error) => {
                        tracing::warn!("Kiro SSO 回调失败: {}", error);
                        let _ = write_callback_page(&mut stream, false).await;
                        break;
                    }
                }
            }
            request = manual_callback_rx.recv() => {
                let Some(request) = request else {
                    break;
                };
                if process_manual_callback(&mut state, request, &mut callback_tx).await {
                    break;
                }
            }
            _ = &mut shutdown_rx => break,
            _ = &mut deadline => break,
        };
    }
}

async fn accept_callback_connection(
    listeners: &[tokio::net::TcpListener],
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match listeners {
        [primary, secondary, ..] => {
            tokio::select! {
                result = primary.accept() => result,
                result = secondary.accept() => result,
            }
        }
        [primary] => primary.accept().await,
        [] => future::pending().await,
    }
}

async fn run_manual_callback_server(
    mut state: KiroSsoServerState,
    callback_tx: oneshot::Sender<KiroSsoCapture>,
    mut manual_callback_rx: mpsc::Receiver<ManualCallbackRequest>,
) {
    let mut callback_tx = Some(callback_tx);
    let deadline = tokio::time::sleep(Duration::from_secs(LOGIN_TIMEOUT_SECS));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            request = manual_callback_rx.recv() => {
                let Some(request) = request else {
                    break;
                };
                if process_manual_callback(&mut state, request, &mut callback_tx).await {
                    break;
                }
            }
            _ = &mut deadline => break,
        }
    }
}

async fn process_manual_callback(
    state: &mut KiroSsoServerState,
    request: ManualCallbackRequest,
    callback_tx: &mut Option<oneshot::Sender<KiroSsoCapture>>,
) -> bool {
    let (result, should_finish) = match callback_target_from_input(&request.callback_url) {
        Ok(path_and_query) => match handle_callback(state, &path_and_query).await {
            CallbackOutcome::Pending => (ManualCallbackResult::Pending, false),
            CallbackOutcome::Redirect(location) => {
                (ManualCallbackResult::Redirect(location), false)
            }
            CallbackOutcome::Captured(capture) => {
                if let Some(tx) = callback_tx.take() {
                    let _ = tx.send(capture);
                }
                (ManualCallbackResult::Submitted, true)
            }
            CallbackOutcome::Failed(error) => {
                let message = error.to_string();
                tracing::warn!("Kiro SSO 手动回调失败: {}", message);
                (ManualCallbackResult::Failed(message), true)
            }
        },
        Err(error) => (ManualCallbackResult::Failed(error.to_string()), false),
    };
    let _ = request.response_tx.send(result);
    should_finish
}

fn parse_request_target(first_line: &str) -> Option<&str> {
    first_line
        .strip_prefix("GET ")?
        .split_once(" HTTP/")
        .map(|(target, _)| target)
}

fn callback_target_from_input(input: &str) -> anyhow::Result<String> {
    oauth_callback::target_from_input(input)
}

async fn handle_callback(state: &mut KiroSsoServerState, path_and_query: &str) -> CallbackOutcome {
    let parsed = match oauth_callback::parse_target(path_and_query) {
        Ok(value) => value,
        Err(error) => return CallbackOutcome::Failed(error),
    };
    let path = parsed.path.as_str();
    let params = parsed.params;

    if is_external_idp_descriptor(path, &params) {
        if state.leg2.is_some() {
            return CallbackOutcome::Pending;
        }
        let issuer_url = params
            .get("issuer_url")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let client_id = params
            .get("client_id")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let scopes = params.get("scopes").cloned().unwrap_or_default();
        let login_hint = params.get("login_hint").cloned().unwrap_or_default();
        if client_id.is_empty() {
            return CallbackOutcome::Failed(anyhow::anyhow!(
                "external IdP descriptor 缺少 client_id"
            ));
        }
        let (auth_endpoint, token_endpoint) =
            match oidc_discover(issuer_url, state.proxy.as_ref(), state.tls_backend).await {
                Ok(value) => value,
                Err(error) => return CallbackOutcome::Failed(error),
            };
        let (verifier, challenge) = social::generate_pkce();
        let state2 = uuid::Uuid::new_v4().to_string();
        let redirect_uri = format!("{}{}", KIRO_REDIRECT_URI, KIRO_OAUTH_CALLBACK_PATH);
        let auth_url = external_idp_authorize_url(
            &auth_endpoint,
            client_id,
            &redirect_uri,
            &scopes,
            &challenge,
            &state2,
            &login_hint,
        );
        state.leg2 = Some(ExternalLeg {
            state: state2,
            verifier,
            token_endpoint,
            issuer_url: issuer_url.to_string(),
            client_id: client_id.to_string(),
            scopes,
            redirect_uri,
        });
        return CallbackOutcome::Redirect(auth_url);
    }

    if path == KIRO_OAUTH_CALLBACK_PATH {
        let Some(leg2) = state.leg2.as_ref() else {
            return CallbackOutcome::Pending;
        };
        if params.get("state").map(String::as_str) != Some(leg2.state.as_str()) {
            return CallbackOutcome::Pending;
        }
        if let Some(error) = params.get("error") {
            return CallbackOutcome::Failed(anyhow::anyhow!(
                "external IdP authorization error: {} {}",
                error,
                params
                    .get("error_description")
                    .map(String::as_str)
                    .unwrap_or("")
            ));
        }
        let Some(code) = params.get("code").filter(|v| !v.trim().is_empty()) else {
            return CallbackOutcome::Pending;
        };
        return CallbackOutcome::Captured(KiroSsoCapture {
            kind: KiroSsoCaptureKind::ExternalIdp,
            code: code.to_string(),
            code_verifier: leg2.verifier.clone(),
            redirect_uri: leg2.redirect_uri.clone(),
            token_endpoint: Some(leg2.token_endpoint.clone()),
            issuer_url: Some(leg2.issuer_url.clone()),
            client_id: Some(leg2.client_id.clone()),
            scopes: Some(leg2.scopes.clone()),
        });
    }

    if params.get("state").map(String::as_str) != Some(state.state.as_str()) {
        return CallbackOutcome::Pending;
    }
    if let Some(error) = params.get("error") {
        return CallbackOutcome::Failed(anyhow::anyhow!(
            "SSO authorization error: {} {}",
            error,
            params
                .get("error_description")
                .map(String::as_str)
                .unwrap_or("")
        ));
    }
    let Some(code) = params.get("code").filter(|v| !v.trim().is_empty()) else {
        return CallbackOutcome::Pending;
    };
    CallbackOutcome::Captured(KiroSsoCapture {
        kind: KiroSsoCaptureKind::Social,
        code: code.to_string(),
        code_verifier: state.verifier.clone(),
        redirect_uri: KIRO_REDIRECT_URI.to_string(),
        token_endpoint: None,
        issuer_url: None,
        client_id: None,
        scopes: None,
    })
}

fn is_external_idp_descriptor(
    path: &str,
    params: &std::collections::HashMap<String, String>,
) -> bool {
    path != KIRO_OAUTH_CALLBACK_PATH
        && (params
            .get("login_option")
            .is_some_and(|v| v.eq_ignore_ascii_case("external_idp"))
            || params
                .get("issuer_url")
                .is_some_and(|v| !v.trim().is_empty()))
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
    location: Option<&str>,
) -> std::io::Result<()> {
    let location_header = location
        .map(|v| format!("Location: {}\r\n", v))
        .unwrap_or_default();
    let response = format!(
        "{}\r\n{}Content-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        location_header,
        body.len(),
        body,
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

async fn write_callback_page(stream: &mut tokio::net::TcpStream, ok: bool) -> std::io::Result<()> {
    let msg = if ok {
        "Kiro sign-in complete. You can close this tab and return to the admin panel."
    } else {
        "Kiro sign-in failed. Return to the admin panel and try again."
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Kiro Sign-In</title></head><body style=\"font-family:sans-serif;padding:2rem\"><p>{}</p></body></html>",
        msg
    );
    write_response(stream, "HTTP/1.1 200 OK", &body, None).await
}

pub fn validate_external_idp_endpoint(raw_url: &str) -> anyhow::Result<()> {
    let url = Url::parse(raw_url.trim()).context("invalid external IdP URL")?;
    if url.scheme() != "https" {
        bail!("external IdP URL must be https");
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() || host.parse::<IpAddr>().is_ok() {
        bail!("external IdP host is invalid");
    }
    if !MICROSOFT_ISSUER_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        bail!("external IdP host {:?} is not allow-listed", host);
    }
    Ok(())
}

fn issuer_from_access_token_jwt(access_token: &str) -> Option<String> {
    let payload = access_token.trim().split('.').nth(1)?;
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
            .or_else(|_| {
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload)
            })
            .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("iss")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

pub fn exp_from_access_token_jwt(access_token: &str) -> Option<i64> {
    let payload = access_token.trim().split('.').nth(1)?;
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
            .or_else(|_| {
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload)
            })
            .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("exp").and_then(|value| value.as_i64())
}

pub fn derive_external_idp_endpoints(
    user_id: &str,
    client_id: &str,
    access_token: &str,
) -> Option<(String, String, String)> {
    let source = if user_id.trim().is_empty() {
        issuer_from_access_token_jwt(access_token)?
    } else {
        user_id.trim().to_string()
    };
    let url = Url::parse(&source).ok()?;
    let host = url.host_str()?;
    let tenant = url
        .path()
        .trim_matches('/')
        .split('/')
        .next()
        .filter(|value| !value.is_empty())?;
    let scheme = if url.scheme().is_empty() {
        "https"
    } else {
        url.scheme()
    };

    let token_endpoint = format!("{scheme}://{host}/{tenant}/oauth2/v2.0/token");
    let issuer_url = format!("{scheme}://{host}/{tenant}/v2.0");
    let scopes = if client_id.trim().is_empty() {
        String::new()
    } else {
        format!(
            "api://{}/codewhisperer:conversations api://{}/codewhisperer:completions offline_access",
            client_id.trim(),
            client_id.trim()
        )
    };
    Some((token_endpoint, issuer_url, scopes))
}

async fn oidc_discover(
    issuer_url: &str,
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<(String, String)> {
    validate_external_idp_endpoint(issuer_url)?;
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    let client = discovery_client(proxy, tls_backend)?;
    let resp = client
        .get(discovery_url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("OIDC discovery request failed")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("OIDC discovery failed (status {})", status);
    }
    #[derive(Deserialize)]
    struct DiscoveryDoc {
        authorization_endpoint: String,
        token_endpoint: String,
    }
    let doc = resp
        .json::<DiscoveryDoc>()
        .await
        .context("failed to parse OIDC discovery document")?;
    validate_external_idp_endpoint(&doc.authorization_endpoint)
        .context("discovered authorization_endpoint rejected")?;
    validate_external_idp_endpoint(&doc.token_endpoint)
        .context("discovered token_endpoint rejected")?;
    Ok((doc.authorization_endpoint, doc.token_endpoint))
}

fn discovery_client(
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(Policy::none());
    match tls_backend {
        TlsBackend::Rustls => builder = builder.use_rustls_tls(),
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }
    if let Some(proxy_config) = proxy {
        let mut reqwest_proxy = Proxy::all(&proxy_config.url)?;
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            reqwest_proxy = reqwest_proxy.basic_auth(username, password);
        }
        builder = builder.proxy(reqwest_proxy);
    }
    Ok(builder.build()?)
}

fn external_idp_authorize_url(
    auth_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &str,
    challenge: &str,
    state: &str,
    login_hint: &str,
) -> String {
    let mut url = Url::parse(auth_endpoint).expect("validated OIDC authorization endpoint");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", scopes)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("response_mode", "query")
            .append_pair("state", state);
        if !login_hint.trim().is_empty() {
            query.append_pair("login_hint", login_hint);
        }
    }
    url.to_string()
}

fn build_kiro_desktop_user_agent(kiro_version: &str, machine_id: &str) -> String {
    format!("KiroIDE-{}-{}", kiro_version, machine_id)
}

pub async fn exchange_social_code(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    machine_id: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<SocialCreateTokenResponse> {
    let client = build_client(proxy, 30, config.tls_backend)?;
    let body = serde_json::json!({
        "code": code.trim(),
        "code_verifier": code_verifier,
        "redirect_uri": redirect_uri,
    });
    let resp = client
        .post(KIRO_SOCIAL_TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            build_kiro_desktop_user_agent(&config.kiro_version, machine_id),
        )
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        bail!(
            "Kiro SSO social token exchange failed {}: {}",
            status,
            body_text
        );
    }
    resp.json::<SocialCreateTokenResponse>()
        .await
        .map_err(Into::into)
}

pub async fn exchange_external_idp_code(
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    scopes: Option<&str>,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<crate::kiro::model::token_refresh::ExternalIdpTokenResponse> {
    let client = build_client(proxy, 30, config.tls_backend)?;
    let mut form = vec![
        ("client_id", client_id),
        ("grant_type", "authorization_code"),
        ("code", code.trim()),
        ("redirect_uri", redirect_uri),
        ("code_verifier", code_verifier),
    ];
    if let Some(scopes) = scopes.filter(|v| !v.trim().is_empty()) {
        form.push(("scope", scopes));
    }
    let resp = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await?;
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    let data: crate::kiro::model::token_refresh::ExternalIdpTokenResponse =
        serde_json::from_str(&body_text).context("解析 External IdP token 响应失败")?;
    if !status.is_success() || data.access_token.is_empty() {
        bail!(
            "External IdP token exchange failed {}: {}",
            status,
            crate::common::redact::redact_secret_text(&body_text)
        );
    }
    Ok(data)
}

pub fn extract_email_from_jwt(access_token: &str) -> Option<String> {
    let payload = access_token.trim().split('.').nth(1)?;
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
            .or_else(|_| {
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload)
            })
            .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    ["email", "preferred_username", "upn", "unique_name"]
        .iter()
        .find_map(|key| claims.get(*key)?.as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::{
        KIRO_REDIRECT_FROM, KIRO_REDIRECT_URI, build_kiro_desktop_user_agent,
        build_kiro_sign_in_url, callback_target_from_input, derive_external_idp_endpoints,
        exp_from_access_token_jwt, external_idp_authorize_url, is_external_idp_descriptor,
        start_login_manual,
    };
    use crate::kiro::auth::oauth_callback::{parse_query_string, split_path_query};
    use crate::model::config::Config;
    use base64::Engine;
    use std::collections::HashMap;

    #[test]
    fn callback_target_accepts_full_localhost_url() {
        let target =
            callback_target_from_input("http://localhost:3128/oauth/callback?code=abc&state=xyz")
                .unwrap();
        assert_eq!(target, "/oauth/callback?code=abc&state=xyz");
    }

    #[test]
    fn callback_target_accepts_path_and_query() {
        let target = callback_target_from_input("/oauth/callback?code=abc").unwrap();
        assert_eq!(target, "/oauth/callback?code=abc");
    }

    #[tokio::test]
    async fn manual_login_does_not_bind_callback_port() {
        let session = start_login_manual(&Config::default(), None).unwrap();
        assert!(session.server_handle.is_none());
        assert!(
            session
                .sign_in_url
                .starts_with("https://app.kiro.dev/signin?")
        );
    }

    #[test]
    fn kiro_web_external_idp_descriptor_is_recognized() {
        let target = callback_target_from_input(
            "http://localhost:3128/signin/callback?login_option=external_idp&issuer_url=https%3A%2F%2Flogin.microsoftonline.com%2Fcommon%2Fv2.0&client_id=client-1&state=state-1&scopes=openid+profile+email",
        )
        .unwrap();
        let (path, query) = split_path_query(&target);
        let params = parse_query_string(query.unwrap_or_default());

        assert_eq!(path, "/signin/callback");
        assert!(is_external_idp_descriptor(path, &params));
        assert_eq!(
            params.get("issuer_url").map(String::as_str),
            Some("https://login.microsoftonline.com/common/v2.0")
        );
        assert_eq!(
            params.get("scopes").map(String::as_str),
            Some("openid profile email")
        );
    }

    #[test]
    fn kiro_sign_in_url_contains_required_oauth_params() {
        let raw_url = build_kiro_sign_in_url("state-1", "challenge-1").unwrap();
        let url = reqwest::Url::parse(&raw_url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(
            url.as_str().split('?').next().unwrap(),
            "https://app.kiro.dev/signin"
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state-1"));
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some("challenge-1")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(KIRO_REDIRECT_URI)
        );
        assert_eq!(
            params.get("redirect_from").map(String::as_str),
            Some(KIRO_REDIRECT_FROM)
        );
    }

    #[test]
    fn external_idp_authorize_url_contains_required_oauth_params() {
        let raw_url = external_idp_authorize_url(
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            "client-1",
            "http://localhost:3128/oauth/callback",
            "openid profile email",
            "challenge-1",
            "state-1",
            "user@example.com",
        );
        let url = reqwest::Url::parse(&raw_url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("client-1")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://localhost:3128/oauth/callback")
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some("openid profile email")
        );
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some("challenge-1")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("response_mode").map(String::as_str),
            Some("query")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state-1"));
        assert_eq!(
            params.get("login_hint").map(String::as_str),
            Some("user@example.com")
        );
    }

    #[test]
    fn kiro_sso_social_user_agent_includes_machine_id() {
        assert_eq!(
            build_kiro_desktop_user_agent("0.6.18", "machine-1"),
            "KiroIDE-0.6.18-machine-1"
        );
    }

    #[test]
    fn external_idp_endpoints_are_derived_from_user_id_or_access_token_issuer() {
        let user_id = "https://login.microsoftonline.com/5fbc183e-3d09-4043-b36f-0c49d3665977/v2.0.8db0e2eb-d491-4a1a-98f1-cbdc12bb60a0";
        let client_id = "fa6d79bf-cdaa-495e-8359-78aab7c7cd9b";
        let (token_endpoint, issuer_url, scopes) =
            derive_external_idp_endpoints(user_id, client_id, "").unwrap();

        assert_eq!(
            token_endpoint,
            "https://login.microsoftonline.com/5fbc183e-3d09-4043-b36f-0c49d3665977/oauth2/v2.0/token"
        );
        assert_eq!(
            issuer_url,
            "https://login.microsoftonline.com/5fbc183e-3d09-4043-b36f-0c49d3665977/v2.0"
        );
        assert!(scopes.contains(&format!("api://{client_id}/codewhisperer:conversations")));
        assert!(scopes.contains("offline_access"));

        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"iss":"{user_id}"}}"#));
        let jwt = format!("header.{payload}.sig");
        let (jwt_endpoint, jwt_issuer, _) =
            derive_external_idp_endpoints("", client_id, &jwt).unwrap();
        assert_eq!(jwt_endpoint, token_endpoint);
        assert_eq!(jwt_issuer, issuer_url);
    }

    #[test]
    fn access_token_exp_is_read_from_jwt_payload() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"exp":2000000000}"#);
        let jwt = format!("header.{payload}.sig");

        assert_eq!(exp_from_access_token_jwt(&jwt), Some(2_000_000_000));
        assert_eq!(exp_from_access_token_jwt(""), None);
        assert_eq!(exp_from_access_token_jwt("not-a-jwt"), None);
    }
}
