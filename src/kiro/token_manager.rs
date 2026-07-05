//! 令牌管理模块
//!
//! 负责令牌过期检测和刷新，支持社交登录和 IAM Identity Center 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use futures::future::select_all;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration as StdDuration, Instant};

use crate::common::utf8::floor_char_boundary;
use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::affinity::CredentialAffinity;
use crate::kiro::background_refresh::{
    BackgroundRefreshConfig, BackgroundRefresher, RefreshResult,
};
use crate::kiro::endpoint::{
    AMAZONQ_ENDPOINT_NAME, AmazonQEndpoint, CLI_ENDPOINT_NAME, CODEWHISPERER_ENDPOINT_NAME,
    CliEndpoint, CodewhispererEndpoint, IDE_ENDPOINT_NAME, IdeEndpoint, KiroEndpoint,
    RequestContext,
};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::token_refresh::{
    ExternalIdpTokenResponse, IdcRefreshRequest, IdcRefreshResponse, RefreshRequest,
    RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::model::claude::claude_model_match_key;
use crate::model::config::Config;

/// 检查令牌是否在指定时间内过期
pub(crate) fn is_token_expiring_within(
    credentials: &KiroCredentials,
    minutes: i64,
) -> Option<bool> {
    credentials
        .expires_at
        .as_ref()
        .and_then(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires| expires <= Utc::now() + Duration::minutes(minutes))
}

/// 检查令牌是否已过期（使用 120 秒刷新偏移）
pub(crate) fn is_token_expired(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 2).unwrap_or(true)
}

/// 检查令牌是否即将过期（10分钟内）
pub(crate) fn is_token_expiring_soon(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 10).unwrap_or(false)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

fn normalize_model_id(model: &str) -> Option<String> {
    claude_model_match_key(model)
}

fn effective_weight(weight: u32) -> usize {
    weight.max(1) as usize
}

/// 生成 API 密钥脱敏展示(前 4 + ... + 后 4,长度不足或非 ASCII 回退 ***)
fn mask_api_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 16 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "***".to_string()
    }
}

fn has_non_empty_secret(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

/// 验证 refreshToken 的基本有效性
pub(crate) fn validate_refresh_token(credentials: &KiroCredentials) -> anyhow::Result<()> {
    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;

    if refresh_token.is_empty() {
        bail!("refreshToken 为空");
    }

    if refresh_token.len() < 100 || refresh_token.ends_with("...") || refresh_token.contains("...")
    {
        bail!(
            "refreshToken 已被截断（长度: {} 字符）。\n\
             这通常是 Kiro IDE 为了防止凭据被第三方工具使用而故意截断的。",
            refresh_token.len()
        );
    }

    Ok(())
}

/// refreshToken 永久失效错误
///
/// 当服务端返回 400 + `invalid_grant` 时，表示 refreshToken 已被撤销或过期，
/// 不应重试，需立即禁用对应凭据。
#[derive(Debug)]
pub(crate) struct RefreshTokenInvalidError {
    pub message: String,
}

impl fmt::Display for RefreshTokenInvalidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshTokenInvalidError {}

/// 刷新令牌
pub(crate) async fn refresh_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    // API 密钥凭据不支持令牌刷新：底层契约级拦截
    // 其他调用点（try_ensure_token / 活跃路径 / add_credential）在调用前已显式分流 API 密钥；
    // 仅 force_refresh_token_for 未分流，此处 bail 让错误自然传播为 400 BAD_REQUEST。
    if credentials.is_api_key_credential() {
        bail!("API 密钥凭据不支持刷新令牌");
    }

    validate_refresh_token(credentials)?;

    if credentials.is_external_idp_credential() {
        refresh_external_idp_token(credentials, config, proxy).await
    } else if credentials.is_aws_sso_oidc_credential() {
        refresh_idc_token(credentials, config, proxy).await
    } else {
        refresh_social_token(credentials, config, proxy).await
    }
}

/// 刷新 External IdP 令牌（Microsoft 365 / Entra ID）
async fn refresh_external_idp_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 External IdP 令牌...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("External IdP 刷新需要 clientId"))?;
    let token_endpoint = credentials
        .token_endpoint
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("External IdP 刷新需要 tokenEndpoint"))?;

    let client = build_client(proxy, 60, config.tls_backend)?;
    let mut form = vec![
        ("client_id", client_id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
    ];
    if let Some(scopes) = credentials
        .scopes
        .as_deref()
        .filter(|v| !v.trim().is_empty())
    {
        form.push(("scope", scopes));
    }

    let response = client
        .post(token_endpoint)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .await?;

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    let data: ExternalIdpTokenResponse =
        serde_json::from_str(&body_text).unwrap_or(ExternalIdpTokenResponse {
            access_token: String::new(),
            refresh_token: None,
            expires_in: None,
            error: None,
            error_description: None,
        });

    if !status.is_success() || data.access_token.is_empty() {
        let redacted_body = crate::common::redact::redact_secret_text(&body_text);
        if data.error.as_deref() == Some("invalid_grant") {
            return Err(RefreshTokenInvalidError {
                message: format!(
                    "External IdP refreshToken 已失效 (invalid_grant): {}",
                    data.error_description
                        .as_deref()
                        .unwrap_or(redacted_body.as_str())
                ),
            }
            .into());
        }
        bail!("External IdP 令牌刷新失败: {} {}", status, redacted_body);
    }

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);
    if let Some(new_refresh_token) = data.refresh_token.filter(|v| !v.is_empty()) {
        new_credentials.refresh_token = Some(new_refresh_token);
    }
    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

/// 刷新社交登录令牌
async fn refresh_social_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新社交登录令牌...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);

    let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);
    let refresh_domain = format!("prod.{}.auth.desktop.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = RefreshRequest {
        refresh_token: refresh_token.to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("KiroIDE-{}-{}", kiro_version, machine_id),
        )
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .header("host", &refresh_domain)
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        // 脱敏上游响应，防令牌反射泄入日志/错误响应
        let redacted_body = crate::common::redact::redact_secret_text(&body_text);

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!(
                    "社交登录 refreshToken 已失效 (invalid_grant): {}",
                    redacted_body
                ),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "OAuth 凭据已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新令牌",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OAuth 服务暂时不可用",
            _ => "令牌刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, redacted_body);
    }

    let data: RefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(profile_arn) = KiroCredentials::clean_profile_arn(data.profile_arn) {
        new_credentials.profile_arn = Some(profile_arn);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

/// 刷新 IAM Identity Center 令牌 (AWS SSO OIDC)
async fn refresh_idc_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 IAM Identity Center 令牌...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IAM Identity Center 刷新需要 clientId"))?;
    let client_secret = credentials
        .client_secret
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IAM Identity Center 刷新需要 clientSecret"))?;

    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);
    let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let x_amz_user_agent = "aws-sdk-js/3.980.0 KiroIDE";
    let user_agent = format!(
        "aws-sdk-js/3.980.0 ua/2.1 os/{} lang/js md/nodejs#{} api/sso-oidc#3.980.0 m/E KiroIDE",
        os_name, node_version
    );

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = IdcRefreshRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        refresh_token: refresh_token.to_string(),
        grant_type: "refresh_token".to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("content-type", "application/json")
        .header("x-amz-user-agent", x_amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let redacted_body = crate::common::redact::redact_secret_text(&body_text);

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!(
                    "IAM Identity Center refreshToken 已失效 (invalid_grant): {}",
                    redacted_body
                ),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "IAM Identity Center 凭据已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新令牌",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OIDC 服务暂时不可用",
            _ => "IAM Identity Center 令牌刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, redacted_body);
    }

    let data: IdcRefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    // 同步更新 profile_arn（如果 IAM Identity Center 响应中包含）
    if let Some(profile_arn) = KiroCredentials::clean_profile_arn(data.profile_arn) {
        new_credentials.profile_arn = Some(profile_arn);
    }

    Ok(new_credentials)
}

/// 根据凭据生效的端点名称构造对应的 `KiroEndpoint` 实例
///
/// 就地构造端点，保持 token_manager 低频 REST 路径与主 provider registry 的端点集合一致。
///
/// 主链路（API/MCP 调用）仍走 main.rs 注入到 `Provider` 的端点 registry；
/// 此 helper 仅服务于 `get_usage_limits` 这种 token_manager 内部低频路径，
/// 避免把端点 registry 注入 `MultiTokenManager` 结构带来的扩散修改。
fn endpoint_for_credentials(
    credentials: &KiroCredentials,
    config: &Config,
) -> anyhow::Result<Box<dyn KiroEndpoint>> {
    match credentials.effective_endpoint_name(Some(&config.default_endpoint)) {
        IDE_ENDPOINT_NAME => Ok(Box::new(IdeEndpoint::new())),
        CODEWHISPERER_ENDPOINT_NAME => Ok(Box::new(CodewhispererEndpoint::new())),
        AMAZONQ_ENDPOINT_NAME => Ok(Box::new(AmazonQEndpoint::new())),
        CLI_ENDPOINT_NAME => Ok(Box::new(CliEndpoint::new())),
        name => bail!("未知端点: {}", name),
    }
}

/// 获取使用额度信息
pub(crate) async fn get_usage_limits(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    need_email: bool,
) -> anyhow::Result<UsageLimitsResponse> {
    tracing::debug!(
        endpoint = %credentials.effective_endpoint_name(Some(&config.default_endpoint)),
        "正在获取使用额度信息..."
    );

    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let endpoint = endpoint_for_credentials(credentials, config)?;
    let ctx = RequestContext {
        credentials,
        token,
        machine_id: &machine_id,
        config,
    };
    let usage = endpoint.usage_request_parts(&ctx, need_email)?;

    let client = build_client(proxy, 60, config.tls_backend)?;
    let mut request = client.get(&usage.url);
    for (name, value) in usage.headers {
        request = request.header(name, value);
    }

    let response = request.send().await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let redacted_body = crate::common::redact::redact_secret_text(&body_text);
        let error_msg = match status.as_u16() {
            401 => "认证失败，令牌无效或已过期",
            403 => "权限不足，无法获取使用额度",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取使用额度失败",
        };
        bail!("{}: {} {}", error_msg, status, redacted_body);
    }

    let body_text = response.text().await?;
    let data: UsageLimitsResponse = serde_json::from_str(&body_text).map_err(|e| {
        tracing::error!(
            "getUsageLimits JSON 解析失败: {}，原始响应: {}",
            e,
            crate::common::redact::redact_secret_text(&body_text)
        );
        anyhow::anyhow!("JSON 解析失败: {}", e)
    })?;
    Ok(data)
}

/// 切换上游 overage 开关
///
/// 调用 Kiro `setUserPreference` 接口写入 `overageConfiguration.overageStatus`。
/// 上游 200 即视为成功，不解析响应体。
pub(crate) async fn set_user_preference(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    overage_status: &str,
) -> anyhow::Result<()> {
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let endpoint = endpoint_for_credentials(credentials, config)?;
    let ctx = RequestContext {
        credentials,
        token,
        machine_id: &machine_id,
        config,
    };
    let parts = endpoint.set_preference_request_parts(&ctx, overage_status)?;

    let client = build_client(proxy, 60, config.tls_backend)?;
    let mut request = client.post(&parts.url).body(parts.body);
    for (name, value) in parts.headers {
        request = request.header(name, value);
    }

    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let redacted_body = crate::common::redact::redact_secret_text(&body_text);
        let msg = match status.as_u16() {
            401 => "认证失败，令牌无效或已过期",
            403 => "权限不足，无法切换超额开关",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "切换超额开关失败",
        };
        bail!("{}: {} {}", msg, status, redacted_body);
    }
    Ok(())
}

// ============================================================================
// 多凭据 Token 管理器
// ============================================================================

/// 单个凭据条目的状态
struct CredentialEntry {
    /// 凭据唯一 ID
    id: u64,
    /// 凭据信息
    credentials: KiroCredentials,
    /// API 调用连续失败次数
    failure_count: u32,
    /// 令牌刷新连续失败次数
    refresh_failure_count: u32,
    /// 是否已禁用
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）
    disabled_reason: Option<DisabledReason>,
    /// API 调用成功次数
    success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    last_used_at: Option<String>,
    /// 认证类禁用的下一次自动重探时间（A4 自愈调度）。
    ///
    /// 运行时字段：不落盘，重启后重置为 None（重启即视为一次全新重探）。
    /// A2 状态映射：Some(pending) 即 OPEN 态；到点被重探一次即 HALF_OPEN。
    reprobe_next: Option<DateTime<Utc>>,
    /// 自愈退避级别（0 = 未调度重探）。指数退避：1→1min 2→5min 3→30min 4+→2h。
    ///
    /// 运行时字段：不落盘。
    recovery_backoff_level: u8,
    /// 最近一次失败快照（运行时字段，不落盘）。
    ///
    /// 供前端秒级轮询感知封禁/调用失败：`code` 为稳定分类标签，
    /// 前端映射为可读文案；`at` 为失败发生时刻。成功/恢复/手动启用时清空。
    last_error: Option<CredentialLastError>,
}

/// 凭据最近一次失败快照（运行时内存字段）。
#[derive(Debug, Clone)]
struct CredentialLastError {
    code: &'static str,
    at: DateTime<Utc>,
}

impl CredentialEntry {
    fn record_error(&mut self, code: &'static str) {
        self.last_error = Some(CredentialLastError {
            code,
            at: Utc::now(),
        });
    }

    fn clear_error(&mut self) {
        self.last_error = None;
    }
}

fn credentials_snapshot_for_persistence(entries: &[CredentialEntry]) -> Vec<KiroCredentials> {
    entries
        .iter()
        .map(|e| {
            let mut cred = e.credentials.clone();
            cred.canonicalize_auth_method();
            cred.profile_arn = KiroCredentials::clean_profile_arn(cred.profile_arn.take());
            if let Some(reason) = persistent_disabled_reason(e.disabled_reason) {
                cred.disabled = true;
                cred.meta.disabled_reason = Some(reason.to_string());
            } else {
                cred.disabled = false;
            }
            cred
        })
        .collect()
}

fn persistent_disabled_reason(reason: Option<DisabledReason>) -> Option<&'static str> {
    match reason {
        Some(DisabledReason::Manual) => Some("manual"),
        Some(DisabledReason::AuthenticationFailed) => Some("AuthenticationFailed"),
        Some(DisabledReason::CredentialSuspended) => Some("AccountSuspended"),
        Some(DisabledReason::InvalidConfig) => Some("InvalidConfig"),
        _ => None,
    }
}

fn error_code_for_reason(reason: DisabledReason) -> &'static str {
    match reason {
        DisabledReason::Manual => "manual",
        DisabledReason::TooManyFailures => "too_many_failures",
        DisabledReason::TooManyRefreshFailures => "too_many_refresh_failures",
        DisabledReason::QuotaExceeded => "quota_exceeded",
        DisabledReason::InvalidRefreshToken => "invalid_refresh_token",
        DisabledReason::InvalidConfig => "invalid_config",
        DisabledReason::AuthenticationFailed => "authentication_failed",
        DisabledReason::CredentialSuspended => "credential_suspended",
        DisabledReason::InsufficientBalance => "insufficient_balance",
        DisabledReason::ModelUnavailable => "model_unavailable",
    }
}

fn non_empty_trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

fn next_reset_date_from_unix(value: Option<f64>) -> Option<String> {
    let timestamp = value?.floor() as i64;
    if timestamp <= 0 {
        return None;
    }
    DateTime::<Utc>::from_timestamp(timestamp, 0).map(|dt| dt.format("%Y-%m-%d").to_string())
}

fn social_login_matches_existing(
    existing: &KiroCredentials,
    refresh_token: &str,
    user_id: Option<&str>,
) -> bool {
    if !is_social_login_match_candidate(existing) {
        return false;
    }
    if let Some(user_id) = user_id
        && non_empty_trimmed(existing.user_id.as_deref()) == Some(user_id)
    {
        return true;
    }
    existing.refresh_token.as_deref() == Some(refresh_token)
}

fn is_social_login_match_candidate(credentials: &KiroCredentials) -> bool {
    if credentials.is_api_key_credential() {
        return false;
    }
    if matches!(
        KiroCredentials::normalize_provider_for_auth_method(credentials.provider.clone(), "social")
            .as_deref(),
        Some("Google") | Some("GitHub")
    ) {
        return true;
    }
    if credentials
        .auth_method
        .as_deref()
        .map(|method| method.eq_ignore_ascii_case("social"))
        .unwrap_or(false)
    {
        return true;
    }
    credentials.auth_method.is_none()
        && credentials.provider.is_none()
        && credentials.client_id.is_none()
        && credentials.client_secret.is_none()
        && credentials.refresh_token.is_some()
}

fn apply_social_login_update(existing: &mut KiroCredentials, incoming: &KiroCredentials) {
    existing.access_token = incoming.access_token.clone();
    existing.refresh_token = incoming.refresh_token.clone();
    existing.profile_arn = incoming.profile_arn.clone();
    existing.expires_at = incoming.expires_at.clone();
    existing.user_id = incoming.user_id.clone();
    existing.meta.subscription_title = incoming.meta.subscription_title.clone();
    existing.meta.overage_status = incoming.meta.overage_status.clone();

    if non_empty_trimmed(existing.auth_method.as_deref()).is_none() {
        existing.auth_method = incoming.auth_method.clone();
    }
    if non_empty_trimmed(existing.provider.as_deref()).is_none() {
        existing.provider = incoming.provider.clone();
    }
    if existing
        .machine_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        existing.machine_id = incoming.machine_id.clone();
    }
    if existing
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .is_none()
    {
        existing.email = incoming.email.clone();
    }
    existing.provider =
        KiroCredentials::normalize_provider_for_auth_method(existing.provider.take(), "social");
    existing.canonicalize_auth_method();
}

/// 禁用原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum DisabledReason {
    /// Admin API 手动禁用
    Manual,
    /// 连续失败达到阈值后自动禁用
    TooManyFailures,
    /// 令牌刷新连续失败达到阈值后自动禁用
    TooManyRefreshFailures,
    /// 额度已用尽（如 MONTHLY_REQUEST_COUNT）
    QuotaExceeded,
    /// Refresh Token 永久失效（服务端返回 invalid_grant）
    InvalidRefreshToken,
    /// 凭据配置无效（如 authMethod=api_key 但缺少 apiKey）
    InvalidConfig,
    /// 认证失败（如 invalid_grant 之外的认证错误）
    AuthenticationFailed,
    /// 上游判定该凭据不可继续使用
    CredentialSuspended,
    /// 余额不足
    InsufficientBalance,
    /// 模型临时不可用（全局禁用）
    ModelUnavailable,
}

/// 判定某禁用原因是否属于「认证类可自愈」集合（A4 自动重探资格）。
///
/// 仅认证凭证本身临时失效的场景可自动重探：AuthenticationFailed /
/// TooManyRefreshFailures / InvalidRefreshToken。其余原因语义不同，
/// 不走本路径：suspended/manual/config 为设计上的粘性禁用；quota/balance
/// 由各自路径恢复；ModelUnavailable 由全局 check_and_recover 恢复。
fn is_auth_recoverable_reason(reason: DisabledReason) -> bool {
    matches!(
        reason,
        DisabledReason::AuthenticationFailed
            | DisabledReason::TooManyRefreshFailures
            | DisabledReason::InvalidRefreshToken
    )
}

/// 自愈重探的指数退避时长：1→1min 2→5min 3→30min 4+→2h（封顶）。
fn backoff_duration(level: u8) -> Duration {
    match level {
        0 | 1 => Duration::minutes(1),
        2 => Duration::minutes(5),
        3 => Duration::minutes(30),
        _ => Duration::hours(2),
    }
}

/// 统计数据持久化条目
#[derive(Serialize, Deserialize)]
struct StatsEntry {
    success_count: u64,
    last_used_at: Option<String>,
}

// ============================================================================
// Admin API 公开结构
// ============================================================================

/// 凭据条目快照（用于 Admin API 读取）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEntrySnapshot {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 调度权重（0/1=普通，2+=更高份额）
    pub weight: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 身份提供方（Google / GitHub / BuilderId / AzureAD 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Kiro 用户 ID（导入来源展示元数据）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// 导入来源 ID（可能是非数字字符串 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_account_id: Option<String>,
    /// 用户自定义显示名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 导入来源状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 导入来源添加时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    /// 显示昵称
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// 导入来源分组 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// 导入来源标签关联数组
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_links: Option<serde_json::Value>,
    /// 导入来源原始 usage API 响应
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_data: Option<serde_json::Value>,
    /// 是否存在导入来源可用模型缓存
    pub has_available_models_cache: bool,
    /// 导入来源失败次数（区别于运行时 failure_count）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_failure_count: Option<u32>,
    /// 导入来源最后失败时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_last_failure_at: Option<String>,
    /// 导入来源禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_disabled_reason: Option<String>,
    /// 导入来源成功次数（区别于运行时 success_count）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_success_count: Option<u64>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// 是否保存了访问令牌（不返回明文）
    pub has_token: bool,
    /// 是否保存了刷新令牌（不返回明文）
    pub has_refresh_token: bool,
    /// 是否保存了 clientId（不返回明文，避免把租户客户端信息散落到列表接口）
    pub has_client_id: bool,
    /// 是否保存了 clientSecret（不返回明文）
    pub has_client_secret: bool,
    /// 是否保存了 ID 令牌（不返回明文）
    pub has_id_token: bool,
    /// 是否保存了 API 密钥（列表接口仅另行返回脱敏值）
    pub has_api_key: bool,
    /// 是否保存了代理认证信息（不返回明文）
    pub has_proxy_credentials: bool,
    /// 令牌过期时间
    pub expires_at: Option<String>,
    /// 凭据级区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// 凭据级认证区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,
    /// 凭据级 API 区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,
    /// 凭据级机器 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// AWS SSO Start URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    /// 本地缓存 clientIdHash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_hash: Option<String>,
    /// 本地缓存 SSO session ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_session_id: Option<String>,
    /// External IdP token endpoint
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// External IdP issuer URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    /// External IdP scopes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    /// 订阅类型
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    /// 订阅标题
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_title: Option<String>,
    /// 剩余天数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_remaining: Option<i64>,
    /// 远端 overage 开关状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<String>,
    /// 远端 overage 能力
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_capability: Option<String>,
    /// 远端 overage 上限
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_cap: Option<f64>,
    /// 远端 overage 单价
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_rate: Option<f64>,
    /// 当前 overage 消耗
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_overages: Option<f64>,
    /// overage 最近同步时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_checked_at: Option<i64>,
    /// 封禁状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_status: Option<String>,
    /// 封禁原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_reason: Option<String>,
    /// 封禁时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_time: Option<i64>,
    /// 用量快照
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_current: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<i64>,
    /// 试用用量快照
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_current: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_expires_at: Option<i64>,
    /// 持久化统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_credits: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<serde_json::Value>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// apiKey 的 SHA-256 哈希（仅 API 密钥凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// apiKey 的脱敏展示（仅 API 密钥凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// 绑定的代理池 ID（None = 未绑定代理池）
    pub proxy_id: Option<u64>,
    /// 令牌刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 端点名称（未显式配置时返回 None，由 Admin 层回退到默认值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 当前可用 permit 数（per-cred Semaphore 剩余）
    pub available_permits: usize,
    /// 该凭据 permit 容量上限（= 当前 per_credential_concurrency）
    pub max_permits: usize,
    /// 凭据级并发配置（None=回退全局 per_credential_concurrency）
    pub concurrency: Option<u32>,
    /// 最近一次失败的分类标签（运行时字段，成功/恢复后为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<String>,
    /// 最近一次失败的发生时刻（RFC3339，运行时字段）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<String>,
}

/// 凭据管理器状态快照
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSnapshot {
    /// 凭据条目列表
    pub entries: Vec<CredentialEntrySnapshot>,
    /// 总凭据数量
    pub total: usize,
    /// 可用凭据数量
    pub available: usize,
}

/// 高频轮询用的轻量运行时快照条目。
///
/// 只采集 `get_runtime_stats` 实际消费的字段，锁内不做 SHA-256、
/// 不 clone JSON、不 clone 信号量 map，避免整表深克隆开销。
pub struct RuntimeEntrySnapshot {
    pub id: u64,
    pub disabled: bool,
    pub last_used_at: Option<String>,
    pub available_permits: usize,
    pub max_permits: usize,
    pub last_error_code: Option<&'static str>,
    pub last_error_at: Option<String>,
}

/// 缓存余额信息（用于 Admin API）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedBalanceInfo {
    /// 凭据 ID
    pub id: u64,
    /// 缓存的剩余额度
    pub remaining: f64,
    /// 缓存时间（Unix 毫秒时间戳）
    pub cached_at: u64,
    /// 缓存存活时间（秒）
    pub ttl_secs: u64,
}

/// 余额缓存条目（内部）
struct CachedBalance {
    remaining: f64,
    /// 超额额度剩余（overage_status=ENABLED 时 > 0；未开启或未查为 0）
    overage_remaining: f64,
    cached_at: std::time::Instant,
    /// 是否已初始化（区分"未获取过余额"和"余额为零"）
    initialized: bool,
    /// 最近一段时间的使用次数（用于判断高频/低频）
    recent_usage: u32,
    /// 上次重置使用计数的时间
    usage_reset_at: std::time::Instant,
}

/// 多凭据令牌管理器
///
/// 支持多个凭据的管理，实现固定优先级 + 故障转移策略
/// 故障统计基于 API 调用结果，而非令牌刷新结果
pub struct MultiTokenManager {
    config: RwLock<Config>,
    proxy: RwLock<Option<ProxyConfig>>,
    /// 凭据条目列表
    entries: Mutex<Vec<CredentialEntry>>,
    /// 每凭据令牌刷新锁，确保同一凭据同一时间只有一个刷新操作；
    /// 不同凭据之间并行，避免多凭据同时过期被串行化。
    refresh_locks: Mutex<HashMap<u64, Arc<TokioMutex<()>>>>,
    /// 凭据文件路径（用于回写）
    credentials_path: Option<PathBuf>,
    /// 是否为多凭据格式（数组格式才回写）
    is_multiple_format: bool,
    /// 最近一次统计持久化时间（用于 debounce）
    last_stats_save_at: Mutex<Option<Instant>>,
    /// 统计数据是否有未落盘更新
    stats_dirty: AtomicBool,
    /// 余额缓存（用于负载均衡和故障转移时选择最优凭据）
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    /// 每个凭据已知可用模型列表。缺失或空列表按默认语义乐观放行。
    model_lists: Mutex<HashMap<u64, HashSet<String>>>,
    /// MODEL_TEMPORARILY_UNAVAILABLE 错误累计（达到阈值后全局禁用）
    model_unavailable_count: AtomicU32,
    /// 选凭据轮询计数器（多个凭据评分相同时兜底使用，避免总选第一个）
    selection_rr: AtomicU64,
    /// 全局禁用恢复时间（None 表示当前没有全局禁用）
    global_recovery_time: Mutex<Option<DateTime<Utc>>>,
    /// 后台令牌刷新任务（启动后由 Drop 自动停止）
    background_refresher: Mutex<Option<Arc<BackgroundRefresher>>>,
    /// 单凭据并发信号量（按 id 维度限流，permit 数 = config.per_credential_concurrency）
    credential_semaphores: Mutex<HashMap<u64, Arc<Semaphore>>>,
    /// 全局并发信号量（None 表示不限，对应 config.global_concurrency = 0）
    global_semaphore: Mutex<Option<Arc<Semaphore>>>,
    /// Credit usage 观察者（用于将 meteringEvent 同步到 admin disk 缓存）
    credit_observer: Mutex<Option<std::sync::Weak<dyn CreditUsageObserver>>>,
    /// 会话亲和：让同一会话连续请求黏住同一凭据（提升上游 prompt cache 命中率）
    session_affinity: CredentialAffinity,
    /// 客户端亲和：让同一客户端 API 密钥在开启亲和时优先复用同一凭据
    client_affinity: CredentialAffinity,
    /// 代理池运行时来源；凭证只保存 proxyId，API 调用时回填真实代理配置。
    proxy_manager: RwLock<Option<Arc<crate::kiro::proxy_manager::ProxyManager>>>,
    profile_arn_suppressed_until: Mutex<HashMap<u64, Instant>>,
    /// 上游 429 后的短期屏蔽；仅影响调度，不持久化、不等同禁用。
    rate_limited_until: Mutex<HashMap<u64, Instant>>,
}

/// 每个凭据最大 API 调用失败次数
const MAX_FAILURES_PER_CREDENTIAL: u32 = 3;
/// 统计数据持久化防抖间隔
const STATS_SAVE_DEBOUNCE: StdDuration = StdDuration::from_secs(30);

/// 余额缓存：高频渠道 TTL（10 分钟）
const BALANCE_TTL_HIGH_FREQ_SECS: u64 = 600;
/// 余额缓存：低频渠道 TTL（30 分钟）
const BALANCE_TTL_LOW_FREQ_SECS: u64 = 1800;
/// 余额缓存：低余额渠道 TTL（24 小时，避免无意义反复刷低值）
const BALANCE_TTL_LOW_BALANCE_SECS: u64 = 86400;
/// 余额缓存：高频判定阈值（USAGE_COUNT_RESET_SECS 内使用超过此次数视为高频）
const HIGH_FREQ_THRESHOLD: u32 = 20;
/// 余额缓存：使用计数重置周期（10 分钟）
const USAGE_COUNT_RESET_SECS: u64 = 600;
/// 余额缓存：低余额阈值（小于此值切到长 TTL）
pub const LOW_BALANCE_THRESHOLD: f64 = 1.0;

/// MODEL_TEMPORARILY_UNAVAILABLE 累计阈值（达到后全局禁用所有凭据）
const MODEL_UNAVAILABLE_THRESHOLD: u32 = 2;
/// 全局禁用自动恢复延迟（分钟）
const GLOBAL_DISABLE_RECOVERY_MINUTES: i64 = 5;
const PROFILE_ARN_UNSUPPORTED_SUPPRESSION: StdDuration = StdDuration::from_secs(24 * 60 * 60);
const RATE_LIMIT_COOLDOWN: StdDuration = StdDuration::from_secs(60);

/// API 调用上下文
///
/// 绑定特定凭据的调用上下文，确保 token、credentials 和 id 的一致性
///
/// 注意：持有 Semaphore permit，不可 Clone（permit 不支持 Clone 语义；
/// Drop 时自动归还配额，调用结束即释放该凭据的并发占用）
pub struct CallContext {
    /// 凭据 ID（用于 report_success/report_failure）
    pub id: u64,
    /// 凭据信息（用于构建请求头）
    pub credentials: KiroCredentials,
    /// 访问 Token
    pub token: String,
    /// 单凭据并发 permit（Drop 时归还该凭据信号量配额）
    pub(crate) _credential_permit: Option<OwnedSemaphorePermit>,
    /// 全局并发 permit（Drop 时归还全局信号量配额；None = 未启用全局限流）
    pub(crate) _global_permit: Option<OwnedSemaphorePermit>,
    /// 代理并发 permit（Drop 时归还代理信号量配额；None = 未绑定代理池或不限并发）
    pub(crate) _proxy_permit: Option<OwnedSemaphorePermit>,
}

enum GlobalPermitAttempt {
    Disabled,
    Acquired(OwnedSemaphorePermit),
    Busy,
}

