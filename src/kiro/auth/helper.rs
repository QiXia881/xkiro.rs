//! 本机社交登录助手（远程部署场景）
//!
//! 远程 xkiro 无法接收 OAuth 回调（回调只命中 127.0.0.1），因此让用户在本机运行
//! 此 helper：本机起回调服务 + 生成 PKCE + 输出登录地址 + 等回调 + 换取令牌，
//! 最后把最终凭据 POST 回远程 xkiro 的 `/auth/social/complete/{session}` 端点。

use std::io::Write;
use std::time::Duration;

use serde_json::json;
use tokio::sync::oneshot;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::auth::social;
use crate::kiro::machine_id;
use crate::model::config::Config;

const CALLBACK_TIMEOUT_SECS: u64 = 300;

/// helper 子命令入口参数
pub struct HelperArgs {
    pub server: String,
    pub session: String,
    pub provider: String,
    pub api_key: Option<String>,
    pub auth_endpoint: Option<String>,
    pub proxy: Option<String>,
}

pub async fn run(args: HelperArgs) -> anyhow::Result<()> {
    let provider = social::resolve_provider_names(&args.provider)?;

    let api_key = match args.api_key {
        Some(key) if !key.trim().is_empty() => key.trim().to_string(),
        _ => prompt_api_key()?,
    };
    if api_key.is_empty() {
        anyhow::bail!("Admin API 密钥不能为空");
    }

    let server = args.server.trim_end_matches('/').to_string();
    let auth_endpoint = args
        .auth_endpoint
        .unwrap_or_else(|| social::KIRO_AUTH_ENDPOINT.to_string());
    let proxy = args.proxy.as_deref().map(ProxyConfig::new);

    let config = Config::default();
    let machine_id = machine_id::generate_account_machine_id();

    // 1. PKCE + 本机回调服务器
    let (code_verifier, code_challenge) = social::generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<social::OAuthCallbackData>();
    let (port, _server_handle) = social::start_callback_server(tx)?;
    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", port);

    // 2. 构建登录 URL，交给用户自行用隔离浏览器访问
    let login_url = social::build_login_url(
        &auth_endpoint,
        provider.portal_idp,
        &state,
        &code_challenge,
        &redirect_uri,
    );

    println!(
        "\n请复制下面的地址到无痕窗口、隐私窗口或其他浏览器中完成 {} 登录：\n",
        provider.credential_provider
    );
    println!("    {}\n", login_url);
    println!("请勿直接复用普通浏览器窗口，否则可能沿用上一次已登录的浏览器身份。\n");
    println!("等待授权回调中（最长 {} 秒）…", CALLBACK_TIMEOUT_SECS);

    // 3. 等待回调
    let callback = match tokio::time::timeout(Duration::from_secs(CALLBACK_TIMEOUT_SECS), rx).await
    {
        Ok(Ok(cb)) => cb,
        Ok(Err(_)) => anyhow::bail!("回调通道已关闭，登录未完成"),
        Err(_) => anyhow::bail!("等待回调超时（{} 秒）", CALLBACK_TIMEOUT_SECS),
    };

    if callback.state != state {
        anyhow::bail!("OAuth state 不匹配，可能存在并发登录或安全风险");
    }

    // 4. 本机换取令牌
    println!("已收到回调，正在换取令牌...");
    let token_resp = social::exchange_code_for_token(
        &auth_endpoint,
        &callback.code,
        &code_verifier,
        &redirect_uri,
        &machine_id,
        &config,
        proxy.as_ref(),
    )
    .await?;

    // 5. 回传到远程 xkiro
    println!("正在回传凭据到 {}…", server);
    let body = json!({
        "accessToken": token_resp.access_token,
        "refreshToken": token_resp.refresh_token,
        "profileArn": token_resp.profile_arn,
        "expiresAt": token_resp.expires_at,
        "expiresIn": token_resp.expires_in,
        "machineId": machine_id,
    });

    let client = build_client(proxy.as_ref(), 30, config.tls_backend)?;
    let url = format!("{}/api/admin/auth/social/complete/{}", server, args.session);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("x-api-key", &api_key)
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("回传失败 {}: {}", status, text);
    }

    println!("\n✓ 登录成功，凭据已添加到远程 xkiro。请返回 Admin UI 查看。");
    Ok(())
}

fn prompt_api_key() -> anyhow::Result<String> {
    print!("请输入 Admin API 密钥: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}
