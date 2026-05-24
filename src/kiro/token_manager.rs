//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use futures::future::select_all;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration as StdDuration, Instant};

use crate::common::utf8::floor_char_boundary;
use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::affinity::SessionAffinity;
use crate::kiro::background_refresh::{BackgroundRefreshConfig, BackgroundRefresher, RefreshResult};
use crate::kiro::endpoint::{
    CLI_ENDPOINT_NAME, CliEndpoint, IDE_ENDPOINT_NAME, IdeEndpoint, KiroEndpoint, RequestContext,
};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::token_refresh::{
    IdcRefreshRequest, IdcRefreshResponse, RefreshRequest, RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::model::config::Config;

/// 检查 Token 是否在指定时间内过期
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

/// 检查 Token 是否已过期（提前 5 分钟判断）
pub(crate) fn is_token_expired(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 5).unwrap_or(true)
}

/// 检查 Token 是否即将过期（10分钟内）
pub(crate) fn is_token_expiring_soon(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 10).unwrap_or(false)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// 生成 API Key 脱敏展示(前 4 + ... + 后 4,长度不足或非 ASCII 回退 ***)
fn mask_api_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 16 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "***".to_string()
    }
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
             这通常是 Kiro IDE 为了防止凭证被第三方工具使用而故意截断的。",
            refresh_token.len()
        );
    }

    Ok(())
}

/// Refresh Token 永久失效错误
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

/// 刷新 Token
pub(crate) async fn refresh_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    // API Key 凭据不支持 Token 刷新：底层契约级拦截
    // 其他调用点（try_ensure_token / 活跃路径 / add_credential）在调用前已显式分流 API Key；
    // 仅 force_refresh_token_for 未分流，此处 bail 让错误自然传播为 400 BAD_REQUEST。
    if credentials.is_api_key_credential() {
        bail!("API Key 凭据不支持刷新 Token");
    }

    validate_refresh_token(credentials)?;

    // 根据 auth_method 选择刷新方式
    // 如果未指定 auth_method，根据是否有 clientId/clientSecret 自动判断
    let auth_method = credentials.auth_method.as_deref().unwrap_or_else(|| {
        if credentials.client_id.is_some() && credentials.client_secret.is_some() {
            "idc"
        } else {
            "social"
        }
    });

    if auth_method.eq_ignore_ascii_case("idc")
        || auth_method.eq_ignore_ascii_case("builder-id")
        || auth_method.eq_ignore_ascii_case("iam")
    {
        refresh_idc_token(credentials, config, proxy).await
    } else {
        refresh_social_token(credentials, config, proxy).await
    }
}