enum PoolProxyAttempt {
    Ready(Option<OwnedSemaphorePermit>),
    SkipCredential,
}

/// Credit usage 观察者：每次 meteringEvent 命中后回调
///
/// 实现方（如 `AdminService`）通常持有自己的 disk balance cache，
/// 需要在请求级 metering 上报时同步扣减，避免 dashboard 显示与运行时
/// 余额缓存（rank_candidates 调度依据）出现漂移。
pub trait CreditUsageObserver: Send + Sync {
    /// 当某凭据应用 credit 扣减后调用
    ///
    /// * `id` - 凭据 ID
    /// * `credit` - 本次 metering 上报的 credit 数（>0）
    /// * `new_primary_remaining` - 扣减后 primary 剩余
    /// * `new_overage_remaining` - 扣减后 overage 剩余
    fn on_credit_usage(
        &self,
        id: u64,
        credit: f64,
        new_primary_remaining: f64,
        new_overage_remaining: f64,
    );
}

impl MultiTokenManager {
    /// 创建多凭据 Token 管理器
    ///
    /// # Arguments
    /// * `config` - 应用配置
    /// * `credentials` - 凭据列表
    /// * `proxy` - 可选的代理配置
    /// * `credentials_path` - 凭据文件路径（用于回写）
    /// * `is_multiple_format` - 是否为多凭据格式（数组格式才回写）
    pub fn new(
        config: Config,
        credentials: Vec<KiroCredentials>,
        proxy: Option<ProxyConfig>,
        credentials_path: Option<PathBuf>,
        is_multiple_format: bool,
    ) -> anyhow::Result<Self> {
        // 计算当前最大 ID，为没有 ID 的凭据分配新 ID
        let max_existing_id = credentials.iter().filter_map(|c| c.id).max().unwrap_or(0);
        let mut next_id = max_existing_id + 1;
        let mut has_new_ids = false;
        let mut has_new_machine_ids = false;
        let mut has_allow_overage_import_migrations = false;
        let mut has_profile_arn_migrations = false;
        let entries: Vec<CredentialEntry> = credentials
            .into_iter()
            .map(|mut cred| {
                cred.canonicalize_auth_method();
                if cred.normalize_profile_arn() {
                    has_profile_arn_migrations = true;
                }
                if cred.apply_allow_overage_import_hint() {
                    has_allow_overage_import_migrations = true;
                }
                let id = cred.id.unwrap_or_else(|| {
                    let id = next_id;
                    next_id += 1;
                    cred.id = Some(id);
                    has_new_ids = true;
                    id
                });
                if machine_id::ensure_credential_machine_id(&mut cred) {
                    has_new_machine_ids = true;
                }
                CredentialEntry {
                    id,
                    credentials: cred.clone(),
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled: cred.disabled, // 从配置文件读取 disabled 状态
                    disabled_reason: if cred.disabled {
                        Some(DisabledReason::Manual)
                    } else {
                        None
                    },
                    success_count: 0,
                    last_used_at: None,
                    reprobe_next: None,
                    recovery_backoff_level: 0,
                    last_error: None,
                }
            })
            .collect();

        // 校验 API 密钥凭据配置完整性：authMethod=api_key 时必须提供 apiKey
        let mut entries = entries;
        for entry in &mut entries {
            if entry.credentials.api_key.is_none()
                && entry
                    .credentials
                    .auth_method
                    .as_deref()
                    .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                    .unwrap_or(false)
            {
                tracing::warn!(
                    "凭据 #{} 配置了 authMethod=api_key 但缺少 apiKey 字段，已自动禁用",
                    entry.id
                );
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::InvalidConfig);
            }
        }

        // 检测重复 ID
        let mut seen_ids = std::collections::HashSet::new();
        let mut duplicate_ids = Vec::new();
        for entry in &entries {
            if !seen_ids.insert(entry.id) {
                duplicate_ids.push(entry.id);
            }
        }
        if !duplicate_ids.is_empty() {
            anyhow::bail!("检测到重复的凭据 ID: {:?}", duplicate_ids);
        }

        // 初始化余额缓存（为每个凭据创建初始条目，支持负载均衡）
        let now = std::time::Instant::now();
        let initial_cache: HashMap<u64, CachedBalance> = entries
            .iter()
            .map(|e| {
                (
                    e.id,
                    CachedBalance {
                        remaining: 0.0,
                        overage_remaining: 0.0,
                        cached_at: now,
                        initialized: false,
                        recent_usage: 0,
                        usage_reset_at: now,
                    },
                )
            })
            .collect();

        // 按凭据 id 初始化单凭据并发信号量（每个凭据一把 Semaphore）
        // 配额优先取凭据级 `concurrency`，未设置则回退到全局 `per_credential_concurrency`
        let per_cred_limit = config.per_credential_concurrency.max(1);
        let credential_semaphores: HashMap<u64, Arc<Semaphore>> = entries
            .iter()
            .map(|e| {
                let n = e
                    .credentials
                    .concurrency
                    .map(|v| v as usize)
                    .unwrap_or(per_cred_limit)
                    .max(1);
                (e.id, Arc::new(Semaphore::new(n)))
            })
            .collect();
        // 全局并发信号量：global_concurrency=0 表示不启用全局限流（None）
        let global_semaphore: Option<Arc<Semaphore>> = if config.global_concurrency > 0 {
            Some(Arc::new(Semaphore::new(config.global_concurrency)))
        } else {
            None
        };

        let manager = Self {
            config: RwLock::new(config),
            proxy: RwLock::new(proxy),
            entries: Mutex::new(entries),
            refresh_locks: Mutex::new(HashMap::new()),
            credentials_path,
            is_multiple_format,
            last_stats_save_at: Mutex::new(None),
            stats_dirty: AtomicBool::new(false),
            balance_cache: Mutex::new(initial_cache),
            model_lists: Mutex::new(HashMap::new()),
            model_unavailable_count: AtomicU32::new(0),
            selection_rr: AtomicU64::new(0),
            global_recovery_time: Mutex::new(None),
            background_refresher: Mutex::new(None),
            credential_semaphores: Mutex::new(credential_semaphores),
            global_semaphore: Mutex::new(global_semaphore),
            credit_observer: Mutex::new(None),
            session_affinity: CredentialAffinity::default(),
            client_affinity: CredentialAffinity::default(),
            proxy_manager: RwLock::new(None),
            profile_arn_suppressed_until: Mutex::new(HashMap::new()),
            rate_limited_until: Mutex::new(HashMap::new()),
        };

        // 如果有新分配的 ID、新生成的 machineId 或 allowOverage 导入提示归一，立即持久化到配置文件
        if has_new_ids
            || has_new_machine_ids
            || has_allow_overage_import_migrations
            || has_profile_arn_migrations
        {
            if let Err(e) = manager.persist_credentials() {
                tracing::warn!("补全凭据 ID/machineId 后持久化失败: {}", e);
            } else {
                tracing::info!("已补全凭据 ID/machineId 并写回配置文件");
            }
        }

        // 加载持久化的统计数据（success_count, last_used_at）
        manager.load_stats();

