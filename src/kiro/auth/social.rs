use std::net::TcpListener;

use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::auth::oauth_callback;
use crate::kiro::model::token_refresh::{SocialCreateTokenRequest, SocialCreateTokenResponse};
use crate::model::config::Config;

pub const KIRO_AUTH_ENDPOINT: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";
pub const MANUAL_CALLBACK_PORT: u16 = 3128;
const GITHUB_CREDENTIAL_PROVIDER: &str = "GitHub";
const GITHUB_PORTAL_IDP: &str = "Github";
const GOOGLE_PROVIDER: &str = "Google";

// Mirrors the port list from Kiro IDE's portal-auth-provider
const CALLBACK_PORTS: &[u16] = &[
    3128, 4649, 6588, 8008, 9091, 49153, 50153, 51153, 52153, 53153,
];

#[derive(Debug, Clone)]
pub struct OAuthCallbackData {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocialProviderNames {
    pub credential_provider: &'static str,
    pub portal_idp: &'static str,
}

pub fn resolve_provider_names(provider: &str) -> anyhow::Result<SocialProviderNames> {
    let provider = provider.trim();
    match provider.to_ascii_lowercase().as_str() {
        "google" => Ok(SocialProviderNames {
            credential_provider: GOOGLE_PROVIDER,
            portal_idp: GOOGLE_PROVIDER,
        }),
        "github" => Ok(SocialProviderNames {
            credential_provider: GITHUB_CREDENTIAL_PROVIDER,
            portal_idp: GITHUB_PORTAL_IDP,
        }),
        _ => anyhow::bail!(
            "不支持的社交登录提供方: {}（应为 GitHub 或 Google）",
            provider
        ),
    }
}

// Drop sends shutdown signal to the callback server, releasing the port.
pub struct ServerHandle {
    _shutdown_tx: oneshot::Sender<()>,
}

pub fn start_callback_server(
    tx: oneshot::Sender<OAuthCallbackData>,
) -> anyhow::Result<(u16, ServerHandle)> {
    let (port, std_listener) = bind_available_port()?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        run_callback_server(std_listener, tx, shutdown_rx).await;
    });

    Ok((
        port,
        ServerHandle {
            _shutdown_tx: shutdown_tx,
        },
    ))
}

pub fn manual_redirect_uri() -> String {
    format!("http://127.0.0.1:{}/oauth/callback", MANUAL_CALLBACK_PORT)
}

fn bind_available_port() -> anyhow::Result<(u16, std::net::TcpListener)> {
    for &port in CALLBACK_PORTS {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                listener.set_nonblocking(true)?;
                return Ok((port, listener));
            }
            Err(_) => continue,
        }
    }
    anyhow::bail!(
        "所有回调端口均被占用，请确保没有其他程序占用 {:?}",
        CALLBACK_PORTS
    )
}

async fn run_callback_server(
    std_listener: std::net::TcpListener,
    tx: oneshot::Sender<OAuthCallbackData>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let port = std_listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let listener = match TcpListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("社交登录回调服务器初始化失败 (port {}): {}", port, e);
            return;
        }
    };

    tracing::info!("社交登录回调服务器已启动: http://127.0.0.1:{}", port);

    let mut tx = Some(tx);
    loop {
        let (mut stream, _addr) = tokio::select! {
            result = listener.accept() => match result {
                Ok(s) => s,
                Err(_) => break,
            },
            _ = &mut shutdown_rx => {
                tracing::info!("社交登录回调服务器关闭，端口 {} 已释放", port);
                break;
            }
        };

        let mut buf = vec![0u8; 4096];
        let n = match stream.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => continue,
        };

        let request = String::from_utf8_lossy(&buf[..n]);
        let first_line = request.lines().next().unwrap_or("");

        if let Some(path_and_query) = first_line.strip_prefix("GET ").and_then(|s| {
            s.strip_suffix(" HTTP/1.1")
                .or_else(|| s.strip_suffix(" HTTP/1.0"))
        }) {
            if let Some(callback) = parse_callback(path_and_query) {
                let body = "<html><head><meta charset='utf-8'><title>登录成功</title></head><body style='font-family:sans-serif;text-align:center;padding:60px'><h2>&#10003; 登录成功</h2><p>令牌已更新，请返回 xkiro.rs 管理界面。</p><p style='color:#888;font-size:13px'>此标签页可以关闭。</p></body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;

                if let Some(sender) = tx.take() {
                    let _ = sender.send(callback);
                }
                break;
            } else if path_and_query.starts_with("/oauth/callback")
                || path_and_query.starts_with("/signin/callback")
            {
                let error_msg = path_and_query
                    .split('?')
                    .nth(1)
                    .and_then(|q| {
                        let p = oauth_callback::parse_query_string(q);
                        p.get("error_description")
                            .or_else(|| p.get("error"))
                            .cloned()
                    })
                    .unwrap_or_else(|| "未知错误".to_string());

                let body = format!(
                    "<html><head><meta charset='utf-8'><title>登录失败</title></head><body style='font-family:sans-serif;text-align:center;padding:60px'><h2>&#10007; 登录失败</h2><p>{}</p></body></html>",
                    error_msg
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                break;
            }
        }

        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await;
    }
}

fn parse_callback(path_and_query: &str) -> Option<OAuthCallbackData> {
    let callback = oauth_callback::parse_target(path_and_query).ok()?;
    if callback.path != "/oauth/callback" && callback.path != "/signin/callback" {
        return None;
    }
    if callback.error_message().is_some() {
        return None;
    }

    let code = callback.code?;
    let state = callback.state.unwrap_or_default();

    Some(OAuthCallbackData { code, state })
}