/// 刷新 Social Token
async fn refresh_social_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 Social Token...");

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

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("Social refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "OAuth 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OAuth 服务暂时不可用",
            _ => "Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: RefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

/// 刷新 IdC Token (AWS SSO OIDC)
async fn refresh_idc_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 IdC Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientId"))?;
    let client_secret = credentials
        .client_secret
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientSecret"))?;

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

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("IdC refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "IdC 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OIDC 服务暂时不可用",
            _ => "IdC Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
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

    // 同步更新 profile_arn（如果 IdC 响应中包含）
    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    Ok(new_credentials)
}

/// 根据凭据生效的 endpoint 名称构造对应的 `KiroEndpoint` 实例
///
/// 与 BK 原版 `token_manager.rs` L438-447 字节级对齐：就地 `Box::new` 两种 endpoint
/// （`IdeEndpoint` / `CliEndpoint` 均无状态，零成本）。
///
/// 主链路（API/MCP 调用）仍走 main.rs 注入到 `Provider` 的 endpoint registry；
/// 此 helper 仅服务于 `get_usage_limits` 这种 token_manager 内部低频路径，
/// 避免把 endpoint registry 注入 `MultiTokenManager` 结构带来的扩散修改。
fn endpoint_for_credentials(
    credentials: &KiroCredentials,
    config: &Config,
) -> anyhow::Result<Box<dyn KiroEndpoint>> {
    match credentials.effective_endpoint_name(Some(&config.default_endpoint)) {
        IDE_ENDPOINT_NAME => Ok(Box::new(IdeEndpoint::new())),
        CLI_ENDPOINT_NAME => Ok(Box::new(CliEndpoint::new())),
        name => bail!("未知 endpoint: {}", name),
    }
}

/// 获取使用额度信息
pub(crate) async fn get_usage_limits(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
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
    let usage = endpoint.usage_request_parts(&ctx)?;

    let client = build_client(proxy, 60, config.tls_backend)?;
    let mut request = client.get(&usage.url);
    for (name, value) in usage.headers {
        request = request.header(name, value);
    }

    let response = request.send().await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let error_msg = match status.as_u16() {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法获取使用额度",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取使用额度失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let body_text = response.text().await?;
    let data: UsageLimitsResponse = serde_json::from_str(&body_text).map_err(|e| {
        tracing::error!(
            "getUsageLimits JSON 解析失败: {}，原始响应: {}",
            e,
            body_text
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
        let msg = match status.as_u16() {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法切换超额开关",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "切换超额开关失败",
        };
        bail!("{}: {} {}", msg, status, body_text);
    }
    Ok(())
}

// ============================================================================
// 多凭据 Token 管理器
// ============================================================================

/// 解析路径的真实目标（穿透 symlink）。
///
/// 用于 `persist_credentials` 的原子写入：保证 rename 替换的是 symlink
/// 指向的真实文件，而不是 symlink 本身。
///
/// 优先 `canonicalize`（目标存在时最可靠），失败时 fallback 到 `read_link`，
/// 都失败则返回原路径（保持向后兼容）。
fn resolve_symlink_target(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }

    if let Ok(target) = std::fs::read_link(path) {
        if target.is_absolute() {
            return target;
        }
        if let Some(parent) = path.parent() {
            return parent.join(target);
        }
        return target;
    }

    path.to_path_buf()
}

/// 单个凭据条目的状态
struct CredentialEntry {
    /// 凭据唯一 ID
    id: u64,
    /// 凭据信息
    credentials: KiroCredentials,
    /// API 调用连续失败次数
    failure_count: u32,
    /// Token 刷新连续失败次数
    refresh_failure_count: u32,
    /// 是否已禁用
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）
    disabled_reason: Option<DisabledReason>,
    /// API 调用成功次数
    success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    last_used_at: Option<String>,
}

/// 禁用原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum DisabledReason {
    /// Admin API 手动禁用
    Manual,
    /// 连续失败达到阈值后自动禁用
    TooManyFailures,
    /// Token 刷新连续失败达到阈值后自动禁用
    TooManyRefreshFailures,
    /// 额度已用尽（如 MONTHLY_REQUEST_COUNT）
    QuotaExceeded,
    /// Refresh Token 永久失效（服务端返回 invalid_grant）
    InvalidRefreshToken,
    /// 凭据配置无效（如 authMethod=api_key 但缺少 kiroApiKey）
    InvalidConfig,
    /// 认证失败（如 invalid_grant 之外的认证错误）
    AuthenticationFailed,
    /// 账户被暂停
    AccountSuspended,
    /// 余额不足
    InsufficientBalance,
    /// 模型临时不可用（全局禁用）
    ModelUnavailable,
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
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// Token 过期时间
    pub expires_at: Option<String>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
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
    /// Token 刷新连续失败次数
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

/// 多凭据 Token 管理器
///
/// 支持多个凭据的管理，实现固定优先级 + 故障转移策略
/// 故障统计基于 API 调用结果，而非 Token 刷新结果
pub struct MultiTokenManager {
    config: RwLock<Config>,
    proxy: RwLock<Option<ProxyConfig>>,
    /// 凭据条目列表
    entries: Mutex<Vec<CredentialEntry>>,
    /// 每凭据 Token 刷新锁，确保同一凭据同一时间只有一个刷新操作；
    /// 不同凭据之间并行，避免多账号同时过期被串行化。
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
    /// MODEL_TEMPORARILY_UNAVAILABLE 错误累计（达到阈值后全局禁用）
    model_unavailable_count: AtomicU32,
    /// 选凭据轮询计数器（多个凭据评分相同时兜底使用，避免总选第一个）
    selection_rr: AtomicU64,
    /// 全局禁用恢复时间（None 表示当前没有全局禁用）
    global_recovery_time: Mutex<Option<DateTime<Utc>>>,
    /// 后台 Token 刷新任务（启动后由 Drop 自动停止）
    background_refresher: Mutex<Option<Arc<BackgroundRefresher>>>,
    /// 单凭据并发信号量（按 id 维度限流，permit 数 = config.per_credential_concurrency）
    credential_semaphores: Mutex<HashMap<u64, Arc<Semaphore>>>,
    /// 全局并发信号量（None 表示不限，对应 config.global_concurrency = 0）
    global_semaphore: Mutex<Option<Arc<Semaphore>>>,
    /// Session 亲和性：让同一会话连续请求黏住同一凭据（提升上游 prompt cache 命中率）
    session_affinity: SessionAffinity,
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
        let config_ref = &config;

        let entries: Vec<CredentialEntry> = credentials
            .into_iter()
            .map(|mut cred| {
                cred.canonicalize_auth_method();
                let id = cred.id.unwrap_or_else(|| {
                    let id = next_id;
                    next_id += 1;
                    cred.id = Some(id);
                    has_new_ids = true;
                    id
                });
                if cred.machine_id.is_none() {
                    cred.machine_id =
                        Some(machine_id::generate_from_credentials(&cred, config_ref));
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
                }
            })
            .collect();

        // 校验 API Key 凭据配置完整性：authMethod=api_key 时必须提供 kiroApiKey
        let mut entries = entries;
        for entry in &mut entries {
            if entry.credentials.kiro_api_key.is_none()
                && entry
                    .credentials
                    .auth_method
                    .as_deref()
                    .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                    .unwrap_or(false)
            {
                tracing::warn!(
                    "凭据 #{} 配置了 authMethod=api_key 但缺少 kiroApiKey 字段，已自动禁用",
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
            model_unavailable_count: AtomicU32::new(0),
            selection_rr: AtomicU64::new(0),
            global_recovery_time: Mutex::new(None),
            background_refresher: Mutex::new(None),
            credential_semaphores: Mutex::new(credential_semaphores),
            global_semaphore: Mutex::new(global_semaphore),
            session_affinity: SessionAffinity::default(),
        };

        // 如果有新分配的 ID 或新生成的 machineId，立即持久化到配置文件
        if has_new_ids || has_new_machine_ids {
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

    /// 给 acquire_context 用的候选排序：返回排好序的凭据 id 列表。
    ///
    /// 排序字典序（每一项为前一项的 tie-breaker）：
    /// 1. `priority` asc：用户优先级（0 最高），分层匹配，同 priority 才参与下层比较。
    /// 2. `has_primary` desc：还有正式剩余（>=1.0）的永远压制只剩超额的，
    ///    即使后者总额更大。
    /// 3. `primary_bucket = log2(primary).floor()` desc：正式剩余多的优先（量级粒度，
    ///    避免 ±几次调用的浮点抖动反复重排）。
    /// 4. `has_overage` desc：primary 全为 0 时，**超额已开启且有剩余**的优先；
    ///    上游未开 overage 时缓存内 overage_remaining=0 → has_overage=false → 排末尾。
    /// 5. `overage_bucket = log2(overage).floor()` desc：超额剩余多的优先。
    /// 6. `in_flight = max_permits - available_permits` asc：仅作最末位 tie-breaker，
    ///    LEAST_REQUEST 兜底打散并发；不能盖过余额，否则会出现"剩余少但暂时空闲"
    ///    压过"剩余多但 1 个在飞"的反直觉行为。
    /// 7. 完整 6 元组相同的段：rr 轮转，公平分摊。
    ///
    /// acquire_context 拿到列表后挨个 `try_acquire_owned`，第一个抢到 permit 的就用。
    fn rank_candidates(&self, model: Option<&str>) -> Vec<u64> {
        // 1. 过滤可用候选
        let candidate_ids: Vec<u64> = {
            let entries = self.entries.lock();
            let is_opus = model
                .map(|m| m.to_lowercase().contains("opus"))
                .unwrap_or(false);
            entries
                .iter()
                .filter(|e| !e.disabled)
                .filter(|e| !is_opus || e.credentials.supports_opus())
                .map(|e| e.id)
                .collect()
        };

        if candidate_ids.is_empty() {
            return Vec::new();
        }

        let rr_offset =
            self.selection_rr.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;

        // 2. 拷贝每个候选的 Semaphore Arc（短锁），稍后无锁查 available_permits
        let sema_map: HashMap<u64, Arc<Semaphore>> = {
            let map = self.credential_semaphores.lock();
            candidate_ids
                .iter()
                .filter_map(|&id| map.get(&id).map(|s| (id, s.clone())))
                .collect()
        };

        // 3. 取每个候选的 max_permits（凭据级 override 优先，回退全局）和 priority
        let global_per_cred = self.config.read().per_credential_concurrency.max(1);
        let (max_permits_map, priority_map): (HashMap<u64, usize>, HashMap<u64, u32>) = {
            let entries = self.entries.lock();
            let mut max_map = HashMap::new();
            let mut pri_map = HashMap::new();
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
                }
            }
            (max_map, pri_map)
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

        // 5. 排序键：(priority asc, has_primary desc, primary_bucket desc,
        //              has_overage desc, overage_bucket desc, in_flight asc)
        //    - priority asc：用户优先级（0 最高），分层匹配的最外层
        //    - has_primary desc：还有正式剩余的永远压制只剩超额的（即使后者总额更大）
        //    - primary_bucket desc：在 has_primary 同档内，正式剩余多的优先
        //    - has_overage desc：primary 全 0 时，开了超额且有剩余的优先
        //    - overage_bucket desc：超额剩余多的优先
        //    - in_flight asc：以上全 tie 时，把当前最少在飞的放前面（仅作 tie-breaker，
        //      不能盖过余额；否则会出现"剩余少但空闲"压过"剩余多但 1 个在飞"的反直觉行为）
        const REMAINING_EPS: f64 = 1e-3;
        let bucket = |r: f64| -> i32 {
            if r <= REMAINING_EPS {
                i32::MIN
            } else {
                r.log2().floor() as i32
            }
        };
        let mut scored: Vec<(u64, u32, bool, i32, bool, i32, usize)> = candidate_ids
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
                let pri = priority_map.get(&id).copied().unwrap_or(0);
                (
                    id,
                    pri,
                    has_primary,
                    bucket(primary.max(REMAINING_EPS)),
                    has_overage,
                    bucket(overage.max(REMAINING_EPS)),
                    in_flight,
                )
            })
            .collect();

        scored.sort_by(|a, b| {
            a.1.cmp(&b.1) // priority asc
                .then(b.2.cmp(&a.2)) // has_primary desc
                .then(b.3.cmp(&a.3)) // primary_bucket desc
                .then(b.4.cmp(&a.4)) // has_overage desc
                .then(b.5.cmp(&a.5)) // overage_bucket desc
                .then(a.6.cmp(&b.6)) // in_flight asc (tie-breaker)
        });

        // 6. 完整 6 元组相同的段做 rr 轮转（同优先级同余额量级同负载，公平打散）
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
            let start = if len > 0 { rr_offset % len } else { 0 };
            for k in 0..len {
                result.push(scored[i + (start + k) % len].0);
            }
            i = j;
        }
        result
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
        let mut sema_pairs: Vec<(u64, std::sync::Arc<Semaphore>)> = Vec::with_capacity(candidates.len());
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
                    let permit = sema.acquire_owned().await.map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;
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
        let sid = match session_id {
            Some(s) if !s.is_empty() => s,
            _ => return self.acquire_context(model).await,
        };

        // 命中亲和绑定 → 校验后尝试复用
        if let Some(bound_id) = self.session_affinity.get(sid) {
            // 校验：未禁用 + 模型允许（与 rank_candidates 同源筛选）
            let usable = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == bound_id && !e.disabled)
                    .map(|e| Self::credential_supports_model(e, model))
                    .unwrap_or(false)
            };
            if !usable {
                tracing::debug!(
                    session = %sid,
                    credential_id = %bound_id,
                    "亲和命中但凭据已禁用 / 模型不允许，重选"
                );
                self.session_affinity.remove(sid);
            } else {
                // 尝试 try_acquire 绑定凭据的 sema
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
                    if let Some(creds) = credentials {
                        let global_arc_opt = { self.global_semaphore.lock().clone() };
                        let global_permit = if let Some(g) = global_arc_opt {
                            match g.acquire_owned().await {
                                Ok(p) => Some(p),
                                Err(e) => {
                                    drop(per_cred_permit);
                                    return Err(anyhow::anyhow!(
                                        "global semaphore closed: {}",
                                        e
                                    ));
                                }
                            }
                        } else {
                            None
                        };
                        match self.try_ensure_token(bound_id, &creds).await {
                            Ok(mut ctx) => {
                                ctx._credential_permit = Some(per_cred_permit);
                                ctx._global_permit = global_permit;
                                self.session_affinity.touch(sid);
                                return Ok(ctx);
                            }
                            Err(e) => {
                                drop(per_cred_permit);
                                drop(global_permit);
                                tracing::debug!(
                                    session = %sid,
                                    credential_id = %bound_id,
                                    error = %e,
                                    "亲和绑定凭据 token 刷新失败，回退到 rank"
                                );
                                // 与 acquire_context 主路径同步记账，
                                // 否则该凭据看起来比实际健康
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
                        session = %sid,
                        credential_id = %bound_id,
                        "亲和绑定凭据 sema 满，本次分流（保留绑定）"
                    );
                    // 保留绑定：分流到 rank 选出的其他凭据，但不覆盖 affinity 映射，
                    // 等下次该 session 请求时再尝试黏回原凭据。
                    return self.acquire_context(model).await;
                }
            }
        }

        // 未命中 / 凭据不可用 / token 刷新失败：rank 选 + 建立（或覆盖到新）绑定
        let ctx = self.acquire_context(model).await?;
        self.session_affinity.set(sid, ctx.id);
        Ok(ctx)
    }

    /// 检查凭据是否支持指定模型（与 rank_candidates 筛选规则一致）
    fn credential_supports_model(entry: &CredentialEntry, model: Option<&str>) -> bool {
        let Some(m) = model else { return true };
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
    /// 如果 Token 过期或即将过期，会自动刷新
    /// Token 刷新失败会累计到当前凭据，达到阈值后禁用并切换
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    pub async fn acquire_context(&self, model: Option<&str>) -> anyhow::Result<CallContext> {
        // 检查是否需要自动恢复（5 分钟全局禁用）
        self.check_and_recover();

        let total = self.total_count();
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize).max(1);
        let mut attempt_count = 0;

        // 排队等待超时：从 config 动态读取（默认 60s）
        let timeout_secs = self.config.read().acquire_wait_timeout_secs;
        let wait_timeout = std::time::Duration::from_secs(timeout_secs);

        loop {
            if attempt_count >= max_attempts {
                anyhow::bail!(
                    "所有凭据均无法获取有效 Token（可用: {}/{}）",
                    self.available_count(),
                    total
                );
            }

            // 1. 收集候选（统一加权排序：priority asc, recent_usage asc, remaining desc）
            let mut candidates = self.rank_candidates(model);

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
                        }
                    }
                    drop(entries);
                    candidates = self.rank_candidates(model);
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
            let credentials = match credentials {
                Some(c) => c,
                None => {
                    // 等待期间凭据被禁用 → 释放 permit 重新选择
                    drop(per_cred_permit);
                    attempt_count += 1;
                    continue;
                }
            };

            // 5. 获取 global permit（如配置了 global_concurrency > 0）
            //    注意锁顺序：先拿 per-cred permit，再拿 global permit；不会跟其他路径形成环
            let global_arc_opt = { self.global_semaphore.lock().clone() };
            let global_permit = if let Some(g) = global_arc_opt {
                match g.acquire_owned().await {
                    Ok(p) => Some(p),
                    Err(e) => {
                        drop(per_cred_permit);
                        return Err(anyhow::anyhow!("global semaphore closed: {}", e));
                    }
                }
            } else {
                None
            };

            // 6. 尝试获取/刷新 Token，并把 permit 注入 CallContext（Drop 自动归还）
            match self.try_ensure_token(id, &credentials).await {
                Ok(mut ctx) => {
                    ctx._credential_permit = Some(per_cred_permit);
                    ctx._global_permit = global_permit;
                    return Ok(ctx);
                }
                Err(e) => {
                    // 早 drop：刷新失败时尽快归还 permit，避免占用排队席位
                    drop(per_cred_permit);
                    drop(global_permit);

                    // refreshToken 永久失效 → 立即禁用，不累计重试
                    let has_available =
                        if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                            tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, e);
                            self.report_refresh_token_invalid(id)
                        } else {
                            tracing::warn!("凭据 #{} Token 刷新失败: {}", id, e);
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
    // 对齐 BK：MODEL_TEMPORARILY_UNAVAILABLE 错误累计触发全局禁用，
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

        for entry in entries.iter_mut() {
            if !entry.disabled {
                entry.disabled = true;
                entry.disabled_reason = Some(reason);
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
    /// 余额不足、认证失败、账户暂停等不会被自动恢复
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

    /// 标记凭据为认证失败（如 invalid_grant，不会被自动恢复）
    #[allow(dead_code)]
    pub fn mark_authentication_failed(&self, id: u64) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::AuthenticationFailed);
                tracing::warn!("凭据 #{} 已标记为认证失败", id);
            }
        }
    }

    /// 标记凭据为账户暂停（不会被自动恢复）
    #[allow(dead_code)]
    pub fn mark_account_suspended(&self, id: u64) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::AccountSuspended);
                tracing::warn!("凭据 #{} 已标记为账户暂停", id);
            }
        }
    }

    /// 标记凭据为余额不足（不会被自动恢复）
    pub fn mark_insufficient_balance(&self, id: u64) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InsufficientBalance);
            tracing::warn!("凭据 #{} 已标记为余额不足", id);
        }
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
    /// rank 用 (has_primary, primary_bucket, overage_bucket) 区分主备额度优先级。
    pub fn update_balance_cache_full(
        &self,
        id: u64,
        remaining: f64,
        overage_remaining: f64,
    ) {
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
    /// 每次成功获取 Token 时调用，递增 recent_usage；超过重置周期则清零
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

    /// 取指定凭据的刷新锁（懒分配）
    ///
    /// 不同 id 互不阻塞，多账号同时过期可并行刷新；
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

    /// 尝试使用指定凭据获取有效 Token
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
        // API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需刷新
        if credentials.is_api_key_credential() {
            let token = credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            return Ok(CallContext {
                id,
                credentials: credentials.clone(),
                token,
                _credential_permit: None,
                _global_permit: None,
            });
        }

        // 第一次检查（无锁）：快速判断是否需要刷新
        let needs_refresh = is_token_expired(credentials) || is_token_expiring_soon(credentials);

        let creds = if needs_refresh {
            // 获取该凭据的刷新锁，同 id 串行、不同 id 并行
            let lock = self.refresh_lock_for(id);
            let _guard = lock.lock().await;

            // 第二次检查：获取锁后重新读取凭据，因为其他请求可能已经完成刷新
            let current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 不存在", id))?
            };

            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                // 确实需要刷新
                let proxy_snap = self.proxy.read().clone();
                let config_snap = self.config.read().clone();
                let effective_proxy = current_creds.effective_proxy(proxy_snap.as_ref());
                let new_creds =
                    refresh_token(&current_creds, &config_snap, effective_proxy.as_ref()).await?;

                if is_token_expired(&new_creds) {
                    anyhow::bail!("刷新后的 Token 仍然无效或已过期");
                }

                // 更新凭据
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = new_creds.clone();
                    }
                }

                // 回写凭据到文件（仅多凭据格式），失败只记录警告
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                }

                new_creds
            } else {
                // 其他请求已经完成刷新，直接使用新凭据
                tracing::debug!("Token 已被其他请求刷新，跳过刷新");
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
        // 收集所有凭据
        //
        // 仅持久化「手动禁用」状态：自动禁用（失败阈值 / 额度用尽 / 认证失败 /
        // 模型不可用等）不落盘，避免重启后自动禁用被误标记为手动禁用，导致无法自愈。
        let credentials: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    cred.disabled = e.disabled_reason == Some(DisabledReason::Manual);
                    cred
                })
                .collect()
        };
        self.write_credentials_snapshot(&credentials)
    }

    /// 把指定 snapshot 原子写入凭据文件。
    ///
    /// 与 [`persist_credentials`] 的区别是：调用方自己负责构造 snapshot
    /// （可在 in-memory 真正落更前用临时快照试写），常用于 admin API 的
    /// 「先 persist、再改 in-memory」语义，避免写盘失败时内存与磁盘不一致。
    fn write_credentials_snapshot(
        &self,
        credentials: &[KiroCredentials],
    ) -> anyhow::Result<bool> {
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

        // 原子写入：先写临时文件，再 rename 替换目标文件
        // rename 在同一文件系统上是原子操作，避免进程崩溃导致凭据文件损坏
        // 解析 symlink 以确保 rename 写入真实目标（而非替换 symlink 本身）
        let real_path = resolve_symlink_target(path);
        let tmp_path = real_path.with_extension("json.tmp");

        let do_atomic_write = || -> anyhow::Result<()> {
            // 尝试保留原文件权限（避免 umask 导致权限放宽）
            let original_perms = std::fs::metadata(&real_path).ok().map(|m| m.permissions());

            std::fs::write(&tmp_path, &json)
                .with_context(|| format!("写入临时凭据文件失败: {:?}", tmp_path))?;

            if let Some(perms) = original_perms {
                // best-effort：权限复制失败不阻塞回写
                let _ = std::fs::set_permissions(&tmp_path, perms);
            }

            // 跨平台原子替换：Windows 上 rename 无法覆盖已存在文件，需先删除
            #[cfg(windows)]
            if real_path.exists() {
                std::fs::remove_file(&real_path)
                    .with_context(|| format!("删除旧凭据文件失败: {:?}", real_path))?;
            }

            std::fs::rename(&tmp_path, &real_path).with_context(|| {
                format!("原子替换凭据文件失败: {:?} -> {:?}", tmp_path, real_path)
            })?;
            Ok(())
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
                if let Err(e) = std::fs::write(&path, json) {
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

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.success_count += 1;
                entry.last_used_at = Some(Utc::now().to_rfc3339());
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次）",
                    id,
                    entry.success_count
                );
            }
        }
        self.save_stats_debounced();
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

    /// 报告指定凭据刷新 Token 失败。
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
            let refresh_failure_count = entry.refresh_failure_count;

            tracing::warn!(
                "凭据 #{} Token 刷新失败（{}/{}）",
                id,
                refresh_failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if refresh_failure_count < MAX_FAILURES_PER_CREDENTIAL {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);

            tracing::error!(
                "凭据 #{} Token 已连续刷新失败 {} 次，已被禁用",
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
            self.session_affinity.remove_by_credential(id);
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
        self.session_affinity.remove_by_credential(id);
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
        let max_permits = self.config.read().per_credential_concurrency;

        ManagerSnapshot {
            entries: entries
                .iter()
                .map(|e| CredentialEntrySnapshot {
                    id: e.id,
                    priority: e.credentials.priority,
                    disabled: e.disabled,
                    failure_count: e.failure_count,
                    auth_method: if e.credentials.is_api_key_credential() {
                        Some("api_key".to_string())
                    } else {
                        e.credentials.auth_method.as_deref().map(|m| {
                            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                                "idc".to_string()
                            } else {
                                m.to_string()
                            }
                        })
                    },
                    has_profile_arn: e.credentials.profile_arn.is_some(),
                    expires_at: if e.credentials.is_api_key_credential() {
                        None // API Key 凭据本地不维护过期时间（服务端策略未知）
                    } else {
                        e.credentials.expires_at.clone()
                    },
                    refresh_token_hash: if e.credentials.is_api_key_credential() {
                        None
                    } else {
                        e.credentials.refresh_token.as_deref().map(sha256_hex)
                    },
                    api_key_hash: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(sha256_hex)
                    } else {
                        None
                    },
                    masked_api_key: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(mask_api_key)
                    } else {
                        None
                    },
                    email: e.credentials.email.clone(),
                    success_count: e.success_count,
                    last_used_at: e.last_used_at.clone(),
                    has_proxy: e.credentials.proxy_url.is_some(),
                    proxy_url: e.credentials.proxy_url.clone(),
                    refresh_failure_count: e.refresh_failure_count,
                    disabled_reason: e.disabled_reason.map(|r| match r {
                        DisabledReason::Manual => "Manual",
                        DisabledReason::TooManyFailures => "TooManyFailures",
                        DisabledReason::TooManyRefreshFailures => "TooManyRefreshFailures",
                        DisabledReason::QuotaExceeded => "QuotaExceeded",
                        DisabledReason::InvalidRefreshToken => "InvalidRefreshToken",
                        DisabledReason::InvalidConfig => "InvalidConfig",
                        DisabledReason::AuthenticationFailed => "AuthenticationFailed",
                        DisabledReason::AccountSuspended => "AccountSuspended",
                        DisabledReason::InsufficientBalance => "InsufficientBalance",
                        DisabledReason::ModelUnavailable => "ModelUnavailable",
                    }.to_string()),
                    endpoint: e.credentials.endpoint.clone(),
                    available_permits: sema_snapshot
                        .get(&e.id)
                        .map(|s| s.available_permits())
                        .unwrap_or(0),
                    max_permits,
                    concurrency: e.credentials.concurrency,
                })
                .collect(),
            total: entries.len(),
            available,
        }
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
        } else {
            entry.disabled_reason = Some(DisabledReason::Manual);
        }
        drop(entries);
        if disabled {
            self.session_affinity.remove_by_credential(id);
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

    /// 设置凭据级最大并发数（Admin API）
    ///
    /// 语义：先把“含新 concurrency 的 snapshot”原子写入凭据文件，写盘成功后再修改
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
                anyhow::bail!(
                    "凭据 #{} 因配置无效被禁用，请修正配置后重启服务",
                    id
                );
            }
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.disabled = false;
            entry.disabled_reason = None;
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
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiro_api_key，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            // 检查是否需要刷新 token
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let lock = self.refresh_lock_for(id);
                let _guard = lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };

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
                            entry.credentials = new_creds.clone();
                        }
                    }
                    // 持久化失败只记录警告，不影响本次请求
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
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

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let proxy_snap = self.proxy.read().clone();
        let config_snap = self.config.read().clone();
        let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());
        let usage_limits = get_usage_limits(&credentials, &config_snap, &token, effective_proxy.as_ref()).await?;

        // 更新订阅等级到凭据（仅在发生变化时持久化）
        if let Some(subscription_title) = usage_limits.subscription_title() {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let old_title = entry.credentials.subscription_title.clone();
                    if old_title.as_deref() != Some(subscription_title) {
                        entry.credentials.subscription_title =
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

        // 同步余额到内部 balance_cache（用于负载均衡的动态 TTL）
        // 公式对齐 BK L2326-2329: remaining = (limit - used).max(0.0)
        let used = usage_limits.current_usage();
        let limit = usage_limits.usage_limit();
        let remaining = (limit - used).max(0.0);
        let overage_enabled = usage_limits.overage_status() == Some("ENABLED");
        let overage_remaining = if overage_enabled {
            ((usage_limits.overage_cap()) - (used - limit).max(0.0)).max(0.0)
        } else {
            0.0
        };
        self.update_balance_cache_full(id, remaining, overage_remaining);

        Ok(usage_limits)
    }

    /// 切换指定凭据的上游 overage 开关
    ///
    /// 调 Kiro `setUserPreference` 接口，成功后返回。本方法不维护本地缓存，
    /// 调用方需要拿最新状态时另行 `get_usage_limits_for`。
    pub async fn set_overage_status_for(
        &self,
        id: u64,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);
            if needs_refresh {
                let lock = self.refresh_lock_for(id);
                let _guard = lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };
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
                            entry.credentials = new_creds.clone();
                        }
                    }
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
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

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

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

    /// 添加新凭据（Admin API）
    ///
    /// # 流程
    /// 1. 验证凭据基本字段（API Key: kiroApiKey 不为空; OAuth: refreshToken 不为空）
    /// 2. 基于 kiroApiKey 或 refreshToken 的 SHA-256 哈希检测重复
    /// 3. OAuth: 尝试刷新 Token 验证凭据有效性; API Key: 跳过
    /// 4. 分配新 ID（当前最大 ID + 1）
    /// 5. 添加到 entries 列表
    /// 6. 持久化到配置文件
    ///
    /// # 返回
    /// - `Ok(u64)` - 新凭据 ID
    /// - `Err(_)` - 验证失败或添加失败
    pub async fn add_credential(&self, new_cred: KiroCredentials) -> anyhow::Result<u64> {
        // 1. 基本验证
        if new_cred.is_api_key_credential() {
            let api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("kiroApiKey 为空");
            }
        } else {
            validate_refresh_token(&new_cred)?;
        }

        // 2. 基于哈希检测重复
        if new_cred.is_api_key_credential() {
            let new_api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 kiroApiKey"))?;
            let new_api_key_hash = sha256_hex(new_api_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .kiro_api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（kiroApiKey 重复）");
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

        // 3. 验证凭据有效性（API Key 无需网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential() {
            new_cred.clone()
        } else {
            let proxy_snap = self.proxy.read().clone();
            let config_snap = self.config.read().clone();
            let effective_proxy = new_cred.effective_proxy(proxy_snap.as_ref());
            refresh_token(&new_cred, &config_snap, effective_proxy.as_ref()).await?
        };

        // 4. 分配新 ID
        let new_id = {
            let entries = self.entries.lock();
            entries.iter().map(|e| e.id).max().unwrap_or(0) + 1
        };

        // 5. 设置 ID 并保留用户输入的元数据
        validated_cred.id = Some(new_id);
        validated_cred.priority = new_cred.priority;
        validated_cred.auth_method = new_cred.auth_method.map(|m| {
            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                "idc".to_string()
            } else {
                m
            }
        });
        validated_cred.client_id = new_cred.client_id;
        validated_cred.client_secret = new_cred.client_secret;
        validated_cred.region = new_cred.region;
        validated_cred.auth_region = new_cred.auth_region;
        validated_cred.api_region = new_cred.api_region;
        validated_cred.machine_id = new_cred.machine_id;
        validated_cred.email = new_cred.email;
        validated_cred.proxy_url = new_cred.proxy_url;
        validated_cred.proxy_username = new_cred.proxy_username;
        validated_cred.proxy_password = new_cred.proxy_password;
        validated_cred.kiro_api_key = new_cred.kiro_api_key;
        validated_cred.concurrency = new_cred.concurrency;

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
            });
        }

        // 6. 持久化
        self.persist_credentials()?;

        tracing::info!("成功添加凭据 #{}", new_id);
        Ok(new_id)
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
        // 清掉绑到此凭据的所有 session 亲和
        self.session_affinity.remove_by_credential(id);

        // 持久化更改
        self.persist_credentials()?;

        // 立即回写统计数据，清除已删除凭据的残留条目
        self.save_stats();

        tracing::info!("已删除凭据 #{}", id);
        Ok(())
    }

    /// 强制刷新指定凭据的 Token（Admin API）
    ///
    /// 无条件调用上游 API 重新获取 access token，不检查是否过期。
    /// 适用于排查问题、Token 异常但未过期、主动更新凭据状态等场景。
    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<()> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // 同凭据串行，多凭据并行
        let lock = self.refresh_lock_for(id);
        let _guard = lock.lock().await;

        // 无条件调用 refresh_token
        let proxy_snap = self.proxy.read().clone();
        let config_snap = self.config.read().clone();
        let effective_proxy = credentials.effective_proxy(proxy_snap.as_ref());
        let new_creds =
            refresh_token(&credentials, &config_snap, effective_proxy.as_ref()).await?;

        // 更新 entries 中对应凭据
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials = new_creds;
                entry.refresh_failure_count = 0;
            }
        }

        // 持久化
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("强制刷新 Token 后持久化失败: {}", e);
        }

        tracing::info!("凭据 #{} Token 已强制刷新", id);
        Ok(())
    }

    /// 设置单凭据最大并发数（Admin API/运行时调整）
    ///
    /// # 行为
    /// - n >= 1（0 无意义）；n == 旧值时 noop
    /// - 写入 config 内存值后，对所有已注册的 per-credential `Semaphore`：
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

        // 收集所有 Arc<Semaphore> 后释放锁，避免长时间持锁
        let semas: Vec<Arc<Semaphore>> = {
            let map = self.credential_semaphores.lock();
            map.values().cloned().collect()
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

    // ==================== 后台 Token 刷新 API ====================
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

    /// 启动后台 Token 刷新任务
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
                    match manager.refresh_token_for_credential(id).await {
                        Ok(_) => true,
                        Err(e) => {
                            tracing::warn!("后台刷新凭据 #{} Token 失败: {}", id, e);
                            false
                        }
                    }
                })
            },
            move |mins| {
                manager_for_ids.get_expiring_credential_ids(mins.max(refresh_before_mins))
            },
        ) {
            tracing::error!("启动后台刷新任务失败: {}", e);
        }

        *self.background_refresher.lock() = Some(Arc::clone(&refresher));
        refresher
    }

    /// 刷新指定凭据的 Token（带优雅降级）
    ///
    /// 如果刷新失败但现有 Token 仍未过期，返回 fallback 结果继续使用现有 Token。
    pub async fn refresh_token_for_credential(&self, id: u64) -> anyhow::Result<RefreshResult> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据无需刷新
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
                        entry.credentials = new_creds.clone();
                        entry.refresh_failure_count = 0;
                    }
                }
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败: {}", e);
                }
                let expires_at = new_creds.expires_at.unwrap_or_default();
                Ok(RefreshResult::success(id, expires_at))
            }
            Err(e) => {
                if !is_token_expired(&credentials) {
                    let expires_at = credentials.expires_at.unwrap_or_default();
                    tracing::warn!(
                        "凭据 #{} Token 刷新失败，使用现有 Token（优雅降级）: {}",
                        id,
                        e
                    );
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
    fn test_is_token_expired_within_5_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(3);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expired(&credentials));
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
        credentials.kiro_api_key = Some("ksk_test_key_123".to_string());
        credentials.auth_method = Some("api_key".to_string());

        let result = refresh_token(&credentials, &config, None).await;

        assert!(result.is_err(), "API Key 凭据应被 refresh_token 拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("API Key 凭据不支持刷新"),
            "期望错误消息包含 'API Key 凭据不支持刷新'，实际: {}",
            err_msg
        );
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
        api_key_cred.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        let id = result.unwrap();
        assert!(id > 0);
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_api_key() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing_key".to_string());
        existing.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.kiro_api_key = Some("ksk_existing_key".to_string());
        duplicate.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("kiroApiKey 重复"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_empty_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some(String::new());
        cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("kiroApiKey 为空"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_missing_key_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        // kiro_api_key is None

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("缺少 kiroApiKey"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_and_oauth_coexist() {
        let config = Config::default();

        let mut oauth_cred = KiroCredentials::default();
        oauth_cred.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![oauth_cred], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_new_key".to_string());
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
    fn test_multi_token_manager_api_key_missing_kiro_api_key_auto_disabled() {
        let config = Config::default();

        // auth_method=api_key 但缺少 kiro_api_key → 应被自动禁用
        let mut bad_cred = KiroCredentials::default();
        bad_cred.auth_method = Some("api_key".to_string());
        // kiro_api_key 保持 None

        let mut good_cred = KiroCredentials::default();
        good_cred.refresh_token = Some("valid_token".to_string());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 1); // bad_cred 被禁用，只剩 1 个可用
    }

    #[test]
    fn test_multi_token_manager_api_key_with_kiro_api_key_not_disabled() {
        let config = Config::default();

        // auth_method=api_key 且有 kiro_api_key → 不应被禁用
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("ksk_test123".to_string());

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
    async fn test_multi_token_manager_acquire_context_balanced_retries_until_bad_credential_disabled() {
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

        let err = manager.acquire_context(None).await.err().unwrap().to_string();
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

        let err = manager.acquire_context(None).await.err().unwrap().to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
        assert_eq!(manager.available_count(), 0);
    }

    // ============ 凭据级 Region 优先级测试 ============

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
        // 验证 IdC OIDC endpoint URL 使用凭据 auth_region
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
        // 验证 Social refresh endpoint URL 使用凭据 auth_region
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
            let ctx2 = tokio_timeout(
                StdDuration::from_millis(500),
                manager.acquire_context(None),
            )
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
                MultiTokenManager::new(config, vec![make_cred("only")], None, None, false)
                    .unwrap(),
            );

            let ctx1 = manager.acquire_context(None).await.unwrap();
            let id1 = ctx1.id;

            // 后台等待者：因唯一凭据满会进 wait_any_credential
            let manager_clone = Arc::clone(&manager);
            let waiter =
                tokio::spawn(async move { manager_clone.acquire_context(None).await });

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
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("only")],
                None,
                None,
                false,
            )
            .unwrap();

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
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("only")],
                None,
                None,
                false,
            )
            .unwrap();

            {
                let _ctx = manager.acquire_context(None).await.unwrap();
                // 离开作用域 → CallContext drop → permit 归还
            }

            // 下一次应立即可拿（不应等 timeout）
            let _ctx2 = tokio_timeout(
                StdDuration::from_millis(500),
                manager.acquire_context(None),
            )
            .await
            .expect("drop 后 permit 应已归还，acquire 应立即成功")
            .unwrap();
        }

        /// 测试 5：等待中的 acquire future 被取消时，不泄漏 permit
        #[tokio::test]
        async fn test_cancel_safe_no_permit_leak() {
            // 长 timeout，避免触发 sentinel
            let config = make_config(1, 30);
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("only")],
                None,
                None,
                false,
            )
            .unwrap();

            let ctx1 = manager.acquire_context(None).await.unwrap();

            // 用外层 timeout 强制取消等待中的 acquire（模拟客户端断开）
            let cancelled = tokio_timeout(
                StdDuration::from_millis(150),
                manager.acquire_context(None),
            )
            .await;
            assert!(cancelled.is_err(), "应被外层 timeout 取消");

            // 释放第一个 permit
            drop(ctx1);

            // 若取消时泄漏了"幽灵 permit"，这里会再次卡住超时
            let _ctx2 = tokio_timeout(
                StdDuration::from_secs(1),
                manager.acquire_context(None),
            )
            .await
            .expect("取消的 future 必须归还 permit，否则池被永久占满")
            .unwrap();
        }

        /// 测试 6：动态扩缩容 — set_per_credential_concurrency 增加 permit
        #[tokio::test]
        async fn test_dynamic_resize_increase() {
            let config = make_config(1, 2);
            let manager = MultiTokenManager::new(
                config,
                vec![make_cred("only")],
                None,
                None,
                false,
            )
            .unwrap();

            // 当前 per_cred=1，先占满
            let ctx1 = manager.acquire_context(None).await.unwrap();

            // 在线扩到 3：底层 add_permits(2)
            manager.set_per_credential_concurrency(1, 3).unwrap();

            // 应该再拿到 2 个（新增 permit 立即可用）
            let ctx2 = tokio_timeout(
                StdDuration::from_millis(500),
                manager.acquire_context(None),
            )
            .await
            .expect("扩容后第 2 个 permit 应立即可拿")
            .unwrap();
            let ctx3 = tokio_timeout(
                StdDuration::from_millis(500),
                manager.acquire_context(None),
            )
            .await
            .expect("扩容后第 3 个 permit 应立即可拿")
            .unwrap();

            // 第 4 个：超出新上限 3，应进入等待并被外层 timeout 取消
            let r = tokio_timeout(
                StdDuration::from_millis(200),
                manager.acquire_context(None),
            )
            .await;
            assert!(r.is_err(), "已达扩容后上限，第 4 个应等待");

            drop(ctx1);
            drop(ctx2);
            drop(ctx3);
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
}