        Ok(manager)
    }

    /// 获取配置的克隆（RwLock 持锁仅瞬时）
    pub fn config(&self) -> Config {
        self.config.read().clone()
    }

    /// 在写锁内修改全局配置（Admin 热更新使用）
    ///
    /// 闭包返回 `Err(_)` 时不会持久化也不会留下半改状态——闭包负责
    /// 在校验失败时立刻返回错误，调用方再决定如何映射错误。
    /// 闭包返回 `Ok(_)` 时调用方有责任在闭包内调用 `cfg.save()`。
    /// 所有运行时镜像（如 `MultiTokenManager.proxy`）的同步由调用方完成。
    pub fn with_config_mut<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&mut Config) -> R,
    {
        let mut guard = self.config.write();
        f(&mut guard)
    }

    /// 更新全局代理配置（Admin 热更新）
    pub fn update_proxy(&self, proxy: Option<ProxyConfig>) {
        *self.proxy.write() = proxy;
    }

    /// 更新全局默认 region（Admin 热更新）
    pub fn update_region(&self, region: String) {
        self.config.write().region = region;
    }

    /// 更新全局默认 endpoint（Admin 热更新）
    pub fn update_default_endpoint(&self, default_endpoint: String) {
        self.config.write().default_endpoint = default_endpoint;
    }

    /// 获取凭据总数
    pub fn total_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 获取可用凭据数量
    pub fn available_count(&self) -> usize {
        self.entries.lock().iter().filter(|e| !e.disabled).count()
    }

    fn allow_over_usage(&self) -> bool {
        self.config.read().allow_over_usage
    }

    pub fn set_model_list(&self, id: u64, model_ids: impl IntoIterator<Item = String>) {
        let normalized: HashSet<String> = model_ids
            .into_iter()
            .filter_map(|model| normalize_model_id(&model))
            .collect();
        self.model_lists.lock().insert(id, normalized);
    }

    pub fn get_model_list(&self, id: u64) -> Vec<String> {
        let mut models: Vec<String> = self
            .model_lists
            .lock()
            .get(&id)
            .map(|models| models.iter().cloned().collect())
            .unwrap_or_default();
        models.sort();
        models
    }

    fn credential_has_model(&self, id: u64, model: Option<&str>) -> bool {
        let Some(model) = model.and_then(normalize_model_id) else {
            return true;
        };
        let model_lists = self.model_lists.lock();
        let Some(list) = model_lists.get(&id) else {
            return true;
        };
        list.is_empty() || list.contains(&model)
    }

    fn active_rate_limited_ids(&self) -> HashSet<u64> {
        let now = Instant::now();
        let mut rate_limited = self.rate_limited_until.lock();
        rate_limited.retain(|_, until| *until > now);
        rate_limited.keys().copied().collect()
    }

    fn is_rate_limited(&self, id: u64) -> bool {
        let now = Instant::now();
        let mut rate_limited = self.rate_limited_until.lock();
        match rate_limited.get(&id).copied() {
            Some(until) if until > now => true,
            Some(_) => {
                rate_limited.remove(&id);
                false
            }
            None => false,
        }
    }

    /// 给 acquire_context 用的候选排序：返回排好序的凭据 id 列表。
    ///
    /// 排序字典序（每一项为前一项的 tie-breaker）：
    /// 1. `priority` asc：用户优先级（0 最高），分层匹配，同 priority 才参与下层比较。
    /// 2. `quota_tier` asc：正式额度可用 > 超额可用 > 无已知额度。
    /// 3. `load = in_flight / max_permits` asc：按容量归一化的当前占用率。
    /// 4. `primary_q = (primary*1000).round()` desc：正式剩余多的优先。
    /// 5. `overage_q` desc：超额剩余多的优先。
    /// 6. `in_flight = max_permits - available_permits` asc：同负载率时少在飞的优先。
    /// 7. 完整排序键相同的段：rr 轮转，公平分摊。
    ///
    /// acquire_context 拿到列表后挨个 `try_acquire_owned`，第一个抢到 permit 的就用。
    fn rank_candidates(&self, model: Option<&str>) -> Vec<u64> {
        self.rank_candidates_excluding(model, &HashSet::new())
    }

    fn rank_candidates_excluding(&self, model: Option<&str>, excluded: &HashSet<u64>) -> Vec<u64> {
        // 1. 过滤可用候选
        let rate_limited = self.active_rate_limited_ids();
        let candidate_ids: Vec<u64> = {
            let entries = self.entries.lock();
            let is_opus = model
                .map(|m| m.to_lowercase().contains("opus"))
                .unwrap_or(false);
            let eligible: Vec<u64> = entries
                .iter()
                .filter(|e| !e.disabled)
                .filter(|e| !excluded.contains(&e.id))
                .filter(|e| self.credential_has_model(e.id, model))
                .filter(|e| !is_opus || e.credentials.supports_opus())
                .map(|e| e.id)
                .collect();
            let not_rate_limited: Vec<u64> = eligible
                .iter()
                .copied()
                .filter(|id| !rate_limited.contains(id))
                .collect();
            if not_rate_limited.is_empty() {
                eligible
            } else {
                not_rate_limited
            }
        };

        if candidate_ids.is_empty() {
            return Vec::new();
        }

        let rr_offset = self
            .selection_rr
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;

        // 2. 拷贝每个候选的 Semaphore Arc（短锁），稍后无锁查 available_permits
        let sema_map: HashMap<u64, Arc<Semaphore>> = {
            let map = self.credential_semaphores.lock();
            candidate_ids
                .iter()
                .filter_map(|&id| map.get(&id).map(|s| (id, s.clone())))
                .collect()
        };

        // 3. 取每个候选的 max_permits（凭据级 override 优先，回退全局）、priority 和 weight
        let global_per_cred = self.config.read().per_credential_concurrency.max(1);
        let (max_permits_map, priority_map, weight_map): (
            HashMap<u64, usize>,
            HashMap<u64, u32>,
            HashMap<u64, usize>,
        ) = {
            let entries = self.entries.lock();
            let mut max_map = HashMap::new();
            let mut pri_map = HashMap::new();
            let mut weight_map = HashMap::new();
            for &id in &candidate_ids {
                if let Some(entry) = entries.iter().find(|e| e.id == id) {
                    let max = entry
                        .credentials
                        .concurrency
                        .map(|v| v as usize)
                        .unwrap_or(global_per_cred)
                        .max(1);
                    max_map.insert(id, max);
                    pri_map.insert(id, entry.credentials.priority);
                    weight_map.insert(id, effective_weight(entry.credentials.weight));
                }
            }
            (max_map, pri_map, weight_map)
        };

        // 4. 取余额缓存（区分已初始化 vs 未初始化）
        //    每个候选返回 (primary_remaining, overage_remaining)
        //    未初始化的凭据用 unknown_fallback（已知余额的平均值），避免排序末尾死循环
        let balances: HashMap<u64, Option<(f64, f64)>> = {
            let cache = self.balance_cache.lock();
            candidate_ids
                .iter()
                .map(|&id| {
                    let val = cache.get(&id).and_then(|c| {
                        if c.initialized {
                            Some((c.remaining, c.overage_remaining))
                        } else {
                            None
                        }
                    });
                    (id, val)
                })
                .collect()
        };
        // 已知 primary 的均值兜底；都未知则 1.0
        let unknown_fallback: f64 = {
            let known: Vec<f64> = balances.values().filter_map(|v| v.map(|p| p.0)).collect();
            if known.is_empty() {
                1.0
            } else {
                let avg = known.iter().sum::<f64>() / known.len() as f64;
                avg.max(1e-3)
            }
        };

        // 5. 排序键：(priority asc, quota_tier asc, load asc,
        //              primary_q desc, overage_q desc, in_flight asc)
        //    - priority asc：用户优先级（0 最高），分层匹配的最外层
        //    - quota_tier asc：正式额度可用永远压制只剩超额/无额度的候选
        //    - load asc：同档位先平衡占用率，避免大并发凭据因容量高持续吃掉突发请求
        //    - primary_q / overage_q desc：占用率相同后再看剩余额度
        //    - in_flight asc：负载率与余额都相同后，当前少在飞的优先
        const REMAINING_EPS: f64 = 1e-3;
        let quantize = |r: f64| -> i64 { (r.max(0.0) * 1000.0).round() as i64 };
        let mut scored: Vec<(u64, u32, u8, usize, i64, i64, usize)> = candidate_ids
            .iter()
            .map(|&id| {
                let max = *max_permits_map.get(&id).unwrap_or(&global_per_cred);
                let avail = sema_map
                    .get(&id)
                    .map(|s| s.available_permits())
                    .unwrap_or(max);
                let in_flight = max.saturating_sub(avail);
                let cached = balances.get(&id).copied().flatten();
                let (primary, overage) = match cached {
                    Some((p, o)) => (p.max(0.0), o.max(0.0)),
                    None => (unknown_fallback.max(REMAINING_EPS), 0.0),
                };
                let has_primary = primary >= 1.0;
                let has_overage = overage >= 1.0;
                let quota_tier = if has_primary {
                    0
                } else if has_overage {
                    1
                } else {
                    2
                };
                let load = in_flight.saturating_mul(1_000_000) / max.max(1);
                let pri = priority_map.get(&id).copied().unwrap_or(0);
                (
                    id,
                    pri,
                    quota_tier,
                    load,
                    quantize(primary),
                    quantize(overage),
                    in_flight,
                )
            })
            .collect();

        scored.sort_by(|a, b| {
            a.1.cmp(&b.1) // priority asc
                .then(a.2.cmp(&b.2)) // quota_tier asc
                .then(a.3.cmp(&b.3)) // normalized load asc
                .then(b.4.cmp(&a.4)) // primary_q desc
                .then(b.5.cmp(&a.5)) // overage_q desc
                .then(a.6.cmp(&b.6)) // in_flight asc
        });

        // 6. 完整排序键相同的段做加权 rr（同优先级同额度同负载才按 weight 打散）
        let mut result: Vec<u64> = Vec::with_capacity(scored.len());
        let mut i = 0;
        while i < scored.len() {
            let mut j = i + 1;
            while j < scored.len()
                && scored[j].1 == scored[i].1
                && scored[j].2 == scored[i].2
                && scored[j].3 == scored[i].3
                && scored[j].4 == scored[i].4
                && scored[j].5 == scored[i].5
                && scored[j].6 == scored[i].6
            {
                j += 1;
            }
            let len = j - i;
            let mut weighted_segment = Vec::with_capacity(len);
            for item in scored.iter().take(j).skip(i) {
                let id = item.0;
                let weight = *weight_map.get(&id).unwrap_or(&1);
                for _ in 0..weight {
                    weighted_segment.push(id);
                }
            }
            let weighted_len = weighted_segment.len();
            let start = if weighted_len > 0 {
                rr_offset % weighted_len
            } else {
                0
            };
            let mut seen = HashSet::with_capacity(len);
            for k in 0..weighted_len {
                let id = weighted_segment[(start + k) % weighted_len];
                if seen.insert(id) {
                    result.push(id);
                    if seen.len() == len {
                        break;
                    }
                }
            }
            i = j;
        }
        result
    }

    fn try_acquire_global_permit(&self) -> anyhow::Result<GlobalPermitAttempt> {
        let global_arc_opt = { self.global_semaphore.lock().clone() };
        let Some(global) = global_arc_opt else {
            return Ok(GlobalPermitAttempt::Disabled);
        };

        match global.try_acquire_owned() {
            Ok(permit) => Ok(GlobalPermitAttempt::Acquired(permit)),
            Err(TryAcquireError::NoPermits) => Ok(GlobalPermitAttempt::Busy),
            Err(TryAcquireError::Closed) => Err(anyhow::anyhow!("global semaphore closed")),
        }
    }

    async fn acquire_global_permit(
        &self,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Option<OwnedSemaphorePermit>> {
        let global_arc_opt = { self.global_semaphore.lock().clone() };
        let Some(global) = global_arc_opt else {
            return Ok(None);
        };

        match tokio::time::timeout(timeout, global.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Some(permit)),
            Ok(Err(e)) => Err(anyhow::anyhow!("global semaphore closed: {}", e)),
            Err(_) => Err(anyhow::anyhow!("global semaphore wait timeout")),
        }
    }

    /// 在多个候选凭据上同时排队，谁先空出来就用谁。
    ///
    /// - 先收集每个 id 的 `Arc<Semaphore>`，构造 `acquire_owned()` future；
    /// - `select_all` 等任意一个先 ready；
    /// - 整体套 `tokio::time::timeout`，超时 → `KiroError::CredentialQueueTimeout`。
    ///
    /// 注意：此方法只用于 candidates 全部 try_acquire 失败后的"全员排队"分支；
    /// 主路径仍是 acquire_context 顺序 try_acquire_owned 抢先。
    async fn wait_any_credential(
        &self,
        candidates: &[u64],
        timeout: std::time::Duration,
    ) -> anyhow::Result<(u64, OwnedSemaphorePermit)> {
        if candidates.is_empty() {
            anyhow::bail!("no candidate credentials to wait on");
        }

        // 收集每个候选的 Semaphore Arc（不同时持有 entries 锁，避免锁顺序倒挂）
        let mut sema_pairs: Vec<(u64, std::sync::Arc<Semaphore>)> =
            Vec::with_capacity(candidates.len());
        {
            let map = self.credential_semaphores.lock();
            for &id in candidates {
                if let Some(s) = map.get(&id) {
                    sema_pairs.push((id, s.clone()));
                }
            }
        }

        if sema_pairs.is_empty() {
            anyhow::bail!("no semaphores registered for given candidates");
        }

        // 构造一组 boxed future：每个 future 绑定其 id，acquire_owned 成功后返回 (id, permit)
        let futures: Vec<_> = sema_pairs
            .into_iter()
            .map(|(id, sema)| {
                Box::pin(async move {
                    let permit = sema
                        .acquire_owned()
                        .await
                        .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;
                    Ok::<(u64, OwnedSemaphorePermit), anyhow::Error>((id, permit))
                })
            })
            .collect();

        match tokio::time::timeout(timeout, select_all(futures)).await {
            Ok((Ok((id, permit)), _idx, _rest)) => Ok((id, permit)),
            Ok((Err(e), _idx, _rest)) => Err(e),
            Err(_) => Err(anyhow::anyhow!("credential queue wait timeout")),
        }
    }

    /// 按 session_id 亲和获取调用上下文
    ///
    /// 同一 session（per-conversation UUID）连续请求黏住同一凭据，
    /// 提升上游 prompt cache 命中率。**严禁**用 device-level user_id 作 key
    /// （机器哈希常量，会让所有 session 永久粘连同一凭据破坏平摊）。
    ///
    /// 流程：
    /// 1. 命中绑定且凭据 enabled / 模型匹配 / sema 抢到 → 复用，touch
    /// 2. 命中但 sema 抢不到（in_flight 满）→ 移除绑定，回退到 rank 分流
    ///    （避免长期黏在拥堵凭据上）
    /// 3. 命中但凭据 disabled / 模型不允许 → 清绑定，rank 重选
    /// 4. 未命中 / session_id 缺失 → rank 选 + 新建绑定
    ///
    /// 参数 `session_id`：
    /// - `None` 或空：等同 `acquire_context`（不建立绑定）
    /// - `Some(uuid)`：以此 UUID 为 key 走亲和路径
    pub async fn acquire_context_for_session(
        &self,
        session_id: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        self.acquire_context_for_session_excluding(session_id, model, &HashSet::new())
            .await
    }

    pub(crate) async fn acquire_context_for_session_excluding(
        &self,
        session_id: Option<&str>,
        model: Option<&str>,
        excluded: &HashSet<u64>,
    ) -> anyhow::Result<CallContext> {
        self.acquire_context_for_affinity_key(
            session_id,
            model,
            excluded,
            &self.session_affinity,
            "session",
        )
        .await
    }

    pub async fn acquire_context_for_client(
        &self,
        client_key: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        self.acquire_context_for_client_excluding(client_key, model, &HashSet::new())
            .await
    }

    pub(crate) async fn acquire_context_for_client_excluding(
        &self,
        client_key: Option<&str>,
        model: Option<&str>,
        excluded: &HashSet<u64>,
    ) -> anyhow::Result<CallContext> {
        self.acquire_context_for_affinity_key(
            client_key,
            model,
            excluded,
            &self.client_affinity,
            "client",
        )
        .await
    }

    pub(crate) async fn acquire_context_for_route_excluding(
        &self,
        session_id: Option<&str>,
        client_key: Option<&str>,
        model: Option<&str>,
        excluded: &HashSet<u64>,
    ) -> anyhow::Result<CallContext> {
        if session_id.is_some_and(|key| !key.is_empty()) {
            return self
                .acquire_context_for_session_excluding(session_id, model, excluded)
                .await;
        }
        if client_key.is_some_and(|key| !key.is_empty()) {
            return self
                .acquire_context_for_client_excluding(client_key, model, excluded)
                .await;
        }
        self.acquire_context_excluding(model, excluded).await
    }

    async fn acquire_context_for_affinity_key(
        &self,
        key: Option<&str>,
        model: Option<&str>,
        excluded: &HashSet<u64>,
        affinity: &CredentialAffinity,
        key_kind: &'static str,
    ) -> anyhow::Result<CallContext> {
        if !self.config.read().session_affinity_enabled {
            return self.acquire_context_excluding(model, excluded).await;
        }

        let key = match key {
            Some(s) if !s.is_empty() => s,
            _ => return self.acquire_context_excluding(model, excluded).await,
        };

        if let Some(bound_id) = affinity.get(key) {
            let usable = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == bound_id && !e.disabled && !excluded.contains(&e.id))
                    .map(|e| {
                        self.credential_supports_model(e, model) && !self.is_rate_limited(e.id)
                    })
                    .unwrap_or(false)
            };
            if !usable {
                tracing::debug!(
                    affinity_kind = key_kind,
                    credential_id = %bound_id,
                    "亲和命中但凭据不可用，重选"
                );
                affinity.remove(key);
            } else {
                let sema_opt = {
                    let map = self.credential_semaphores.lock();
                    map.get(&bound_id).cloned()
                };
                if let Some(sema) = sema_opt
                    && let Ok(per_cred_permit) = sema.try_acquire_owned()
                {
                    let credentials = {
                        let entries = self.entries.lock();
                        entries
                            .iter()
                            .find(|e| e.id == bound_id && !e.disabled)
                            .map(|e| e.credentials.clone())
                    };
                    if let Some(mut creds) = credentials {
                        let proxy_permit =
                            match self.acquire_pool_proxy_for_call(bound_id, &mut creds) {
                                PoolProxyAttempt::Ready(permit) => permit,
                                PoolProxyAttempt::SkipCredential => {
                                    drop(per_cred_permit);
                                    return self.acquire_context_excluding(model, excluded).await;
                                }
                            };
                        let global_permit = match self.try_acquire_global_permit()? {
                            GlobalPermitAttempt::Disabled => None,
                            GlobalPermitAttempt::Acquired(permit) => Some(permit),
                            GlobalPermitAttempt::Busy => {
                                drop(proxy_permit);
                                drop(per_cred_permit);
                                return self.acquire_context_excluding(model, excluded).await;
                            }
                        };
                        match self.try_ensure_token(bound_id, &creds).await {
                            Ok(mut ctx) => {
                                ctx.credentials.proxy_url = creds.proxy_url.clone();
                                ctx.credentials.proxy_username = creds.proxy_username.clone();
                                ctx.credentials.proxy_password = creds.proxy_password.clone();
                                ctx._credential_permit = Some(per_cred_permit);
                                ctx._global_permit = global_permit;
                                ctx._proxy_permit = proxy_permit;
                                affinity.touch(key);
                                return Ok(ctx);
                            }
                            Err(e) => {
                                drop(per_cred_permit);
                                drop(global_permit);
                                drop(proxy_permit);
                                tracing::debug!(
                                    affinity_kind = key_kind,
                                    credential_id = %bound_id,
                                    error = %e,
                                    "亲和绑定凭据令牌刷新失败，回退到 rank"
                                );
                                if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                                    self.report_refresh_token_invalid(bound_id);
                                } else {
                                    self.report_refresh_failure(bound_id);
                                }
                            }
                        }
                    } else {
                        drop(per_cred_permit);
                    }
                } else {
                    tracing::debug!(
                        affinity_kind = key_kind,
                        credential_id = %bound_id,
                        "亲和绑定凭据 sema 满，本次分流（保留绑定）"
                    );
                    return self.acquire_context_excluding(model, excluded).await;
                }
            }
        }

        let ctx = self.acquire_context_excluding(model, excluded).await?;
        affinity.set(key, ctx.id);
        Ok(ctx)
    }

    /// 检查凭据是否支持指定模型（与 rank_candidates 筛选规则一致）
    fn credential_supports_model(&self, entry: &CredentialEntry, model: Option<&str>) -> bool {
        let Some(m) = model else { return true };
        if !self.credential_has_model(entry.id, model) {
            return false;
        }
        if !m.to_lowercase().contains("opus") {
            return true;
        }
        entry.credentials.supports_opus()
    }

    /// 获取 API 调用上下文
    ///
    /// 返回绑定了 id、credentials 和 token 的调用上下文
    /// 确保整个 API 调用过程中使用一致的凭据信息
    ///
    /// 如果令牌过期或即将过期，会自动刷新
    /// 令牌刷新失败会累计到当前凭据，达到阈值后禁用并切换
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    pub async fn acquire_context(&self, model: Option<&str>) -> anyhow::Result<CallContext> {
        self.acquire_context_excluding(model, &HashSet::new()).await
    }

    pub async fn acquire_context_for_credential(
        &self,
        id: u64,
        model: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        self.check_and_recover();

        let wait_timeout =
            std::time::Duration::from_secs(self.config.read().acquire_wait_timeout_secs);
        let mut reserved_global_permit: Option<OwnedSemaphorePermit> = None;

        loop {
            let mut credentials = {
                let entries = self.entries.lock();
                let entry = entries
                    .iter()
                    .find(|entry| entry.id == id)
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 不存在", id))?;
                if entry.disabled {
                    anyhow::bail!("凭据 #{} 已禁用", id);
                }
                if !self.credential_supports_model(entry, model) {
                    anyhow::bail!("凭据 #{} 不支持模型 {:?}", id, model);
                }
                if self.is_rate_limited(id) {
                    anyhow::bail!("凭据 #{} 正在限流冷却中", id);
                }
                entry.credentials.clone()
            };

            let sema = {
                let map = self.credential_semaphores.lock();
                map.get(&id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 未注册并发信号量", id))?
            };
            let per_cred_permit = tokio::time::timeout(wait_timeout, sema.acquire_owned())
                .await
                .map_err(|_| anyhow::anyhow!("credential queue wait timeout"))?
                .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

            let proxy_permit = match self.acquire_pool_proxy_for_call(id, &mut credentials) {
                PoolProxyAttempt::Ready(permit) => permit,
                PoolProxyAttempt::SkipCredential => {
                    drop(per_cred_permit);
                    anyhow::bail!("凭据 #{} 绑定的代理不可用或并发已满", id);
                }
            };

            let global_permit = if let Some(permit) = reserved_global_permit.take() {
                Some(permit)
            } else {
                match self.try_acquire_global_permit()? {
                    GlobalPermitAttempt::Disabled => None,
                    GlobalPermitAttempt::Acquired(permit) => Some(permit),
                    GlobalPermitAttempt::Busy => {
                        drop(proxy_permit);
                        drop(per_cred_permit);
                        reserved_global_permit = self.acquire_global_permit(wait_timeout).await?;
                        continue;
                    }
                }
            };

            match self.try_ensure_token(id, &credentials).await {
                Ok(mut ctx) => {
                    ctx.credentials.proxy_url = credentials.proxy_url.clone();
                    ctx.credentials.proxy_username = credentials.proxy_username.clone();
                    ctx.credentials.proxy_password = credentials.proxy_password.clone();
                    ctx._credential_permit = Some(per_cred_permit);
                    ctx._global_permit = global_permit;
                    ctx._proxy_permit = proxy_permit;
                    return Ok(ctx);
                }
                Err(e) => {
                    drop(per_cred_permit);
                    drop(global_permit);
                    drop(proxy_permit);
                    if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                        self.report_refresh_token_invalid(id);
                    } else {
                        self.report_refresh_failure(id);
                    }
                    return Err(e);
                }
            }
        }
    }

    pub(crate) async fn acquire_context_excluding(
        &self,
        model: Option<&str>,
        excluded: &HashSet<u64>,
    ) -> anyhow::Result<CallContext> {
        // 检查是否需要自动恢复（5 分钟全局禁用）
        self.check_and_recover();

        let total = self.total_count();
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize).max(1);
        let mut attempt_count = 0;

        // 排队等待超时：从 config 动态读取（默认 60s）
        let timeout_secs = self.config.read().acquire_wait_timeout_secs;
        let wait_timeout = std::time::Duration::from_secs(timeout_secs);
        let mut reserved_global_permit: Option<OwnedSemaphorePermit> = None;

        loop {
            if attempt_count >= max_attempts {
                anyhow::bail!(
                    "所有凭据均无法获取有效令牌（可用: {}/{}）",
                    self.available_count(),
                    total
                );
            }

            // 1. 收集候选（统一加权排序：priority asc, recent_usage asc, remaining desc）
            let mut candidates = self.rank_candidates_excluding(model, excluded);

            // 候选为空：尝试 TooManyFailures 自愈（等价于重启）
            if candidates.is_empty() {
                let mut entries = self.entries.lock();
                if entries.iter().any(|e| {
                    e.disabled && e.disabled_reason == Some(DisabledReason::TooManyFailures)
                }) {
                    tracing::warn!(
                        "所有凭据均已被自动禁用，执行自愈：重置失败计数并重新启用（等价于重启）"
                    );
                    for e in entries.iter_mut() {
                        if e.disabled_reason == Some(DisabledReason::TooManyFailures) {
                            e.disabled = false;
                            e.disabled_reason = None;
                            e.failure_count = 0;
                            e.clear_error();
                        }
                    }
                    drop(entries);
                    candidates = self.rank_candidates_excluding(model, excluded);
                }
            }

            if candidates.is_empty() {
                // 注意：available_count() 内部会取 entries 锁，必须在 lock 释放后调用
                let available = self.available_count();
                anyhow::bail!("所有凭据均已禁用（{}/{}）", available, total);
            }

            // 2. 按候选顺序尝试 try_acquire_owned（非阻塞，首胜出）
            //    这样\"最优凭据被占满\"时能立刻跳到次优，避免单点排队退化
            let mut acquired: Option<(u64, OwnedSemaphorePermit)> = None;
            for &cid in &candidates {
                let sema_opt = {
                    let map = self.credential_semaphores.lock();
                    map.get(&cid).cloned()
                };
                if let Some(sema) = sema_opt
                    && let Ok(permit) = sema.try_acquire_owned()
                {
                    acquired = Some((cid, permit));
                    break;
                }
            }

            // 3. 全部 try_acquire 失败 → 进入"全员排队"分支，等待任一凭据释放
            let (id, per_cred_permit) = if let Some(pair) = acquired {
                pair
            } else {
                drop(reserved_global_permit.take());
                match self.wait_any_credential(&candidates, wait_timeout).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!("等待凭据 permit 失败: {}", e);
                        return Err(e);
                    }
                }
            };

            // 4. 拿到 per-cred permit 后取 credentials 副本
            //    注意：等待期间凭据可能被禁用（被踢出 entries），需要防御性检查
            let credentials = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id && !e.disabled)
                    .map(|e| e.credentials.clone())
            };
            let mut credentials = match credentials {
                Some(c) => c,
                None => {
                    // 等待期间凭据被禁用 → 释放 permit 重新选择
                    drop(per_cred_permit);
                    attempt_count += 1;
                    continue;
                }
            };

            let proxy_permit = match self.acquire_pool_proxy_for_call(id, &mut credentials) {
                PoolProxyAttempt::Ready(permit) => permit,
                PoolProxyAttempt::SkipCredential => {
                    drop(per_cred_permit);
                    attempt_count += 1;
                    continue;
                }
            };

            // 5. 获取 global permit（如配置了 global_concurrency > 0）
            //    等待全局并发时不能持有单凭据 permit，否则会污染凭据负载排序。
            let global_permit = if let Some(permit) = reserved_global_permit.take() {
                Some(permit)
            } else {
                match self.try_acquire_global_permit()? {
                    GlobalPermitAttempt::Disabled => None,
                    GlobalPermitAttempt::Acquired(permit) => Some(permit),
                    GlobalPermitAttempt::Busy => {
                        drop(proxy_permit);
                        drop(per_cred_permit);
                        reserved_global_permit = self.acquire_global_permit(wait_timeout).await?;
                        continue;
                    }
                }
            };

            // 6. 尝试获取/刷新令牌，并把 permit 注入 CallContext（Drop 自动归还）
            match self.try_ensure_token(id, &credentials).await {
                Ok(mut ctx) => {
                    ctx.credentials.proxy_url = credentials.proxy_url.clone();
                    ctx.credentials.proxy_username = credentials.proxy_username.clone();
                    ctx.credentials.proxy_password = credentials.proxy_password.clone();
                    ctx._credential_permit = Some(per_cred_permit);
                    ctx._global_permit = global_permit;
                    ctx._proxy_permit = proxy_permit;
                    return Ok(ctx);
                }
                Err(e) => {
                    // 早 drop：刷新失败时尽快归还 permit，避免占用排队席位
                    drop(per_cred_permit);
                    drop(global_permit);
                    drop(proxy_permit);

                    // refreshToken 永久失效 → 立即禁用，不累计重试
                    let has_available = if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                        tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, e);
                        self.report_refresh_token_invalid(id)
                    } else {
                        tracing::warn!("凭据 #{} 令牌刷新失败: {}", id, e);
                        self.report_refresh_failure(id)
                    };
                    attempt_count += 1;
                    if !has_available {
                        anyhow::bail!("所有凭据均已禁用（0/{}）", total);
                    }
                }
            }
        }
    }

    // ============================================================
    // 全局健康协调（model_unavailable + 全局禁用 + 自动恢复）
    // MODEL_TEMPORARILY_UNAVAILABLE 累计触发全局禁用，
    // 5 分钟后自动恢复 ModelUnavailable 类型禁用。
    // ============================================================

    /// 报告 MODEL_TEMPORARILY_UNAVAILABLE 错误
    ///
    /// 累计达到阈值后禁用所有凭据，5 分钟后自动恢复
    /// 返回是否触发了全局禁用
    pub fn report_model_unavailable(&self) -> bool {
        let count = self.model_unavailable_count.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::warn!(
            "MODEL_TEMPORARILY_UNAVAILABLE 错误（{}/{}）",
            count,
            MODEL_UNAVAILABLE_THRESHOLD
        );

        if count >= MODEL_UNAVAILABLE_THRESHOLD {
            self.disable_all_credentials(DisabledReason::ModelUnavailable);
            true
        } else {
            false
        }
    }

    /// 禁用所有凭据并设置全局恢复时间
    fn disable_all_credentials(&self, reason: DisabledReason) {
        let mut entries = self.entries.lock();
        let mut recovery_time = self.global_recovery_time.lock();

        let code = error_code_for_reason(reason);
        for entry in entries.iter_mut() {
            if !entry.disabled {
                entry.disabled = true;
                entry.disabled_reason = Some(reason);
                entry.record_error(code);
            }
        }

        // 设置恢复时间
        let recover_at = Utc::now() + Duration::minutes(GLOBAL_DISABLE_RECOVERY_MINUTES);
        *recovery_time = Some(recover_at);

        tracing::error!(
            "所有凭据已被禁用（原因: {:?}），将于 {} 自动恢复",
            reason,
            recover_at.format("%H:%M:%S")
        );
    }

    /// 检查并执行自动恢复
    ///
    /// 如果已到恢复时间，恢复因 ModelUnavailable 禁用的凭据
    /// 余额不足、认证失败、上游暂停等不会被自动恢复
    ///
    /// 返回是否执行了恢复
    pub fn check_and_recover(&self) -> bool {
        let should_recover = {
            let recovery_time = self.global_recovery_time.lock();
            recovery_time.map(|t| Utc::now() >= t).unwrap_or(false)
        };

        if !should_recover {
            return false;
        }

        let mut entries = self.entries.lock();
        let mut recovery_time = self.global_recovery_time.lock();
        let mut recovered_count = 0;

        for entry in entries.iter_mut() {
            // 只恢复因 ModelUnavailable 禁用的凭据
            if entry.disabled && entry.disabled_reason == Some(DisabledReason::ModelUnavailable) {
                entry.disabled = false;
                entry.disabled_reason = None;
                entry.failure_count = 0;
                entry.clear_error();
                recovered_count += 1;
            }
        }

        // 重置全局状态
        *recovery_time = None;
        self.model_unavailable_count.store(0, Ordering::SeqCst);

        if recovered_count > 0 {
            tracing::info!("已自动恢复 {} 个凭据", recovered_count);
        }

        recovered_count > 0
    }

    /// 获取全局恢复时间（用于 Admin API）
    #[allow(dead_code)]
    pub fn get_recovery_time(&self) -> Option<DateTime<Utc>> {
        *self.global_recovery_time.lock()
    }

    fn mark_credential_banned(&self, id: u64, reason: DisabledReason, ban_reason: &'static str) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                let now = Utc::now();
                entry.disabled = true;
                entry.disabled_reason = Some(reason);
                entry.record_error(error_code_for_reason(reason));
                entry.last_used_at = Some(now.to_rfc3339());
                entry.credentials.disabled = true;
                entry.credentials.meta.disabled_reason =
                    persistent_disabled_reason(Some(reason)).map(std::string::ToString::to_string);
                entry.credentials.meta.ban_status = Some("BANNED".to_string());
                entry.credentials.meta.ban_reason = Some(ban_reason.to_string());
                entry.credentials.meta.ban_time = Some(now.timestamp());
                // A4：认证类禁用登记首次自愈重探（level 1 → 1min）。
                // 非认证类（如 CredentialSuspended）保持粘性禁用，不调度。
                if is_auth_recoverable_reason(reason) {
                    entry.recovery_backoff_level = 1;
                    entry.reprobe_next = Some(now + backoff_duration(1));
                }
            }
        }
        self.remove_affinity_by_credential(id);
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("持久化凭据封禁状态失败: {}", e);
        }
        self.save_stats_debounced();
    }

    /// 标记凭据为认证失败（如 invalid_grant，不会被自动恢复）
    pub fn mark_authentication_failed(&self, id: u64) {
        self.mark_credential_banned(
            id,
            DisabledReason::AuthenticationFailed,
            "Authentication failed - token invalid or expired",
        );
        tracing::warn!("凭据 #{} 已标记为认证失败", id);
    }

    /// 标记凭据被上游暂停（不会被自动恢复）
    pub fn mark_credential_suspended_by_upstream(&self, id: u64) {
        self.mark_credential_banned(
            id,
            DisabledReason::CredentialSuspended,
            "AWS temporarily suspended - unusual user activity detected",
        );
        tracing::warn!("凭据 #{} 已标记为上游暂停", id);
    }

    /// 标记凭据为余额不足（不会被自动恢复）
    pub fn mark_insufficient_balance(&self, id: u64) -> bool {
        if self.allow_over_usage() {
            tracing::warn!("凭据 #{} 余额不足，但 allowOverUsage=true，保持可路由", id);
            return false;
        }

        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InsufficientBalance);
            entry.record_error("insufficient_balance");
            tracing::warn!("凭据 #{} 已标记为余额不足", id);
            return true;
        }
        false
    }

    // ============================================================
    // 余额缓存（balance_cache）相关方法
    // ============================================================

    /// 获取缓存的余额（动态 TTL：低余额 > 高频 > 低频）
    ///
    /// 缓存不存在或已过期时返回 0.0，调用方应回退到优先级选择
    #[allow(dead_code)]
    fn get_cached_balance(&self, id: u64) -> f64 {
        let cache = self.balance_cache.lock();
        if let Some(entry) = cache.get(&id) {
            // 动态 TTL：低余额 > 低频 > 高频
            let ttl = if entry.remaining < LOW_BALANCE_THRESHOLD {
                BALANCE_TTL_LOW_BALANCE_SECS
            } else if entry.recent_usage >= HIGH_FREQ_THRESHOLD {
                BALANCE_TTL_HIGH_FREQ_SECS
            } else {
                BALANCE_TTL_LOW_FREQ_SECS
            };
            if entry.cached_at.elapsed().as_secs() < ttl {
                return entry.remaining;
            }
        }
        // 缓存不存在或过期，返回 0（会回退到优先级选择）
        0.0
    }

    /// 更新余额缓存（含超额额度）
    ///
    /// `overage_remaining` 仅在 overage_status=ENABLED 时 > 0；
    /// rank 用 (has_primary, primary_q, overage_q) 区分主备额度优先级（千分之一量化）。
    pub fn update_balance_cache_full(&self, id: u64, remaining: f64, overage_remaining: f64) {
        let mut cache = self.balance_cache.lock();
        let now = std::time::Instant::now();
        let (recent_usage, usage_reset_at) = cache
            .get(&id)
            .map(|e| (e.recent_usage, e.usage_reset_at))
            .unwrap_or((0, now));
        cache.insert(
            id,
            CachedBalance {
                remaining,
                overage_remaining,
                cached_at: now,
                initialized: true,
                recent_usage,
                usage_reset_at,
            },
        );
    }

    /// 从持久化缓存恢复余额信息（用于服务启动后恢复 Admin UI 展示）
    ///
    /// `cached_at_unix_secs` 为持久化时记录的 Unix 时间戳（秒）。
    /// 系统刚重启或 uptime < age_secs 时，将 cached_at 设为足够旧的时间点，
    /// 确保 TTL 判定视为已过期，下一次调用会触发重新刷新。
    #[allow(dead_code)]
    pub fn restore_balance_cache(&self, id: u64, remaining: f64, cached_at_unix_secs: f64) {
        let mut cache = self.balance_cache.lock();
        let now_instant = std::time::Instant::now();
        let now_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let age_secs = (now_unix_secs - cached_at_unix_secs).max(0.0);
        // 若系统 uptime < age_secs（如刚重启），checked_sub 会返回 None，
        // 此时设为足够旧的时间点（now - 24h），确保 TTL 判定视为已过期
        let restored_cached_at = now_instant
            .checked_sub(std::time::Duration::from_secs_f64(age_secs))
            .unwrap_or_else(|| {
                now_instant
                    .checked_sub(std::time::Duration::from_secs(86400))
                    .unwrap_or(now_instant)
            });

        let (recent_usage, usage_reset_at) = cache
            .get(&id)
            .map(|e| (e.recent_usage, e.usage_reset_at))
            .unwrap_or((0, now_instant));

        cache.insert(
            id,
            CachedBalance {
                remaining,
                overage_remaining: 0.0,
                cached_at: restored_cached_at,
                initialized: true,
                recent_usage,
                usage_reset_at,
            },
        );
    }

    /// 检查是否需要刷新余额缓存
    ///
    /// 未初始化的缓存返回 true 立即触发刷新；已初始化的根据动态 TTL 判断
    #[allow(dead_code)]
    pub fn should_refresh_balance(&self, id: u64) -> bool {
        let cache = self.balance_cache.lock();
        if let Some(entry) = cache.get(&id) {
            // 未初始化的缓存需要立即刷新
            if !entry.initialized {
                return true;
            }
            // 使用动态 TTL 判断是否过期
            let ttl = if entry.remaining < LOW_BALANCE_THRESHOLD {
                BALANCE_TTL_LOW_BALANCE_SECS
            } else if entry.recent_usage >= HIGH_FREQ_THRESHOLD {
                BALANCE_TTL_HIGH_FREQ_SECS
            } else {
                BALANCE_TTL_LOW_FREQ_SECS
            };
            entry.cached_at.elapsed().as_secs() >= ttl
        } else {
            true // 无缓存，需要刷新
        }
    }

    /// 记录凭据使用（用于动态 TTL 计算和负载均衡）
    ///
    /// 每次成功获取令牌时调用，递增 recent_usage；超过重置周期则清零
    pub fn record_usage(&self, id: u64) {
        let mut cache = self.balance_cache.lock();
        let now = std::time::Instant::now();
        if let Some(entry) = cache.get_mut(&id) {
            // 重置周期过期则清零
            if entry.usage_reset_at.elapsed().as_secs() >= USAGE_COUNT_RESET_SECS {
                entry.recent_usage = 1;
                entry.usage_reset_at = now;
            } else {
                entry.recent_usage = entry.recent_usage.saturating_add(1);
            }
        } else {
            // 缓存条目不存在时创建新条目（余额未知设为 0）
            cache.insert(
                id,
                CachedBalance {
                    remaining: 0.0,
                    overage_remaining: 0.0,
                    cached_at: now,
                    initialized: false,
                    recent_usage: 1,
                    usage_reset_at: now,
                },
            );
        }
    }

    /// 失效指定凭据的运行时余额缓存（标记为未初始化、TTL=0）
    ///
    /// 在调用 `setUserPreference` 类的状态变更操作后立即调用，
    /// 让下一次余额查询透传到上游获取最新值。
    pub fn invalidate_balance_cache(&self, id: u64) {
        let mut cache = self.balance_cache.lock();
        if let Some(entry) = cache.get_mut(&id) {
            entry.initialized = false;
            entry.recent_usage = 0;
        }
    }

    /// 注册 credit usage 观察者（弱引用，避免循环引用）
    ///
    /// AdminService 可注册自身以便 metering 时同步更新 disk balance_cache。
    pub fn set_credit_observer(&self, observer: std::sync::Weak<dyn CreditUsageObserver>) {
        *self.credit_observer.lock() = Some(observer);
    }

    /// 应用 meteringEvent 上报的 credit 消耗：从运行时余额缓存扣减
    ///
    /// 仅对已初始化的缓存生效（未查过余额的凭据先不扣，避免基于 0 反而把 remaining 拉负）。
    /// 优先扣 primary remaining，不足再扣 overage_remaining。返回是否已应用。
    /// 同时回调观察者，让 admin 端 disk cache 同步更新。
    pub fn apply_credit_usage(&self, id: u64, credit: f64) -> bool {
        if !credit.is_finite() || credit <= 0.0 {
            return false;
        }
        let (new_remaining, new_overage, applied) = {
            let mut cache = self.balance_cache.lock();
            let Some(entry) = cache.get_mut(&id) else {
                return false;
            };
            if !entry.initialized {
                return false;
            }
            let from_primary = entry.remaining.min(credit);
            entry.remaining = (entry.remaining - from_primary).max(0.0);
            let leftover = (credit - from_primary).max(0.0);
            if leftover > 0.0 {
                entry.overage_remaining = (entry.overage_remaining - leftover).max(0.0);
            }
            (entry.remaining, entry.overage_remaining, true)
        };
        if applied {
            tracing::debug!(
                credential_id = id,
                credit,
                new_primary = new_remaining,
                new_overage = new_overage,
                "已应用 metering 扣减"
            );
            if let Some(weak) = self.credit_observer.lock().as_ref() {
                if let Some(observer) = weak.upgrade() {
                    observer.on_credit_usage(id, credit, new_remaining, new_overage);
                }
            }
        }
        applied
    }

    /// 取指定凭据的刷新锁（懒分配）
    ///
    /// 不同 id 互不阻塞，多凭据同时过期可并行刷新；
    /// 同 id 重复 acquire 仍然串行，由 caller 双重 check 防重复刷。
    fn refresh_lock_for(&self, id: u64) -> Arc<TokioMutex<()>> {
        let mut map = self.refresh_locks.lock();
        map.entry(id)
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    /// 获取所有凭据的缓存余额信息（用于 Admin API 展示）
    ///
    /// 返回每个凭据的缓存余额、缓存时间（Unix 毫秒）和动态 TTL（秒）
    pub fn get_all_cached_balances(&self) -> Vec<CachedBalanceInfo> {
        // 先获取 entries 的 ID 列表，避免同时持有两个锁
        let entry_ids: Vec<u64> = {
            let entries = self.entries.lock();
            entries.iter().map(|e| e.id).collect()
        };

        let cache = self.balance_cache.lock();
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        entry_ids
            .iter()
            .filter_map(|&id| {
                cache.get(&id).map(|cached| {
                    // 计算动态 TTL
                    let ttl_secs = if !cached.initialized {
                        // 未初始化的缓存，TTL 设为 0（已过期）
                        0
                    } else if cached.remaining < LOW_BALANCE_THRESHOLD {
                        BALANCE_TTL_LOW_BALANCE_SECS
                    } else if cached.recent_usage >= HIGH_FREQ_THRESHOLD {
                        BALANCE_TTL_HIGH_FREQ_SECS
                    } else {
                        BALANCE_TTL_LOW_FREQ_SECS
                    };

                    // 计算缓存时间的 Unix 毫秒时间戳
                    let elapsed_ms = cached.cached_at.elapsed().as_millis() as u64;
                    let cached_at_unix_ms = now_unix_ms.saturating_sub(elapsed_ms);

                    CachedBalanceInfo {
                        id,
                        remaining: cached.remaining,
                        cached_at: cached_at_unix_ms,
                        ttl_secs,
                    }
                })
            })
            .collect()
    }

    /// 尝试使用指定凭据获取有效令牌
    ///
    /// 使用双重检查锁定模式，确保同一时间只有一个刷新操作
    ///
    /// # Arguments
    /// * `id` - 凭据 ID，用于更新正确的条目
    /// * `credentials` - 凭据信息
    async fn try_ensure_token(
        &self,
        id: u64,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<CallContext> {
        // API 密钥凭据直接使用 apiKey 作为 Bearer 令牌，无需刷新
        if credentials.is_api_key_credential() {
            let token = credentials
                .api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API 密钥凭据缺少 apiKey"))?;
            return Ok(CallContext {
                id,
                credentials: credentials.clone(),
                token,
                _credential_permit: None,
                _global_permit: None,
                _proxy_permit: None,
            });
        }

        // 第一次检查（无锁）：快速判断是否需要刷新
        let needs_refresh = is_token_expired(credentials) || is_token_expiring_soon(credentials);

        let creds = if needs_refresh {
            // 获取该凭据的刷新锁，同 id 串行、不同 id 并行
            let lock = self.refresh_lock_for(id);
            let _guard = lock.lock().await;

            // 第二次检查：获取锁后重新读取凭据，因为其他请求可能已经完成刷新
            let mut current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 不存在", id))?
            };
            current_creds.proxy_url = credentials.proxy_url.clone();
            current_creds.proxy_username = credentials.proxy_username.clone();
            current_creds.proxy_password = credentials.proxy_password.clone();

            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                // 确实需要刷新
                let proxy_snap = self.proxy.read().clone();
                let config_snap = self.config.read().clone();
                let effective_proxy = current_creds.effective_proxy(proxy_snap.as_ref());
                let (new_creds, refreshed) =
                    match refresh_token(&current_creds, &config_snap, effective_proxy.as_ref())
                        .await
                    {
                        Ok(new_creds) => (new_creds, true),
                        Err(error) if !is_token_expired(&current_creds) => {
                            tracing::warn!(
                                "凭据 #{} 令牌主动刷新失败，沿用仍在刷新宽限期外的现有令牌: {}",
                                id,
                                error
                            );
                            (current_creds, false)
                        }
                        Err(error) => return Err(error),
                    };

                if is_token_expired(&new_creds) {
                    anyhow::bail!("刷新后的令牌仍然无效或已过期");
                }

                if refreshed {
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = Self::credential_for_persistence(new_creds.clone());
                        }
                    }

                    // 回写凭据到文件（仅多凭据格式），失败只记录警告
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("令牌刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                }

                new_creds
            } else {
                // 其他请求已经完成刷新，直接使用新凭据
                tracing::debug!("令牌已被其他请求刷新，跳过刷新");
                current_creds
            }
        } else {
            credentials.clone()
        };

        let token = creds
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有可用的 accessToken"))?;

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.refresh_failure_count = 0;
            }
        }

        Ok(CallContext {
            id,
            credentials: creds,
            token,
            _credential_permit: None,
            _global_permit: None,
            _proxy_permit: None,
        })
    }

    /// 将凭据列表回写到源文件
    ///
    /// 仅在以下条件满足时回写：
    /// - 源文件是多凭据格式（数组）
    /// - credentials_path 已设置
    ///
    /// # Returns
    /// - `Ok(true)` - 成功写入文件
    /// - `Ok(false)` - 跳过写入（非多凭据格式或无路径配置）
    /// - `Err(_)` - 写入失败
    fn persist_credentials(&self) -> anyhow::Result<bool> {
        let credentials: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            credentials_snapshot_for_persistence(&entries)
        };
        self.write_credentials_snapshot(&credentials)
    }

    /// 把指定 snapshot 原子写入凭据文件。
    ///
    /// 与 [`persist_credentials`] 的区别是：调用方自己负责构造 snapshot
    /// （可在 in-memory 真正落更前用临时快照试写），常用于 admin API 的
    /// 「先 persist、再改 in-memory」语义，避免写盘失败时内存与磁盘不一致。
    fn write_credentials_snapshot(&self, credentials: &[KiroCredentials]) -> anyhow::Result<bool> {
        use anyhow::Context;

        // 仅多凭据格式才回写
        if !self.is_multiple_format {
            return Ok(false);
        }

        let path = match &self.credentials_path {
            Some(p) => p,
            None => return Ok(false),
        };

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(credentials).context("序列化凭据失败")?;

        // 原子写入 + chmod 0o600 (Unix)：rename 在同一文件系统上是原子操作，
        // 临时文件在 rename 前 set_permissions(0o600)，防同主机其他用户读取凭据。
        // 解析 symlink 以确保 rename 写入真实目标（而非替换 symlink 本身）。
        let real_path = crate::common::io::resolve_symlink_target(path);

        let do_atomic_write = || -> anyhow::Result<()> {
            crate::common::io::atomic_write_string_secure(&real_path, &json)
                .with_context(|| format!("原子写入凭据文件失败: {:?}", real_path))
        };

        // 写入文件（在 Tokio runtime 内使用 block_in_place 避免阻塞 worker）
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(do_atomic_write)?;
        } else {
            do_atomic_write()?;
        }

        tracing::debug!("已回写凭据到文件: {:?}", path);
        Ok(true)
    }

    /// 获取缓存目录（凭据文件所在目录）
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// 统计数据文件路径
    fn stats_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("kiro_stats.json"))
    }

    /// 从磁盘加载统计数据并应用到当前条目
    fn load_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };

        let stats: HashMap<String, StatsEntry> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("解析统计缓存失败，将忽略: {}", e);
                return;
            }
        };

        let mut entries = self.entries.lock();
        for entry in entries.iter_mut() {
            if let Some(s) = stats.get(&entry.id.to_string()) {
                entry.success_count = s.success_count;
                entry.last_used_at = s.last_used_at.clone();
            }
        }
        *self.last_stats_save_at.lock() = Some(Instant::now());
        self.stats_dirty.store(false, Ordering::Relaxed);
        tracing::info!("已从缓存加载 {} 条统计数据", stats.len());
    }

    /// 将当前统计数据持久化到磁盘
    fn save_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let stats: HashMap<String, StatsEntry> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    (
                        e.id.to_string(),
                        StatsEntry {
                            success_count: e.success_count,
                            last_used_at: e.last_used_at.clone(),
                        },
                    )
                })
                .collect()
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                if let Err(e) = crate::common::io::atomic_write_string(&path, &json) {
                    tracing::warn!("保存统计缓存失败: {}", e);
                } else {
                    *self.last_stats_save_at.lock() = Some(Instant::now());
                    self.stats_dirty.store(false, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!("序列化统计数据失败: {}", e),
        }
    }

    /// 标记统计数据已更新，并按 debounce 策略决定是否立即落盘
    fn save_stats_debounced(&self) {
        self.stats_dirty.store(true, Ordering::Relaxed);

        let should_flush = {
            let last = *self.last_stats_save_at.lock();
            match last {
                Some(last_saved_at) => last_saved_at.elapsed() >= STATS_SAVE_DEBOUNCE,
                None => true,
            }
        };

        if should_flush {
            self.save_stats();
        }
    }

    /// 报告指定凭据 API 调用成功
    ///
    /// 重置该凭据的失败计数
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_success(&self, id: u64) {
        // 记录使用次数（用于动态 TTL 计算和负载均衡）
        self.record_usage(id);
        self.rate_limited_until.lock().remove(&id);

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.success_count += 1;
                entry.last_used_at = Some(Utc::now().to_rfc3339());
                entry.clear_error();
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次）",
                    id,
                    entry.success_count
                );
            }
        }
        self.save_stats_debounced();
    }

    pub fn report_rate_limited(&self, id: u64) -> bool {
        self.rate_limited_until
            .lock()
            .insert(id, Instant::now() + RATE_LIMIT_COOLDOWN);
        self.remove_affinity_by_credential(id);
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.record_error("rate_limited");
        }
        entries.iter().any(|e| !e.disabled)
    }

    fn sync_usage_snapshot_from_limits(&self, id: u64, usage_limits: &UsageLimitsResponse) {
        let current_usage = usage_limits.current_usage();
        let usage_limit = usage_limits.usage_limit();
        let usage_percent = usage_limits.usage_ratio();
        let current_overages = usage_limits.current_overages_or_primary();
        let checked_at = Utc::now().timestamp();
        let next_reset_date = next_reset_date_from_unix(usage_limits.next_date_reset);

        let changed = {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
                return;
            };
            let credentials = &mut entry.credentials;
            let mut changed = false;

            if let Some(status) = usage_limits.overage_status()
                && credentials.meta.overage_status.as_deref() != Some(status)
            {
                credentials.meta.overage_status = Some(status.to_string());
                changed = true;
            }
            if let Some(capability) = usage_limits.overage_capability()
                && credentials.meta.overage_capability.as_deref() != Some(capability)
            {
                credentials.meta.overage_capability = Some(capability.to_string());
                changed = true;
            }
            if let Some(subscription_type) = usage_limits.subscription_type()
                && credentials.meta.subscription_type.as_deref() != Some(subscription_type.as_str())
            {
                credentials.meta.subscription_type = Some(subscription_type);
                changed = true;
            }
            if let Some(subscription_title) = usage_limits.subscription_title()
                && credentials.meta.subscription_title.as_deref() != Some(subscription_title)
            {
                credentials.meta.subscription_title = Some(subscription_title.to_string());
                changed = true;
            }
            if credentials.meta.overage_cap != Some(usage_limits.overage_cap()) {
                credentials.meta.overage_cap = Some(usage_limits.overage_cap());
                changed = true;
            }
            if credentials.meta.overage_rate != Some(usage_limits.overage_rate()) {
                credentials.meta.overage_rate = Some(usage_limits.overage_rate());
                changed = true;
            }
            if credentials.meta.current_overages != Some(current_overages) {
                credentials.meta.current_overages = Some(current_overages);
                changed = true;
            }
            if credentials.meta.usage_current != Some(current_usage) {
                credentials.meta.usage_current = Some(current_usage);
                changed = true;
            }
            if credentials.meta.usage_limit != Some(usage_limit) {
                credentials.meta.usage_limit = Some(usage_limit);
                changed = true;
            }
            if credentials.meta.usage_percent != Some(usage_percent) {
                credentials.meta.usage_percent = Some(usage_percent);
                changed = true;
            }
            if let Some(next_reset_date) = next_reset_date
                && credentials.meta.next_reset_date.as_deref() != Some(next_reset_date.as_str())
            {
                credentials.meta.next_reset_date = Some(next_reset_date);
                changed = true;
            }
            if let Some(trial_current) = usage_limits.trial_usage_current()
                && credentials.meta.trial_usage_current != Some(trial_current)
            {
                credentials.meta.trial_usage_current = Some(trial_current);
                changed = true;
            }
            if let Some(trial_limit) = usage_limits.trial_usage_limit()
                && credentials.meta.trial_usage_limit != Some(trial_limit)
            {
                credentials.meta.trial_usage_limit = Some(trial_limit);
                changed = true;
            }
            if let Some(trial_limit) = usage_limits.trial_usage_limit() {
                let trial_percent = if trial_limit > 0.0 {
                    usage_limits.trial_usage_current().unwrap_or_default() / trial_limit
                } else {
                    0.0
                };
                if credentials.meta.trial_usage_percent != Some(trial_percent) {
                    credentials.meta.trial_usage_percent = Some(trial_percent);
                    changed = true;
                }
            }
            if let Some(trial_status) = usage_limits.trial_status()
                && credentials.meta.trial_status.as_deref() != Some(trial_status)
            {
                credentials.meta.trial_status = Some(trial_status.to_string());
                changed = true;
            }
            if let Some(trial_expires_at) = usage_limits.trial_expires_at()
                && credentials.meta.trial_expires_at != Some(trial_expires_at)
            {
                credentials.meta.trial_expires_at = Some(trial_expires_at);
                changed = true;
            }
            if credentials.meta.overage_checked_at != Some(checked_at) {
                credentials.meta.overage_checked_at = Some(checked_at);
                changed = true;
            }
            if credentials.meta.last_refresh != Some(checked_at) {
                credentials.meta.last_refresh = Some(checked_at);
                changed = true;
            }

            changed
        };

        if changed && let Err(e) = self.persist_credentials() {
            tracing::warn!("usage limits 快照持久化失败（不影响本次请求）: {}", e);
        }
    }

    /// 报告指定凭据 API 调用失败
    ///
    /// 增加失败计数，达到阈值时禁用凭据并切换到优先级最高的可用凭据
    /// 返回是否还有可用凭据可以重试
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.failure_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.record_error("call_failed");
            let failure_count = entry.failure_count;

            tracing::warn!(
                "凭据 #{} API 调用失败（{}/{}）",
                id,
                failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyFailures);
                entry.record_error("too_many_failures");
                tracing::error!("凭据 #{} 已连续失败 {} 次，已被禁用", id, failure_count);

                if !entries.iter().any(|e| !e.disabled) {
                    tracing::error!("所有凭据均已禁用！");
                }
            }

            entries.iter().any(|e| !e.disabled)
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据额度已用尽
    ///
    /// 用于处理 402 Payment Required 且 reason 为 `MONTHLY_REQUEST_COUNT` 的场景：
    /// - 立即禁用该凭据（不等待连续失败阈值）
    /// - 切换到下一个可用凭据继续重试
    /// - 返回是否还有可用凭据
    pub fn report_quota_exhausted(&self, id: u64) -> bool {
        if self.allow_over_usage() {
            tracing::warn!(
                "凭据 #{} 额度已用尽（MONTHLY_REQUEST_COUNT），但 allowOverUsage=true，保持可路由",
                id
            );
            return self.entries.lock().iter().any(|e| !e.disabled);
        }

        let result = {
            let mut entries = self.entries.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::QuotaExceeded);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.record_error("quota_exceeded");
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;

            tracing::error!("凭据 #{} 额度已用尽（MONTHLY_REQUEST_COUNT），已被禁用", id);

            let has_available = entries.iter().any(|e| !e.disabled);
            if !has_available {
                tracing::error!("所有凭据均已禁用！");
            }
            has_available
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据刷新令牌失败。
    ///
    /// 连续刷新失败达到阈值后禁用凭据，阈值内保持原状，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let (result, disabled_now) = {
            let mut entries = self.entries.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.refresh_failure_count += 1;
            entry.record_error("refresh_failed");
            let refresh_failure_count = entry.refresh_failure_count;

            tracing::warn!(
                "凭据 #{} 令牌刷新失败（{}/{}）",
                id,
                refresh_failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if refresh_failure_count < MAX_FAILURES_PER_CREDENTIAL {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);
            entry.record_error("too_many_refresh_failures");
            // A4：登记首次自愈重探（level 1 → 1min）。
            entry.recovery_backoff_level = 1;
            entry.reprobe_next = Some(Utc::now() + backoff_duration(1));

            tracing::error!(
                "凭据 #{} 令牌已连续刷新失败 {} 次，已被禁用",
                id,
                refresh_failure_count
            );

            let has_available = entries.iter().any(|e| !e.disabled);
            if !has_available {
                tracing::error!("所有凭据均已禁用！");
            }
            (has_available, true)
        };
        if disabled_now {
            self.remove_affinity_by_credential(id);
        }
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据的 refreshToken 永久失效（invalid_grant）。
    ///
    /// 立即禁用凭据，不累计、不重试。
    /// 返回是否还有可用凭据。
    pub fn report_refresh_token_invalid(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InvalidRefreshToken);
            entry.record_error("invalid_refresh_token");
            // A4：登记首次自愈重探（level 1 → 1min）。
            entry.recovery_backoff_level = 1;
            entry.reprobe_next = Some(Utc::now() + backoff_duration(1));

            tracing::error!(
                "凭据 #{} refreshToken 已失效 (invalid_grant)，已立即禁用",
                id
            );

            let has_available = entries.iter().any(|e| !e.disabled);
            if !has_available {
                tracing::error!("所有凭据均已禁用！");
            }
            has_available
        };
        self.remove_affinity_by_credential(id);
        self.save_stats_debounced();
        result
    }

    // ========================================================================
    // Admin API 方法
    // ========================================================================

    /// 获取管理器状态快照（用于 Admin API）
    pub fn snapshot(&self) -> ManagerSnapshot {
        let entries = self.entries.lock();
        let available = entries.iter().filter(|e| !e.disabled).count();
        // 短暂 lock sema map 拷贝 Arc（纯内存查询，不阻塞），释放后再在 map 闭包内读 available_permits
        let sema_snapshot: std::collections::HashMap<u64, std::sync::Arc<tokio::sync::Semaphore>> =
            self.credential_semaphores.lock().clone();
        let global_per_cred = self.config.read().per_credential_concurrency.max(1);

        ManagerSnapshot {
            entries: entries
                .iter()
                .map(|e| CredentialEntrySnapshot {
                    id: e.id,
                    priority: e.credentials.priority,
                    weight: e.credentials.weight,
                    disabled: e.disabled,
                    failure_count: e.failure_count,
                    auth_method: if e.credentials.is_api_key_credential() {
                        Some("api_key".to_string())
                    } else {
                        e.credentials.canonical_auth_method().map(str::to_string)
                    },
                    provider: e.credentials.provider.clone(),
                    user_id: e.credentials.user_id.clone(),
                    source_account_id: e.credentials.meta.source_account_id.clone(),
                    label: e.credentials.meta.label.clone(),
                    status: e.credentials.meta.status.clone(),
                    added_at: e.credentials.meta.added_at.clone(),
                    nickname: e.credentials.meta.nickname.clone(),
                    group_id: e.credentials.meta.group_id.clone(),
                    tag_links: e.credentials.meta.tag_links.clone(),
                    usage_data: e.credentials.meta.usage_data.clone(),
                    has_available_models_cache: e.credentials.meta.available_models_cache.is_some(),
                    source_failure_count: e.credentials.meta.failure_count,
                    source_last_failure_at: e.credentials.meta.last_failure_at.clone(),
                    source_disabled_reason: e.credentials.meta.disabled_reason.clone(),
                    source_success_count: e.credentials.meta.success_count,
                    has_profile_arn: e.credentials.profile_arn_trimmed().is_some(),
                    has_token: has_non_empty_secret(&e.credentials.access_token),
                    has_refresh_token: has_non_empty_secret(&e.credentials.refresh_token),
                    has_client_id: has_non_empty_secret(&e.credentials.client_id),
                    has_client_secret: has_non_empty_secret(&e.credentials.client_secret),
                    has_id_token: has_non_empty_secret(&e.credentials.id_token),
                    has_api_key: has_non_empty_secret(&e.credentials.api_key),
                    has_proxy_credentials: has_non_empty_secret(&e.credentials.proxy_username)
                        || has_non_empty_secret(&e.credentials.proxy_password),
                    expires_at: if e.credentials.is_api_key_credential() {
                        None // API 密钥凭据本地不维护过期时间（服务端策略未知）
                    } else {
                        e.credentials.expires_at.clone()
                    },
                    region: e.credentials.region.clone(),
                    auth_region: e.credentials.auth_region.clone(),
                    api_region: e.credentials.api_region.clone(),
                    machine_id: e.credentials.machine_id.clone(),
                    start_url: e.credentials.start_url.clone(),
                    client_id_hash: e.credentials.client_id_hash.clone(),
                    sso_session_id: e.credentials.sso_session_id.clone(),
                    token_endpoint: e.credentials.token_endpoint.clone(),
                    issuer_url: e.credentials.issuer_url.clone(),
                    scopes: e.credentials.scopes.clone(),
                    subscription_type: e.credentials.meta.subscription_type.clone(),
                    subscription_title: e.credentials.meta.subscription_title.clone(),
                    days_remaining: e.credentials.meta.days_remaining,
                    overage_status: e.credentials.meta.overage_status.clone(),
                    overage_capability: e.credentials.meta.overage_capability.clone(),
                    overage_cap: e.credentials.meta.overage_cap,
                    overage_rate: e.credentials.meta.overage_rate,
                    current_overages: e.credentials.meta.current_overages,
                    overage_checked_at: e.credentials.meta.overage_checked_at,
                    ban_status: e.credentials.meta.ban_status.clone(),
                    ban_reason: e.credentials.meta.ban_reason.clone(),
                    ban_time: e.credentials.meta.ban_time,
                    usage_current: e.credentials.meta.usage_current,
                    usage_limit: e.credentials.meta.usage_limit,
                    usage_percent: e.credentials.meta.usage_percent,
                    next_reset_date: e.credentials.meta.next_reset_date.clone(),
                    last_refresh: e.credentials.meta.last_refresh,
                    trial_usage_current: e.credentials.meta.trial_usage_current,
                    trial_usage_limit: e.credentials.meta.trial_usage_limit,
                    trial_usage_percent: e.credentials.meta.trial_usage_percent,
                    trial_status: e.credentials.meta.trial_status.clone(),
                    trial_expires_at: e.credentials.meta.trial_expires_at,
                    request_count: e.credentials.meta.request_count,
                    error_count: e.credentials.meta.error_count,
                    total_tokens: e.credentials.meta.total_tokens,
                    total_credits: e.credentials.meta.total_credits,
                    last_used: e.credentials.meta.last_used_at,
                    created_at: e.credentials.meta.created_at,
                    tags: e.credentials.meta.tags.clone(),
                    refresh_token_hash: if e.credentials.is_api_key_credential() {
                        None
                    } else {
                        e.credentials.refresh_token.as_deref().map(sha256_hex)
                    },
                    api_key_hash: if e.credentials.is_api_key_credential() {
                        e.credentials.api_key.as_deref().map(sha256_hex)
                    } else {
                        None
                    },
                    masked_api_key: if e.credentials.is_api_key_credential() {
                        e.credentials.api_key.as_deref().map(mask_api_key)
                    } else {
                        None
                    },
                    email: e.credentials.email.clone(),
                    success_count: e.success_count,
                    last_used_at: e.last_used_at.clone(),
                    has_proxy: e.credentials.proxy_url.is_some()
                        || e.credentials.proxy_id.is_some(),
                    proxy_url: e.credentials.proxy_url.clone(),
                    proxy_id: e.credentials.proxy_id,
                    refresh_failure_count: e.refresh_failure_count,
                    disabled_reason: e.disabled_reason.map(|r| {
                        match r {
                            DisabledReason::Manual => "Manual",
                            DisabledReason::TooManyFailures => "TooManyFailures",
                            DisabledReason::TooManyRefreshFailures => "TooManyRefreshFailures",
                            DisabledReason::QuotaExceeded => "QuotaExceeded",
                            DisabledReason::InvalidRefreshToken => "InvalidRefreshToken",
                            DisabledReason::InvalidConfig => "InvalidConfig",
                            DisabledReason::AuthenticationFailed => "AuthenticationFailed",
                            DisabledReason::CredentialSuspended => "AccountSuspended",
                            DisabledReason::InsufficientBalance => "InsufficientBalance",
                            DisabledReason::ModelUnavailable => "ModelUnavailable",
                        }
                        .to_string()
                    }),
                    endpoint: e.credentials.endpoint.clone(),
                    available_permits: sema_snapshot
                        .get(&e.id)
                        .map(|s| s.available_permits())
                        .unwrap_or(0),
                    max_permits: e
                        .credentials
                        .concurrency
                        .map(|v| (v as usize).max(1))
                        .unwrap_or(global_per_cred),
                    concurrency: e.credentials.concurrency,
                    last_error_code: e.last_error.as_ref().map(|le| le.code.to_string()),
                    last_error_at: e.last_error.as_ref().map(|le| le.at.to_rfc3339()),
                })
                .collect(),
            total: entries.len(),
            available,
        }
    }

    /// 高频轮询用的轻量快照：只采集运行时字段，不做 SHA-256 / JSON 深克隆。
    ///
    /// 与 `snapshot()` 的区别是产出结构极小，供秒级轮询的 `get_runtime_stats` 使用。
    pub fn runtime_snapshot(&self) -> Vec<RuntimeEntrySnapshot> {
        let entries = self.entries.lock();
        // Arc 克隆廉价，沿用 snapshot() 的锁顺序（entries → sema map）规避锁序风险。
        let sema_snapshot: std::collections::HashMap<u64, std::sync::Arc<tokio::sync::Semaphore>> =
            self.credential_semaphores.lock().clone();
        let global_per_cred = self.config.read().per_credential_concurrency.max(1);

        entries
            .iter()
            .map(|e| RuntimeEntrySnapshot {
                id: e.id,
                disabled: e.disabled,
                last_used_at: e.last_used_at.clone(),
                available_permits: sema_snapshot
                    .get(&e.id)
                    .map(|s| s.available_permits())
                    .unwrap_or(0),
                max_permits: e
                    .credentials
                    .concurrency
                    .map(|v| (v as usize).max(1))
                    .unwrap_or(global_per_cred),
                last_error_code: e.last_error.as_ref().map(|le| le.code),
                last_error_at: e.last_error.as_ref().map(|le| le.at.to_rfc3339()),
            })
            .collect()
    }

    /// 采集当前未禁用凭据的 ID 列表（纯内存读取）。
    ///
    /// 供余额刷新、批量刷新等只需 ID 的路径使用，避免走 `snapshot()` 的整表深克隆。
    pub fn active_credential_ids(&self) -> Vec<u64> {
        self.entries
            .lock()
            .iter()
            .filter(|e| !e.disabled)
            .map(|e| e.id)
            .collect()
    }

    /// 设置凭据禁用状态（Admin API）
    ///
    /// 语义：先把"含变更的 snapshot"原子写入凭据文件，写盘成功后再修改 in-memory。
    /// 写盘失败 → 返回 Err，in-memory 维持原状，磁盘与内存保持一致。
    pub fn set_disabled(&self, id: u64, disabled: bool) -> anyhow::Result<()> {
        // 1) 锁内构造含变更的 snapshot（不修改真 entries）
        let snapshot: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            if !entries.iter().any(|e| e.id == id) {
                anyhow::bail!("凭据不存在: {}", id);
            }
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    // 默认沿用 in-memory 当前的「手动禁用」标记
                    let mut is_manual_disabled = e.disabled_reason == Some(DisabledReason::Manual);
                    if e.id == id {
                        is_manual_disabled = disabled;
                    }
                    cred.disabled = is_manual_disabled;
                    cred
                })
                .collect()
        };

        // 2) 先持久化（含变更）；失败直接返回，in-memory 不动
        self.write_credentials_snapshot(&snapshot)?;

        // 3) 持久化成功后，再回写 in-memory
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
        entry.disabled = disabled;
        if !disabled {
            // 启用时重置失败计数
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.disabled_reason = None;
            entry.clear_error();
        } else {
            entry.disabled_reason = Some(DisabledReason::Manual);
        }
        drop(entries);
        if disabled {
            self.remove_affinity_by_credential(id);
        }
        Ok(())
    }

    /// 设置凭据优先级（Admin API）
    ///
    /// 语义：先把"含新优先级的 snapshot"原子写入凭据文件，写盘成功后再修改
    /// in-memory 优先级。写盘失败 → 返回 Err，in-memory 维持原状。
    pub fn set_priority(&self, id: u64, priority: u32) -> anyhow::Result<()> {
        // 1) 锁内构造含新优先级的 snapshot（不修改真 entries）
        let snapshot: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            if !entries.iter().any(|e| e.id == id) {
                anyhow::bail!("凭据不存在: {}", id);
            }
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    cred.disabled = e.disabled_reason == Some(DisabledReason::Manual);
                    if e.id == id {
                        cred.priority = priority;
                    }
                    cred
                })
                .collect()
        };

        // 2) 先持久化；失败直接返回，in-memory 不动
        self.write_credentials_snapshot(&snapshot)?;

        // 3) 持久化成功后，再回写 in-memory
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.priority = priority;
        }
        Ok(())
    }

    pub fn update_credential_fields(
        &self,
        id: u64,
        enabled: Option<bool>,
        nickname: Option<Option<String>>,
        machine_id: Option<Option<String>>,
        weight: Option<u32>,
        proxy_url: Option<Option<String>>,
    ) -> anyhow::Result<()> {
        let snapshot: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            if !entries.iter().any(|e| e.id == id) {
                anyhow::bail!("凭据不存在: {}", id);
            }
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    let mut manual_disabled = e.disabled_reason == Some(DisabledReason::Manual);
                    if e.id == id {
                        if let Some(enabled) = enabled {
                            manual_disabled = !enabled;
                        }
                        if let Some(nickname) = &nickname {
                            cred.meta.nickname = nickname.clone();
                        }
                        if let Some(machine_id) = &machine_id {
                            cred.machine_id =
                                machine_id::normalize_optional_machine_id(machine_id.clone());
                        }
                        if let Some(weight) = weight {
                            cred.weight = weight;
                        }
                        if let Some(proxy_url) = &proxy_url {
                            cred.proxy_url = proxy_url.clone();
                        }
                    }
                    cred.disabled = manual_disabled;
                    cred
                })
                .collect()
        };

        self.write_credentials_snapshot(&snapshot)?;

        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
        if let Some(enabled) = enabled {
            entry.disabled = !enabled;
            if enabled {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.disabled_reason = None;
                entry.clear_error();
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
            }
        }
        if let Some(weight) = weight {
            entry.credentials.weight = weight;
        }
        if let Some(nickname) = nickname {
            entry.credentials.meta.nickname = nickname;
        }
        if let Some(machine_id) = machine_id {
            entry.credentials.machine_id = machine_id::normalize_optional_machine_id(machine_id);
        }
        if let Some(proxy_url) = proxy_url {
            entry.credentials.proxy_url = proxy_url;
        }
        let disabled = entry.disabled;
        drop(entries);
        if disabled {
            self.remove_affinity_by_credential(id);
        }
        Ok(())
    }

    pub fn set_proxy_id(&self, id: u64, proxy_id: Option<u64>) -> anyhow::Result<()> {
        let snapshot: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            if !entries.iter().any(|e| e.id == id) {
                anyhow::bail!("凭据不存在: {}", id);
            }
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    cred.disabled = e.disabled_reason == Some(DisabledReason::Manual);
                    if e.id == id {
                        cred.proxy_id = proxy_id;
                    }
                    Self::credential_for_persistence(cred)
                })
                .collect()
        };

        self.write_credentials_snapshot(&snapshot)?;

        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
        entry.credentials.proxy_id = proxy_id;
        if proxy_id.is_some() {
            entry.credentials.proxy_url = None;
            entry.credentials.proxy_username = None;
            entry.credentials.proxy_password = None;
        }
        Ok(())
    }

    pub fn choose_replacement_proxy(
        &self,
        credential_id: u64,
        excluded_proxy_ids: &HashSet<u64>,
    ) -> Option<u64> {
        let (credential_region, current_proxy_id) = {
            let entries = self.entries.lock();
            let entry = entries
                .iter()
                .find(|entry| entry.id == credential_id && !entry.disabled)?;
            (entry.credentials.region.clone(), entry.credentials.proxy_id)
        };

        let proxy_manager = self.proxy_manager()?;
        let proxies = proxy_manager.list();
        let mut load: HashMap<u64, usize> = HashMap::new();
        for (_, _, proxy_id, disabled) in self.credential_region_bindings() {
            if disabled {
                continue;
            }
            if let Some(proxy_id) = proxy_id {
                *load.entry(proxy_id).or_insert(0) += 1;
            }
        }

        let mut candidates: Vec<_> = proxies
            .into_iter()
            .filter(|view| !view.entry.disabled && !view.health.dead)
            .filter_map(|view| {
                let proxy_id = view.entry.id?;
                if excluded_proxy_ids.contains(&proxy_id) || Some(proxy_id) == current_proxy_id {
                    return None;
                }
                let region_mismatch =
                    match (credential_region.as_deref(), view.entry.region.as_deref()) {
                        (Some(credential_region), Some(proxy_region)) => {
                            credential_region != proxy_region
                        }
                        _ => false,
                    };
                let current_load = *load.get(&proxy_id).unwrap_or(&0);
                let available_permits = view.available_permits.unwrap_or(usize::MAX);
                Some((
                    proxy_id,
                    region_mismatch,
                    current_load,
                    std::cmp::Reverse(available_permits),
                ))
            })
            .collect();
        candidates.sort_by_key(|(_, region_mismatch, current_load, available_permits)| {
            (*region_mismatch, *current_load, *available_permits)
        });
        let chosen = candidates.first().map(|(proxy_id, _, _, _)| *proxy_id);
        tracing::info!(
            credential_id,
            current_proxy_id = ?current_proxy_id,
            credential_region = credential_region.as_deref().unwrap_or("<none>"),
            excluded_proxy_count = excluded_proxy_ids.len(),
            candidate_count = candidates.len(),
            chosen_proxy_id = ?chosen,
            "选择替代代理用于模型探测"
        );
        chosen
    }

    pub fn credential_region_bindings(&self) -> Vec<(u64, Option<String>, Option<u64>, bool)> {
        let entries = self.entries.lock();
        entries
            .iter()
            .map(|e| {
                (
                    e.id,
                    e.credentials.region.clone(),
                    e.credentials.proxy_id,
                    e.disabled,
                )
            })
            .collect()
    }

    pub fn credentials_bound_to_proxy(&self, proxy_id: u64) -> Vec<u64> {
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|e| e.credentials.proxy_id == Some(proxy_id))
            .map(|e| e.id)
            .collect()
    }

    fn remove_affinity_by_credential(&self, id: u64) {
        self.session_affinity.remove_by_credential(id);
        self.client_affinity.remove_by_credential(id);
    }

    /// 清空所有调度亲和绑定（关闭 affinity 开关后调用）
    pub fn clear_session_affinity(&self) {
        self.session_affinity.clear();
        self.client_affinity.clear();
    }

    pub fn set_proxy_manager(
        &self,
        proxy_manager: Option<Arc<crate::kiro::proxy_manager::ProxyManager>>,
    ) {
        *self.proxy_manager.write() = proxy_manager;
    }

    fn proxy_manager(&self) -> Option<Arc<crate::kiro::proxy_manager::ProxyManager>> {
        self.proxy_manager.read().clone()
    }

    fn backfill_pool_proxy(&self, credentials: &mut KiroCredentials) {
        if let Some(proxy_id) = credentials.proxy_id
            && let Some(proxy_manager) = self.proxy_manager()
            && let Some(entry) = proxy_manager.get(proxy_id)
        {
            credentials.proxy_url = Some(entry.url);
            credentials.proxy_username = entry.username;
            credentials.proxy_password = entry.password;
        }
    }

    fn credential_for_persistence(mut credentials: KiroCredentials) -> KiroCredentials {
        credentials.normalize_profile_arn();
        if credentials.proxy_id.is_some() {
            credentials.proxy_url = None;
            credentials.proxy_username = None;
            credentials.proxy_password = None;
        }
        credentials
    }

    fn acquire_pool_proxy_for_call(
        &self,
        credential_id: u64,
        credentials: &mut KiroCredentials,
    ) -> PoolProxyAttempt {
        let Some(proxy_id) = credentials.proxy_id else {
            return PoolProxyAttempt::Ready(None);
        };
        let Some(proxy_manager) = self.proxy_manager() else {
            return PoolProxyAttempt::Ready(None);
        };
        if !proxy_manager.is_usable(proxy_id) {
            tracing::debug!(
                "凭据 #{} 绑定代理 #{} 不可用，本轮跳过",
                credential_id,
                proxy_id
            );
            return PoolProxyAttempt::SkipCredential;
        }

        let permit = match proxy_manager.semaphore_for(proxy_id) {
            Some(semaphore) => match semaphore.try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    tracing::debug!(
                        "代理 #{} 并发已满，凭据 #{} 本轮跳过",
                        proxy_id,
                        credential_id
                    );
                    return PoolProxyAttempt::SkipCredential;
                }
            },
            None => None,
        };

        if let Some(entry) = proxy_manager.get(proxy_id) {
            credentials.proxy_url = Some(entry.url);
            credentials.proxy_username = entry.username;
            credentials.proxy_password = entry.password;
        }
        PoolProxyAttempt::Ready(permit)
    }

    /// 设置凭据级最大并发数（Admin API）
    ///
    /// 语义：先把"含新 concurrency 的 snapshot"原子写入凭据文件，写盘成功后再修改
    /// in-memory 与单凭据 Semaphore。写盘失败 → 返回 Err，
    /// in-memory 与 Semaphore 维持原状。
    ///
    /// # 行为
    /// - `Some(0)` 视为非法（避免凭据被永久阻塞）
    /// - `None` 表示清除凭据级覆盖，回退到全局 `per_credential_concurrency`
    /// - Semaphore 替换策略：直接 `replace` 整个 `Arc<Semaphore>`；
    ///   旧 permit 在 Drop 时归还到旧 Arc（无副作用），新请求走新 Arc
    pub fn set_credential_concurrency(
        &self,
        id: u64,
        concurrency: Option<u32>,
    ) -> anyhow::Result<()> {
        // 0 校验：避免凭据被永久阻塞
        if let Some(0) = concurrency {
            anyhow::bail!("concurrency 必须 >= 1（None 表示回退到全局默认）");
        }

        // 1) 锁内构造含新 concurrency 的 snapshot（不修改真 entries）
        let snapshot: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            if !entries.iter().any(|e| e.id == id) {
                anyhow::bail!("凭据不存在: {}", id);
            }
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    cred.disabled = e.disabled_reason == Some(DisabledReason::Manual);
                    if e.id == id {
                        cred.concurrency = concurrency;
                    }
                    cred
                })
                .collect()
        };

        // 2) 先持久化；失败直接返回，in-memory 不动
        self.write_credentials_snapshot(&snapshot)?;

        // 3) 持久化成功后，再回写 in-memory
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.concurrency = concurrency;
        }

        // 4) 重建对应单凭据 Semaphore（直接替换 Arc）
        //    - 配额优先取凭据级 concurrency，None 时回退全局 per_credential_concurrency
        //    - 旧 permit Drop 时归还旧 Arc，无副作用；新请求走新 Arc
        let n = concurrency
            .map(|v| v as usize)
            .unwrap_or_else(|| self.config.read().per_credential_concurrency)
            .max(1);
        {
            let mut map = self.credential_semaphores.lock();
            map.insert(id, Arc::new(Semaphore::new(n)));
        }

        tracing::info!(
            "凭据 #{} 单凭据并发数已设置为: {} (override = {:?})",
            id,
            n,
            concurrency
        );
        Ok(())
    }

    /// 重置凭据失败计数并重新启用（Admin API）
    pub fn reset_and_enable(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            if entry.disabled_reason == Some(DisabledReason::InvalidConfig) {
                anyhow::bail!("凭据 #{} 因配置无效被禁用，请修正配置后重启服务", id);
            }
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.disabled = false;
            entry.disabled_reason = None;
            entry.clear_error();
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置指定凭据的 region / api_region 覆盖（Admin API）
    ///
    /// 传 `None` 表示清除该字段，回退到全局默认 region。
    pub fn set_region(
        &self,
        id: u64,
        region: Option<String>,
        api_region: Option<String>,
    ) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.region = region;
            entry.credentials.api_region = api_region;
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置指定凭据的 endpoint 覆盖（Admin API）
    ///
    /// 传 `None` 表示清除该字段，回退到全局默认 endpoint。
    pub fn set_endpoint(&self, id: u64, endpoint: Option<String>) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.endpoint = endpoint;
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 获取指定凭据的使用额度（Admin API）
    pub async fn get_usage_limits_for(&self, id: u64) -> anyhow::Result<UsageLimitsResponse> {
        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        if !credentials.is_api_key_credential() {
            self.ensure_rest_profile_arn_for(id).await?;
            credentials = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
            };
        }

        self.backfill_pool_proxy(&mut credentials);

        // API 密钥凭据直接使用 apiKey，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API 密钥凭据缺少 apiKey"))?
        } else {
            // 检查是否需要刷新 token
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let lock = self.refresh_lock_for(id);
                let _guard = lock.lock().await;
                let mut current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };
                self.backfill_pool_proxy(&mut current_creds);

                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let proxy_snap = self.proxy.read().clone();
                    let config_snap = self.config.read().clone();
                    let effective_proxy = current_creds.effective_proxy(proxy_snap.as_ref());
                    let new_creds =
                        refresh_token(&current_creds, &config_snap, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = Self::credential_for_persistence(new_creds.clone());
                        }
                    }
                    // 持久化失败只记录警告，不影响本次请求
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("令牌刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);

        let proxy_snap = self.proxy.read().clone();
        let config_snap = self.config.read().clone();
        let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());
        let need_email = credentials
            .email
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        let usage_limits = get_usage_limits(
            &credentials,
            &config_snap,
            &token,
            effective_proxy.as_ref(),
            need_email,
        )
        .await?;

        // 更新订阅等级到凭据（仅在发生变化时持久化）
        if let Some(subscription_title) = usage_limits.subscription_title() {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let old_title = entry.credentials.meta.subscription_title.clone();
                    if old_title.as_deref() != Some(subscription_title) {
                        entry.credentials.meta.subscription_title =
                            Some(subscription_title.to_string());
                        tracing::info!(
                            "凭据 #{} 订阅等级已更新: {:?} -> {}",
                            id,
                            old_title,
                            subscription_title
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if changed {
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("订阅等级更新后持久化失败（不影响本次请求）: {}", e);
                }
            }
        }

        // 回填 email（仅当当前为空且 API 返回有效 email 时）
        let resp_email = usage_limits
            .user_info
            .as_ref()
            .and_then(|u| u.email.as_deref())
            .or(usage_limits.email.as_deref())
            .filter(|s| !s.is_empty());
        if let Some(email) = resp_email {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    if entry.credentials.email.is_none() {
                        entry.credentials.email = Some(email.to_string());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if changed {
                tracing::info!("凭据 #{} email 已回填: {}", id, email);
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("email 回填后持久化失败（不影响本次请求）: {}", e);
                }
            }
        }

        self.sync_usage_snapshot_from_limits(id, &usage_limits);

        // 同步余额到内部 balance_cache（用于负载均衡的动态 TTL）
        // 本地余额缓存使用非负剩余额，避免过量使用时调度评分反向增益。
        let remaining = usage_limits.primary_remaining();
        let overage_remaining = usage_limits.primary_overage_remaining();
        self.update_balance_cache_full(id, remaining, overage_remaining);

        Ok(usage_limits)
    }

    /// 拉取指定凭据可用模型列表
    ///
    /// 包含令牌自动刷新；不维护磁盘缓存，调用方按需缓存。
    /// API 密钥凭据返回错误（无法访问 ListAvailableModels）。
    pub async fn list_available_models_for(
        &self,
        id: u64,
        model_provider: Option<&str>,
    ) -> anyhow::Result<crate::kiro::models::ListAvailableModelsResponse> {
        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        if credentials.is_api_key_credential() {
            anyhow::bail!("API 密钥凭据不支持查询模型列表");
        }

        self.ensure_rest_profile_arn_for(id).await?;
        credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        self.backfill_pool_proxy(&mut credentials);
        let needs_refresh = is_token_expired(&credentials) || is_token_expiring_soon(&credentials);
        let token = if needs_refresh {
            let lock = self.refresh_lock_for(id);
            let _guard = lock.lock().await;
            let mut current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
            };
            self.backfill_pool_proxy(&mut current_creds);
            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                let proxy_snap = self.proxy.read().clone();
                let config_snap = self.config.read().clone();
                let effective_proxy = current_creds.effective_proxy(proxy_snap.as_ref());
                let new_creds =
                    refresh_token(&current_creds, &config_snap, effective_proxy.as_ref()).await?;
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = Self::credential_for_persistence(new_creds.clone());
                    }
                }
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("令牌刷新后持久化失败（不影响本次请求）: {}", e);
                }
                new_creds
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
            } else {
                current_creds
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        } else {
            credentials
                .access_token
                .clone()
                .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
        };

        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);
        let proxy_snap = self.proxy.read().clone();
        let config_snap = self.config.read().clone();
        let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());

        crate::kiro::models::fetch_all_available_models(
            &credentials,
            &config_snap,
            &token,
            effective_proxy.as_ref(),
            model_provider,
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))
    }

    /// 解析凭据的 profileArn
    ///
    /// 1. 如果凭据已有 profile_arn，直接返回
    /// 2. 尝试 `ListAvailableProfiles` API（带重试）
    /// 3. 回退到令牌刷新（refresh token 响应中可能包含 profileArn）
    ///
    /// 成功后更新凭据的 profile_arn 字段并持久化。
    pub async fn resolve_profile_arn_for(&self, id: u64) -> anyhow::Result<String> {
        // 1. 检查是否已有 profile_arn
        {
            let entries = self.entries.lock();
            if let Some(entry) = entries.iter().find(|e| e.id == id) {
                if let Some(arn) = entry.credentials.profile_arn_trimmed() {
                    return Ok(arn.to_string());
                }
            }
        }

        // API 密钥凭据不支持 profile ARN 解析
        {
            let entries = self.entries.lock();
            if let Some(entry) = entries.iter().find(|e| e.id == id) {
                if entry.credentials.is_api_key_credential() {
                    anyhow::bail!("API 密钥凭据不支持 profile ARN 解析");
                }
            }
        }

        // 获取 token 和配置
        let (credentials, token) = self.get_credentials_and_token(id).await?;
        let proxy_snap = self.proxy.read().clone();
        let config_snap = self.config.read().clone();
        let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());

        // 2. 尝试 ListAvailableProfiles（带重试）
        let profile_lookup_suppressed = self.is_profile_arn_resolution_suppressed(id);
        let (profile_lookup_err, profile_unsupported) = if profile_lookup_suppressed {
            (
                "profile ARN resolution skipped: previous Builder ID profile lookup was unsupported"
                    .to_string(),
                false,
            )
        } else {
            match crate::kiro::models::list_available_profiles_with_retry(
                &credentials,
                &config_snap,
                &token,
                effective_proxy.as_ref(),
            )
            .await
            {
                Ok(arn) => {
                    if let Some(arn) = KiroCredentials::clean_profile_arn(Some(arn)) {
                        tracing::info!(
                            "凭据 #{} 通过 ListAvailableProfiles 获取到 profile_arn: {}",
                            id,
                            arn
                        );
                        self.update_profile_arn(id, &arn);
                        return Ok(arn);
                    } else {
                        tracing::debug!("凭据 #{} ListAvailableProfiles 返回非法 profile_arn", id);
                        (
                            "ListAvailableProfiles 返回非法 profile_arn".to_string(),
                            false,
                        )
                    }
                }
                Err(e) => {
                    let profile_unsupported = Self::is_builder_id_profile_unsupported_error(&e);
                    tracing::debug!("凭据 #{} ListAvailableProfiles 失败: {}", id, e);
                    (e, profile_unsupported)
                }
            }
        };

        // 3. 回退：刷新 token 获取 profileArn
        if credentials.refresh_token.is_some() {
            let proxy_snap = self.proxy.read().clone();
            let config_snap = self.config.read().clone();
            let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());
            match refresh_token(&credentials, &config_snap, effective_proxy.as_ref()).await {
                Ok(new_creds) => {
                    if let Some(arn) =
                        KiroCredentials::clean_profile_arn(new_creds.profile_arn.clone())
                    {
                        tracing::info!("凭据 #{} 通过 token 刷新获取到 profile_arn: {}", id, arn);
                        {
                            let mut entries = self.entries.lock();
                            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                                entry.credentials =
                                    Self::credential_for_persistence(new_creds.clone());
                            }
                        }
                        self.profile_arn_suppressed_until.lock().remove(&id);
                        if let Err(e) = self.persist_credentials() {
                            tracing::warn!("profile_arn 更新后持久化失败: {}", e);
                        }
                        return Ok(arn);
                    }
                }
                Err(e) => {
                    tracing::debug!("凭据 #{} token 刷新未返回 profile_arn: {}", id, e);
                }
            }
        }

        if profile_lookup_suppressed {
            anyhow::bail!("{}", profile_lookup_err);
        } else if profile_unsupported {
            self.suppress_profile_arn_resolution(id);
            anyhow::bail!("Builder ID 凭据不支持 profile ARN: {}", profile_lookup_err);
        }

        anyhow::bail!("凭据 #{} 无法解析 profile_arn", id)
    }

    async fn ensure_rest_profile_arn_for(&self, id: u64) -> anyhow::Result<()> {
        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);
        if credentials.is_api_key_credential() || credentials.profile_arn_trimmed().is_some() {
            return Ok(());
        }

        match self.resolve_profile_arn_for(id).await {
            Ok(_) => Ok(()),
            Err(e) if Self::is_profile_arn_resolution_soft_error(&e) => {
                tracing::debug!("凭据 #{} profile_arn 解析软失败，继续 REST 请求: {}", id, e);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn is_builder_id_profile_unsupported_error(message: &str) -> bool {
        message.contains("403")
            && message.contains("AWS Builder ID is not supported for this operation")
    }

    pub(crate) fn is_profile_arn_resolution_soft_error(error: &anyhow::Error) -> bool {
        let message = error.to_string();
        message.contains("Builder ID 凭据不支持 profile ARN")
            || message.contains("profile ARN resolution skipped")
    }

    fn suppress_profile_arn_resolution(&self, id: u64) {
        self.profile_arn_suppressed_until
            .lock()
            .insert(id, Instant::now() + PROFILE_ARN_UNSUPPORTED_SUPPRESSION);
    }

    fn is_profile_arn_resolution_suppressed(&self, id: u64) -> bool {
        let mut suppressed = self.profile_arn_suppressed_until.lock();
        let Some(until) = suppressed.get(&id).copied() else {
            return false;
        };
        if Instant::now() > until {
            suppressed.remove(&id);
            return false;
        }
        true
    }

    /// 辅助方法：获取凭据和 token
    async fn get_credentials_and_token(
        &self,
        id: u64,
    ) -> anyhow::Result<(KiroCredentials, String)> {
        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);

        if credentials.is_api_key_credential() {
            let token = credentials
                .api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API 密钥凭据缺少 apiKey"))?;
            return Ok((credentials, token));
        }

        let needs_refresh = is_token_expired(&credentials) || is_token_expiring_soon(&credentials);
        let token = if needs_refresh {
            let lock = self.refresh_lock_for(id);
            let _guard = lock.lock().await;
            let mut current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
            };
            self.backfill_pool_proxy(&mut current_creds);
            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                let proxy_snap = self.proxy.read().clone();
                let config_snap = self.config.read().clone();
                let effective_proxy = current_creds.effective_proxy(proxy_snap.as_ref());
                let new_creds =
                    refresh_token(&current_creds, &config_snap, effective_proxy.as_ref()).await?;
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = Self::credential_for_persistence(new_creds.clone());
                    }
                }
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("令牌刷新后持久化失败: {}", e);
                }
                new_creds
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
            } else {
                current_creds
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        } else {
            credentials
                .access_token
                .clone()
                .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
        };

        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);

        Ok((credentials, token))
    }

    /// 更新凭据的 profile_arn
    fn update_profile_arn(&self, id: u64, arn: &str) {
        let Some(arn) = KiroCredentials::clean_profile_arn(Some(arn.to_string())) else {
            tracing::warn!("忽略非法 profile_arn: {}", arn);
            return;
        };
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials.profile_arn = Some(arn);
            }
        }
        self.profile_arn_suppressed_until.lock().remove(&id);
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("profile_arn 更新后持久化失败: {}", e);
        }
    }

    /// 切换指定凭据的上游 overage 开关
    ///
    /// 调 Kiro `setUserPreference` 接口，成功后返回。本方法不维护本地缓存，
    /// 调用方需要拿最新状态时另行 `get_usage_limits_for`。
    pub async fn set_overage_status_for(&self, id: u64, enabled: bool) -> anyhow::Result<()> {
        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);

        if !credentials.is_api_key_credential() {
            self.resolve_profile_arn_for(id).await?;
        }

        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);

        let token = if credentials.is_api_key_credential() {
            credentials
                .api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API 密钥凭据缺少 apiKey"))?
        } else {
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);
            if needs_refresh {
                let lock = self.refresh_lock_for(id);
                let _guard = lock.lock().await;
                let mut current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };
                self.backfill_pool_proxy(&mut current_creds);
                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let proxy_snap = self.proxy.read().clone();
                    let config_snap = self.config.read().clone();
                    let effective_proxy = current_creds.effective_proxy(proxy_snap.as_ref());
                    let new_creds =
                        refresh_token(&current_creds, &config_snap, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = Self::credential_for_persistence(new_creds.clone());
                        }
                    }
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("令牌刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);

        let proxy_snap = self.proxy.read().clone();
        let config_snap = self.config.read().clone();
        let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());
        let overage_status = if enabled { "ENABLED" } else { "DISABLED" };
        set_user_preference(
            &credentials,
            &config_snap,
            &token,
            effective_proxy.as_ref(),
            overage_status,
        )
        .await?;

        Ok(())
    }

    /// 检查是否存在具有相同 refreshToken 前缀的凭据
    ///
    /// 用于批量导入时的去重检查，通过比较 refreshToken 前 32 字符判断是否重复
    /// 使用 floor_char_boundary 安全截断，避免在多字节字符中间切割导致 panic
    pub fn has_refresh_token_prefix(&self, refresh_token: &str) -> bool {
        let prefix_len = floor_char_boundary(refresh_token, 32);
        let new_prefix = &refresh_token[..prefix_len];

        let entries = self.entries.lock();
        entries.iter().any(|e| {
            e.credentials
                .refresh_token
                .as_ref()
                .map(|rt| {
                    let existing_prefix_len = floor_char_boundary(rt, 32);
                    &rt[..existing_prefix_len] == new_prefix
                })
                .unwrap_or(false)
        })
    }

    /// 按 ID 列表导出原始凭据（用于完整备份和外部导出视图）
    ///
    /// 返回顺序与 `ids` 相同；不存在的 ID 跳过。
    /// 调用方负责保护明文 refreshToken / clientSecret 不外泄到非授权场景。
    pub fn export_credentials_by_ids(&self, ids: &[u64]) -> Vec<KiroCredentials> {
        let entries = self.entries.lock();
        ids.iter()
            .filter_map(|id| {
                entries
                    .iter()
                    .find(|e| e.id == *id)
                    .map(|e| e.credentials.clone())
            })
            .collect()
    }

    /// 按 ID 列表导出凭据 + 启用状态（用于外部导出视图）
    ///
    /// 返回 `(credentials, enabled)`；顺序与 `ids` 相同；不存在的 ID 跳过。
    pub fn export_credentials_with_state_by_ids(
        &self,
        ids: &[u64],
    ) -> Vec<(KiroCredentials, bool)> {
        let entries = self.entries.lock();
        ids.iter()
            .filter_map(|id| {
                entries
                    .iter()
                    .find(|e| e.id == *id)
                    .map(|e| (e.credentials.clone(), !e.disabled))
            })
            .collect()
    }

    /// 添加新凭据（Admin API）
    ///
    /// # 流程
    /// 1. 验证凭据基本字段（API 密钥: apiKey 不为空; OAuth: refreshToken 不为空）
    /// 2. 基于 apiKey 或 refreshToken 的 SHA-256 哈希检测重复
    /// 3. OAuth: 尝试刷新令牌验证凭据有效性; API 密钥: 跳过
    /// 4. 分配新 ID（当前最大 ID + 1）
    /// 5. 先持久化包含新凭据的快照
    /// 6. 添加到 entries 列表
    ///
    /// # 返回
    /// - `Ok(u64)` - 新凭据 ID
    /// - `Err(_)` - 验证失败或添加失败
    pub async fn add_credential(&self, mut new_cred: KiroCredentials) -> anyhow::Result<u64> {
        machine_id::ensure_credential_machine_id(&mut new_cred);

        // 1. 基本验证
        if new_cred.is_api_key_credential() {
            let api_key = new_cred
                .api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API 密钥凭据缺少 apiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("apiKey 为空");
            }
        } else {
            validate_refresh_token(&new_cred)?;
        }

        // 2. 基于哈希检测重复
        if new_cred.is_api_key_credential() {
            let new_api_key = new_cred
                .api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 apiKey"))?;
            let new_api_key_hash = sha256_hex(new_api_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（apiKey 重复）");
            }
        } else {
            let new_refresh_token = new_cred
                .refresh_token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;
            let new_refresh_token_hash = sha256_hex(new_refresh_token);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_refresh_token_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（refreshToken 重复）");
            }
        }

        // 3. 验证凭据有效性（API 密钥无需网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential() {
            new_cred.clone()
        } else {
            let proxy_snap = self.proxy.read().clone();
            let config_snap = self.config.read().clone();
            let mut probe_cred = new_cred.clone();
            self.backfill_pool_proxy(&mut probe_cred);
            let effective_proxy = probe_cred.effective_proxy(proxy_snap.as_ref());
            refresh_token(&probe_cred, &config_snap, effective_proxy.as_ref()).await?
        };

        // 4. 分配新 ID
        let (new_id, mut persistence_snapshot) = {
            let entries = self.entries.lock();
            let new_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
            (new_id, credentials_snapshot_for_persistence(&entries))
        };

        // 5. 设置 ID 并保留用户输入的元数据
        validated_cred.id = Some(new_id);
        validated_cred.priority = new_cred.priority;
        validated_cred.weight = new_cred.weight;
        validated_cred.auth_method = new_cred
            .auth_method
            .as_deref()
            .map(KiroCredentials::canonical_auth_method_name)
            .map(str::to_string);
        validated_cred.client_id = new_cred.client_id;
        validated_cred.client_secret = new_cred.client_secret;
        validated_cred.token_endpoint = new_cred.token_endpoint;
        validated_cred.issuer_url = new_cred.issuer_url;
        validated_cred.scopes = new_cred.scopes;
        validated_cred.provider = new_cred.provider;
        validated_cred.user_id = new_cred.user_id;
        validated_cred.start_url = new_cred.start_url;
        validated_cred.client_id_hash = new_cred.client_id_hash;
        validated_cred.id_token = new_cred.id_token;
        validated_cred.sso_session_id = new_cred.sso_session_id;
        validated_cred.region = new_cred.region;
        validated_cred.auth_region = new_cred.auth_region;
        validated_cred.api_region = new_cred.api_region;
        validated_cred.machine_id = new_cred.machine_id;
        validated_cred.email = new_cred.email;
        validated_cred.proxy_url = new_cred.proxy_url;
        validated_cred.proxy_username = new_cred.proxy_username;
        validated_cred.proxy_password = new_cred.proxy_password;
        validated_cred.proxy_id = new_cred.proxy_id;
        validated_cred.api_key = new_cred.api_key;
        validated_cred.concurrency = new_cred.concurrency;
        validated_cred.canonicalize_auth_method();
        validated_cred.normalize_profile_arn();
        if validated_cred.proxy_id.is_some() {
            validated_cred.proxy_url = None;
            validated_cred.proxy_username = None;
            validated_cred.proxy_password = None;
        }

        let mut persisted_new_cred = Self::credential_for_persistence(validated_cred.clone());
        persisted_new_cred.disabled = false;
        persistence_snapshot.push(persisted_new_cred);
        self.write_credentials_snapshot(&persistence_snapshot)?;

        // 为新凭据登记并发信号量（配额优先取凭据级 concurrency，未设则回退到全局配置）
        // 必须放在 entries.lock 之前，因为后续 push 会 move validated_cred
        {
            let per_cred_limit = self.config.read().per_credential_concurrency.max(1);
            let n = validated_cred
                .concurrency
                .map(|v| v as usize)
                .unwrap_or(per_cred_limit)
                .max(1);
            let mut sema_map = self.credential_semaphores.lock();
            sema_map.insert(new_id, Arc::new(Semaphore::new(n)));
        }

        {
            let mut entries = self.entries.lock();
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled: false,
                disabled_reason: None,
                success_count: 0,
                last_used_at: None,
                reprobe_next: None,
                recovery_backoff_level: 0,
                last_error: None,
            });
        }

        tracing::info!("成功添加凭据 #{}", new_id);
        Ok(new_id)
    }

    pub fn add_prevalidated_credential(
        &self,
        mut credential: KiroCredentials,
    ) -> anyhow::Result<u64> {
        if credential.is_api_key_credential() {
            let api_key = credential
                .api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API 密钥凭据缺少 apiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("apiKey 为空");
            }
        } else {
            validate_refresh_token(&credential)?;
            if credential
                .access_token
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
            {
                anyhow::bail!("预验证 OAuth 凭据缺少 accessToken");
            }
        }

        let duplicate_exists = {
            let entries = self.entries.lock();
            if credential.is_api_key_credential() {
                let new_api_key = credential
                    .api_key
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("缺少 apiKey"))?;
                let new_api_key_hash = sha256_hex(new_api_key);
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            } else {
                let new_refresh_token = credential
                    .refresh_token
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;
                let new_refresh_token_hash = sha256_hex(new_refresh_token);
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_refresh_token_hash.as_str())
                })
            }
        };
        if duplicate_exists {
            anyhow::bail!("凭据已存在");
        }

        let (new_id, mut persistence_snapshot) = {
            let entries = self.entries.lock();
            let preferred_id = credential
                .id
                .filter(|id| *id > 0 && !entries.iter().any(|entry| entry.id == *id));
            let new_id =
                preferred_id.unwrap_or_else(|| entries.iter().map(|e| e.id).max().unwrap_or(0) + 1);
            (new_id, credentials_snapshot_for_persistence(&entries))
        };

        credential.id = Some(new_id);
        credential.canonicalize_auth_method();
        credential.normalize_profile_arn();
        machine_id::ensure_credential_machine_id(&mut credential);
        let disabled = credential.disabled;

        persistence_snapshot.push(Self::credential_for_persistence(credential.clone()));
        self.write_credentials_snapshot(&persistence_snapshot)?;

        {
            let per_cred_limit = self.config.read().per_credential_concurrency.max(1);
            let n = credential
                .concurrency
                .map(|v| v as usize)
                .unwrap_or(per_cred_limit)
                .max(1);
            let mut sema_map = self.credential_semaphores.lock();
            sema_map.insert(new_id, Arc::new(Semaphore::new(n)));
        }

        {
            let mut entries = self.entries.lock();
            entries.push(CredentialEntry {
                id: new_id,
                credentials: credential,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled,
                disabled_reason: if disabled {
                    Some(DisabledReason::Manual)
                } else {
                    None
                },
                success_count: 0,
                last_used_at: None,
                reprobe_next: None,
                recovery_backoff_level: 0,
                last_error: None,
            });
        }

        tracing::info!("成功导入预验证凭据 #{}", new_id);
        Ok(new_id)
    }

    fn validate_imported_credential_material(credential: &KiroCredentials) -> anyhow::Result<()> {
        if let Some(0) = credential.concurrency {
            anyhow::bail!("concurrency 必须 >= 1");
        }
        if credential.is_api_key_credential() {
            let api_key = credential
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("API 密钥凭据缺少 apiKey"))?;
            if !api_key.starts_with("ksk_") {
                tracing::warn!("导入 API 密钥凭据的 apiKey 未使用 ksk_ 前缀");
            }
            return Ok(());
        }
        validate_refresh_token(credential)
    }

    fn imported_secret_duplicate_exists(
        entries: &[CredentialEntry],
        credential: &KiroCredentials,
        exclude_id: Option<u64>,
    ) -> bool {
        let new_api_key_hash = credential.api_key.as_deref().map(sha256_hex);
        let new_refresh_token_hash = credential.refresh_token.as_deref().map(sha256_hex);
        entries.iter().any(|entry| {
            if exclude_id == Some(entry.id) {
                return false;
            }
            if let Some(new_hash) = new_api_key_hash.as_deref()
                && entry
                    .credentials
                    .api_key
                    .as_deref()
                    .map(sha256_hex)
                    .as_deref()
                    == Some(new_hash)
            {
                return true;
            }
            if let Some(new_hash) = new_refresh_token_hash.as_deref()
                && entry
                    .credentials
                    .refresh_token
                    .as_deref()
                    .map(sha256_hex)
                    .as_deref()
                    == Some(new_hash)
            {
                return true;
            }
            false
        })
    }

    fn prepare_imported_credential(
        &self,
        mut credential: KiroCredentials,
        id: u64,
    ) -> anyhow::Result<KiroCredentials> {
        credential.id = Some(id);
        credential.canonicalize_auth_method();
        credential.normalize_profile_arn();
        Self::validate_imported_credential_material(&credential)?;
        machine_id::ensure_credential_machine_id(&mut credential);
        Ok(credential)
    }

    fn credential_semaphore_capacity(&self, credential: &KiroCredentials) -> usize {
        credential
            .concurrency
            .map(|value| value as usize)
            .unwrap_or_else(|| self.config.read().per_credential_concurrency)
            .max(1)
    }

    fn fill_optional_string(target: &mut Option<String>, source: &Option<String>) -> bool {
        if target
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
            && source
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_some()
        {
            *target = source.clone();
            return true;
        }
        false
    }

    fn fill_optional_value<T: Clone>(target: &mut Option<T>, source: &Option<T>) -> bool {
        if target.is_none() && source.is_some() {
            *target = source.clone();
            return true;
        }
        false
    }

    fn merge_imported_missing_fields(
        target: &mut KiroCredentials,
        source: &KiroCredentials,
    ) -> bool {
        let mut changed = false;
        changed |= Self::fill_optional_string(&mut target.access_token, &source.access_token);
        changed |= Self::fill_optional_string(&mut target.refresh_token, &source.refresh_token);
        changed |= Self::fill_optional_string(&mut target.profile_arn, &source.profile_arn);
        changed |= Self::fill_optional_string(&mut target.expires_at, &source.expires_at);
        changed |= Self::fill_optional_string(&mut target.auth_method, &source.auth_method);
        changed |= Self::fill_optional_string(&mut target.provider, &source.provider);
        changed |= Self::fill_optional_string(&mut target.user_id, &source.user_id);
        changed |= Self::fill_optional_string(&mut target.client_id, &source.client_id);
        changed |= Self::fill_optional_string(&mut target.client_secret, &source.client_secret);
        changed |= Self::fill_optional_string(&mut target.token_endpoint, &source.token_endpoint);
        changed |= Self::fill_optional_string(&mut target.issuer_url, &source.issuer_url);
        changed |= Self::fill_optional_string(&mut target.scopes, &source.scopes);
        changed |= Self::fill_optional_string(&mut target.start_url, &source.start_url);
        changed |= Self::fill_optional_string(&mut target.client_id_hash, &source.client_id_hash);
        changed |= Self::fill_optional_string(&mut target.id_token, &source.id_token);
        changed |= Self::fill_optional_string(&mut target.sso_session_id, &source.sso_session_id);
        changed |= Self::fill_optional_string(&mut target.region, &source.region);
        changed |= Self::fill_optional_string(&mut target.auth_region, &source.auth_region);
        changed |= Self::fill_optional_string(&mut target.api_region, &source.api_region);
        changed |= Self::fill_optional_string(&mut target.machine_id, &source.machine_id);
        changed |= Self::fill_optional_string(&mut target.email, &source.email);
        changed |= Self::fill_optional_string(
            &mut target.meta.source_account_id,
            &source.meta.source_account_id,
        );
        changed |= Self::fill_optional_string(&mut target.meta.label, &source.meta.label);
        changed |= Self::fill_optional_string(&mut target.meta.status, &source.meta.status);
        changed |= Self::fill_optional_string(&mut target.meta.added_at, &source.meta.added_at);
        changed |= Self::fill_optional_string(&mut target.meta.password, &source.meta.password);
        changed |= Self::fill_optional_string(
            &mut target.meta.subscription_title,
            &source.meta.subscription_title,
        );
        changed |= Self::fill_optional_string(
            &mut target.meta.overage_status,
            &source.meta.overage_status,
        );
        changed |= Self::fill_optional_value(&mut target.meta.usage_data, &source.meta.usage_data);
        changed |= Self::fill_optional_string(&mut target.meta.group_id, &source.meta.group_id);
        changed |= Self::fill_optional_value(&mut target.meta.tag_links, &source.meta.tag_links);
        changed |= Self::fill_optional_value(
            &mut target.meta.available_models_cache,
            &source.meta.available_models_cache,
        );
        changed |=
            Self::fill_optional_value(&mut target.meta.failure_count, &source.meta.failure_count);
        changed |= Self::fill_optional_string(
            &mut target.meta.last_failure_at,
            &source.meta.last_failure_at,
        );
        changed |= Self::fill_optional_string(
            &mut target.meta.disabled_reason,
            &source.meta.disabled_reason,
        );
        changed |=
            Self::fill_optional_value(&mut target.meta.success_count, &source.meta.success_count);
        changed |= Self::fill_optional_string(&mut target.meta.csrf_token, &source.meta.csrf_token);
        changed |= Self::fill_optional_string(&mut target.meta.nickname, &source.meta.nickname);
        changed |= Self::fill_optional_string(&mut target.meta.ban_status, &source.meta.ban_status);
        changed |= Self::fill_optional_string(&mut target.meta.ban_reason, &source.meta.ban_reason);
        changed |= Self::fill_optional_value(&mut target.meta.ban_time, &source.meta.ban_time);
        changed |= Self::fill_optional_string(
            &mut target.meta.subscription_type,
            &source.meta.subscription_type,
        );
        changed |=
            Self::fill_optional_value(&mut target.meta.days_remaining, &source.meta.days_remaining);
        changed |=
            Self::fill_optional_value(&mut target.meta.usage_current, &source.meta.usage_current);
        changed |=
            Self::fill_optional_value(&mut target.meta.usage_limit, &source.meta.usage_limit);
        changed |=
            Self::fill_optional_value(&mut target.meta.usage_percent, &source.meta.usage_percent);
        changed |= Self::fill_optional_string(
            &mut target.meta.next_reset_date,
            &source.meta.next_reset_date,
        );
        changed |=
            Self::fill_optional_value(&mut target.meta.last_refresh, &source.meta.last_refresh);
        changed |= Self::fill_optional_value(
            &mut target.meta.trial_usage_current,
            &source.meta.trial_usage_current,
        );
        changed |= Self::fill_optional_value(
            &mut target.meta.trial_usage_limit,
            &source.meta.trial_usage_limit,
        );
        changed |= Self::fill_optional_value(
            &mut target.meta.trial_usage_percent,
            &source.meta.trial_usage_percent,
        );
        changed |=
            Self::fill_optional_string(&mut target.meta.trial_status, &source.meta.trial_status);
        changed |= Self::fill_optional_value(
            &mut target.meta.trial_expires_at,
            &source.meta.trial_expires_at,
        );
        changed |= Self::fill_optional_string(
            &mut target.meta.overage_capability,
            &source.meta.overage_capability,
        );
        changed |=
            Self::fill_optional_value(&mut target.meta.overage_cap, &source.meta.overage_cap);
        changed |=
            Self::fill_optional_value(&mut target.meta.overage_rate, &source.meta.overage_rate);
        changed |= Self::fill_optional_value(
            &mut target.meta.current_overages,
            &source.meta.current_overages,
        );
        changed |= Self::fill_optional_value(
            &mut target.meta.overage_checked_at,
            &source.meta.overage_checked_at,
        );
        changed |=
            Self::fill_optional_value(&mut target.meta.request_count, &source.meta.request_count);
        changed |=
            Self::fill_optional_value(&mut target.meta.error_count, &source.meta.error_count);
        changed |=
            Self::fill_optional_value(&mut target.meta.total_tokens, &source.meta.total_tokens);
        changed |=
            Self::fill_optional_value(&mut target.meta.total_credits, &source.meta.total_credits);
        changed |=
            Self::fill_optional_value(&mut target.meta.last_used_at, &source.meta.last_used_at);
        changed |= Self::fill_optional_value(&mut target.meta.created_at, &source.meta.created_at);
        changed |= Self::fill_optional_value(&mut target.meta.tags, &source.meta.tags);
        changed |= Self::fill_optional_string(&mut target.proxy_url, &source.proxy_url);
        changed |= Self::fill_optional_string(&mut target.proxy_username, &source.proxy_username);
        changed |= Self::fill_optional_string(&mut target.proxy_password, &source.proxy_password);
        changed |= Self::fill_optional_value(&mut target.proxy_id, &source.proxy_id);
        changed |= Self::fill_optional_string(&mut target.api_key, &source.api_key);
        changed |= Self::fill_optional_string(&mut target.endpoint, &source.endpoint);
        if target.priority == 0 && source.priority != 0 {
            target.priority = source.priority;
            changed = true;
        }
        if target.weight == 0 && source.weight != 0 {
            target.weight = source.weight;
            changed = true;
        }
        if target.concurrency.is_none() && source.concurrency.is_some() {
            target.concurrency = source.concurrency;
            changed = true;
        }
        target.canonicalize_auth_method();
        changed
    }

    pub fn add_imported_credential(&self, credential: KiroCredentials) -> anyhow::Result<u64> {
        Self::validate_imported_credential_material(&credential)?;
        let (new_id, credential, mut persistence_snapshot) = {
            let entries = self.entries.lock();
            if Self::imported_secret_duplicate_exists(&entries, &credential, None) {
                anyhow::bail!("凭据已存在");
            }
            let preferred_id = credential
                .id
                .filter(|id| *id > 0 && !entries.iter().any(|entry| entry.id == *id));
            let new_id =
                preferred_id.unwrap_or_else(|| entries.iter().map(|e| e.id).max().unwrap_or(0) + 1);
            let credential = self.prepare_imported_credential(credential, new_id)?;
            let mut snapshot = credentials_snapshot_for_persistence(&entries);
            snapshot.push(credential.clone());
            (new_id, credential, snapshot)
        };

        self.write_credentials_snapshot(&persistence_snapshot)?;
        persistence_snapshot.clear();

        let capacity = self.credential_semaphore_capacity(&credential);
        {
            let mut sema_map = self.credential_semaphores.lock();
            sema_map.insert(new_id, Arc::new(Semaphore::new(capacity)));
        }

        let disabled = credential.disabled;
        {
            let mut entries = self.entries.lock();
            entries.push(CredentialEntry {
                id: new_id,
                credentials: credential,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled,
                disabled_reason: if disabled {
                    Some(DisabledReason::Manual)
                } else {
                    None
                },
                success_count: 0,
                last_used_at: None,
                reprobe_next: None,
                recovery_backoff_level: 0,
                last_error: None,
            });
        }
        if disabled {
            self.remove_affinity_by_credential(new_id);
        }
        tracing::info!("成功导入完整备份凭据 #{}", new_id);
        Ok(new_id)
    }

    pub fn merge_imported_credential_missing(
        &self,
        id: u64,
        incoming: KiroCredentials,
    ) -> anyhow::Result<()> {
        Self::validate_imported_credential_material(&incoming)?;
        let (updated, snapshot): (KiroCredentials, Vec<KiroCredentials>) = {
            let entries = self.entries.lock();
            if !entries.iter().any(|entry| entry.id == id) {
                anyhow::bail!("凭据不存在: {}", id);
            }
            if Self::imported_secret_duplicate_exists(&entries, &incoming, Some(id)) {
                anyhow::bail!("导入凭据与其它已有凭据的密钥重复");
            }
            let mut updated = entries
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| entry.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            Self::merge_imported_missing_fields(&mut updated, &incoming);
            updated.id = Some(id);
            let updated = self.prepare_imported_credential(updated, id)?;
            let snapshot = entries
                .iter()
                .map(|entry| {
                    if entry.id == id {
                        let mut cred = updated.clone();
                        cred.disabled = entry.disabled_reason == Some(DisabledReason::Manual);
                        cred
                    } else {
                        let mut cred = entry.credentials.clone();
                        cred.canonicalize_auth_method();
                        cred.disabled = entry.disabled_reason == Some(DisabledReason::Manual);
                        cred
                    }
                })
                .collect();
            (updated, snapshot)
        };

        self.write_credentials_snapshot(&snapshot)?;

        let capacity = self.credential_semaphore_capacity(&updated);
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials = updated;
        }
        {
            let mut sema_map = self.credential_semaphores.lock();
            sema_map.insert(id, Arc::new(Semaphore::new(capacity)));
        }
        Ok(())
    }

    pub fn replace_imported_credential(
        &self,
        id: u64,
        incoming: KiroCredentials,
    ) -> anyhow::Result<()> {
        Self::validate_imported_credential_material(&incoming)?;
        let (replacement, snapshot): (KiroCredentials, Vec<KiroCredentials>) = {
            let entries = self.entries.lock();
            if !entries.iter().any(|entry| entry.id == id) {
                anyhow::bail!("凭据不存在: {}", id);
            }
            if Self::imported_secret_duplicate_exists(&entries, &incoming, Some(id)) {
                anyhow::bail!("导入凭据与其它已有凭据的密钥重复");
            }
            let replacement = self.prepare_imported_credential(incoming, id)?;
            let snapshot = entries
                .iter()
                .map(|entry| {
                    if entry.id == id {
                        replacement.clone()
                    } else {
                        let mut cred = entry.credentials.clone();
                        cred.canonicalize_auth_method();
                        cred.disabled = entry.disabled_reason == Some(DisabledReason::Manual);
                        cred
                    }
                })
                .collect();
            (replacement, snapshot)
        };

        self.write_credentials_snapshot(&snapshot)?;

        let capacity = self.credential_semaphore_capacity(&replacement);
        let disabled = replacement.disabled;
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials = replacement;
            entry.disabled = disabled;
            entry.disabled_reason = if disabled {
                Some(DisabledReason::Manual)
            } else {
                None
            };
            if !disabled {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
            }
        }
        {
            let mut sema_map = self.credential_semaphores.lock();
            sema_map.insert(id, Arc::new(Semaphore::new(capacity)));
        }
        if disabled {
            self.remove_affinity_by_credential(id);
        }
        Ok(())
    }

    pub fn upsert_prevalidated_social_credential(
        &self,
        mut credential: KiroCredentials,
    ) -> anyhow::Result<u64> {
        validate_refresh_token(&credential)?;
        if credential
            .access_token
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            anyhow::bail!("社交登录凭据缺少 accessToken");
        }
        if !matches!(
            credential.auth_method.as_deref(),
            Some(method) if method.eq_ignore_ascii_case("social")
        ) {
            anyhow::bail!("社交登录凭据 authMethod 必须为 social");
        }
        credential.provider = KiroCredentials::normalize_provider_for_auth_method(
            credential.provider.take(),
            "social",
        );
        if !matches!(
            credential.provider.as_deref(),
            Some("Google") | Some("GitHub")
        ) {
            anyhow::bail!("社交登录凭据 provider 必须为 Google 或 GitHub");
        }

        let refresh_token = credential
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?
            .to_string();
        let user_id = non_empty_trimmed(credential.user_id.as_deref()).map(str::to_string);
        credential.canonicalize_auth_method();
        credential.normalize_profile_arn();

        enum Upsert {
            Updated {
                id: u64,
                credentials: KiroCredentials,
            },
            Inserted {
                id: u64,
                credentials: KiroCredentials,
            },
        }

        let (upsert, persistence_snapshot) = {
            let entries = self.entries.lock();
            if let Some(existing) = entries.iter().find(|entry| {
                social_login_matches_existing(
                    &entry.credentials,
                    &refresh_token,
                    user_id.as_deref(),
                )
            }) {
                let mut updated = existing.credentials.clone();
                apply_social_login_update(&mut updated, &credential);
                updated.id = Some(existing.id);

                let mut snapshot = credentials_snapshot_for_persistence(&entries);
                if let Some(persisted) = snapshot
                    .iter_mut()
                    .find(|cred| cred.id == Some(existing.id))
                {
                    let manual_disabled = existing.disabled_reason == Some(DisabledReason::Manual);
                    *persisted = updated.clone();
                    persisted.disabled = manual_disabled;
                }

                (
                    Upsert::Updated {
                        id: existing.id,
                        credentials: updated,
                    },
                    snapshot,
                )
            } else {
                let new_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                credential.id = Some(new_id);
                credential.disabled = false;

                let mut snapshot = credentials_snapshot_for_persistence(&entries);
                snapshot.push(credential.clone());

                (
                    Upsert::Inserted {
                        id: new_id,
                        credentials: credential,
                    },
                    snapshot,
                )
            }
        };

        self.write_credentials_snapshot(&persistence_snapshot)?;

        match upsert {
            Upsert::Updated { id, credentials } => {
                let mut entries = self.entries.lock();
                let entry = entries
                    .iter_mut()
                    .find(|entry| entry.id == id)
                    .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
                entry.credentials = credentials;
                tracing::info!("社交登录已更新已有凭据 #{}", id);
                Ok(id)
            }
            Upsert::Inserted { id, credentials } => {
                {
                    let per_cred_limit = self.config.read().per_credential_concurrency.max(1);
                    let n = credentials
                        .concurrency
                        .map(|v| v as usize)
                        .unwrap_or(per_cred_limit)
                        .max(1);
                    let mut sema_map = self.credential_semaphores.lock();
                    sema_map.insert(id, Arc::new(Semaphore::new(n)));
                }

                let mut entries = self.entries.lock();
                entries.push(CredentialEntry {
                    id,
                    credentials,
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled: false,
                    disabled_reason: None,
                    success_count: 0,
                    last_used_at: None,
                    reprobe_next: None,
                    recovery_backoff_level: 0,
                    last_error: None,
                });
                tracing::info!("社交登录已添加新凭据 #{}", id);
                Ok(id)
            }
        }
    }

    /// 删除凭据（Admin API）
    ///
    /// # 前置条件
    /// - 凭据必须已禁用（disabled = true）
    ///
    /// # 行为
    /// 1. 验证凭据存在
    /// 2. 验证凭据已禁用
    /// 3. 从 entries 移除
    /// 4. 持久化到文件
    ///
    /// # 返回
    /// - `Ok(())` - 删除成功
    /// - `Err(_)` - 凭据不存在、未禁用或持久化失败
    pub fn delete_credential(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();

            // 查找凭据
            let entry = entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            // 检查是否已禁用
            if !entry.disabled {
                anyhow::bail!("只能删除已禁用的凭据（请先禁用凭据 #{}）", id);
            }

            // 删除凭据
            entries.retain(|e| e.id != id);
        }

        // 移除被删凭据的并发信号量（entries 锁释放后再操作，避免锁顺序冲突）
        {
            let mut sema_map = self.credential_semaphores.lock();
            sema_map.remove(&id);
        }
        // 清掉绑到此凭据的所有调度亲和
        self.remove_affinity_by_credential(id);

        // 持久化更改
        self.persist_credentials()?;

        // 立即回写统计数据，清除已删除凭据的残留条目
        self.save_stats();

        tracing::info!("已删除凭据 #{}", id);
        Ok(())
    }

    /// 强制刷新指定凭据的令牌（Admin API）
    ///
    /// 无条件调用上游 API 重新获取 access token，不检查是否过期。
    /// 适用于排查问题、令牌异常但未过期、主动更新凭据状态等场景。
    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<()> {
        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);

        // 同凭据串行，多凭据并行
        let lock = self.refresh_lock_for(id);
        let _guard = lock.lock().await;

        // 无条件调用 refresh_token
        let proxy_snap = self.proxy.read().clone();
        let config_snap = self.config.read().clone();
        let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());
        let new_creds = refresh_token(&credentials, &config_snap, effective_proxy.as_ref()).await?;

        // 更新 entries 中对应凭据
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials = Self::credential_for_persistence(new_creds.clone());
                entry.refresh_failure_count = 0;
            }
        }

        // 持久化
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("强制刷新令牌后持久化失败: {}", e);
        }

        tracing::info!("凭据 #{} 令牌已强制刷新", id);
        Ok(())
    }

    /// 设置单凭据最大并发数（Admin API/运行时调整）
    ///
    /// # 行为
    /// - n >= 1（0 无意义）；n == 旧值时 noop
    /// - 写入 config 内存值后，对未设置凭据级 override 的 per-credential `Semaphore`：
    ///     - 增配额：`add_permits(delta)`
    ///     - 减配额：`tokio::spawn` 异步 `acquire_many(delta).forget()`，不阻塞调用方
    /// - m2 阶段不持久化；admin API 阶段对接持久化钩子
    ///
    /// # 参数
    /// - `old`：变更前的旧值（由调用方从 with_config_mut 之前的 snapshot 读取，避免
    ///   service 层在闭包内提前写入 cfg 后此处再读 config 读到新值导致 noop）
    /// - `n`：变更后的新值
    pub fn set_per_credential_concurrency(&self, old: usize, n: usize) -> anyhow::Result<()> {
        if n == 0 {
            anyhow::bail!("per_credential_concurrency 必须 >= 1");
        }

        let old = old.max(1);
        if old == n {
            return Ok(());
        }

        // 注：config 持久化由调用方（admin service.update_global_config）在 with_config_mut
        // 闭包内统一完成；此处仅做运行时 Semaphore 配额调整，避免双写竞态。

        let default_limited_ids: HashSet<u64> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .filter(|entry| entry.credentials.concurrency.is_none())
                .map(|entry| entry.id)
                .collect()
        };

        // 收集受全局默认值控制的 Arc<Semaphore> 后释放锁，避免长时间持锁
        let semas: Vec<Arc<Semaphore>> = {
            let map = self.credential_semaphores.lock();
            default_limited_ids
                .iter()
                .filter_map(|id| map.get(id).cloned())
                .collect()
        };

        if n > old {
            let delta = n - old;
            for sema in semas {
                sema.add_permits(delta);
            }
        } else {
            let delta = (old - n) as u32;
            for sema in semas {
                tokio::spawn(async move {
                    if let Ok(permits) = sema.acquire_many(delta).await {
                        permits.forget();
                    }
                });
            }
        }

        tracing::info!("单凭据最大并发数已设置为: {} (旧值 {})", n, old);
        Ok(())
    }

    /// 设置全局最大并发数（Admin API/运行时调整）
    ///
    /// # 行为
    /// - 0 表示不限；n == 旧值时 noop
    /// - 0 ↔ N 切换：直接替换 `Option<Arc<Semaphore>>`；
    ///   注意此时已 hold 的 permit 归还到旧 Arc（旧 Arc Drop 时无副作用），
    ///   可能造成新 Arc 短暂"虚空"配额——非致命，但应避免在高并发下频繁 0↔N 切换
    /// - N1 → N2 (都 > 0)：在原 Arc 上 `add_permits` 或 `acquire_many+forget`
    /// - m2 阶段不持久化
    ///
    /// # 参数
    /// - `old`：变更前的旧值（由调用方在 with_config_mut 之前从 snapshot 读取，避免双写竞态）
    /// - `n`：变更后的新值（0 表示不限制全局并发）
    pub fn set_global_concurrency(&self, old: usize, n: usize) -> anyhow::Result<()> {
        if old == n {
            return Ok(());
        }

        // 注：config 持久化由调用方（admin service.update_global_config）在 with_config_mut
        // 闭包内统一完成；此处仅做运行时 global_semaphore 切换，避免双写竞态。

        let mut slot = self.global_semaphore.lock();
        match (old, n) {
            (0, _) => {
                // 0 → N：新建 Arc 替换（旧持有者归还到 None / 旧 Arc，无副作用）
                *slot = if n > 0 {
                    Some(Arc::new(Semaphore::new(n)))
                } else {
                    None
                };
            }
            (_, 0) => {
                // N → 0：直接置 None；旧 permit holders Drop 时归还到旧 Arc，无副作用
                *slot = None;
            }
            _ => {
                // N1 → N2 (都 > 0)：在原 Arc 上调整配额
                if let Some(sema) = slot.as_ref().cloned() {
                    if n > old {
                        sema.add_permits(n - old);
                    } else {
                        let delta = (old - n) as u32;
                        tokio::spawn(async move {
                            if let Ok(permits) = sema.acquire_many(delta).await {
                                permits.forget();
                            }
                        });
                    }
                } else {
                    // 理论不可达：old > 0 但 slot is None；兜底新建
                    *slot = Some(Arc::new(Semaphore::new(n)));
                }
            }
        }

        tracing::info!("全局最大并发数已设置为: {} (旧值 {})", n, old);
        Ok(())
    }

    // ==================== 后台令牌刷新 API ====================
    /// 获取所有即将过期的凭据 ID（不含已禁用条目）
    ///
    /// # Arguments
    /// * `minutes_before_expiry` - 提前多少分钟视为「即将过期」
    pub fn get_expiring_credential_ids(&self, minutes_before_expiry: i64) -> Vec<u64> {
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|e| {
                !e.disabled
                    && !e.credentials.is_api_key_credential()
                    && is_token_expiring_within(&e.credentials, minutes_before_expiry)
                        .unwrap_or(false)
            })
            .map(|e| e.id)
            .collect()
    }

    /// 获取到点应自愈重探的认证类禁用凭据 ID（A4）。
    ///
    /// 资格条件：已禁用 且 禁用原因属于认证类可自愈集合 且 已到 reprobe_next
    /// 且 非 api_key 凭据。对应 A2 的 OPEN→HALF_OPEN：本 tick 允许单次重探。
    pub fn get_ids_due_for_reprobe(&self) -> Vec<u64> {
        let now = Utc::now();
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|e| {
                e.disabled
                    && e.disabled_reason
                        .map(is_auth_recoverable_reason)
                        .unwrap_or(false)
                    && e.reprobe_next.map(|t| now >= t).unwrap_or(false)
                    && !e.credentials.is_api_key_credential()
            })
            .map(|e| e.id)
            .collect()
    }

    /// 应用一次自愈重探结果（A2 HALF_OPEN 的落地）。
    ///
    /// 仅对当前处于「认证类禁用」的条目生效；非该状态直接忽略，
    /// 从而保证非禁用即将过期凭据的常规刷新行为完全不受影响。
    /// - success=true：HALF_OPEN→CLOSED，重新启用并清空退避与封禁元数据。
    /// - success=false：HALF_OPEN→OPEN，保持禁用并推进退避（level+1，封顶 4）。
    ///
    /// success 的判定由调用方负责：必须是「真实刷到新令牌」，而非
    /// refresh_token_for_credential 的优雅降级（fallback），因为被禁用凭据的
    /// 令牌已失效，降级复用旧令牌不代表认证已恢复。
    fn apply_reprobe_outcome(&self, id: u64, success: bool) {
        let mut re_enabled = false;
        {
            let mut entries = self.entries.lock();
            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return,
            };
            if !(entry.disabled
                && entry
                    .disabled_reason
                    .map(is_auth_recoverable_reason)
                    .unwrap_or(false))
            {
                return;
            }
            if success {
                entry.disabled = false;
                entry.disabled_reason = None;
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.reprobe_next = None;
                entry.recovery_backoff_level = 0;
                entry.clear_error();
                entry.credentials.disabled = false;
                entry.credentials.meta.disabled_reason = None;
                entry.credentials.meta.ban_status = None;
                entry.credentials.meta.ban_reason = None;
                entry.credentials.meta.ban_time = None;
                re_enabled = true;
                tracing::info!("凭据 #{} 自愈重探成功，已重新启用", id);
            } else {
                let new_level = entry.recovery_backoff_level.saturating_add(1).min(4);
                entry.recovery_backoff_level = new_level;
                entry.reprobe_next = Some(Utc::now() + backoff_duration(new_level));
                tracing::warn!(
                    "凭据 #{} 自愈重探失败，退避升至 level {}（下次 {}）",
                    id,
                    new_level,
                    entry
                        .reprobe_next
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_default()
                );
            }
        }
        if re_enabled {
            if let Err(e) = self.persist_credentials() {
                tracing::warn!("凭据 #{} 自愈重新启用后持久化失败: {}", id, e);
            }
        }
    }

    /// 判定某凭据当前是否处于「认证类禁用」（自愈重探对象）。
    fn is_auth_disabled_for_reprobe(&self, id: u64) -> bool {
        let entries = self.entries.lock();
        entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| {
                e.disabled
                    && e.disabled_reason
                        .map(is_auth_recoverable_reason)
                        .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// 启动后台令牌刷新任务
    ///
    /// 重复调用会先停止旧任务，再启动新任务。
    pub fn start_background_refresh(
        self: &Arc<Self>,
        config: BackgroundRefreshConfig,
    ) -> Arc<BackgroundRefresher> {
        // 停止已有任务（如果存在）
        if let Some(old) = self.background_refresher.lock().take() {
            old.stop();
        }

        let refresher = Arc::new(BackgroundRefresher::new(config.clone()));
        let manager_for_refresh = Arc::clone(self);
        let manager_for_ids = Arc::clone(self);
        let refresh_before_mins = config.refresh_before_expiry_mins;

        if let Err(e) = refresher.start(
            move |id| {
                let manager = Arc::clone(&manager_for_refresh);
                Box::pin(async move {
                    // 复用同一刷新入口。若该 id 当前是认证类禁用，则本次即为
                    // A4 自愈重探（A2 HALF_OPEN）：需按「是否真正刷到新令牌」
                    // 判定成败并推进/清空退避；否则保持原有即将过期刷新语义不变。
                    let is_reprobe = manager.is_auth_disabled_for_reprobe(id);
                    match manager.refresh_token_for_credential(id).await {
                        Ok(result) => {
                            if is_reprobe {
                                // fallback（优雅降级）不代表认证恢复，仅真实刷新算成功。
                                let fresh = result.success && !result.used_fallback;
                                manager.apply_reprobe_outcome(id, fresh);
                                fresh
                            } else {
                                true
                            }
                        }
                        Err(e) => {
                            tracing::warn!("后台刷新凭据 #{} 令牌失败: {}", id, e);
                            if is_reprobe {
                                manager.apply_reprobe_outcome(id, false);
                            }
                            false
                        }
                    }
                })
            },
            move |mins| {
                // A4：常规即将过期 ID 与到点自愈重探 ID 取并集（去重）。
                let mut ids =
                    manager_for_ids.get_expiring_credential_ids(mins.max(refresh_before_mins));
                for id in manager_for_ids.get_ids_due_for_reprobe() {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                ids
            },
        ) {
            tracing::error!("启动后台刷新任务失败: {}", e);
        }

        *self.background_refresher.lock() = Some(Arc::clone(&refresher));
        refresher
    }

    /// 刷新指定凭据的令牌（带优雅降级）
    ///
    /// 如果刷新失败但现有令牌仍未过期，返回 fallback 结果继续使用现有令牌。
    pub async fn refresh_token_for_credential(&self, id: u64) -> anyhow::Result<RefreshResult> {
        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };
        self.backfill_pool_proxy(&mut credentials);

        // API 密钥凭据无需刷新
        if credentials.is_api_key_credential() {
            let expires_at = credentials.expires_at.unwrap_or_default();
            return Ok(RefreshResult::success(id, expires_at));
        }

        let lock = self.refresh_lock_for(id);
        let _guard = lock.lock().await;
        let proxy_snap = self.proxy.read().clone();
        let config_snap = self.config.read().clone();
        let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());

        match refresh_token(&credentials, &config_snap, effective_proxy.as_ref()).await {
            Ok(new_creds) => {
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = Self::credential_for_persistence(new_creds.clone());
                        entry.refresh_failure_count = 0;
                    }
                }
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("令牌刷新后持久化失败: {}", e);
                }
                let expires_at = new_creds.expires_at.unwrap_or_default();
                Ok(RefreshResult::success(id, expires_at))
            }
            Err(e) => {
                if !is_token_expired(&credentials) {
                    let expires_at = credentials.expires_at.unwrap_or_default();
                    tracing::warn!("凭据 #{} 令牌刷新失败，使用现有令牌（优雅降级）: {}", id, e);
                    Ok(RefreshResult::fallback(id, expires_at))
                } else {
                    Err(e)
                }
            }
        }
    }
}

