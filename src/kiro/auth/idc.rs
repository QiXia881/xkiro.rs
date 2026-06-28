use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::auth::social;
use crate::kiro::model::token_refresh::{
    CreateTokenRequest, CreateTokenResponse, OidcErrorResponse, RegisterClientRequest,
    RegisterClientResponse, StartDeviceAuthorizationRequest, StartDeviceAuthorizationResponse,
};
use crate::model::config::Config;

#[derive(Debug)]
pub enum PollResult {
    Pending,
    SlowDown,
    Success(CreateTokenResponse),
    Expired,
    Error(anyhow::Error),
}

pub const BUILDER_ID_START_URL: &str = "https://view.awsapps.com/start";
pub const IAM_SSO_REDIRECT_URI: &str = "http://127.0.0.1/oauth/callback";

const IAM_SSO_CODE_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

#[derive(Debug)]
pub struct IamSsoCodeStart {
    pub client_id: String,
    pub client_secret: String,
    pub code_verifier: String,
    pub state: String,
    pub redirect_uri: String,
    pub authorize_url: String,
    pub expires_in: i64,
}

#[derive(Debug)]
pub struct ImportedSsoToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    pub client_id: String,
    pub client_secret: String,
}

fn oidc_endpoint(region: &str) -> String {
    format!("https://oidc.{}.amazonaws.com", region)
}

fn build_device_register_request(start_url: &str) -> RegisterClientRequest {
    RegisterClientRequest {
        client_name: "Kiro".to_string(),
        client_type: "public".to_string(),
        scopes: IAM_SSO_CODE_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect(),
        grant_types: vec![
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            "refresh_token".to_string(),
        ],
        issuer_url: Some(start_url.to_string()),
    }
}

fn build_sso_token_register_request(start_url: &str) -> RegisterClientRequest {
    RegisterClientRequest {
        client_name: "Kiro API Proxy".to_string(),
        client_type: "public".to_string(),
        scopes: IAM_SSO_CODE_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect(),
        grant_types: vec![
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            "refresh_token".to_string(),
        ],
        issuer_url: Some(start_url.to_string()),
    }
}