pub fn callback_from_input(input: &str) -> anyhow::Result<OAuthCallbackData> {
    let callback = oauth_callback::parse_input(input)?;
    if callback.path != "/oauth/callback" && callback.path != "/signin/callback" {
        anyhow::bail!("无效的社交 OAuth 回调 URL");
    }
    if let Some(error) = callback.error_message() {
        anyhow::bail!("社交 OAuth 回调错误: {}", error);
    }
    let code = callback
        .code
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("社交 OAuth 回调缺少 code"))?;
    let state = callback.state.unwrap_or_default();
    Ok(OAuthCallbackData { code, state })
}
fn base64url_encode(data: &[u8]) -> String {
    let b64 = base64_encode_standard(data);
    b64.replace('+', "-").replace('/', "_").replace('=', "")
}

fn base64_encode_standard(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        out.push(CHARS[b0 >> 2] as char);
        out.push(CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((b1 & 0xf) << 2) | (b2 >> 6)] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[b2 & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = fastrand::u8(..).wrapping_add(i as u8);
    }
    let uuid_bytes = uuid::Uuid::new_v4().as_bytes().to_owned();
    for (i, b) in bytes.iter_mut().enumerate() {
        *b ^= uuid_bytes[i % 16];
    }

    let verifier = base64url_encode(&bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    let challenge = base64url_encode(&digest);

    (verifier, challenge)
}

pub fn build_login_url(
    auth_endpoint: &str,
    provider: &str,
    state: &str,
    code_challenge: &str,
    redirect_uri: &str,
) -> String {
    // 注意：不要在此追加 prompt=select_account。已用 curl 实测验证，Kiro 中间层
    // (auth.desktop.kiro.dev/login → Cognito → github/authorize) 在第一跳就剥离了所有
    // 额外 query 参数，prompt 永远到不了 GitHub/Google。必须由用户把登录地址复制到
    // 无痕窗口、隐私窗口或其他浏览器中访问，不靠 URL 参数强制选号。
    format!(
        "{}/login?idp={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        auth_endpoint.trim_end_matches('/'),
        urlencoding::encode(provider),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(code_challenge),
        urlencoding::encode(state),
    )
}

fn build_social_user_agent(kiro_version: &str, machine_id: &str) -> String {
    format!("KiroIDE-{}-{}", kiro_version, machine_id)
}

pub async fn exchange_code_for_token(
    auth_endpoint: &str,
    code: &str,
    code_verifier: &str,
    full_redirect_uri: &str,
    machine_id: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<SocialCreateTokenResponse> {
    let url = format!("{}/oauth/token", auth_endpoint);
    let client = build_client(proxy, 30, config.tls_backend)?;

    let body = SocialCreateTokenRequest {
        code: code.to_string(),
        code_verifier: code_verifier.to_string(),
        redirect_uri: full_redirect_uri.to_string(),
        invitation_code: None,
    };

    let user_agent = build_social_user_agent(&config.kiro_version, machine_id);

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("User-Agent", &user_agent)
        .header("host", auth_endpoint.trim_start_matches("https://"))
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("社交登录令牌交换失败 {}: {}", status, body_text);
    }

    resp.json::<SocialCreateTokenResponse>()
        .await
        .map_err(|e| anyhow::anyhow!("解析社交登录令牌响应失败: {}", e))
}

#[cfg(test)]
mod tests {
    use super::{
        build_login_url, build_social_user_agent, callback_from_input, manual_redirect_uri,
        resolve_provider_names,
    };
    use std::collections::HashMap;

    #[test]
    fn login_url_matches_kiro_auth_service_shape() {
        let provider = resolve_provider_names("GitHub").unwrap();
        let raw_url = build_login_url(
            "https://prod.us-east-1.auth.desktop.kiro.dev",
            provider.portal_idp,
            "state-1",
            "challenge-1",
            "kiro://app/callback",
        );
        let url = reqwest::Url::parse(&raw_url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(
            url.as_str().split('?').next().unwrap(),
            "https://prod.us-east-1.auth.desktop.kiro.dev/login"
        );
        assert_eq!(params.get("idp").map(String::as_str), Some("Github"));
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("kiro://app/callback")
        );
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some("challenge-1")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state-1"));
    }

    #[test]
    fn provider_names_keep_canonical_credential_and_portal_idp() {
        let github = resolve_provider_names("GitHub").unwrap();
        assert_eq!(github.credential_provider, "GitHub");
        assert_eq!(github.portal_idp, "Github");

        let github_portal_alias = resolve_provider_names("Github").unwrap();
        assert_eq!(github_portal_alias, github);

        let google = resolve_provider_names("google").unwrap();
        assert_eq!(google.credential_provider, "Google");
        assert_eq!(google.portal_idp, "Google");
    }

    #[test]
    fn social_token_user_agent_includes_machine_id() {
        assert_eq!(
            build_social_user_agent("0.6.18", "machine-1"),
            "KiroIDE-0.6.18-machine-1"
        );
    }

    #[test]
    fn callback_from_input_accepts_full_localhost_url() {
        let callback =
            callback_from_input("http://127.0.0.1:3128/oauth/callback?code=abc&state=xyz").unwrap();
        assert_eq!(callback.code, "abc");
        assert_eq!(callback.state, "xyz");
    }

    #[test]
    fn callback_from_input_accepts_path_and_query() {
        let callback = callback_from_input("/oauth/callback?code=abc%2Fdef&state=xyz").unwrap();
        assert_eq!(callback.code, "abc/def");
        assert_eq!(callback.state, "xyz");
    }

    #[test]
    fn manual_redirect_uri_uses_first_kiro_callback_port() {
        assert_eq!(
            manual_redirect_uri(),
            "http://127.0.0.1:3128/oauth/callback"
        );
    }
}