impl Drop for MultiTokenManager {
    fn drop(&mut self) {
        if self.stats_dirty.load(Ordering::Relaxed) {
            self.save_stats();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn social_login_credential(
        provider: &str,
        refresh_token: &str,
        user_id: Option<&str>,
        machine_id: &str,
    ) -> KiroCredentials {
        KiroCredentials {
            access_token: Some(format!("access-{provider}-{machine_id}")),
            refresh_token: Some(refresh_token.to_string()),
            profile_arn: Some(format!("arn:aws:codewhisperer:profile/{provider}")),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            auth_method: Some("social".to_string()),
            provider: Some(provider.to_string()),
            user_id: user_id.map(str::to_string),
            machine_id: Some(machine_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn usage_endpoint_resolver_accepts_all_registered_upstream_endpoints() {
        let mut config = Config::default();
        for endpoint_name in ["ide", "codewhisperer", "amazonq", "cli"] {
            config.default_endpoint = endpoint_name.to_string();
            let credentials = KiroCredentials::default();
            let endpoint = endpoint_for_credentials(&credentials, &config).unwrap();

            assert_eq!(endpoint.name(), endpoint_name);
        }
    }

    #[test]
    fn test_builder_id_profile_unsupported_error_is_soft() {
        let err = r#"ListAvailableProfiles 403 Forbidden: {"message":"AWS Builder ID is not supported for this operation.","reason":null}"#;
        assert!(MultiTokenManager::is_builder_id_profile_unsupported_error(
            err
        ));
    }

    #[test]
    fn test_profile_arn_resolution_soft_error_accepts_unsupported_and_suppressed() {
        let err = anyhow::anyhow!(
            "Builder ID 凭据不支持 profile ARN: ListAvailableProfiles 403 Forbidden"
        );
        assert!(MultiTokenManager::is_profile_arn_resolution_soft_error(
            &err
        ));

        let skipped = anyhow::anyhow!(
            "profile ARN resolution skipped: previous Builder ID profile lookup was unsupported"
        );
        assert!(MultiTokenManager::is_profile_arn_resolution_soft_error(
            &skipped
        ));
    }

    #[test]
    fn test_profile_arn_resolution_suppression_expires() {
        let mut credentials = KiroCredentials::default();
        credentials.id = Some(7);
        credentials.access_token = Some("access".to_string());

        let manager =
            MultiTokenManager::new(Config::default(), vec![credentials], None, None, true)
                .expect("manager should build");

        assert!(!manager.is_profile_arn_resolution_suppressed(7));
        manager.suppress_profile_arn_resolution(7);
        assert!(manager.is_profile_arn_resolution_suppressed(7));

        manager
            .profile_arn_suppressed_until
            .lock()
            .insert(7, Instant::now() - StdDuration::from_secs(1));
        assert!(!manager.is_profile_arn_resolution_suppressed(7));
        assert!(!manager.profile_arn_suppressed_until.lock().contains_key(&7));
    }

    #[test]
    fn allow_overage_import_hint_is_persisted_as_overage_status() {
        let dir = std::env::temp_dir().join(format!(
            "xkiro-allow-overage-migration-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(
            &path,
            r#"[
  {"id": 1, "refreshToken": "allow", "allowOverage": true},
  {"id": 2, "refreshToken": "preset", "allowOverage": true, "overageStatus": "DISABLED"}
]"#,
        )
        .unwrap();

        let config = crate::kiro::model::credentials::CredentialsConfig::load(&path).unwrap();
        let credentials = config.clone().into_sorted_credentials();
        let _manager = MultiTokenManager::new(
            Config::default(),
            credentials,
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();

        let reloaded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let credential_records = reloaded.as_array().unwrap();
        assert_eq!(credential_records[0]["overageStatus"], "ENABLED");
        assert_eq!(credential_records[1]["overageStatus"], "DISABLED");
        assert!(credential_records[0].get("allowOverage").is_none());
        assert!(credential_records[1].get("allowOverage").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_update_profile_arn_clears_resolution_suppression() {
        let mut credentials = KiroCredentials::default();
        credentials.id = Some(7);
        credentials.access_token = Some("access".to_string());

        let manager =
            MultiTokenManager::new(Config::default(), vec![credentials], None, None, true)
                .expect("manager should build");
        manager.suppress_profile_arn_resolution(7);

        manager.update_profile_arn(7, "arn:aws:codewhisperer:profile/test");

        assert!(!manager.is_profile_arn_resolution_suppressed(7));
        let entries = manager.entries.lock();
        assert_eq!(
            entries[0].credentials.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:profile/test")
        );
    }

    #[test]
    fn test_is_token_expired_with_expired_token() {
        let mut credentials = KiroCredentials::default();
        credentials.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_with_valid_token() {
        let mut credentials = KiroCredentials::default();
        let future = Utc::now() + Duration::hours(1);
        credentials.expires_at = Some(future.to_rfc3339());
        assert!(!is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_within_refresh_skew() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(1);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_not_expired_with_five_minutes_left() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(5);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(!is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_no_expires_at() {
        let credentials = KiroCredentials::default();
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_within_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(8);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_beyond_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(15);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(!is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_validate_refresh_token_missing() {
        let credentials = KiroCredentials::default();
        let result = validate_refresh_token(&credentials);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_refresh_token_valid() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("a".repeat(150));
        let result = validate_refresh_token(&credentials);
        assert!(result.is_ok());
    }

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[tokio::test]
    async fn test_refresh_token_rejects_api_key_credential() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.api_key = Some("ksk_test_key_123".to_string());
        credentials.auth_method = Some("api_key".to_string());

        let result = refresh_token(&credentials, &config, None).await;

        assert!(result.is_err(), "API 密钥凭据应被 refresh_token 拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("API 密钥凭据不支持刷新"),
            "期望错误消息包含 'API 密钥凭据不支持刷新'，实际: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_add_credential_preserves_source_metadata_without_refresh() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, Vec::new(), None, None, true).unwrap();
        let mut credentials = KiroCredentials::default();
        credentials.api_key = Some("ksk_test_key_123".to_string());
        credentials.auth_method = Some("api_key".to_string());
        credentials.provider = Some("AzureAD".to_string());
        credentials.user_id = Some("user-1".to_string());
        credentials.token_endpoint =
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token".to_string());
        credentials.issuer_url = Some("https://login.microsoftonline.com/tenant/v2.0".to_string());
        credentials.scopes = Some("openid profile offline_access".to_string());
        credentials.start_url = Some("https://d-123.awsapps.com/start".to_string());
        credentials.client_id_hash = Some("hash-1".to_string());
        credentials.sso_session_id = Some("session-1".to_string());
        credentials.region = Some("eu-west-1".to_string());
        credentials.machine_id = Some("machine-1".to_string());

        let id = manager.add_credential(credentials).await.unwrap();
        let exported = manager.export_credentials_by_ids(&[id]);
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].provider.as_deref(), Some("AzureAD"));
        assert_eq!(exported[0].user_id.as_deref(), Some("user-1"));
        assert_eq!(
            exported[0].token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token")
        );
        assert_eq!(
            exported[0].issuer_url.as_deref(),
            Some("https://login.microsoftonline.com/tenant/v2.0")
        );
        assert_eq!(
            exported[0].scopes.as_deref(),
            Some("openid profile offline_access")
        );
        assert_eq!(
            exported[0].start_url.as_deref(),
            Some("https://d-123.awsapps.com/start")
        );
        assert_eq!(exported[0].client_id_hash.as_deref(), Some("hash-1"));
        assert_eq!(exported[0].sso_session_id.as_deref(), Some("session-1"));
        assert_eq!(exported[0].region.as_deref(), Some("eu-west-1"));
        assert_eq!(exported[0].machine_id.as_deref(), Some("machine-1"));
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_refresh_token() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.refresh_token = Some("a".repeat(150));

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("凭据已存在"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_success() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.api_key = Some("ksk_test_key_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        let id = result.unwrap();
        assert!(id > 0);
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_add_prevalidated_credential_preserves_disabled_import_state() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("a".repeat(150));
        cred.access_token = Some("access-token".to_string());
        cred.disabled = true;
        cred.provider = Some("BuilderId".to_string());
        cred.user_id = Some("builder-user".to_string());
        cred.start_url = Some("https://view.awsapps.com/start".to_string());
        cred.client_id_hash = Some("hash-1".to_string());
        cred.id_token = Some("id-token-1".to_string());
        cred.sso_session_id = Some("session-1".to_string());

        let id = manager.add_prevalidated_credential(cred).unwrap();
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 0);

        let exported = manager.export_credentials_with_state_by_ids(&[id]);
        assert_eq!(exported.len(), 1);
        assert!(exported[0].0.disabled);
        assert_eq!(exported[0].0.provider.as_deref(), Some("BuilderId"));
        assert_eq!(exported[0].0.user_id.as_deref(), Some("builder-user"));
        assert_eq!(
            exported[0].0.start_url.as_deref(),
            Some("https://view.awsapps.com/start")
        );
        assert_eq!(exported[0].0.client_id_hash.as_deref(), Some("hash-1"));
        assert_eq!(exported[0].0.id_token.as_deref(), Some("id-token-1"));
        assert_eq!(exported[0].0.sso_session_id.as_deref(), Some("session-1"));
        assert!(!exported[0].1);
    }

    #[test]
    fn test_social_login_upsert_updates_existing_by_user_id() {
        let mut existing =
            social_login_credential("Google", &"a".repeat(150), Some("user-1"), "machine-old");
        existing.email = Some("old@example.com".to_string());
        let manager =
            MultiTokenManager::new(Config::default(), vec![existing], None, None, false).unwrap();

        let mut incoming =
            social_login_credential("Google", &"b".repeat(150), Some("user-1"), "machine-new");
        incoming.email = Some("new@example.com".to_string());
        incoming.meta.subscription_title = Some("KIRO PRO+".to_string());

        let id = manager
            .upsert_prevalidated_social_credential(incoming)
            .unwrap();

        assert_eq!(id, 1);
        assert_eq!(manager.total_count(), 1);
        let exported = manager.export_credentials_with_state_by_ids(&[id]);
        assert_eq!(exported.len(), 1);
        let updated = &exported[0].0;
        let expected_refresh_token = "b".repeat(150);
        assert_eq!(
            updated.refresh_token.as_deref(),
            Some(expected_refresh_token.as_str())
        );
        assert_eq!(updated.user_id.as_deref(), Some("user-1"));
        assert_eq!(updated.machine_id.as_deref(), Some("machine-old"));
        assert_eq!(updated.email.as_deref(), Some("old@example.com"));
        assert_eq!(
            updated.meta.subscription_title.as_deref(),
            Some("KIRO PRO+")
        );
    }

    #[test]
    fn test_social_login_upsert_updates_existing_by_refresh_token() {
        let refresh_token = "c".repeat(150);
        let existing = social_login_credential("Github", &refresh_token, None, "machine-old");
        let manager =
            MultiTokenManager::new(Config::default(), vec![existing], None, None, false).unwrap();

        let incoming = social_login_credential("GitHub", &refresh_token, None, "machine-new");

        let id = manager
            .upsert_prevalidated_social_credential(incoming)
            .unwrap();

        assert_eq!(id, 1);
        assert_eq!(manager.total_count(), 1);
        let exported = manager.export_credentials_with_state_by_ids(&[id]);
        assert_eq!(exported[0].0.provider.as_deref(), Some("GitHub"));
        assert_eq!(exported[0].0.machine_id.as_deref(), Some("machine-old"));
    }

    #[test]
    fn test_social_login_upsert_drops_uuid_profile_id() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();
        let mut incoming =
            social_login_credential("Google", &"g".repeat(150), Some("user-uuid"), "machine-new");
        incoming.profile_arn = Some("e3438419-4424-4e57-8990-ef76bd749a44".to_string());

        let id = manager
            .upsert_prevalidated_social_credential(incoming)
            .unwrap();

        let exported = manager.export_credentials_with_state_by_ids(&[id]);
        assert_eq!(exported[0].0.profile_arn, None);
        assert_eq!(exported[0].0.management_profile_arn(), None);
    }

    #[test]
    fn test_social_login_upsert_does_not_update_idc_with_same_user_id() {
        let mut idc = KiroCredentials {
            auth_method: Some("idc".to_string()),
            provider: Some("BuilderId".to_string()),
            refresh_token: Some("d".repeat(150)),
            access_token: Some("access-idc".to_string()),
            user_id: Some("shared-user".to_string()),
            client_id: Some("client-1".to_string()),
            client_secret: Some("secret-1".to_string()),
            ..Default::default()
        };
        idc.id = Some(1);
        let manager =
            MultiTokenManager::new(Config::default(), vec![idc], None, None, false).unwrap();

        let incoming = social_login_credential(
            "Google",
            &"e".repeat(150),
            Some("shared-user"),
            "machine-new",
        );
        let id = manager
            .upsert_prevalidated_social_credential(incoming)
            .unwrap();

        assert_eq!(id, 2);
        assert_eq!(manager.total_count(), 2);
        let exported = manager.export_credentials_with_state_by_ids(&[1, 2]);
        assert_eq!(exported[0].0.provider.as_deref(), Some("BuilderId"));
        assert_eq!(exported[1].0.provider.as_deref(), Some("Google"));
    }

    fn start_token_test_server(status: &str, body: &'static str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind token test server");
        let addr = listener.local_addr().expect("read token test server addr");
        let status = status.to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0_u8; 2048];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}/token")
    }

    fn external_idp_credential(token_endpoint: String) -> KiroCredentials {
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("external_idp".to_string());
        cred.refresh_token = Some("r".repeat(150));
        cred.client_id = Some("client-id".to_string());
        cred.token_endpoint = Some(token_endpoint);
        cred
    }

    #[tokio::test]
    async fn test_add_credential_rejects_when_refresh_fails() {
        let endpoint = start_token_test_server(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"bad refresh"}"#,
        );
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let result = manager
            .add_credential(external_idp_credential(endpoint))
            .await;

        assert!(result.is_err(), "刷新失败时 add_credential 必须返回 Err");
        assert_eq!(manager.total_count(), 0, "刷新失败不应新增半成品凭据");
    }

    #[tokio::test]
    async fn test_add_credential_uses_upstream_expires_in() {
        const EXPIRES_IN: i64 = 3600;
        let endpoint = start_token_test_server(
            "200 OK",
            r#"{"access_token":"at-new","refresh_token":"rt-rotated","expires_in":3600}"#,
        );
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let before = Utc::now();
        let id = manager
            .add_credential(external_idp_credential(endpoint))
            .await
            .expect("successful refresh should add credential");
        let after = Utc::now();

        let stored = manager
            .export_credentials_by_ids(&[id])
            .pop()
            .expect("stored credential should be exportable");
        assert_eq!(stored.access_token.as_deref(), Some("at-new"));
        assert_eq!(stored.refresh_token.as_deref(), Some("rt-rotated"));

        let expires_at = stored
            .expires_at
            .as_deref()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|v| v.with_timezone(&Utc))
            .expect("expires_at should be set from upstream expires_in");
        assert!(expires_at >= before + Duration::seconds(EXPIRES_IN - 5));
        assert!(expires_at <= after + Duration::seconds(EXPIRES_IN + 5));
    }

    #[tokio::test]
    async fn test_acquire_uses_five_minute_token_when_proactive_refresh_fails() {
        let endpoint = start_token_test_server(
            "400 Bad Request",
            r#"{"error":"temporarily_unavailable","error_description":"try later"}"#,
        );
        let mut cred = external_idp_credential(endpoint);
        cred.access_token = Some("still-valid".to_string());
        cred.expires_at = Some((Utc::now() + Duration::minutes(5)).to_rfc3339());

        let manager = MultiTokenManager::new(Config::default(), vec![cred], None, None, false)
            .expect("manager should build");

        let ctx = manager
            .acquire_context(None)
            .await
            .expect("five-minute token should remain usable when proactive refresh fails");

        assert_eq!(ctx.token, "still-valid");
    }

    #[tokio::test]
    async fn test_acquire_rejects_token_inside_refresh_skew_when_refresh_fails() {
        let endpoint = start_token_test_server(
            "400 Bad Request",
            r#"{"error":"temporarily_unavailable","error_description":"try later"}"#,
        );
        let mut cred = external_idp_credential(endpoint);
        cred.access_token = Some("too-close".to_string());
        cred.expires_at = Some((Utc::now() + Duration::minutes(1)).to_rfc3339());

        let manager = MultiTokenManager::new(Config::default(), vec![cred], None, None, false)
            .expect("manager should build");

        let err = match manager.acquire_context(None).await {
            Ok(_) => panic!("token inside skew should not be used after refresh failure"),
            Err(error) => error.to_string(),
        };

        assert!(!err.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_credential_persist_fails_keeps_in_memory() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(
            config,
            vec![existing],
            None,
            Some(std::path::PathBuf::from(
                "/proc/xkiro-nonexistent/credentials.json",
            )),
            true,
        )
        .unwrap();

        let snapshot_before = manager.snapshot();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.api_key = Some("ksk_new_key".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_err(), "持久化失败时 add_credential 必须返回 Err");

        let snapshot_after = manager.snapshot();
        assert_eq!(
            snapshot_after.entries.len(),
            snapshot_before.entries.len(),
            "持久化失败后 in-memory 不应新增凭据"
        );
        assert_eq!(manager.total_count(), snapshot_before.entries.len());
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_api_key() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.api_key = Some("ksk_existing_key".to_string());
        existing.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.api_key = Some("ksk_existing_key".to_string());
        duplicate.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("apiKey 重复"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_empty_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.api_key = Some(String::new());
        cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("apiKey 为空"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_missing_key_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        // api_key is None

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("缺少 apiKey"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_and_oauth_coexist() {
        let config = Config::default();

        let mut oauth_cred = KiroCredentials::default();
        oauth_cred.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![oauth_cred], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.api_key = Some("ksk_new_key".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    // MultiTokenManager 测试

    #[test]
    fn test_multi_token_manager_new() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.priority = 0;
        let mut cred2 = KiroCredentials::default();
        cred2.priority = 1;

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    #[test]
    fn test_multi_token_manager_new_drops_microsoft_uuid_profile_id() {
        let mut cred = KiroCredentials {
            id: Some(1),
            profile_arn: Some("e3438419-4424-4e57-8990-ef76bd749a44".to_string()),
            auth_method: Some("external_idp".to_string()),
            provider: Some("Microsoft".to_string()),
            ..Default::default()
        };
        cred.refresh_token = Some("r".repeat(150));

        let manager = MultiTokenManager::new(Config::default(), vec![cred], None, None, false)
            .expect("manager should build");

        let exported = manager.export_credentials_by_ids(&[1]);
        assert_eq!(exported[0].profile_arn, None);
    }

    #[test]
    fn test_multi_token_manager_empty_credentials() {
        let config = Config::default();
        let result = MultiTokenManager::new(config, vec![], None, None, false);
        // 支持 0 个凭据启动（可通过管理面板添加）
        assert!(result.is_ok());
        let manager = result.unwrap();
        assert_eq!(manager.total_count(), 0);
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_add_prevalidated_credential_drops_microsoft_uuid_profile_id() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();
        let credential = KiroCredentials {
            access_token: Some("access".to_string()),
            refresh_token: Some("r".repeat(150)),
            profile_arn: Some("e3438419-4424-4e57-8990-ef76bd749a44".to_string()),
            auth_method: Some("external_idp".to_string()),
            provider: Some("Microsoft".to_string()),
            ..Default::default()
        };

        let id = manager.add_prevalidated_credential(credential).unwrap();

        let exported = manager.export_credentials_by_ids(&[id]);
        assert_eq!(exported[0].profile_arn, None);
    }

    #[test]
    fn test_add_imported_credential_drops_microsoft_uuid_profile_id() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();
        let credential = KiroCredentials {
            access_token: Some("access".to_string()),
            refresh_token: Some("r".repeat(150)),
            profile_arn: Some("e3438419-4424-4e57-8990-ef76bd749a44".to_string()),
            auth_method: Some("external_idp".to_string()),
            provider: Some("Microsoft".to_string()),
            client_id: Some("client".to_string()),
            token_endpoint: Some(
                "https://login.microsoftonline.com/t/oauth2/v2.0/token".to_string(),
            ),
            ..Default::default()
        };

        let id = manager.add_imported_credential(credential).unwrap();

        let exported = manager.export_credentials_by_ids(&[id]);
        assert_eq!(exported[0].profile_arn, None);
    }

    #[test]
    fn test_merge_imported_credential_drops_microsoft_uuid_profile_id() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();
        let existing = KiroCredentials {
            access_token: Some("access".to_string()),
            refresh_token: Some("r".repeat(150)),
            auth_method: Some("external_idp".to_string()),
            provider: Some("Microsoft".to_string()),
            client_id: Some("client".to_string()),
            token_endpoint: Some(
                "https://login.microsoftonline.com/t/oauth2/v2.0/token".to_string(),
            ),
            ..Default::default()
        };
        let id = manager.add_imported_credential(existing).unwrap();
        let incoming = KiroCredentials {
            profile_arn: Some("e3438419-4424-4e57-8990-ef76bd749a44".to_string()),
            auth_method: Some("external_idp".to_string()),
            refresh_token: Some("s".repeat(150)),
            ..Default::default()
        };

        manager
            .merge_imported_credential_missing(id, incoming)
            .unwrap();

        let exported = manager.export_credentials_by_ids(&[id]);
        assert_eq!(exported[0].profile_arn, None);
    }

    #[test]
    fn test_multi_token_manager_duplicate_ids() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.id = Some(1);
        let mut cred2 = KiroCredentials::default();
        cred2.id = Some(1); // 重复 ID

        let result = MultiTokenManager::new(config, vec![cred1, cred2], None, None, false);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("重复的凭据 ID"),
            "错误消息应包含 '重复的凭据 ID'，实际: {}",
            err_msg
        );
    }

    #[test]
    fn test_multi_token_manager_api_key_missing_api_key_auto_disabled() {
        let config = Config::default();

        // auth_method=api_key 但缺少 api_key → 应被自动禁用
        let mut bad_cred = KiroCredentials::default();
        bad_cred.auth_method = Some("api_key".to_string());
        // api_key 保持 None

        let mut good_cred = KiroCredentials::default();
        good_cred.refresh_token = Some("valid_token".to_string());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 1); // bad_cred 被禁用，只剩 1 个可用
    }

    #[test]
    fn test_multi_token_manager_api_key_with_api_key_not_disabled() {
        let config = Config::default();

        // auth_method=api_key 且有 api_key → 不应被禁用
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.api_key = Some("ksk_test123".to_string());

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_report_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        // 前两次失败不会禁用（使用 ID 1）
        assert!(manager.report_failure(1));
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 2);

        // 第三次失败会禁用第一个凭据
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 1);

        // 继续失败第二个凭据（使用 ID 2）
        assert!(manager.report_failure(2));
        assert!(manager.report_failure(2));
        assert!(!manager.report_failure(2)); // 所有凭据都禁用了
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_report_success() {
        let config = Config::default();
        let cred = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 失败两次（使用 ID 1）
        manager.report_failure(1);
        manager.report_failure(1);

        // 成功后重置计数（使用 ID 1）
        manager.report_success(1);

        // 再失败两次不会禁用
        manager.report_failure(1);
        manager.report_failure(1);
        assert_eq!(manager.available_count(), 1);
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_auto_recovers_all_disabled() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(2);
        }

        assert_eq!(manager.available_count(), 0);

        // 应触发自愈：重置失败计数并重新启用，避免必须重启进程
        let ctx = manager.acquire_context(None).await.unwrap();
        assert!(ctx.token == "t1" || ctx.token == "t2");
        assert_eq!(manager.available_count(), 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_balanced_retries_until_bad_credential_disabled()
     {
        let config = Config::default();

        let mut bad_cred = KiroCredentials::default();
        bad_cred.priority = 0;
        bad_cred.refresh_token = Some("bad".to_string());

        let mut good_cred = KiroCredentials::default();
        good_cred.priority = 1;
        good_cred.access_token = Some("good-token".to_string());
        good_cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();

        let ctx = manager.acquire_context(None).await.unwrap();
        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "good-token");
    }

    #[tokio::test]
    async fn acquire_context_for_credential_uses_requested_id_and_respects_disabled() {
        let mut config = Config::default();
        config.per_credential_concurrency = 1;

        let mut first = KiroCredentials::default();
        first.id = Some(1);
        first.auth_method = Some("api_key".to_string());
        first.api_key = Some("api-key-1".to_string());

        let mut second = KiroCredentials::default();
        second.id = Some(2);
        second.auth_method = Some("api_key".to_string());
        second.api_key = Some("api-key-2".to_string());

        let manager =
            MultiTokenManager::new(config, vec![first, second], None, None, true).unwrap();

        let ctx = manager
            .acquire_context_for_credential(2, None)
            .await
            .unwrap();
        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "api-key-2");
        drop(ctx);

        manager.set_disabled(2, true).unwrap();
        let err = match manager.acquire_context_for_credential(2, None).await {
            Ok(_) => panic!("disabled credential should be rejected"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("已禁用"));
    }

    #[tokio::test]
    async fn test_acquire_context_for_session_excluding_skips_bound_credential() {
        let mut config = Config::default();
        config.session_affinity_enabled = true;

        let mut first = KiroCredentials::default();
        first.priority = 0;
        first.refresh_token = Some(format!("refresh-first-{}", "x".repeat(120)));
        first.access_token = Some("token-first".to_string());
        first.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let mut second = KiroCredentials::default();
        second.priority = 1;
        second.refresh_token = Some(format!("refresh-second-{}", "x".repeat(120)));
        second.access_token = Some("token-second".to_string());
        second.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![first, second], None, None, false).unwrap();

        let ctx1 = manager
            .acquire_context_for_session(Some("session-1"), None)
            .await
            .unwrap();
        assert_eq!(ctx1.id, 1);
        drop(ctx1);

        let excluded = HashSet::from([1]);
        let ctx2 = manager
            .acquire_context_for_session_excluding(Some("session-1"), None, &excluded)
            .await
            .unwrap();
        assert_eq!(ctx2.id, 2);
        drop(ctx2);

        let ctx3 = manager
            .acquire_context_for_session(Some("session-1"), None)
            .await
            .unwrap();
        assert_eq!(ctx3.id, 2);
    }

    #[tokio::test]
    async fn test_rank_candidates_skips_known_unsupported_model() {
        let mut first = KiroCredentials::default();
        first.priority = 0;
        first.access_token = Some("token-first".to_string());
        first.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let mut second = KiroCredentials::default();
        second.priority = 1;
        second.access_token = Some("token-second".to_string());
        second.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(Config::default(), vec![first, second], None, None, false)
                .unwrap();

        manager.set_model_list(1, ["claude-haiku-4.5".to_string()]);
        manager.set_model_list(2, [" Claude-Sonnet-4.5 ".to_string()]);

        let ctx = manager
            .acquire_context(Some("claude-sonnet-4-5"))
            .await
            .unwrap();

        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "token-second");
    }

    #[tokio::test]
    async fn test_rank_candidates_allows_unknown_model_list() {
        let mut first = KiroCredentials::default();
        first.priority = 0;
        first.access_token = Some("token-first".to_string());
        first.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let mut second = KiroCredentials::default();
        second.priority = 1;
        second.access_token = Some("token-second".to_string());
        second.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(Config::default(), vec![first, second], None, None, false)
                .unwrap();

        manager.set_model_list(1, ["claude-haiku-4.5".to_string()]);

        let ctx = manager
            .acquire_context(Some("claude-sonnet-4.5"))
            .await
            .unwrap();

        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "token-second");
    }

    #[test]
    fn test_multi_token_manager_report_refresh_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL - 1) {
            assert!(manager.report_refresh_failure(1));
        }
        assert_eq!(manager.available_count(), 2);

        assert!(manager.report_refresh_failure(1));
        assert_eq!(manager.available_count(), 1);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(first.refresh_failure_count, MAX_FAILURES_PER_CREDENTIAL);
    }

    #[tokio::test]
    async fn test_multi_token_manager_refresh_failure_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_refresh_failure(1);
            manager.report_refresh_failure(2);
        }
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
    }

    #[test]
    fn test_multi_token_manager_report_quota_exhausted() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        assert_eq!(manager.available_count(), 2);
        assert!(manager.report_quota_exhausted(1));
        assert_eq!(manager.available_count(), 1);

        // 再禁用第二个后，无可用凭据
        assert!(!manager.report_quota_exhausted(2));
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn credential_failure_ban_metadata_persists() {
        let dir = std::env::temp_dir().join(format!(
            "xkiro-credential-failure-ban-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(
            &path,
            r#"[
  {"id": 1, "refreshToken": "auth"},
  {"id": 2, "refreshToken": "suspended"}
]"#,
        )
        .unwrap();
        let config = crate::kiro::model::credentials::CredentialsConfig::load(&path).unwrap();
        let credentials = config.into_sorted_credentials();
        let manager = MultiTokenManager::new(
            Config::default(),
            credentials,
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();

        manager.mark_authentication_failed(1);
        manager.mark_credential_suspended_by_upstream(2);

        let saved: Vec<KiroCredentials> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let auth = saved.iter().find(|cred| cred.id == Some(1)).unwrap();
        assert!(auth.disabled);
        assert_eq!(
            auth.meta.disabled_reason.as_deref(),
            Some("AuthenticationFailed")
        );
        assert_eq!(auth.meta.ban_status.as_deref(), Some("BANNED"));
        assert_eq!(
            auth.meta.ban_reason.as_deref(),
            Some("Authentication failed - token invalid or expired")
        );
        assert!(auth.meta.ban_time.is_some());

        let suspended = saved.iter().find(|cred| cred.id == Some(2)).unwrap();
        assert!(suspended.disabled);
        assert_eq!(
            suspended.meta.disabled_reason.as_deref(),
            Some("AccountSuspended")
        );
        assert_eq!(suspended.meta.ban_status.as_deref(), Some("BANNED"));
        assert_eq!(
            suspended.meta.ban_reason.as_deref(),
            Some("AWS temporarily suspended - unusual user activity detected")
        );
        assert!(suspended.meta.ban_time.is_some());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quota_runtime_disable_does_not_persist_credential_ban() {
        let dir = std::env::temp_dir().join(format!(
            "xkiro-credential-quota-soft-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(&path, r#"[{"id": 1, "refreshToken": "quota"}]"#).unwrap();
        let config = crate::kiro::model::credentials::CredentialsConfig::load(&path).unwrap();
        let credentials = config.into_sorted_credentials();
        let manager = MultiTokenManager::new(
            Config::default(),
            credentials,
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();

        assert!(!manager.report_quota_exhausted(1));
        manager.persist_credentials().unwrap();

        let saved: Vec<KiroCredentials> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let quota = saved.iter().find(|cred| cred.id == Some(1)).unwrap();
        assert!(!quota.disabled);
        assert!(quota.meta.ban_status.is_none());
        assert!(quota.meta.ban_reason.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_report_quota_exhausted_keeps_credential_enabled_when_allow_over_usage() {
        let mut config = Config::default();
        config.allow_over_usage = true;
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert!(manager.report_quota_exhausted(1));
        assert_eq!(manager.available_count(), 2);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(!first.disabled);
        assert_eq!(first.disabled_reason, None);
    }

    #[test]
    fn test_mark_insufficient_balance_keeps_credential_enabled_when_allow_over_usage() {
        let mut config = Config::default();
        config.allow_over_usage = true;
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert!(!manager.mark_insufficient_balance(1));
        assert_eq!(manager.available_count(), 2);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(!first.disabled);
        assert_eq!(first.disabled_reason, None);
    }

    #[test]
    fn test_mark_insufficient_balance_disables_by_default() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert!(manager.mark_insufficient_balance(1));
        assert_eq!(manager.available_count(), 1);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(
            first.disabled_reason.as_deref(),
            Some("InsufficientBalance")
        );
    }

    #[test]
    fn usage_limits_snapshot_updates_overage_metadata() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();
        let usage: UsageLimitsResponse = serde_json::from_value(serde_json::json!({
            "nextDateReset": 1893456000.0,
            "subscriptionInfo": {
                "overageCapability": "OVERAGE_CAPABLE"
            },
            "overageConfiguration": {
                "overageStatus": "ENABLED"
            },
            "usageBreakdownList": [{
                "currentUsageWithPrecision": 12.5,
                "usageLimitWithPrecision": 10.0,
                "overageCapWithPrecision": 50.0,
                "overageRate": 0.04,
                "currentOverages": 2.5
            }]
        }))
        .unwrap();

        manager.sync_usage_snapshot_from_limits(1, &usage);

        let snapshot = manager.snapshot();
        let entry = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(entry.overage_status.as_deref(), Some("ENABLED"));
        assert_eq!(entry.overage_capability.as_deref(), Some("OVERAGE_CAPABLE"));
        assert_eq!(entry.overage_cap, Some(50.0));
        assert_eq!(entry.overage_rate, Some(0.04));
        assert_eq!(entry.current_overages, Some(2.5));
        assert_eq!(entry.usage_current, Some(12.5));
        assert_eq!(entry.usage_limit, Some(10.0));
        assert_eq!(entry.usage_percent, Some(1.25));
        assert_eq!(entry.next_reset_date.as_deref(), Some("2030-01-01"));
        assert!(entry.overage_checked_at.is_some());
        assert!(entry.last_refresh.is_some());
    }

    #[test]
    fn usage_limits_snapshot_updates_subscription_and_trial_metadata() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();
        let usage: UsageLimitsResponse = serde_json::from_value(serde_json::json!({
            "subscriptionInfo": {
                "subscriptionName": "KIRO PRO PLUS"
            },
            "usageBreakdownList": [{
                "currentUsage": 7.5,
                "usageLimit": 20.0,
                "freeTrialInfo": {
                    "currentUsage": 1.5,
                    "usageLimit": 5.0,
                    "freeTrialStatus": "ACTIVE",
                    "freeTrialExpiry": 1893555000.9
                }
            }]
        }))
        .unwrap();

        manager.sync_usage_snapshot_from_limits(1, &usage);

        let snapshot = manager.snapshot();
        let entry = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(entry.subscription_title.as_deref(), Some("KIRO PRO PLUS"));
        assert_eq!(entry.subscription_type.as_deref(), Some("PRO"));
        assert_eq!(entry.usage_current, Some(9.0));
        assert_eq!(entry.usage_limit, Some(25.0));
        assert_eq!(entry.usage_percent, Some(9.0 / 25.0));
        assert_eq!(entry.trial_usage_current, Some(1.5));
        assert_eq!(entry.trial_usage_limit, Some(5.0));
        assert_eq!(entry.trial_usage_percent, Some(0.3));
        assert_eq!(entry.trial_status.as_deref(), Some("ACTIVE"));
        assert_eq!(entry.trial_expires_at, Some(1_893_555_000));
    }

    #[tokio::test]
    async fn test_multi_token_manager_quota_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        manager.report_quota_exhausted(1);
        manager.report_quota_exhausted(2);
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_last_error_recorded_on_failure_and_cleared_on_success() {
        let config = Config::default();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        let before = manager.snapshot();
        assert_eq!(before.entries[0].last_error_code, None);
        assert_eq!(before.entries[0].last_error_at, None);

        manager.report_failure(1);
        let after_fail = manager.snapshot();
        assert_eq!(
            after_fail.entries[0].last_error_code.as_deref(),
            Some("call_failed")
        );
        assert!(after_fail.entries[0].last_error_at.is_some());

        manager.report_success(1);
        let after_ok = manager.snapshot();
        assert_eq!(after_ok.entries[0].last_error_code, None);
        assert_eq!(after_ok.entries[0].last_error_at, None);
    }

    #[test]
    fn test_last_error_code_reflects_disable_reason() {
        let config = Config::default();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        manager.report_quota_exhausted(1);
        let snap = manager.snapshot();
        assert_eq!(
            snap.entries[0].last_error_code.as_deref(),
            Some("quota_exceeded")
        );
        assert!(snap.entries[0].last_error_at.is_some());
    }

    // ============ 凭据级区域优先级测试 ============

    #[test]
    fn test_credential_region_priority_uses_credential_auth_region() {
        // 凭据配置了 auth_region 时，应使用凭据的 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-west-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-west-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_credential_region() {
        // 凭据未配置 auth_region 但配置了 region 时，应回退到凭据.region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-central-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_config() {
        // 凭据未配置 auth_region 和 region 时，应回退到 config
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials::default();
        assert!(credentials.auth_region.is_none());
        assert!(credentials.region.is_none());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_multiple_credentials_use_respective_regions() {
        // 多凭据场景下，不同凭据使用各自的 auth_region
        let mut config = Config::default();
        config.region = "ap-northeast-1".to_string();

        let mut cred1 = KiroCredentials::default();
        cred1.auth_region = Some("us-east-1".to_string());

        let mut cred2 = KiroCredentials::default();
        cred2.region = Some("eu-west-1".to_string());

        let cred3 = KiroCredentials::default(); // 无 region，使用 config

        assert_eq!(cred1.effective_auth_region(&config), "us-east-1");
        assert_eq!(cred2.effective_auth_region(&config), "eu-west-1");
        assert_eq!(cred3.effective_auth_region(&config), "ap-northeast-1");
    }

    #[test]
    fn test_idc_oidc_endpoint_uses_credential_auth_region() {
        // 验证 IAM Identity Center OIDC endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);

        assert_eq!(refresh_url, "https://oidc.eu-central-1.amazonaws.com/token");
    }

    #[test]
    fn test_social_refresh_endpoint_uses_credential_auth_region() {
        // 验证社交登录 refresh endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("ap-southeast-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);

        assert_eq!(
            refresh_url,
            "https://prod.ap-southeast-1.auth.desktop.kiro.dev/refreshToken"
        );
    }

    #[test]
    fn test_api_call_uses_effective_api_region() {
        // 验证 API 调用使用 effective_api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-west-1".to_string());

        // 凭据.region 不参与 api_region 回退链
        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.us-west-2.amazonaws.com");
    }

    #[test]
    fn test_api_call_uses_credential_api_region() {
        // 凭据配置了 api_region 时，API 调用应使用凭据的 api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.api_region = Some("eu-central-1".to_string());

        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.eu-central-1.amazonaws.com");
    }

    #[test]
    fn test_credential_region_empty_string_treated_as_set() {
        // 空字符串 auth_region 被视为已设置（虽然不推荐，但行为应一致）
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("".to_string());

        let region = credentials.effective_auth_region(&config);
        // 空字符串被视为已设置，不会回退到 config
        assert_eq!(region, "");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("auth-only".to_string());
        credentials.api_region = Some("api-only".to_string());

        assert_eq!(credentials.effective_auth_region(&config), "auth-only");
        assert_eq!(credentials.effective_api_region(&config), "api-only");
    }

    // ============================================================
    // 凭据并发控制测试 — 单凭据 Semaphore + FIFO 排队 + 动态扩缩
    // ============================================================
    mod concurrency_tests {
        use super::super::*;
        use std::sync::Arc;
        use std::time::Duration as StdDuration;
        use tokio::time::timeout as tokio_timeout;

        /// 构造一个"看起来有效"的凭据：未过期、含 access_token、含 refresh_token
        /// 这样 acquire_context 不会触发刷新逻辑，能快速走完 try_ensure_token
        fn make_cred(tag: &str) -> KiroCredentials {
            let mut c = KiroCredentials::default();
            c.refresh_token = Some(format!("refresh-{}-{}", tag, "x".repeat(120)));
            c.access_token = Some(format!("token-{}", tag));
            c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
            c
        }

        fn make_config(per_cred: usize, timeout_secs: u64) -> Config {
            let mut config = Config::default();
            config.per_credential_concurrency = per_cred;
            config.acquire_wait_timeout_secs = timeout_secs;
            config
        }

        fn make_proxy(
            id: u64,
            max_concurrency: Option<u32>,
        ) -> crate::kiro::proxy_manager::ProxyEntry {
            crate::kiro::proxy_manager::ProxyEntry {
                id: Some(id),
                url: format!("http://proxy-{}.local:8080", id),
                username: Some(format!("user-{}", id)),
                password: Some(format!("pass-{}", id)),
                region: Some("US:California".to_string()),
                country: Some("US".to_string()),
                max_concurrency,
                disabled: false,
                note: None,
            }
        }

        #[tokio::test]
        async fn test_bound_proxy_url_injected_into_context() {
            let config = make_config(1, 5);
            let mut credential = make_cred("bound-proxy");
            credential.proxy_id = Some(7);
            let manager =
                MultiTokenManager::new(config, vec![credential], None, None, false).unwrap();
            let proxy_manager = Arc::new(
                crate::kiro::proxy_manager::ProxyManager::new(vec![make_proxy(7, Some(1))], None)
                    .unwrap(),
            );
            manager.set_proxy_manager(Some(proxy_manager));

            let ctx = manager
                .acquire_context_for_credential(1, None)
                .await
                .unwrap();

            assert_eq!(
                ctx.credentials.proxy_url.as_deref(),
                Some("http://proxy-7.local:8080")
            );
            assert_eq!(ctx.credentials.proxy_username.as_deref(), Some("user-7"));
            assert_eq!(ctx.credentials.proxy_password.as_deref(), Some("pass-7"));
            assert_eq!(ctx.credentials.proxy_id, Some(7));
            assert!(ctx._proxy_permit.is_some());
        }

        #[tokio::test]
        async fn test_bound_proxy_concurrency_limit_blocks_second_credential() {
            let config = make_config(1, 5);
            let mut first = make_cred("first");
            first.proxy_id = Some(7);
            let mut second = make_cred("second");
            second.proxy_id = Some(7);
            let manager =
                MultiTokenManager::new(config, vec![first, second], None, None, false).unwrap();
            let proxy_manager = Arc::new(
                crate::kiro::proxy_manager::ProxyManager::new(vec![make_proxy(7, Some(1))], None)
                    .unwrap(),
            );
            manager.set_proxy_manager(Some(proxy_manager));

            let _ctx = manager
                .acquire_context_for_credential(1, None)
                .await
                .unwrap();
            let error = match manager.acquire_context_for_credential(2, None).await {
                Ok(_) => panic!("第二个凭据不应在同一代理 permit 被占用时获取成功"),
                Err(error) => error.to_string(),
            };

            assert!(error.contains("绑定的代理不可用或并发已满"));
        }

        #[test]
        fn test_credential_for_persistence_clears_pool_proxy_material() {
            let mut credential = make_cred("persist");
            credential.proxy_url = Some("http://runtime-proxy.local:8080".to_string());
            credential.proxy_username = Some("runtime-user".to_string());
            credential.proxy_password = Some("runtime-pass".to_string());
            credential.proxy_id = Some(9);

            let persisted = MultiTokenManager::credential_for_persistence(credential);

            assert_eq!(persisted.proxy_id, Some(9));
            assert!(persisted.proxy_url.is_none());
            assert!(persisted.proxy_username.is_none());
            assert!(persisted.proxy_password.is_none());
        }

        #[test]
        fn test_credential_for_persistence_clears_invalid_profile_arn() {
            let mut credential = make_cred("persist-profile");
            credential.profile_arn = Some("e3438419-4424-4e57-8990-ef76bd749a44".to_string());

            let persisted = MultiTokenManager::credential_for_persistence(credential);

            assert!(persisted.profile_arn.is_none());
        }

        #[test]
        fn test_add_prevalidated_pool_proxy_persists_only_proxy_id() {
            let path = std::env::temp_dir().join(format!(
                "xkiro-prevalidated-proxy-{}.json",
                uuid::Uuid::new_v4()
            ));
            let manager = MultiTokenManager::new(
                make_config(1, 5),
                Vec::new(),
                None,
                Some(path.clone()),
                true,
            )
            .unwrap();
            let mut credential = make_cred("prevalidated");
            credential.proxy_id = Some(9);
            credential.proxy_url = Some("http://runtime-proxy.local:8080".to_string());
            credential.proxy_username = Some("runtime-user".to_string());
            credential.proxy_password = Some("runtime-pass".to_string());

            let id = manager.add_prevalidated_credential(credential).unwrap();

            let content = std::fs::read_to_string(&path).unwrap();
            let persisted: Vec<KiroCredentials> = serde_json::from_str(&content).unwrap();
            let stored = persisted
                .iter()
                .find(|credential| credential.id == Some(id))
                .unwrap();
            assert_eq!(stored.proxy_id, Some(9));
            assert!(stored.proxy_url.is_none());
            assert!(stored.proxy_username.is_none());
            assert!(stored.proxy_password.is_none());
            let _ = std::fs::remove_file(path);
        }

        #[test]
        fn test_choose_replacement_proxy_excludes_current_and_failed() {
            let config = make_config(1, 5);
            let mut credential = make_cred("replace-proxy");
            credential.region = Some("US:California".to_string());
            credential.proxy_id = Some(7);
            let manager =
                MultiTokenManager::new(config, vec![credential], None, None, false).unwrap();
            let mut replacement = make_proxy(8, Some(2));
            replacement.region = Some("US:California".to_string());
            let mut other_region = make_proxy(9, Some(2));
            other_region.region = Some("EU:Frankfurt".to_string());
            let proxy_manager = Arc::new(
                crate::kiro::proxy_manager::ProxyManager::new(
                    vec![make_proxy(7, Some(1)), replacement, other_region],
                    None,
                )
                .unwrap(),
            );
            manager.set_proxy_manager(Some(proxy_manager));

            let mut excluded = HashSet::new();
            excluded.insert(7);

            assert_eq!(manager.choose_replacement_proxy(1, &excluded), Some(8));
        }

        /// 测试 1：候选 A 被占满时，选位逻辑自动跳到 B 立即拿到（不阻塞）
        #[tokio::test]
        async fn test_skip_busy_credential() {
            let config = make_config(1, 5);
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("a"), make_cred("b")],
                None,
                None,
                false,
            )
            .unwrap();

            // 占用任意一个凭据
            let ctx1 = manager.acquire_context(None).await.unwrap();

            // 立即再来一个：应该跳过满的、命中空的，无需等待
            let ctx2 = tokio_timeout(StdDuration::from_millis(500), manager.acquire_context(None))
                .await
                .expect("满凭据应被跳过，另一凭据应立即可拿")
                .unwrap();

            assert_ne!(
                ctx1.id, ctx2.id,
                "两个 ctx 必须分属不同凭据（skip_busy 失败）"
            );
        }

        /// 测试 2：所有凭据满时，acquire 会 await 直到 permit 释放
        #[tokio::test]
        async fn test_wait_when_full_then_acquire() {
            let config = make_config(1, 10);
            let manager = Arc::new(
                MultiTokenManager::new(config, vec![make_cred("only")], None, None, false).unwrap(),
            );

            let ctx1 = manager.acquire_context(None).await.unwrap();
            let id1 = ctx1.id;

            // 后台等待者：因唯一凭据满会进 wait_any_credential
            let manager_clone = Arc::clone(&manager);
            let waiter = tokio::spawn(async move { manager_clone.acquire_context(None).await });

            // 给 waiter 一段时间确认它确实在等
            tokio::time::sleep(StdDuration::from_millis(200)).await;
            assert!(
                !waiter.is_finished(),
                "全满情况下 acquire 应该阻塞，而不是直接返回"
            );

            // 释放第一个 permit，waiter 应被唤醒
            drop(ctx1);

            let ctx2 = tokio_timeout(StdDuration::from_secs(2), waiter)
                .await
                .expect("permit 释放后 waiter 应及时完成")
                .expect("waiter 任务不应 panic")
                .expect("acquire 应成功（permit 已归还）");

            assert_eq!(ctx2.id, id1, "唯一凭据，复用同一 id");
        }

        /// 测试 3：等待超时时返回 sentinel "credential queue wait timeout"
        #[tokio::test]
        async fn test_acquire_timeout_returns_sentinel() {
            // 1s 超时，便于快速验证
            let config = make_config(1, 1);
            let manager =
                MultiTokenManager::new(config, vec![make_cred("only")], None, None, false).unwrap();

            // 占满，且持有不释放
            let _ctx_hold = manager.acquire_context(None).await.unwrap();

            let start = std::time::Instant::now();
            let err = manager
                .acquire_context(None)
                .await
                .err()
                .unwrap()
                .to_string();
            let elapsed = start.elapsed();

            assert!(
                err.contains("credential queue wait timeout"),
                "错误信息应包含 sentinel，实际: {}",
                err
            );
            // 给宽松下界，避免 CI 抖动；上界不卡死，仅防 0ms 立即返回
            assert!(
                elapsed >= StdDuration::from_millis(500),
                "超时应至少接近配置时长 1s，实际: {:?}",
                elapsed
            );
        }

        /// 测试 4：CallContext drop 后，permit 自动归还，下次 acquire 立即成功
        #[tokio::test]
        async fn test_drop_releases_permit() {
            let config = make_config(1, 5);
            let manager =
                MultiTokenManager::new(config, vec![make_cred("only")], None, None, false).unwrap();

            {
                let _ctx = manager.acquire_context(None).await.unwrap();
                // 离开作用域 → CallContext drop → permit 归还
            }

            // 下一次应立即可拿（不应等 timeout）
            let _ctx2 = tokio_timeout(StdDuration::from_millis(500), manager.acquire_context(None))
                .await
                .expect("drop 后 permit 应已归还，acquire 应立即成功")
                .unwrap();
        }

        /// 测试 5：等待中的 acquire future 被取消时，不泄漏 permit
        #[tokio::test]
        async fn test_cancel_safe_no_permit_leak() {
            // 长 timeout，避免触发 sentinel
            let config = make_config(1, 30);
            let manager =
                MultiTokenManager::new(config, vec![make_cred("only")], None, None, false).unwrap();

            let ctx1 = manager.acquire_context(None).await.unwrap();

            // 用外层 timeout 强制取消等待中的 acquire（模拟客户端断开）
            let cancelled =
                tokio_timeout(StdDuration::from_millis(150), manager.acquire_context(None)).await;
            assert!(cancelled.is_err(), "应被外层 timeout 取消");

            // 释放第一个 permit
            drop(ctx1);

            // 若取消时泄漏了"幽灵 permit"，这里会再次卡住超时
            let _ctx2 = tokio_timeout(StdDuration::from_secs(1), manager.acquire_context(None))
                .await
                .expect("取消的 future 必须归还 permit，否则池被永久占满")
                .unwrap();
        }

        /// 测试 6：动态扩缩容 — set_per_credential_concurrency 增加 permit
        #[tokio::test]
        async fn test_dynamic_resize_increase() {
            let config = make_config(1, 2);
            let manager =
                MultiTokenManager::new(config, vec![make_cred("only")], None, None, false).unwrap();

            // 当前 per_cred=1，先占满
            let ctx1 = manager.acquire_context(None).await.unwrap();

            // 在线扩到 3：底层 add_permits(2)
            manager.set_per_credential_concurrency(1, 3).unwrap();

            // 应该再拿到 2 个（新增 permit 立即可用）
            let ctx2 = tokio_timeout(StdDuration::from_millis(500), manager.acquire_context(None))
                .await
                .expect("扩容后第 2 个 permit 应立即可拿")
                .unwrap();
            let ctx3 = tokio_timeout(StdDuration::from_millis(500), manager.acquire_context(None))
                .await
                .expect("扩容后第 3 个 permit 应立即可拿")
                .unwrap();

            // 第 4 个：超出新上限 3，应进入等待并被外层 timeout 取消
            let r =
                tokio_timeout(StdDuration::from_millis(200), manager.acquire_context(None)).await;
            assert!(r.is_err(), "已达扩容后上限，第 4 个应等待");

            drop(ctx1);
            drop(ctx2);
            drop(ctx3);
        }

        #[tokio::test]
        async fn test_global_wait_does_not_reserve_idle_credential_permit() {
            let mut config = make_config(1, 5);
            config.global_concurrency = 1;
            let manager = Arc::new(
                MultiTokenManager::new(
                    config,
                    vec![make_cred("a"), make_cred("b")],
                    None,
                    None,
                    false,
                )
                .unwrap(),
            );

            manager.update_balance_cache_full(1, 100.0, 0.0);
            manager.update_balance_cache_full(2, 50.0, 0.0);

            let ctx1 = manager.acquire_context(None).await.unwrap();
            let idle_id = if ctx1.id == 1 { 2 } else { 1 };
            let waiter_manager = Arc::clone(&manager);
            let waiter = tokio::spawn(async move { waiter_manager.acquire_context(None).await });

            tokio::time::sleep(StdDuration::from_millis(200)).await;
            assert!(
                !waiter.is_finished(),
                "全局并发满时第二个请求应等待 global permit"
            );

            let idle_available = {
                let map = manager.credential_semaphores.lock();
                map.get(&idle_id).unwrap().available_permits()
            };
            assert_eq!(
                idle_available, 1,
                "等待 global permit 时不能预占空闲凭据的 permit"
            );

            drop(ctx1);
            let ctx2 = tokio_timeout(StdDuration::from_secs(1), waiter)
                .await
                .expect("global permit 释放后 waiter 应及时完成")
                .expect("waiter 任务不应 panic")
                .expect("acquire 应成功");
            drop(ctx2);
        }

        #[test]
        fn test_global_default_resize_skips_credential_override() {
            let config = make_config(1, 5);
            let mut overridden = make_cred("override");
            overridden.concurrency = Some(3);
            let manager = MultiTokenManager::new(
                config,
                vec![overridden, make_cred("default")],
                None,
                None,
                false,
            )
            .unwrap();

            manager.set_per_credential_concurrency(1, 2).unwrap();

            let (override_available, default_available) = {
                let map = manager.credential_semaphores.lock();
                (
                    map.get(&1).unwrap().available_permits(),
                    map.get(&2).unwrap().available_permits(),
                )
            };
            assert_eq!(
                override_available, 3,
                "凭据级 concurrency override 不应被全局默认并发热更新改变"
            );
            assert_eq!(default_available, 2);
        }

        #[tokio::test]
        async fn test_normalized_load_prevents_large_capacity_monopoly() {
            let config = make_config(1, 5);
            let mut large = make_cred("large");
            large.concurrency = Some(10);
            let mut medium = make_cred("medium");
            medium.concurrency = Some(3);
            let mut small = make_cred("small");
            small.concurrency = Some(1);
            let mut overage_only = make_cred("overage");
            overage_only.concurrency = Some(1);
            let mut lower_priority = make_cred("lower-priority");
            lower_priority.concurrency = Some(1);
            lower_priority.priority = 1;
            let manager = MultiTokenManager::new(
                config,
                vec![large, medium, small, overage_only, lower_priority],
                None,
                None,
                false,
            )
            .unwrap();

            manager.update_balance_cache_full(1, 100.0, 0.0);
            manager.update_balance_cache_full(2, 75.0, 0.0);
            manager.update_balance_cache_full(3, 50.0, 0.0);
            manager.update_balance_cache_full(4, 0.0, 1000.0);
            manager.update_balance_cache_full(5, 1000.0, 0.0);

            let ctx1 = manager.acquire_context(None).await.unwrap();
            assert_eq!(ctx1.id, 1, "首个请求仍优先高余额凭据");

            let ctx2 = tokio_timeout(StdDuration::from_millis(500), manager.acquire_context(None))
                .await
                .expect("低占用率凭据应立即可拿")
                .unwrap();
            assert_eq!(
                ctx2.id, 2,
                "大并发凭据已有占用后，同额度档位内应先平衡负载率"
            );

            let ctx3 = tokio_timeout(StdDuration::from_millis(500), manager.acquire_context(None))
                .await
                .expect("空闲 primary 凭据应继续优先于已占用凭据")
                .unwrap();
            assert_eq!(ctx3.id, 3);

            let ctx4 = tokio_timeout(StdDuration::from_millis(500), manager.acquire_context(None))
                .await
                .expect("归一化负载低的大并发凭据应重新进入候选前列")
                .unwrap();
            assert_eq!(ctx4.id, 1);

            drop(ctx1);
            drop(ctx2);
            drop(ctx3);
            drop(ctx4);
        }

        #[test]
        fn test_weighted_round_robin_within_identical_rank_segment() {
            let config = make_config(1, 5);
            let mut high_weight = make_cred("high-weight");
            high_weight.weight = 3;
            let normal_a = make_cred("normal-a");
            let normal_b = make_cred("normal-b");
            let manager = MultiTokenManager::new(
                config,
                vec![high_weight, normal_a, normal_b],
                None,
                None,
                false,
            )
            .unwrap();

            let mut counts: HashMap<u64, usize> = HashMap::new();
            for _ in 0..10 {
                let selected = manager.rank_candidates(None)[0];
                *counts.entry(selected).or_default() += 1;
            }

            assert_eq!(counts.get(&1).copied().unwrap_or_default(), 6);
            assert_eq!(counts.get(&2).copied().unwrap_or_default(), 2);
            assert_eq!(counts.get(&3).copied().unwrap_or_default(), 2);
        }

        #[test]
        fn test_weight_does_not_override_priority() {
            let config = make_config(1, 5);
            let mut preferred = make_cred("preferred");
            preferred.priority = 0;
            preferred.weight = 1;
            let mut lower_priority = make_cred("lower-priority");
            lower_priority.priority = 1;
            lower_priority.weight = 100;
            let manager =
                MultiTokenManager::new(config, vec![preferred, lower_priority], None, None, false)
                    .unwrap();

            assert_eq!(manager.rank_candidates(None)[0], 1);
        }

        #[tokio::test]
        async fn test_weight_does_not_override_normalized_load() {
            let config = make_config(1, 5);
            let mut weighted = make_cred("weighted");
            weighted.concurrency = Some(10);
            weighted.weight = 100;
            let mut idle = make_cred("idle");
            idle.concurrency = Some(1);
            let manager =
                MultiTokenManager::new(config, vec![weighted, idle], None, None, false).unwrap();

            let ctx = manager.acquire_context(None).await.unwrap();
            assert_eq!(ctx.id, 1);

            assert_eq!(
                manager.rank_candidates(None)[0],
                2,
                "已有占用时，应先按归一化负载选择空闲凭据，再考虑 weight"
            );

            drop(ctx);
        }

        #[test]
        fn test_rate_limited_credential_is_temporarily_skipped() {
            let config = make_config(1, 5);
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("limited"), make_cred("available")],
                None,
                None,
                false,
            )
            .unwrap();

            manager.report_rate_limited(1);

            assert_eq!(manager.rank_candidates(None), vec![2]);
        }

        #[test]
        fn test_all_rate_limited_credentials_still_fall_back_to_candidates() {
            let config = make_config(1, 5);
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("limited-a"), make_cred("limited-b")],
                None,
                None,
                false,
            )
            .unwrap();

            manager.report_rate_limited(1);
            manager.report_rate_limited(2);

            let ranked = manager.rank_candidates(None);
            assert_eq!(ranked.len(), 2);
            assert!(ranked.contains(&1));
            assert!(ranked.contains(&2));
        }

        #[tokio::test]
        async fn test_session_affinity_skips_rate_limited_bound_credential() {
            let mut config = make_config(1, 5);
            config.session_affinity_enabled = true;
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("bound"), make_cred("fallback")],
                None,
                None,
                false,
            )
            .unwrap();

            let ctx1 = manager
                .acquire_context_for_session(Some("session-1"), None)
                .await
                .unwrap();
            assert_eq!(ctx1.id, 1);
            drop(ctx1);

            manager.report_rate_limited(1);

            let ctx2 = manager
                .acquire_context_for_session(Some("session-1"), None)
                .await
                .unwrap();
            assert_eq!(ctx2.id, 2);
        }