pub async fn register_client(
    region: &str,
    start_url: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<RegisterClientResponse> {
    let url = format!("{}/client/register", oidc_endpoint(region));
    let client = build_client(proxy, 30, config.tls_backend)?;

    let body = build_device_register_request(start_url);

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
        .context("注册 OIDC 客户端请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("注册 OIDC 客户端失败 {}: {}", status, body_text);
    }

    resp.json::<RegisterClientResponse>()
        .await
        .context("解析注册响应失败")
}

async fn register_sso_token_client(
    region: &str,
    start_url: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<RegisterClientResponse> {
    let url = format!("{}/client/register", oidc_endpoint(region));
    let client = build_client(proxy, 30, config.tls_backend)?;
    let body = build_sso_token_register_request(start_url);

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
        .context("注册 SSO Token OIDC 客户端请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("注册 SSO Token OIDC 客户端失败 {}: {}", status, body_text);
    }

    resp.json::<RegisterClientResponse>()
        .await
        .context("解析 SSO Token 注册响应失败")
}

fn build_iam_sso_authorize_url(
    authorize_endpoint: &str,
    client_id: &str,
    code_challenge: &str,
    state: &str,
) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(authorize_endpoint).context("构造 IAM SSO 授权 URL 失败")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", IAM_SSO_REDIRECT_URI)
        .append_pair("scopes", &IAM_SSO_CODE_SCOPES.join(","))
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

fn build_iam_sso_register_request(start_url: &str) -> serde_json::Value {
    serde_json::json!({
        "clientName": "Kiro",
        "clientType": "public",
        "scopes": IAM_SSO_CODE_SCOPES,
        "grantTypes": ["authorization_code", "refresh_token"],
        "redirectUris": [IAM_SSO_REDIRECT_URI],
        "issuerUrl": start_url,
    })
}

pub async fn start_iam_sso_code_authorization(
    region: &str,
    start_url: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<IamSsoCodeStart> {
    let oidc_base = oidc_endpoint(region);
    let client = build_client(proxy, 30, config.tls_backend)?;
    let body = build_iam_sso_register_request(start_url);

    let resp = client
        .post(format!("{oidc_base}/client/register"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("注册 IAM SSO OIDC 客户端请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("注册 IAM SSO OIDC 客户端失败 {}: {}", status, body_text);
    }

    let registered = resp
        .json::<RegisterClientResponse>()
        .await
        .context("解析 IAM SSO OIDC 注册响应失败")?;
    let (code_verifier, code_challenge) = social::generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let authorize_endpoint = registered
        .authorization_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{oidc_base}/authorize"));
    let authorize_url = build_iam_sso_authorize_url(
        &authorize_endpoint,
        &registered.client_id,
        &code_challenge,
        &state,
    )?;

    Ok(IamSsoCodeStart {
        client_id: registered.client_id,
        client_secret: registered.client_secret,
        code_verifier,
        state,
        redirect_uri: IAM_SSO_REDIRECT_URI.to_string(),
        authorize_url,
        expires_in: 600,
    })
}

pub async fn exchange_iam_sso_code(
    region: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<CreateTokenResponse> {
    let url = format!("{}/token", oidc_endpoint(region));
    let client = build_client(proxy, 30, config.tls_backend)?;
    let body = serde_json::json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "grantType": "authorization_code",
        "redirectUri": redirect_uri,
        "code": code.trim(),
        "codeVerifier": code_verifier,
    });

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
        .context("IAM SSO 授权码换 Token 请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("IAM SSO 授权码换 Token 失败 {}: {}", status, body_text);
    }

    resp.json::<CreateTokenResponse>()
        .await
        .context("解析 IAM SSO Token 响应失败")
}

pub async fn start_device_authorization(
    region: &str,
    start_url: &str,
    client_id: &str,
    client_secret: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<StartDeviceAuthorizationResponse> {
    let url = format!("{}/device_authorization", oidc_endpoint(region));
    let client = build_client(proxy, 30, config.tls_backend)?;

    let body = StartDeviceAuthorizationRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        start_url: start_url.to_string(),
    };

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
        .context("发起设备授权请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("发起设备授权失败 {}: {}", status, body_text);
    }

    resp.json::<StartDeviceAuthorizationResponse>()
        .await
        .context("解析设备授权响应失败")
}

pub async fn poll_token(
    region: &str,
    client_id: &str,
    client_secret: &str,
    device_code: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> PollResult {
    let url = format!("{}/token", oidc_endpoint(region));
    let client = match build_client(proxy, 30, config.tls_backend) {
        Ok(client) => client,
        Err(error) => return PollResult::Error(error),
    };

    let body = CreateTokenRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        grant_type: "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        device_code: device_code.to_string(),
    };

    let resp = match client
        .post(&url)
        .header("content-type", "application/json")
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(error) => return PollResult::Error(error.into()),
    };

    let status = resp.status();
    if status.is_success() {
        return match resp.json::<CreateTokenResponse>().await {
            Ok(token) => PollResult::Success(token),
            Err(error) => PollResult::Error(error.into()),
        };
    }

    let body_text = match resp.text().await {
        Ok(body_text) => body_text,
        Err(error) => return PollResult::Error(error.into()),
    };

    if let Ok(error_resp) = serde_json::from_str::<OidcErrorResponse>(&body_text) {
        match error_resp.error.as_str() {
            "authorization_pending" => return PollResult::Pending,
            "slow_down" => return PollResult::SlowDown,
            "expired_token" => return PollResult::Expired,
            "access_denied" => {
                return PollResult::Error(anyhow::anyhow!("用户拒绝了授权请求"));
            }
            _ => {}
        }
    }

    PollResult::Error(anyhow::anyhow!("轮询令牌失败 {}: {}", status, body_text))
}

// ============================================================================
// SSO Token 导入所需的额外 OIDC 函数
// ============================================================================

/// Bearer Token 验证响应
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmIResponse {
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub email: Option<String>,
    pub arn: Option<String>,
}

/// 设备会话 Token 响应
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSessionResponse {
    pub token: String,
}

/// 接受用户代码请求
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptUserCodeRequest {
    pub user_code: String,
    pub user_session_id: String,
}

/// 接受用户代码响应
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcceptUserCodeResponse {
    pub device_context: Option<serde_json::Value>,
}

/// 批准授权请求
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApproveAuthRequest {
    pub device_context: serde_json::Value,
    pub user_session_id: String,
}

/// 验证 Bearer Token（GET /token/whoAmI）
pub async fn verify_bearer_token(
    portal_base: &str,
    bearer_token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<WhoAmIResponse> {
    let url = format!("{}/token/whoAmI", portal_base);
    let client = build_client(proxy, 30, crate::model::config::TlsBackend::default())?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", bearer_token))
        .header("Accept", "application/json")
        .send()
        .await
        .context("验证 Bearer Token 请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("验证 Bearer Token 失败 {}: {}", status, body_text);
    }

    resp.json::<WhoAmIResponse>()
        .await
        .context("解析验证响应失败")
}

/// 获取设备会话 Token（POST /session/device）
pub async fn get_device_session_token(
    portal_base: &str,
    bearer_token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<String> {
    let url = format!("{}/session/device", portal_base);
    let client = build_client(proxy, 30, crate::model::config::TlsBackend::default())?;

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", bearer_token))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .context("获取设备会话 Token 请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("获取设备会话 Token 失败 {}: {}", status, body_text);
    }

    let session_resp = resp
        .json::<DeviceSessionResponse>()
        .await
        .context("解析设备会话响应失败")?;

    Ok(session_resp.token)
}

/// 接受用户代码（POST /device_authorization/accept_user_code）
pub async fn accept_user_code(
    oidc_base: &str,
    user_code: &str,
    device_session_token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<Option<serde_json::Value>> {
    let url = format!("{}/device_authorization/accept_user_code", oidc_base);
    let client = build_client(proxy, 30, crate::model::config::TlsBackend::default())?;

    let body = AcceptUserCodeRequest {
        user_code: user_code.to_string(),
        user_session_id: device_session_token.to_string(),
    };

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("Referer", "https://view.awsapps.com/")
        .json(&body)
        .send()
        .await
        .context("接受用户代码请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("接受用户代码失败 {}: {}", status, body_text);
    }

    let data = resp
        .json::<AcceptUserCodeResponse>()
        .await
        .context("解析接受用户代码响应失败")?;
    Ok(data.device_context)
}

/// 批准授权（POST /device_authorization/associate_token）
pub async fn approve_auth(
    oidc_base: &str,
    device_context: serde_json::Value,
    device_session_token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<()> {
    let url = format!("{}/device_authorization/associate_token", oidc_base);
    let client = build_client(proxy, 30, crate::model::config::TlsBackend::default())?;

    let body = ApproveAuthRequest {
        device_context,
        user_session_id: device_session_token.to_string(),
    };

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("Referer", "https://view.awsapps.com/")
        .json(&body)
        .send()
        .await
        .context("批准授权请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("批准授权失败 {}: {}", status, body_text);
    }

    Ok(())
}

/// 完整的 SSO Token 导入流程（7 步）
pub async fn import_sso_token(
    bearer_token: &str,
    region: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<ImportedSsoToken> {
    let oidc_base = format!("https://oidc.{}.amazonaws.com", region);
    let portal_base = "https://portal.sso.us-east-1.amazonaws.com";
    let start_url = BUILDER_ID_START_URL;

    // 1. 注册 OIDC 客户端
    let client_info = register_sso_token_client(region, start_url, config, proxy).await?;

    // 2. 启动设备授权
    let device_auth = start_device_authorization(
        region,
        start_url,
        &client_info.client_id,
        &client_info.client_secret,
        config,
        proxy,
    )
    .await?;

    // 3. 验证 Bearer Token
    verify_bearer_token(&portal_base, bearer_token, proxy).await?;

    // 4. 获取设备会话 Token
    let session_token = get_device_session_token(&portal_base, bearer_token, proxy).await?;

    // 5. 接受用户代码
    let device_context =
        accept_user_code(&oidc_base, &device_auth.user_code, &session_token, proxy).await?;

    // 6. 批准授权
    if let Some(device_context) = device_context {
        approve_auth(&oidc_base, device_context, &session_token, proxy).await?;
    }

    // 7. 轮询 Token
    let mut interval = device_auth.interval.max(1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);

    loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("SSO Token 导入超时（2 分钟）");
        }

        match poll_token(
            region,
            &client_info.client_id,
            &client_info.client_secret,
            &device_auth.device_code,
            config,
            proxy,
        )
        .await
        {
            PollResult::Success(token) => {
                return Ok(ImportedSsoToken {
                    access_token: token.access_token,
                    refresh_token: token.refresh_token,
                    expires_in: token.expires_in,
                    client_id: client_info.client_id,
                    client_secret: client_info.client_secret,
                });
            }
            PollResult::Pending => {
                tokio::time::sleep(std::time::Duration::from_secs(interval as u64)).await;
            }
            PollResult::SlowDown => {
                interval += 5;
                tokio::time::sleep(std::time::Duration::from_secs(interval as u64)).await;
            }
            PollResult::Expired => {
                anyhow::bail!("设备授权已过期");
            }
            PollResult::Error(e) => {
                let err_msg = e.to_string();
                if err_msg.contains("slow_down") {
                    interval += 5;
                    tokio::time::sleep(std::time::Duration::from_secs(interval as u64)).await;
                } else if err_msg.contains("authorization_pending") {
                    tokio::time::sleep(std::time::Duration::from_secs(interval as u64)).await;
                } else {
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iam_sso_register_request_matches_kiro_go_shape() {
        let value = build_iam_sso_register_request(BUILDER_ID_START_URL);

        assert_eq!(value["clientName"], "Kiro");
        assert_eq!(value["clientType"], "public");
        assert_eq!(value["issuerUrl"], BUILDER_ID_START_URL);
        assert_eq!(
            value["grantTypes"],
            serde_json::json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(
            value["redirectUris"],
            serde_json::json!([IAM_SSO_REDIRECT_URI])
        );
        assert_eq!(value["scopes"], serde_json::json!(IAM_SSO_CODE_SCOPES));
    }

    #[test]
    fn sso_token_register_request_matches_kiro_go_shape() {
        let value =
            serde_json::to_value(build_sso_token_register_request(BUILDER_ID_START_URL)).unwrap();

        assert_eq!(value["clientName"], "Kiro API Proxy");
        assert_eq!(value["clientType"], "public");
        assert_eq!(value["issuerUrl"], BUILDER_ID_START_URL);
        assert_eq!(
            value["grantTypes"],
            serde_json::json!([
                "urn:ietf:params:oauth:grant-type:device_code",
                "refresh_token"
            ])
        );
        assert_eq!(value["scopes"], serde_json::json!(IAM_SSO_CODE_SCOPES));
    }

    #[test]
    fn device_register_request_matches_kiro_go_builder_id_shape() {
        let value =
            serde_json::to_value(build_device_register_request(BUILDER_ID_START_URL)).unwrap();

        assert_eq!(value["clientName"], "Kiro");
        assert_eq!(value["clientType"], "public");
        assert_eq!(value["issuerUrl"], BUILDER_ID_START_URL);
        assert_eq!(
            value["grantTypes"],
            serde_json::json!([
                "urn:ietf:params:oauth:grant-type:device_code",
                "refresh_token"
            ])
        );
        assert_eq!(value["scopes"], serde_json::json!(IAM_SSO_CODE_SCOPES));
    }

    #[test]
    fn sso_token_device_authorization_payloads_use_aws_camel_case() {
        let accept = serde_json::to_value(AcceptUserCodeRequest {
            user_code: "ABCD-EFGH".to_string(),
            user_session_id: "session-1".to_string(),
        })
        .unwrap();
        assert_eq!(accept["userCode"], "ABCD-EFGH");
        assert_eq!(accept["userSessionId"], "session-1");
        assert!(accept.get("user_code").is_none());

        let approve = serde_json::to_value(ApproveAuthRequest {
            device_context: serde_json::json!({"deviceContextId": "ctx-1"}),
            user_session_id: "session-1".to_string(),
        })
        .unwrap();
        assert_eq!(approve["deviceContext"]["deviceContextId"], "ctx-1");
        assert_eq!(approve["userSessionId"], "session-1");
        assert!(approve.get("device_context").is_none());
    }

    #[test]
    fn iam_sso_authorize_url_contains_required_oauth_params() {
        let raw_url = build_iam_sso_authorize_url(
            "https://oidc.us-east-1.amazonaws.com/authorize",
            "client-1",
            "challenge-1",
            "state-1",
        )
        .unwrap();
        let url = reqwest::Url::parse(&raw_url).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(
            url.as_str().split('?').next().unwrap(),
            "https://oidc.us-east-1.amazonaws.com/authorize"
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("client-1")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(IAM_SSO_REDIRECT_URI)
        );
        assert_eq!(
            params.get("scopes").map(String::as_str),
            Some(IAM_SSO_CODE_SCOPES.join(",").as_str())
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
    }
}