        #[tokio::test]
        async fn test_client_affinity_reuses_bound_credential() {
            let mut config = make_config(1, 5);
            config.session_affinity_enabled = true;
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("client-a"), make_cred("client-b")],
                None,
                None,
                false,
            )
            .unwrap();

            let ctx1 = manager
                .acquire_context_for_client(Some("api-key-1"), None)
                .await
                .unwrap();
            let first_id = ctx1.id;
            drop(ctx1);

            let ctx2 = manager
                .acquire_context_for_client(Some("api-key-1"), None)
                .await
                .unwrap();
            assert_eq!(ctx2.id, first_id);
        }

        #[tokio::test]
        async fn test_client_affinity_disabled_keeps_rank_distribution() {
            let mut config = make_config(1, 5);
            config.session_affinity_enabled = false;
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("client-a"), make_cred("client-b")],
                None,
                None,
                false,
            )
            .unwrap();

            let ctx1 = manager
                .acquire_context_for_client(Some("api-key-1"), None)
                .await
                .unwrap();
            let first_id = ctx1.id;
            drop(ctx1);

            let ctx2 = manager
                .acquire_context_for_client(Some("api-key-1"), None)
                .await
                .unwrap();
            assert_ne!(
                ctx2.id, first_id,
                "关闭调度亲和后，同一 API key 不应绕过 rank 平摊"
            );
        }

        #[tokio::test]
        async fn test_client_affinity_skips_busy_bound_credential() {
            let mut config = make_config(1, 5);
            config.session_affinity_enabled = true;
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("client-a"), make_cred("client-b")],
                None,
                None,
                false,
            )
            .unwrap();

            let ctx1 = manager
                .acquire_context_for_client(Some("api-key-1"), None)
                .await
                .unwrap();
            let bound_id = ctx1.id;

            let ctx2 = tokio_timeout(
                StdDuration::from_millis(500),
                manager.acquire_context_for_client(Some("api-key-1"), None),
            )
            .await
            .expect("绑定凭据满载时应立即分流")
            .unwrap();
            assert_ne!(ctx2.id, bound_id);

            drop(ctx1);
            drop(ctx2);

            let ctx3 = manager
                .acquire_context_for_client(Some("api-key-1"), None)
                .await
                .unwrap();
            assert_eq!(ctx3.id, bound_id, "满载分流不应覆盖原有 API key 绑定");
        }

        #[tokio::test]
        async fn test_route_affinity_prefers_session_over_client_key() {
            let mut config = make_config(1, 5);
            config.session_affinity_enabled = true;
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("client-a"), make_cred("client-b")],
                None,
                None,
                false,
            )
            .unwrap();

            let client_ctx = manager
                .acquire_context_for_client(Some("api-key-1"), None)
                .await
                .unwrap();
            let client_bound_id = client_ctx.id;
            drop(client_ctx);

            let session_ctx = manager
                .acquire_context_for_route_excluding(
                    Some("session-1"),
                    Some("api-key-1"),
                    None,
                    &HashSet::new(),
                )
                .await
                .unwrap();
            let session_bound_id = session_ctx.id;
            assert_ne!(
                session_bound_id, client_bound_id,
                "有 session key 时应优先按会话亲和，而不是复用客户端 API 密钥绑定"
            );
            drop(session_ctx);

            let session_ctx2 = manager
                .acquire_context_for_route_excluding(
                    Some("session-1"),
                    Some("api-key-1"),
                    None,
                    &HashSet::new(),
                )
                .await
                .unwrap();
            assert_eq!(session_ctx2.id, session_bound_id);
        }

        /// 测试 7：set_disabled 持久化失败时，in-memory 状态不被改动
        ///
        /// 通过传入一个无法写入的非法路径（/proc 下不允许写），让
        /// write_credentials_snapshot 在 atomic_write 阶段返回 Err，
        /// 验证 setter 不会反序——即先 persist、再改 in-memory 的语义生效。
        #[test]
        fn test_set_disabled_persist_fails_keeps_in_memory() {
            // 单线程 runtime，setter 内部调 block_in_place 需要 multi_thread；
            // 但 setter 本身是同步函数，且 persist 路径会 try_current()，
            // 这里不在 runtime 内调用，让 persist 直接走 do_atomic_write 同步分支。
            let config = make_config(1, 5);
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("a"), make_cred("b")],
                None,
                Some(PathBuf::from("/proc/xkiro-nonexistent/credentials.json")),
                true,
            )
            .unwrap();

            let snapshot_before = manager.snapshot();
            let target_id = snapshot_before.entries[0].id;
            assert!(!snapshot_before.entries[0].disabled, "前置：未禁用");

            // setter 应返回 Err（rename 到 /proc 不允许）
            let r = manager.set_disabled(target_id, true);
            assert!(r.is_err(), "持久化失败时 set_disabled 必须返回 Err");

            // in-memory 应保持原状
            let snapshot_after = manager.snapshot();
            let entry_after = snapshot_after
                .entries
                .iter()
                .find(|e| e.id == target_id)
                .expect("目标凭据应仍存在");
            assert!(
                !entry_after.disabled,
                "持久化失败后 in-memory disabled 不应被改动"
            );
        }

        /// 测试 8：set_priority 持久化失败时，in-memory 状态不被改动
        #[test]
        fn test_set_priority_persist_fails_keeps_in_memory() {
            let config = make_config(1, 5);
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("a"), make_cred("b")],
                None,
                Some(PathBuf::from("/proc/xkiro-nonexistent/credentials.json")),
                true,
            )
            .unwrap();

            let snapshot_before = manager.snapshot();
            let target_id = snapshot_before.entries[0].id;
            let priority_before = snapshot_before.entries[0].priority;

            // setter 应返回 Err（rename 到 /proc 不允许）
            let r = manager.set_priority(target_id, priority_before.saturating_add(5));
            assert!(r.is_err(), "持久化失败时 set_priority 必须返回 Err");

            // in-memory 优先级应保持原状
            let snapshot_after = manager.snapshot();
            let entry_after = snapshot_after
                .entries
                .iter()
                .find(|e| e.id == target_id)
                .expect("目标凭据应仍存在");
            assert_eq!(
                entry_after.priority, priority_before,
                "持久化失败后 in-memory priority 不应被改动"
            );
        }
    }

    // ==================== A4 认证类自愈重探 ====================

    fn auth_recoverable_credential(id: u64) -> KiroCredentials {
        let mut cred = KiroCredentials::default();
        cred.id = Some(id);
        cred.access_token = Some(format!("access-{id}"));
        cred.refresh_token = Some(format!("refresh-{id}"));
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        cred
    }

    #[test]
    fn backoff_duration_follows_exponential_schedule_with_cap() {
        assert_eq!(backoff_duration(1), Duration::minutes(1));
        assert_eq!(backoff_duration(2), Duration::minutes(5));
        assert_eq!(backoff_duration(3), Duration::minutes(30));
        assert_eq!(backoff_duration(4), Duration::hours(2));
        assert_eq!(backoff_duration(5), Duration::hours(2));
    }

    #[test]
    fn is_auth_recoverable_reason_only_matches_auth_disabled_set() {
        assert!(is_auth_recoverable_reason(
            DisabledReason::AuthenticationFailed
        ));
        assert!(is_auth_recoverable_reason(
            DisabledReason::TooManyRefreshFailures
        ));
        assert!(is_auth_recoverable_reason(
            DisabledReason::InvalidRefreshToken
        ));

        assert!(!is_auth_recoverable_reason(
            DisabledReason::CredentialSuspended
        ));
        assert!(!is_auth_recoverable_reason(
            DisabledReason::InsufficientBalance
        ));
        assert!(!is_auth_recoverable_reason(DisabledReason::QuotaExceeded));
        assert!(!is_auth_recoverable_reason(DisabledReason::Manual));
        assert!(!is_auth_recoverable_reason(
            DisabledReason::ModelUnavailable
        ));
    }

    #[test]
    fn get_ids_due_for_reprobe_selects_only_eligible_entries() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                auth_recoverable_credential(1),
                auth_recoverable_credential(2),
                auth_recoverable_credential(3),
                auth_recoverable_credential(4),
            ],
            None,
            None,
            true,
        )
        .expect("manager should build");

        {
            let mut entries = manager.entries.lock();
            for entry in entries.iter_mut() {
                match entry.id {
                    // 认证类禁用，重探时间已过 → 应被选中
                    1 => {
                        entry.disabled = true;
                        entry.disabled_reason = Some(DisabledReason::AuthenticationFailed);
                        entry.recovery_backoff_level = 1;
                        entry.reprobe_next = Some(Utc::now() - Duration::minutes(1));
                    }
                    // 认证类禁用，但重探时间在未来 → 不选
                    2 => {
                        entry.disabled = true;
                        entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);
                        entry.recovery_backoff_level = 1;
                        entry.reprobe_next = Some(Utc::now() + Duration::minutes(5));
                    }
                    // 非认证类禁用（粘性）→ 永不选
                    3 => {
                        entry.disabled = true;
                        entry.disabled_reason = Some(DisabledReason::CredentialSuspended);
                        entry.reprobe_next = Some(Utc::now() - Duration::minutes(1));
                    }
                    // 未禁用 → 永不选
                    _ => {}
                }
            }
        }

        assert_eq!(manager.get_ids_due_for_reprobe(), vec![1]);
    }

    #[test]
    fn apply_reprobe_outcome_drives_open_halfopen_closed_state_machine() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![auth_recoverable_credential(1)],
            None,
            None,
            true,
        )
        .expect("manager should build");

        // OPEN：认证类禁用，level 1，1min 后重探
        {
            let mut entries = manager.entries.lock();
            let entry = &mut entries[0];
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::AuthenticationFailed);
            entry.recovery_backoff_level = 1;
            entry.reprobe_next = Some(Utc::now() + backoff_duration(1));
        }

        // HALF_OPEN→OPEN：重探失败 → level 升至 2，下次约 5min 后
        manager.apply_reprobe_outcome(1, false);
        {
            let entries = manager.entries.lock();
            let entry = &entries[0];
            assert!(entry.disabled, "重探失败应保持禁用");
            assert_eq!(entry.recovery_backoff_level, 2);
            let next = entry.reprobe_next.expect("应有下次重探时间");
            let delta = next - Utc::now();
            assert!(
                delta >= Duration::minutes(4) && delta <= Duration::minutes(6),
                "下次重探应约 5min 后，实际 {delta}"
            );
        }

        // HALF_OPEN→CLOSED：重探成功 → 重新启用，退避清零
        manager.apply_reprobe_outcome(1, true);
        {
            let entries = manager.entries.lock();
            let entry = &entries[0];
            assert!(!entry.disabled, "重探成功应重新启用");
            assert_eq!(entry.disabled_reason, None);
            assert_eq!(entry.failure_count, 0);
            assert_eq!(entry.refresh_failure_count, 0);
            assert_eq!(entry.recovery_backoff_level, 0);
            assert_eq!(entry.reprobe_next, None);
        }
    }

    #[test]
    fn apply_reprobe_outcome_ignores_non_auth_disabled_entries() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![auth_recoverable_credential(1)],
            None,
            None,
            true,
        )
        .expect("manager should build");

        // 非认证类禁用不应被自愈逻辑改动
        {
            let mut entries = manager.entries.lock();
            let entry = &mut entries[0];
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::CredentialSuspended);
        }

        manager.apply_reprobe_outcome(1, true);

        let entries = manager.entries.lock();
        let entry = &entries[0];
        assert!(entry.disabled, "非认证类禁用不应被重新启用");
        assert_eq!(
            entry.disabled_reason,
            Some(DisabledReason::CredentialSuspended)
        );
    }
}
