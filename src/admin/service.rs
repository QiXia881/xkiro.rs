//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::anthropic::middleware::{PromptCacheRuntime, SharedApiKeys, ThinkingRuntimeConfig};
use crate::common::utf8::floor_char_boundary;
use crate::http_client::ProxyConfig;
use crate::kiro::auth::{idc, kiro_sso, oauth_callback, social};
use crate::kiro::endpoint::{CLI_ENDPOINT_NAME, CODEWHISPERER_ENDPOINT_NAME, IDE_ENDPOINT_NAME};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::{LOW_BALANCE_THRESHOLD, MultiTokenManager, refresh_token};
use crate::model::config::{
    CompressionConfig, PromptFilterConfig, SystemPromptPosition, UserPreset,
};
use crate::model::runtime::SharedPromptConfig;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, ApiKeyEntry, ApiKeyListResponse, BalanceResponse,
    BatchOperationRequest, BatchOperationResponse, BatchOperationResultItem,
    BatchRefreshBalanceResponse, BatchRefreshBalanceResultItem, BatchRefreshResponse,
    BatchRefreshResultItem, CachedBalanceItem, CachedBalancesResponse, CompleteIamSsoLoginRequest,
    CompleteKiroSsoLoginRequest, CompleteKiroSsoLoginResponse, CompleteSocialCallbackRequest,
    CompleteSocialLoginRequest, CompressionConfigResponse, CreateApiKeyRequest,
    CreateApiKeyResponse, CredentialStatusItem, CredentialTestResponse, CredentialsStatusResponse,
    EndpointConfigResponse, ExportKamItem, ExportTokenJsonItem, GenerateMachineIdResponse,
    GlobalConfigResponse, ImportAction, ImportItemResult, ImportSsoTokenRequest,
    ImportSsoTokenResponse, ImportSummary, ImportTokenJsonRequest, ImportTokenJsonResponse,
    KiroGoImportCredentialsRequest, KiroGoProxyConfigResponse, KiroGoUpdateAccountRequest,
    KiroSsoAccountResponse, PollBuilderIdLoginResponse, PollIdcLoginResponse,
    PollKiroSsoLoginResponse, PollSocialLoginResponse, PresetItem, PromptFilterConfigResponse,
    PromptFilterRuleDto, ProxyConfigResponse, RequestLogsResponse, RuntimeBalanceSnapshot,
    RuntimeStatsItem, RuntimeStatsResponse, SettingsResponse, SsoTokenImportResultItem,
    StartBuilderIdLoginRequest, StartBuilderIdLoginResponse, StartIamSsoLoginResponse,
    StartIdcLoginRequest, StartIdcLoginResponse, StartKiroSsoLoginRequest,
    StartKiroSsoLoginResponse, StartSocialLoginRequest, StartSocialLoginResponse, StatsResponse,
    SystemPromptResponse, SystemStatusResponse, ThinkingConfigResponse, TokenJsonItem,
    UpdateApiKeyRequest, UpdateCompressionConfigRequest, UpdateEndpointConfigRequest,
    UpdateGlobalConfigRequest, UpdatePromptFilterConfigRequest, UpdateProxyConfigRequest,
    UpdateSettingsRequest, UpdateSystemPromptRequest, UpdateThinkingConfigRequest,
    UpsertUserPresetRequest, VersionResponse,
};
use crate::kiro::token_manager::CachedBalanceInfo;

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

#[derive(Default)]
struct RefreshBalanceStats {
    success: usize,
    failed: usize,
    low_balance_disabled: usize,
}

/// 计算超额额度剩余：仅当 overage_status=ENABLED 时 > 0
fn overage_remaining(balance: &BalanceResponse) -> f64 {
    if balance.overage_status.as_deref() == Some("ENABLED") {
        let used = (balance.current_usage - balance.usage_limit).max(0.0);
        (balance.overage_cap - used).max(0.0)
    } else {
        0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocialAuthSessionKind {
    Manual,
    Helper,
}

/// Social OAuth 登录进行中的会话状态
struct SocialAuthSession {
    kind: SocialAuthSessionKind,
    auth_endpoint: String,
    state: String,
    code_verifier: String,
    redirect_uri: String,
    expires_at: chrono::DateTime<Utc>,
    cred_template: KiroCredentials,
    proxy: Option<ProxyConfig>,
    // helper 模式：complete 投递最终结果，poll 读取。Ok(id)=成功，Err=失败原因
    helper_result: Option<Result<u64, String>>,
    // helper 模式：complete 进入异步 add_credential 期间置位，避免 poll 在该窗口因
    // 过期而清除会话（会丢失结果），也避免重复 complete 互相覆盖结果。
    helper_completing: bool,
}

struct IdcAuthSession {
    region: String,
    client_id: String,
    client_secret: String,
    device_code: String,
    expires_at: chrono::DateTime<Utc>,
    poll_interval: i64,
    cred_template: KiroCredentials,
    proxy: Option<ProxyConfig>,
}

struct IamSsoCodeAuthSession {
    region: String,
    client_id: String,
    client_secret: String,
    code_verifier: String,
    state: String,
    redirect_uri: String,
    expires_at: chrono::DateTime<Utc>,
    cred_template: KiroCredentials,
    proxy: Option<ProxyConfig>,
}

/// Builder ID 设备授权会话
#[derive(Clone)]
struct BuilderIdAuthSession {
    region: String,
    client_id: String,
    client_secret: String,
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_at: chrono::DateTime<Utc>,
    poll_interval: i64,
    cred_template: KiroCredentials,
    proxy: Option<ProxyConfig>,
}

struct KiroSsoAuthSession {
    callback_rx: tokio::sync::Mutex<tokio::sync::oneshot::Receiver<kiro_sso::KiroSsoCapture>>,
    manual_callback_tx: tokio::sync::mpsc::Sender<kiro_sso::ManualCallbackRequest>,
    expires_at: chrono::DateTime<Utc>,
    cred_template: KiroCredentials,
    proxy: Option<ProxyConfig>,
    _server_handle: Option<kiro_sso::ServerHandle>,
}

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// 缓存的模型列表条目（仅进程内）
#[derive(Debug, Clone)]
struct CachedModels {
    cached_at: std::time::Instant,
    data: crate::kiro::models::ListAvailableModelsResponse,
}

const MODELS_CACHE_TTL_SECS: u64 = 30 * 60;

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    /// Kiro Provider 引用，用于 region/endpoint/global_proxy 热更新双层同步
    kiro_provider: Option<Arc<KiroProvider>>,
    /// 共享压缩配置，与 AppState 同源（运行时热更新）
    compression_config: Arc<RwLock<CompressionConfig>>,
    /// 客户端 API Key 运行时状态
    client_api_key_runtime: Arc<RwLock<String>>,
    /// 客户端 API Key 强制开关运行时状态
    require_api_key_runtime: Arc<std::sync::atomic::AtomicBool>,
    /// Admin 密钥运行时状态
    admin_api_key_runtime: Arc<RwLock<String>>,
    /// 共享提示过滤配置，与 AppState 同源（运行时热更新）
    prompt_filter_config: Arc<RwLock<PromptFilterConfig>>,
    /// 共享 thinking 配置，与 AppState 同源（运行时热更新）
    thinking_config: Arc<RwLock<ThinkingRuntimeConfig>>,
    /// Prompt Cache 运行时（共享引用，支持 ttl/accounting 热更新）
    prompt_cache_runtime: Arc<RwLock<PromptCacheRuntime>>,
    /// 系统提示注入运行时（共享引用，支持热更新）
    prompt_runtime: SharedPromptConfig,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    /// 余额查询并发限流（单条 + 批量共享同一信号量）
    ///
    /// 单条 `fetch_balance` 与批量 `force_refresh_balances_batch` 都从这里获取
    /// 许可，确保系统级别上对单凭据池的余额上游调用并发不会失控。
    /// 容量 8 与历史批量 Semaphore 等价。
    balance_semaphore: Arc<Semaphore>,
    /// 模型列表缓存（仅内存，TTL 30 分钟；按 (id, provider) 区分）
    models_cache: Mutex<HashMap<(u64, Option<String>), CachedModels>>,
    /// 进行中的 Social OAuth 登录会话（session_id → SocialAuthSession）
    social_sessions: Mutex<HashMap<String, SocialAuthSession>>,
    /// 进行中的 AWS IdC 设备授权会话（session_id → IdcAuthSession）
    idc_sessions: Mutex<HashMap<String, IdcAuthSession>>,
    /// 进行中的 Kiro-Go IAM SSO authorization-code 会话
    iam_sso_code_sessions: Mutex<HashMap<String, IamSsoCodeAuthSession>>,
    /// 请求日志和统计
    request_stats: super::stats::SharedRequestStats,
    /// 进行中的 Builder ID 登录会话（session_id → BuilderIdAuthSession）
    builder_id_sessions: Mutex<HashMap<String, BuilderIdAuthSession>>,
    /// 进行中的 Kiro hosted SSO 登录会话（Microsoft 365 / Entra ID）
    kiro_sso_sessions: Mutex<HashMap<String, KiroSsoAuthSession>>,
    /// API Key 列表（多 API Key 系统）
    api_keys: SharedApiKeys,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        kiro_provider: Option<Arc<KiroProvider>>,
        compression_config: Arc<RwLock<CompressionConfig>>,
        client_api_key_runtime: Arc<RwLock<String>>,
        require_api_key_runtime: Arc<std::sync::atomic::AtomicBool>,
        admin_api_key_runtime: Arc<RwLock<String>>,
        prompt_filter_config: Arc<RwLock<PromptFilterConfig>>,
        thinking_config: Arc<RwLock<ThinkingRuntimeConfig>>,
        prompt_cache_runtime: Arc<RwLock<PromptCacheRuntime>>,
        prompt_runtime: SharedPromptConfig,
        api_keys: SharedApiKeys,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        Self {
            token_manager,
            kiro_provider,
            compression_config,
            client_api_key_runtime,
            require_api_key_runtime,
            admin_api_key_runtime,
            prompt_filter_config,
            thinking_config,
            prompt_cache_runtime,
            prompt_runtime,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            balance_semaphore: Arc::new(Semaphore::new(8)),
            models_cache: Mutex::new(HashMap::new()),
            social_sessions: Mutex::new(HashMap::new()),
            idc_sessions: Mutex::new(HashMap::new()),
            iam_sso_code_sessions: Mutex::new(HashMap::new()),
            request_stats: super::stats::create_shared_stats(),
            builder_id_sessions: Mutex::new(HashMap::new()),
            kiro_sso_sessions: Mutex::new(HashMap::new()),
            api_keys,
        }
    }

    /// 获取 token_manager 快照（用于批量操作）
    pub fn token_manager_snapshot(&self) -> crate::kiro::token_manager::ManagerSnapshot {
        self.token_manager.snapshot()
    }

    /// 启动后并行预取所有未禁用凭据的余额，写入 disk-cache
    ///
    /// - 不复用运行时 `balance_semaphore`(cap 8)；启动期无其它流量，
    ///   用独立 `Semaphore(32)` 拉高启动并发，所有未禁用凭据近似同时发出
    /// - 已被磁盘缓存命中且未过期的凭据跳过，避免每次启动都打上游
    /// - 单条失败逐条降级，仅日志告警，不阻塞启动
    pub async fn prefetch_balances_on_startup(self: Arc<Self>) {
        let snapshot = self.token_manager.snapshot();
        let now_ts = Utc::now().timestamp() as f64;

        let stale_ids: Vec<u64> = {
            let cache = self.balance_cache.lock();
            snapshot
                .entries
                .iter()
                .filter(|e| !e.disabled)
                .filter_map(|e| {
                    let fresh = cache
                        .get(&e.id)
                        .map(|c| (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64)
                        .unwrap_or(false);
                    if fresh { None } else { Some(e.id) }
                })
                .collect()
        };

        if stale_ids.is_empty() {
            tracing::info!("启动余额预取：磁盘缓存命中所有凭据，跳过");
            return;
        }

        let total = stale_ids.len();
        tracing::info!("启动余额预取：{} 个凭据并行获取（cap=32）", total);
        let stats = self.refresh_balances_concurrent(stale_ids, 32).await;

        tracing::info!(
            "启动余额预取完成：成功 {}，失败 {}，低余额禁用 {}（共 {}）",
            stats.success,
            stats.failed,
            stats.low_balance_disabled,
            total
        );
    }

    /// 启动周期性余额刷新任务
    ///
    /// 每轮读 token_manager.config 的 `balance_refresh_*` 字段（支持热更新）：
    /// - `enabled=false`：跳过本轮拉取，仅 sleep 1 个 tick 后再读
    /// - `interval_secs`：触发周期（最小 180s，外部已 clamp）
    /// - `concurrency`：每批并发上限；凭据数 > 此值时 chunks 顺序分批，
    ///   上一批完成才进入下一批（避免单轮内同时占用过多上游连接）
    ///
    /// 写回 admin disk-cache + token_manager 运行时缓存。低余额自动禁用。
    /// 与启动预取共享 `refresh_balances_concurrent`，复用上游限流。
    pub fn start_periodic_balance_refresh(self: Arc<Self>) {
        tokio::spawn(async move {
            tracing::info!(
                "余额定时刷新已启动: 当前 enabled={}, 间隔={}s, 并发={}",
                self.token_manager.config().balance_refresh_enabled,
                self.token_manager.config().balance_refresh_interval_secs,
                self.token_manager.config().balance_refresh_concurrency,
            );
            // 启动后跳过首轮（启动预取已处理），先 sleep 一轮再开始
            let mut next_sleep = self
                .token_manager
                .config()
                .balance_refresh_interval_secs
                .max(crate::model::config::MIN_BALANCE_REFRESH_INTERVAL_SECS);
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(next_sleep)).await;

                let cfg = self.token_manager.config();
                next_sleep = cfg
                    .balance_refresh_interval_secs
                    .max(crate::model::config::MIN_BALANCE_REFRESH_INTERVAL_SECS);

                if !cfg.balance_refresh_enabled {
                    tracing::debug!("余额定时刷新：已禁用，跳过本轮");
                    continue;
                }
                let concurrency = cfg
                    .balance_refresh_concurrency
                    .clamp(1, crate::model::config::MAX_BALANCE_REFRESH_CONCURRENCY);
                drop(cfg);

                let snapshot = self.token_manager.snapshot();
                let active_ids: Vec<u64> = snapshot
                    .entries
                    .iter()
                    .filter(|e| !e.disabled)
                    .map(|e| e.id)
                    .collect();
                if active_ids.is_empty() {
                    tracing::debug!("余额定时刷新：无活跃凭据");
                    continue;
                }
                let total = active_ids.len();
                // 凭据数 > concurrency 时分批，上一批完成才进入下一批
                let mut agg = RefreshBalanceStats::default();
                for chunk in active_ids.chunks(concurrency) {
                    let stats = self
                        .refresh_balances_concurrent(chunk.to_vec(), concurrency)
                        .await;
                    agg.success += stats.success;
                    agg.failed += stats.failed;
                    agg.low_balance_disabled += stats.low_balance_disabled;
                }
                tracing::info!(
                    "余额定时刷新完成：成功 {}，失败 {}，低余额禁用 {}（共 {}, 批大小 {}）",
                    agg.success,
                    agg.failed,
                    agg.low_balance_disabled,
                    total,
                    concurrency,
                );
            }
        });
    }

    /// 并发刷新指定凭据列表的余额
    ///
    /// - 用 `Semaphore(concurrency)` 限并发；每个成功项写回两层 cache 并同步运行时调度器
    /// - 余额低于 `LOW_BALANCE_THRESHOLD` 自动禁用
    /// - 完成后一次性 `save_balance_cache`
    async fn refresh_balances_concurrent(
        self: &Arc<Self>,
        ids: Vec<u64>,
        concurrency: usize,
    ) -> RefreshBalanceStats {
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut tasks: JoinSet<(u64, Option<BalanceResponse>, Option<String>)> = JoinSet::new();

        for id in ids {
            let token_manager = self.token_manager.clone();
            let sem = sem.clone();
            tasks.spawn(async move {
                let _permit = match sem.acquire().await {
                    Ok(p) => p,
                    Err(e) => return (id, None, Some(format!("acquire semaphore: {e}"))),
                };
                match token_manager.get_usage_limits_for(id).await {
                    Ok(usage) => {
                        let current_usage = usage.current_usage();
                        let usage_limit = usage.usage_limit();
                        let remaining = (usage_limit - current_usage).max(0.0);
                        let usage_percentage = if usage_limit > 0.0 {
                            (current_usage / usage_limit * 100.0).min(100.0)
                        } else {
                            0.0
                        };
                        let resp = BalanceResponse {
                            id,
                            subscription_title: usage.subscription_title().map(|s| s.to_string()),
                            current_usage,
                            usage_limit,
                            remaining,
                            usage_percentage,
                            next_reset_at: usage.next_date_reset,
                            overage_cap: usage.overage_cap(),
                            overage_capability: usage.overage_capability().map(|s| s.to_string()),
                            overage_status: usage.overage_status().map(|s| s.to_string()),
                        };
                        (id, Some(resp), None)
                    }
                    Err(e) => (id, None, Some(e.to_string())),
                }
            });
        }

        let mut stats = RefreshBalanceStats::default();
        let cache_now = Utc::now().timestamp() as f64;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((id, Some(balance), _)) => {
                    let remaining = balance.remaining;
                    let overage_rem = overage_remaining(&balance);
                    {
                        let mut cache = self.balance_cache.lock();
                        cache.insert(
                            id,
                            CachedBalance {
                                cached_at: cache_now,
                                data: balance,
                            },
                        );
                    }
                    self.token_manager
                        .update_balance_cache_full(id, remaining, overage_rem);
                    // 真正不可用 = 正式额度耗尽 AND（超额未开启 OR 超额额度耗尽）
                    let exhausted =
                        remaining < LOW_BALANCE_THRESHOLD && overage_rem < LOW_BALANCE_THRESHOLD;
                    if exhausted {
                        if self.token_manager.mark_insufficient_balance(id) {
                            stats.low_balance_disabled += 1;
                            tracing::warn!(
                                "凭据 #{} 额度耗尽（正式 {:.2}, 超额 remaining={:.2}），已自动禁用",
                                id,
                                remaining,
                                overage_rem
                            );
                        }
                    } else {
                        tracing::debug!(
                            "凭据 #{} 余额已刷新: 正式 {:.2}, 超额 remaining={:.2}",
                            id,
                            remaining,
                            overage_rem
                        );
                    }
                    stats.success += 1;
                }
                Ok((id, None, err)) => {
                    stats.failed += 1;
                    tracing::warn!(
                        "余额刷新失败 #{}: {}",
                        id,
                        err.unwrap_or_else(|| "unknown".to_string())
                    );
                }
                Err(e) => {
                    stats.failed += 1;
                    tracing::warn!("余额刷新 task join 失败: {}", e);
                }
            }
        }
        self.save_balance_cache();
        stats
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                weight: entry.weight,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
                available_permits: entry.available_permits,
                max_permits: entry.max_permits,
                concurrency: entry.concurrency,
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            credentials,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    pub fn update_kiro_go_account(
        &self,
        id: u64,
        req: KiroGoUpdateAccountRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .update_credential_kiro_go_fields(
                id,
                req.enabled,
                req.weight,
                req.proxy_url.map(|v| {
                    let trimmed = v.trim().to_string();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed)
                    }
                }),
            )
            .map_err(|e| self.classify_error(e, id))
    }

    /// 设置单凭据独立并发上限（None=回退到全局 per_credential_concurrency）
    pub fn set_concurrency(
        &self,
        id: u64,
        concurrency: Option<u32>,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_credential_concurrency(id, concurrency)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(
        &self,
        id: u64,
        force: bool,
    ) -> Result<BalanceResponse, AdminServiceError> {
        // force=true 跳过缓存，直接走云端
        if !force {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新 admin 端展示缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        // 同步调度器运行时余额缓存（rank_candidates 派送依据）
        self.token_manager.update_balance_cache_full(
            id,
            balance.remaining,
            overage_remaining(&balance),
        );

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    ///
    /// 异步队列设计：
    /// 1. 先查 snapshot：disabled 凭据直接返回 `InvalidCredential`，不进队列
    /// 2. 通过 `balance_semaphore` 限流（与批量刷新共享，全局并发上限 8）
    /// 3. 拿到 permit 后调用 `get_usage_limits_for`；permit 在函数返回时自动释放
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // disabled 凭据快速失败：不占用队列槽位
        let snapshot = self.token_manager.snapshot();
        if let Some(entry) = snapshot.entries.iter().find(|e| e.id == id) {
            if entry.disabled {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "credential {id} disabled"
                )));
            }
        } else {
            return Err(AdminServiceError::NotFound { id });
        }

        // 进入余额查询队列：拿到 permit 才能继续，超出并发的请求在此排队
        let _permit = self
            .balance_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| {
                AdminServiceError::InternalError(format!("acquire balance semaphore failed: {e}"))
            })?;

        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
            overage_cap: usage.overage_cap(),
            overage_capability: usage.overage_capability().map(|s| s.to_string()),
            overage_status: usage.overage_status().map(|s| s.to_string()),
        })
    }

    /// 拉取指定凭据可用模型列表（30 分钟内存缓存；force=true 跳过缓存）
    ///
    /// API Key 凭据 / 不存在 / 被禁用 → InvalidCredential / NotFound；
    /// 上游 401 / 403 → UpstreamError，由前端展示。
    pub async fn list_available_models(
        &self,
        id: u64,
        model_provider: Option<&str>,
        force: bool,
    ) -> Result<crate::kiro::models::ListAvailableModelsResponse, AdminServiceError> {
        {
            let snapshot = self.token_manager.snapshot();
            let entry = snapshot
                .entries
                .iter()
                .find(|e| e.id == id)
                .ok_or(AdminServiceError::NotFound { id })?;
            if entry.disabled {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "credential {id} disabled"
                )));
            }
        }

        let key = (id, model_provider.map(str::to_string));
        if !force {
            if let Some(cached) = self.models_cache.lock().get(&key) {
                if cached.cached_at.elapsed().as_secs() < MODELS_CACHE_TTL_SECS {
                    return Ok(cached.data.clone());
                }
            }
        }

        let response = self
            .token_manager
            .list_available_models_for(id, model_provider)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;
        self.token_manager.set_model_list(
            id,
            response
                .available_models
                .iter()
                .map(|model| model.model_id.clone()),
        );

        self.models_cache.lock().insert(
            key,
            CachedModels {
                cached_at: std::time::Instant::now(),
                data: response.clone(),
            },
        );
        Ok(response)
    }

    /// 获取所有凭据的缓存余额
    ///
    /// 双源合并：
    /// - `token_manager` 提供运行时缓存（cached_at + 动态 ttl_secs）
    /// - `AdminService` 自身的 disk-backed 5 分钟缓存提供完整快照（usage_limit /
    ///   usage_percentage / subscription_title），保证字段一致性
    pub fn get_cached_balances(&self) -> CachedBalancesResponse {
        // 从 token_manager 获取运行时缓存（含 TTL 信息）
        let runtime_balances: HashMap<u64, CachedBalanceInfo> = self
            .token_manager
            .get_all_cached_balances()
            .into_iter()
            .map(|info| (info.id, info))
            .collect();

        // 以 entries 为基准遍历，磁盘缓存提供完整数据，运行时缓存提供 cached_at/ttl
        // 任一来源命中即返回该凭据的余额条目
        let snapshot_ids: Vec<u64> = self
            .token_manager
            .snapshot()
            .entries
            .iter()
            .map(|e| e.id)
            .collect();

        let disk_cache = self.balance_cache.lock();
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let balances = snapshot_ids
            .into_iter()
            .filter_map(|id| {
                let runtime = runtime_balances.get(&id);
                let disk = disk_cache.get(&id);
                if runtime.is_none() && disk.is_none() {
                    return None;
                }

                // cached_at 优先用 runtime，其次用 disk 自带时间戳
                let (cached_at, ttl_secs) = match runtime {
                    Some(r) => (r.cached_at, r.ttl_secs),
                    None => {
                        let disk_at_ms = disk
                            .map(|d| (d.cached_at * 1000.0) as u64)
                            .unwrap_or(now_unix_ms);
                        // 启动后磁盘命中但 runtime 缺失：用 BALANCE_CACHE_TTL_SECS 兜底
                        (disk_at_ms, BALANCE_CACHE_TTL_SECS as u64)
                    }
                };

                let item = if let Some(cached) = disk {
                    CachedBalanceItem {
                        id,
                        current_usage: cached.data.current_usage,
                        usage_limit: cached.data.usage_limit,
                        remaining: cached.data.remaining,
                        usage_percentage: cached.data.usage_percentage,
                        subscription_title: cached.data.subscription_title.clone(),
                        next_reset_at: cached.data.next_reset_at,
                        overage_cap: cached.data.overage_cap,
                        overage_capability: cached.data.overage_capability.clone(),
                        overage_status: cached.data.overage_status.clone(),
                        cached_at,
                        ttl_secs,
                    }
                } else {
                    let r = runtime.unwrap();
                    CachedBalanceItem {
                        id,
                        current_usage: 0.0,
                        usage_limit: 0.0,
                        remaining: r.remaining,
                        usage_percentage: 0.0,
                        subscription_title: None,
                        next_reset_at: None,
                        overage_cap: 0.0,
                        overage_capability: None,
                        overage_status: None,
                        cached_at,
                        ttl_secs,
                    }
                };
                Some(item)
            })
            .collect();

        CachedBalancesResponse { balances }
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            provider: None,
            user_id: None,
            client_id: req.client_id,
            client_secret: req.client_secret,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            start_url: None,
            client_id_hash: None,
            id_token: None,
            sso_session_id: None,
            priority: req.priority,
            weight: req.weight,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            overage_status: None,
            legacy_allow_overage: false,
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
            concurrency: req.concurrency,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    pub async fn import_kiro_go_credential(
        &self,
        req: KiroGoImportCredentialsRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        let import_refresh_token =
            Self::clean_import_string(Some(req.refresh_token)).ok_or_else(|| {
                AdminServiceError::InvalidRequest("refreshToken is required".to_string())
            })?;
        let access_token = Self::clean_import_string(req.access_token);
        let client_id = Self::clean_import_string(req.client_id);
        let client_secret = Self::clean_import_string(req.client_secret);
        let provider = Self::clean_import_string(req.provider);
        let mut token_endpoint = Self::clean_import_string(req.token_endpoint);
        let mut issuer_url = Self::clean_import_string(req.issuer_url);
        let mut scopes = Self::clean_import_string(req.scopes);
        let start_url = Self::clean_import_string(req.start_url);
        let client_id_hash = Self::clean_import_string(req.client_id_hash);
        let id_token = Self::clean_import_string(req.id_token);
        let sso_session_id = Self::clean_import_string(req.sso_session_id);
        let region =
            Self::clean_import_string(req.region).unwrap_or_else(|| "us-east-1".to_string());
        let auth_region = Self::clean_import_string(req.auth_region);
        let api_region = Self::clean_import_string(req.api_region);
        let user_id = Self::clean_import_string(req.user_id);
        let mut email = Self::clean_import_string(req.email);
        let profile_arn = Self::clean_import_string(req.profile_arn);
        let proxy_url = Self::clean_import_string(req.proxy_url);
        let proxy_username = Self::clean_import_string(req.proxy_username);
        let proxy_password = Self::clean_import_string(req.proxy_password);
        let overage_status = Self::clean_import_string(req.overage_status);
        let endpoint = Self::clean_import_string(req.endpoint);
        if let Some(ref name) = endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }
        let preferred_id = Self::parse_kiro_go_import_id(req.id.as_ref());
        let disabled = Self::kiro_go_import_disabled(req.disabled, req.enabled);
        let concurrency = req.concurrency.filter(|value| *value > 0);

        let mut auth_method = Self::normalize_kiro_go_import_auth_method(
            req.auth_method.as_deref(),
            client_id.as_deref(),
            client_secret.as_deref(),
            token_endpoint.as_deref(),
        );

        let derived = kiro_sso::derive_external_idp_endpoints(
            user_id.as_deref().unwrap_or_default(),
            client_id.as_deref().unwrap_or_default(),
            access_token.as_deref().unwrap_or_default(),
        );
        if let Some((derived_endpoint, _, _)) = derived.as_ref()
            && kiro_sso::validate_external_idp_endpoint(derived_endpoint).is_ok()
            && auth_method != "external_idp"
        {
            auth_method = "external_idp".to_string();
        }

        if auth_method == "external_idp" {
            if let Some((derived_endpoint, derived_issuer, derived_scopes)) = derived {
                if token_endpoint.is_none() {
                    token_endpoint = Some(derived_endpoint);
                }
                if issuer_url.is_none() {
                    issuer_url = Some(derived_issuer);
                }
                if scopes.is_none() {
                    scopes = Some(derived_scopes);
                }
            }
            if client_id.is_none() || token_endpoint.is_none() {
                return Err(AdminServiceError::InvalidCredential(
                    "external_idp requires clientId and tokenEndpoint (or userId/accessToken to derive it)".to_string(),
                ));
            }
            if let Some(endpoint) = token_endpoint.as_deref() {
                kiro_sso::validate_external_idp_endpoint(endpoint).map_err(|e| {
                    AdminServiceError::InvalidCredential(format!(
                        "external IdP endpoint rejected: {}",
                        e
                    ))
                })?;
            }
            if let Some(issuer) = issuer_url.as_deref() {
                kiro_sso::validate_external_idp_endpoint(issuer).map_err(|e| {
                    AdminServiceError::InvalidCredential(format!(
                        "external IdP issuer rejected: {}",
                        e
                    ))
                })?;
            }
        }

        let mut credential = KiroCredentials {
            id: preferred_id,
            access_token: None,
            refresh_token: Some(import_refresh_token),
            profile_arn,
            expires_at: None,
            auth_method: Some(auth_method.clone()),
            provider,
            user_id: user_id.clone(),
            client_id,
            client_secret,
            token_endpoint,
            issuer_url,
            scopes,
            start_url,
            client_id_hash,
            id_token,
            sso_session_id,
            priority: req.priority,
            weight: req.weight,
            concurrency,
            region: Some(region),
            auth_region,
            api_region,
            machine_id: Self::clean_import_string(req.machine_id),
            email,
            subscription_title: None,
            overage_status,
            legacy_allow_overage: false,
            proxy_url,
            proxy_username,
            proxy_password,
            disabled,
            kiro_api_key: None,
            endpoint,
        };

        let mut trusted_on_import = false;
        if auth_method == "external_idp"
            && let Some(access_token) = access_token
            && let Some(exp) = kiro_sso::exp_from_access_token_jwt(&access_token)
            && exp > 0
        {
            credential.access_token = Some(access_token);
            trusted_on_import = true;
            if let Some(expires_at) = chrono::DateTime::<Utc>::from_timestamp(exp, 0) {
                credential.expires_at = Some(expires_at.to_rfc3339());
            }
        }

        if credential.access_token.is_none() {
            let config = self.token_manager.config();
            let global_proxy = config.proxy_url.as_deref().map(|url| {
                let proxy = ProxyConfig::new(url);
                match (&config.proxy_username, &config.proxy_password) {
                    (Some(username), Some(password)) => proxy.with_auth(username, password),
                    _ => proxy,
                }
            });
            let effective_proxy = credential.effective_proxy(global_proxy.as_ref());
            credential = refresh_token(&credential, &config, effective_proxy.as_ref())
                .await
                .map_err(|e| {
                    AdminServiceError::InvalidCredential(format!("Token refresh failed: {}", e))
                })?;
        }

        if credential.email.is_none()
            && let Some(access_token) = credential.access_token.as_deref()
        {
            credential.email = kiro_sso::extract_email_from_jwt(access_token);
        }
        email = credential.email.clone();

        let credential_id = self
            .token_manager
            .add_prevalidated_credential(credential)
            .map_err(|e| self.classify_add_error(e))?;

        if !disabled
            && !trusted_on_import
            && let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await
        {
            tracing::warn!(
                "导入 Kiro-Go 凭据后获取订阅等级失败（不影响凭据添加）: {}",
                e
            );
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    fn clean_import_string(value: Option<String>) -> Option<String> {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn normalize_kiro_go_import_auth_method(
        auth_method: Option<&str>,
        client_id: Option<&str>,
        client_secret: Option<&str>,
        token_endpoint: Option<&str>,
    ) -> String {
        let method = auth_method
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase();
        match method.as_str() {
            "external_idp" | "azuread" | "azure" | "entra" | "entra-id" | "entra_id"
            | "microsoft" | "m365" | "office365" | "external" => "external_idp".to_string(),
            _ if token_endpoint.is_some() => "external_idp".to_string(),
            "social" | "google" | "github" => "social".to_string(),
            "idc" | "builderid" | "builder-id" | "enterprise" | "iam" => "idc".to_string(),
            "" if client_id.is_some() => "idc".to_string(),
            "" => "social".to_string(),
            _ if client_id.is_some() && client_secret.is_some() => "idc".to_string(),
            _ => "social".to_string(),
        }
    }

    fn parse_kiro_go_import_id(value: Option<&serde_json::Value>) -> Option<u64> {
        match value {
            Some(serde_json::Value::Number(number)) => number.as_u64().filter(|id| *id > 0),
            Some(serde_json::Value::String(value)) => {
                value.trim().parse::<u64>().ok().filter(|id| *id > 0)
            }
            _ => None,
        }
    }

    fn kiro_go_import_disabled(disabled: Option<bool>, enabled: Option<bool>) -> bool {
        disabled.unwrap_or_else(|| enabled.map(|value| !value).unwrap_or(false))
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 切换上游 overage 开关（调用 Kiro `setUserPreference`）
    ///
    /// 上游会自行校验资格（INCAPABLE 订阅会返回 4xx）。这里直接透传上游错误，
    /// 不在 admin 侧做资格预检——避免和余额缓存 TTL/未刷新的状态产生不一致。
    pub async fn set_overage_status(
        &self,
        id: u64,
        enabled: bool,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_overage_status_for(id, enabled)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        // 1) 清磁盘缓存：避免后续 get_balance 走 TTL 命中老值
        let removed = {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id).is_some()
        };
        if removed {
            self.save_balance_cache();
        }
        // 2) 清 token_manager 运行时缓存（标记 initialized=false）
        self.token_manager.invalidate_balance_cache(id);

        // 3) 主动拉新值回填两个 cache：overage 开关会改 cap / overage_status，
        //    调度器和前端都需要尽快看到最新值，不能等下一次 get_balance 触发
        match self.fetch_balance(id).await {
            Ok(balance) => {
                {
                    let mut cache = self.balance_cache.lock();
                    cache.insert(
                        id,
                        CachedBalance {
                            cached_at: Utc::now().timestamp() as f64,
                            data: balance.clone(),
                        },
                    );
                }
                self.save_balance_cache();
                self.token_manager.update_balance_cache_full(
                    id,
                    balance.remaining,
                    overage_remaining(&balance),
                );
            }
            Err(e) => {
                // 拉新失败不阻塞 overage 切换成功的语义；下一轮 should_refresh_balance 会兜底
                tracing::warn!("overage 切换后拉新余额失败 #{}: {}", id, e);
            }
        }

        Ok(())
    }

    /// 轻量运行时状态快照（高频轮询用，纯内存读取，不触发任何 IO）
    ///
    /// 字段精简至 dashboard 实时需要的 5 项：
    /// - `id`：凭据主键
    /// - `last_used_at`：最近一次被选中的时间戳（RFC3339）
    /// - `available_permits` / `max_permits`：用于渲染 K/N 并发占用
    /// - `disabled`：手动禁用标记
    pub fn get_runtime_stats(&self) -> RuntimeStatsResponse {
        let snapshot = self.token_manager.snapshot();
        let disk_cache = self.balance_cache.lock();
        let credentials = snapshot
            .entries
            .into_iter()
            .map(|entry| {
                let balance = disk_cache
                    .get(&entry.id)
                    .map(|cached| RuntimeBalanceSnapshot {
                        subscription_title: cached.data.subscription_title.clone(),
                        current_usage: cached.data.current_usage,
                        usage_limit: cached.data.usage_limit,
                        remaining: cached.data.remaining,
                        usage_percentage: cached.data.usage_percentage,
                        next_reset_at: cached.data.next_reset_at,
                        overage_cap: cached.data.overage_cap,
                        overage_capability: cached.data.overage_capability.clone(),
                        overage_status: cached.data.overage_status.clone(),
                    });
                RuntimeStatsItem {
                    id: entry.id,
                    last_used_at: entry.last_used_at.clone(),
                    available_permits: entry.available_permits,
                    max_permits: entry.max_permits,
                    disabled: entry.disabled,
                    balance,
                }
            })
            .collect();
        RuntimeStatsResponse { credentials }
    }

    /// 批量强制刷新 Token（B 端点）
    ///
    /// 用 `Semaphore(8)` 限制并发，`JoinSet` 收集结果。
    /// 单个失败不影响其他凭据，全部完成后返回 `BatchRefreshResponse`。
    /// 内部调用 `force_refresh_token_for(id)`，对 API Key 凭据会 `bail` 走 Err 分支。
    pub async fn force_refresh_tokens_batch(&self, ids: Vec<u64>) -> BatchRefreshResponse {
        // 源头过滤：禁用的凭据直接跳过刷新，不占用并发槽位
        let snapshot = self.token_manager.snapshot();
        let disabled_ids: HashSet<u64> = snapshot
            .entries
            .iter()
            .filter(|e| e.disabled)
            .map(|e| e.id)
            .collect();
        let (active_ids, skipped_ids): (Vec<u64>, Vec<u64>) =
            ids.into_iter().partition(|id| !disabled_ids.contains(id));

        let semaphore = Arc::new(Semaphore::new(8));
        let mut tasks: JoinSet<BatchRefreshResultItem> = JoinSet::new();

        for id in active_ids {
            let token_manager = self.token_manager.clone();
            let semaphore = semaphore.clone();
            tasks.spawn(async move {
                // 获取并发许可（最多 8 个并发刷新）
                let _permit = match semaphore.acquire().await {
                    Ok(p) => p,
                    Err(e) => {
                        return BatchRefreshResultItem {
                            id,
                            success: false,
                            error: Some(format!("acquire semaphore failed: {e}")),
                        };
                    }
                };
                match token_manager.force_refresh_token_for(id).await {
                    Ok(()) => BatchRefreshResultItem {
                        id,
                        success: true,
                        error: None,
                    },
                    Err(e) => BatchRefreshResultItem {
                        id,
                        success: false,
                        error: Some(e.to_string()),
                    },
                }
            });
        }

        let mut results = Vec::new();
        let mut success_count = 0usize;
        let mut failure_count = skipped_ids.len();
        // 禁用条目直接构造失败项加入结果集
        for id in skipped_ids {
            results.push(BatchRefreshResultItem {
                id,
                success: false,
                error: Some("credential disabled".to_string()),
            });
        }
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(item) => {
                    if item.success {
                        success_count += 1;
                    } else {
                        failure_count += 1;
                    }
                    results.push(item);
                }
                Err(e) => {
                    failure_count += 1;
                    results.push(BatchRefreshResultItem {
                        id: 0,
                        success: false,
                        error: Some(format!("task join error: {e}")),
                    });
                }
            }
        }
        // 按 id 升序便于前端展示
        results.sort_by_key(|r| r.id);

        BatchRefreshResponse {
            results,
            success_count,
            failure_count,
        }
    }

    /// 批量强制刷新余额（不入缓存）
    ///
    /// 用 `Semaphore(8)` 限制并发，`JoinSet` 收集结果。
    /// 单个失败不影响其他凭据，全部完成后返回 `BatchRefreshBalanceResponse`。
    /// 内部直接调用 `token_manager.get_usage_limits_for(id)` 获取最新值，
    /// 不写入余额缓存（与单条 force-refresh 余额端点不同，避免大批量回写抖动）。
    pub async fn force_refresh_balances_batch(&self, ids: Vec<u64>) -> BatchRefreshBalanceResponse {
        // 源头过滤：禁用的凭据直接跳过查询，不占用并发槽位
        let snapshot = self.token_manager.snapshot();
        let disabled_ids: HashSet<u64> = snapshot
            .entries
            .iter()
            .filter(|e| e.disabled)
            .map(|e| e.id)
            .collect();
        let (active_ids, skipped_ids): (Vec<u64>, Vec<u64>) =
            ids.into_iter().partition(|id| !disabled_ids.contains(id));

        // 与单条 fetch_balance 共享同一个全局余额查询 Semaphore（容量 8），
        // 避免批量刷新与零散查询互相抢占
        let semaphore = self.balance_semaphore.clone();
        let mut tasks: JoinSet<BatchRefreshBalanceResultItem> = JoinSet::new();

        for id in active_ids {
            let token_manager = self.token_manager.clone();
            let semaphore = semaphore.clone();
            tasks.spawn(async move {
                // 获取并发许可（最多 8 个并发查询）
                let _permit = match semaphore.acquire().await {
                    Ok(p) => p,
                    Err(e) => {
                        return BatchRefreshBalanceResultItem {
                            id,
                            success: false,
                            balance: None,
                            error: Some(format!("acquire semaphore failed: {e}")),
                        };
                    }
                };
                match token_manager.get_usage_limits_for(id).await {
                    Ok(usage) => {
                        // 字段聚合复用 UsageLimitsResponse 的便捷方法，
                        // 公式与 AdminService::fetch_balance 保持一致
                        let current_usage = usage.current_usage();
                        let usage_limit = usage.usage_limit();
                        let remaining = (usage_limit - current_usage).max(0.0);
                        let usage_percentage = if usage_limit > 0.0 {
                            (current_usage / usage_limit * 100.0).min(100.0)
                        } else {
                            0.0
                        };
                        BatchRefreshBalanceResultItem {
                            id,
                            success: true,
                            balance: Some(BalanceResponse {
                                id,
                                subscription_title: usage
                                    .subscription_title()
                                    .map(|s| s.to_string()),
                                current_usage,
                                usage_limit,
                                remaining,
                                usage_percentage,
                                next_reset_at: usage.next_date_reset,
                                overage_cap: usage.overage_cap(),
                                overage_capability: usage
                                    .overage_capability()
                                    .map(|s| s.to_string()),
                                overage_status: usage.overage_status().map(|s| s.to_string()),
                            }),
                            error: None,
                        }
                    }
                    Err(e) => BatchRefreshBalanceResultItem {
                        id,
                        success: false,
                        balance: None,
                        error: Some(e.to_string()),
                    },
                }
            });
        }

        let mut results = Vec::new();
        let mut success_count = 0usize;
        let mut failure_count = skipped_ids.len();
        // 禁用条目直接构造失败项加入结果集
        for id in skipped_ids {
            results.push(BatchRefreshBalanceResultItem {
                id,
                success: false,
                balance: None,
                error: Some("credential disabled".to_string()),
            });
        }
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(item) => {
                    if item.success {
                        success_count += 1;
                    } else {
                        failure_count += 1;
                    }
                    results.push(item);
                }
                Err(e) => {
                    failure_count += 1;
                    results.push(BatchRefreshBalanceResultItem {
                        id: 0,
                        success: false,
                        balance: None,
                        error: Some(format!("task join error: {e}")),
                    });
                }
            }
        }
        // 按 id 升序便于前端展示
        results.sort_by_key(|r| r.id);

        // 批量成功项写回磁盘缓存（与单条 force-refresh 余额端点一致），
        // 让启动后预取与 GET /balances/cached 始终能读到最新快照
        let now_ts = Utc::now().timestamp() as f64;
        {
            let mut cache = self.balance_cache.lock();
            for item in &results {
                if let (true, Some(balance)) = (item.success, item.balance.as_ref()) {
                    cache.insert(
                        item.id,
                        CachedBalance {
                            cached_at: now_ts,
                            data: balance.clone(),
                        },
                    );
                }
            }
        }
        self.save_balance_cache();

        // 同步调度器运行时余额缓存（rank_candidates 派送依据）
        for item in &results {
            if let (true, Some(balance)) = (item.success, item.balance.as_ref()) {
                self.token_manager.update_balance_cache_full(
                    item.id,
                    balance.remaining,
                    overage_remaining(balance),
                );
            }
        }

        BatchRefreshBalanceResponse {
            results,
            success_count,
            failure_count,
        }
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 序列化在锁内完成（CPU-only，微秒级），IO 在锁释放后执行，
        // 避免 rename syscall 在容器/NFS 存储抖动时长时间持有 Mutex
        let json = {
            let cache = self.balance_cache.lock();
            let map: HashMap<String, &CachedBalance> =
                cache.iter().map(|(k, v)| (k.to_string(), v)).collect();
            match serde_json::to_string_pretty(&map) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!("序列化余额缓存失败: {}", e);
                    return;
                }
            }
        }; // MutexGuard 在此释放

        if let Err(e) = crate::common::io::atomic_write_string(path, &json) {
            tracing::warn!("保存余额缓存失败: {}", e);
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据")
        {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    // ============ 全局代理配置（热更新） ============

    /// 设置凭据 Region（凭据级 region/api_region 覆盖）
    pub fn set_region(
        &self,
        id: u64,
        region: Option<String>,
        api_region: Option<String>,
    ) -> Result<(), AdminServiceError> {
        let region = region
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let api_region = api_region
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.token_manager
            .set_region(id, region, api_region)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 设置凭据 endpoint（凭据级 endpoint 覆盖，须命中已注册端点）
    pub fn set_endpoint(&self, id: u64, endpoint: Option<String>) -> Result<(), AdminServiceError> {
        let endpoint = endpoint
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if let Some(name) = endpoint.as_deref()
            && !self.known_endpoints.contains(name)
        {
            let mut known: Vec<&str> = self.known_endpoints.iter().map(|s| s.as_str()).collect();
            known.sort_unstable();
            return Err(AdminServiceError::InvalidCredential(format!(
                "endpoint 必须是已注册值，已注册: {:?}，收到: {}",
                known, name
            )));
        }

        self.token_manager
            .set_endpoint(id, endpoint)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取当前代理配置（脱敏）
    pub fn get_proxy_config(&self) -> ProxyConfigResponse {
        let config = self.token_manager.config();
        ProxyConfigResponse {
            proxy_url: config.proxy_url.clone(),
            has_credentials: config.proxy_username.is_some() && config.proxy_password.is_some(),
        }
    }

    /// 更新代理配置（热更新）
    pub async fn update_proxy_config(
        &self,
        req: UpdateProxyConfigRequest,
    ) -> Result<(), AdminServiceError> {
        // 1. 构建新的 ProxyConfig
        let new_proxy = if let Some(url) = &req.proxy_url {
            if url.trim().is_empty() {
                None
            } else {
                let mut proxy = ProxyConfig::new(url.trim());
                if let (Some(u), Some(p)) = (&req.proxy_username, &req.proxy_password)
                    && !u.trim().is_empty()
                    && !p.trim().is_empty()
                {
                    proxy = proxy.with_auth(u.trim(), p.trim());
                }
                // 如果未提供新认证信息，保留现有认证
                if proxy.username.is_none() {
                    let config = self.token_manager.config();
                    if let (Some(u), Some(p)) = (&config.proxy_username, &config.proxy_password) {
                        proxy = proxy.with_auth(u, p);
                    }
                }
                Some(proxy)
            }
        } else {
            None
        };

        // 2. 先持久化配置（失败时不影响运行时状态）
        self.token_manager.with_config_mut(|cfg| {
            cfg.proxy_url = new_proxy.as_ref().map(|p| p.url.clone());
            cfg.proxy_username = new_proxy.as_ref().and_then(|p| p.username.clone());
            cfg.proxy_password = new_proxy.as_ref().and_then(|p| p.password.clone());
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        // 3. 持久化成功后再应用运行时变更
        // 贴合 BK admin/service.rs:785-808：先 token_manager 后 provider 双层同步
        self.token_manager.update_proxy(new_proxy.clone());
        if let Some(provider) = &self.kiro_provider {
            if let Err(e) = provider.update_global_proxy(new_proxy) {
                tracing::warn!("provider.update_global_proxy 失败（已持久化）: {}", e);
            }
        }

        Ok(())
    }

    pub fn get_settings(&self) -> SettingsResponse {
        let config = self.token_manager.config();
        SettingsResponse {
            api_key: config.api_key.clone(),
            require_api_key: config.require_api_key,
            port: config.port,
            host: config.host.clone(),
            allow_over_usage: config.allow_over_usage,
        }
    }

    pub async fn update_settings(
        &self,
        req: UpdateSettingsRequest,
    ) -> Result<(), AdminServiceError> {
        let new_api_key = req.api_key.clone();
        let new_require_api_key = req.require_api_key;
        let new_password = req
            .password
            .as_deref()
            .map(str::trim)
            .filter(|password| !password.is_empty())
            .map(str::to_string);

        self.token_manager.with_config_mut(|cfg| {
            if let Some(api_key) = &req.api_key {
                cfg.api_key = Some(api_key.trim().to_string()).filter(|s| !s.is_empty());
            }
            if let Some(require_api_key) = req.require_api_key {
                cfg.require_api_key = require_api_key;
            }
            if let Some(password) = &new_password {
                cfg.admin_api_key = Some(password.clone());
            }
            if let Some(allow_over_usage) = req.allow_over_usage {
                cfg.allow_over_usage = allow_over_usage;
            }
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        if let Some(api_key) = new_api_key {
            *self.client_api_key_runtime.write() = api_key.trim().to_string();
        }
        if let Some(require_api_key) = new_require_api_key {
            self.require_api_key_runtime
                .store(require_api_key, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(password) = new_password {
            *self.admin_api_key_runtime.write() = password;
        }

        Ok(())
    }

    pub fn get_thinking_config(&self) -> ThinkingConfigResponse {
        let config = self.token_manager.config();
        ThinkingConfigResponse {
            suffix: config.thinking_suffix.clone(),
            openai_format: config.openai_thinking_format.clone(),
            claude_format: config.claude_thinking_format.clone(),
        }
    }

    pub async fn update_thinking_config(
        &self,
        req: UpdateThinkingConfigRequest,
    ) -> Result<(), AdminServiceError> {
        fn validate_format(value: &str, field: &str) -> Result<(), AdminServiceError> {
            if matches!(value, "reasoning_content" | "thinking" | "think") {
                Ok(())
            } else {
                Err(AdminServiceError::InvalidRequest(format!(
                    "{} 必须是 reasoning_content、thinking 或 think",
                    field
                )))
            }
        }

        validate_format(&req.openai_format, "openaiFormat")?;
        validate_format(&req.claude_format, "claudeFormat")?;
        let suffix = if req.suffix.trim().is_empty() {
            "-thinking".to_string()
        } else {
            req.suffix.trim().to_string()
        };
        let openai_format = req.openai_format;
        let claude_format = req.claude_format;

        self.token_manager.with_config_mut(|cfg| {
            cfg.thinking_suffix = suffix.clone();
            cfg.openai_thinking_format = openai_format.clone();
            cfg.claude_thinking_format = claude_format.clone();
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        *self.thinking_config.write() = ThinkingRuntimeConfig {
            suffix,
            openai_format,
            claude_format,
        };

        Ok(())
    }

    pub fn get_endpoint_config(&self) -> EndpointConfigResponse {
        let config = self.token_manager.config();
        EndpointConfigResponse {
            preferred_endpoint: config.preferred_endpoint.clone(),
            endpoint_fallback: config.endpoint_fallback,
        }
    }

    pub async fn update_endpoint_config(
        &self,
        req: UpdateEndpointConfigRequest,
    ) -> Result<(), AdminServiceError> {
        let internal_endpoint = Self::kiro_go_endpoint_to_internal(&req.preferred_endpoint)?;
        let endpoint_fallback = req.endpoint_fallback;

        self.token_manager.with_config_mut(|cfg| {
            cfg.preferred_endpoint = req.preferred_endpoint.clone();
            cfg.default_endpoint = internal_endpoint.to_string();
            if let Some(value) = endpoint_fallback {
                cfg.endpoint_fallback = value;
            }
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        let config = self.token_manager.config();
        self.token_manager
            .update_default_endpoint(config.default_endpoint.clone());
        if let Some(provider) = &self.kiro_provider {
            if let Err(e) = provider.update_default_endpoint(config.default_endpoint.clone()) {
                tracing::warn!("provider.update_default_endpoint 失败（已持久化）: {}", e);
            }
            provider.update_endpoint_fallback(config.endpoint_fallback);
        }

        Ok(())
    }

    pub fn get_kiro_go_proxy_config(&self) -> KiroGoProxyConfigResponse {
        let config = self.token_manager.config();
        KiroGoProxyConfigResponse {
            proxy_url: config.proxy_url.unwrap_or_default(),
        }
    }

    pub async fn update_kiro_go_proxy_config(
        &self,
        req: UpdateProxyConfigRequest,
    ) -> Result<(), AdminServiceError> {
        if let Some(proxy_url) = &req.proxy_url {
            Self::validate_kiro_go_proxy_url(proxy_url)?;
        }
        self.update_proxy_config(req).await
    }

    pub fn get_prompt_filter_config(&self) -> PromptFilterConfigResponse {
        let config = self.token_manager.config();
        PromptFilterConfigResponse {
            filter_claude_code: config.prompt_filter.filter_claude_code,
            filter_env_noise: config.prompt_filter.filter_env_noise,
            filter_strip_boundaries: config.prompt_filter.filter_strip_boundaries,
            rules: config
                .prompt_filter
                .rules
                .iter()
                .map(Self::prompt_filter_rule_to_dto)
                .collect(),
        }
    }

    pub async fn update_prompt_filter_config(
        &self,
        req: UpdatePromptFilterConfigRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager.with_config_mut(|cfg| {
            cfg.prompt_filter.filter_claude_code = req.filter_claude_code;
            cfg.prompt_filter.filter_env_noise = req.filter_env_noise;
            cfg.prompt_filter.filter_strip_boundaries = req.filter_strip_boundaries;
            cfg.prompt_filter.rules = req
                .rules
                .iter()
                .map(Self::prompt_filter_rule_from_dto)
                .collect();
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        let config = self.token_manager.config();
        *self.prompt_filter_config.write() = config.prompt_filter.clone();
        Ok(())
    }

    fn kiro_go_endpoint_to_internal(value: &str) -> Result<&'static str, AdminServiceError> {
        match value {
            "auto" | "kiro" => Ok(IDE_ENDPOINT_NAME),
            "codewhisperer" => Ok(CODEWHISPERER_ENDPOINT_NAME),
            "amazonq" => Ok(CLI_ENDPOINT_NAME),
            _ => Err(AdminServiceError::InvalidRequest(
                "preferredEndpoint 必须是 auto、kiro、codewhisperer 或 amazonq".to_string(),
            )),
        }
    }

    fn validate_kiro_go_proxy_url(value: &str) -> Result<(), AdminServiceError> {
        let value = value.trim();
        if value.is_empty()
            || value.starts_with("http://")
            || value.starts_with("https://")
            || value.starts_with("socks5://")
            || value.starts_with("socks5h://")
        {
            Ok(())
        } else {
            Err(AdminServiceError::InvalidRequest(
                "proxyURL must start with http://, https://, socks5://, or socks5h://".to_string(),
            ))
        }
    }

    fn prompt_filter_rule_to_dto(
        rule: &crate::model::config::PromptFilterRule,
    ) -> PromptFilterRuleDto {
        PromptFilterRuleDto {
            id: rule.id.clone(),
            name: rule.name.clone(),
            rule_type: rule.rule_type.clone(),
            match_pattern: rule.match_pattern.clone(),
            replace: rule.replace.clone(),
            enabled: rule.enabled,
        }
    }

    fn prompt_filter_rule_from_dto(
        rule: &PromptFilterRuleDto,
    ) -> crate::model::config::PromptFilterRule {
        crate::model::config::PromptFilterRule {
            id: rule.id.clone(),
            name: rule.name.clone(),
            enabled: rule.enabled,
            rule_type: rule.rule_type.clone(),
            match_pattern: rule.match_pattern.clone(),
            replace: rule.replace.clone(),
        }
    }

    /// 获取全局配置
    pub fn get_global_config(&self) -> GlobalConfigResponse {
        let config = self.token_manager.config();
        let c = self.compression_config.read();
        GlobalConfigResponse {
            region: config.region.clone(),
            prompt_cache_ttl_seconds: config.prompt_cache_ttl_seconds,
            prompt_cache_accounting_enabled: config.prompt_cache_accounting_enabled,
            default_endpoint: config.default_endpoint.clone(),
            extract_thinking: config.extract_thinking,
            per_credential_concurrency: config.per_credential_concurrency,
            global_concurrency: config.global_concurrency,
            acquire_wait_timeout_secs: config.acquire_wait_timeout_secs,
            balance_refresh_enabled: config.balance_refresh_enabled,
            balance_refresh_interval_secs: config.balance_refresh_interval_secs,
            balance_refresh_concurrency: config.balance_refresh_concurrency,
            session_affinity_enabled: config.session_affinity_enabled,
            privacy_mode: config.privacy_mode,
            compression: CompressionConfigResponse {
                max_request_body_bytes: c.max_request_body_bytes,
            },
        }
    }

    /// 更新全局配置（热更新）
    ///
    /// 返回更新后的 `GlobalConfigResponse`，前端拿到即可直接渲染，避免
    /// 再发一次 GET 请求；与 `get_global_config()` 同形。
    pub async fn update_global_config(
        &self,
        req: UpdateGlobalConfigRequest,
    ) -> Result<GlobalConfigResponse, AdminServiceError> {
        // 0. 先抓写前快照：用于后续传给 setter。
        //    必须在 with_config_mut 之前抓，否则会读到闭包刚写入的新值，
        //    setter 内部 old==new 时会触发 noop（bug-B 根因）。
        let old_per_credential = self.token_manager.config().per_credential_concurrency;
        let old_global = self.token_manager.config().global_concurrency;

        // 1. 先持久化配置（失败时不影响运行时状态）
        self.token_manager.with_config_mut(|cfg| {
            if let Some(region) = &req.region {
                let trimmed = region.trim();
                if trimmed.is_empty() {
                    return Err(AdminServiceError::InvalidCredential(
                        "Region 不能为空".to_string(),
                    ));
                }
                cfg.region = trimmed.to_string();
            }

            if let Some(ttl_seconds) = req.prompt_cache_ttl_seconds {
                if !matches!(ttl_seconds, 300 | 3600) {
                    return Err(AdminServiceError::InvalidCredential(
                        "Prompt Cache TTL 仅支持 300（5分钟）或 3600（1小时）".to_string(),
                    ));
                }
                cfg.prompt_cache_ttl_seconds = ttl_seconds;
            }

            if let Some(enabled) = req.prompt_cache_accounting_enabled {
                cfg.prompt_cache_accounting_enabled = enabled;
            }

            if let Some(ref endpoint) = req.default_endpoint {
                let trimmed = endpoint.trim();
                if trimmed.is_empty() {
                    return Err(AdminServiceError::InvalidCredential(
                        "默认 endpoint 不能为空".to_string(),
                    ));
                }
                if !self.known_endpoints.contains(trimmed) {
                    let mut known: Vec<&str> =
                        self.known_endpoints.iter().map(|s| s.as_str()).collect();
                    known.sort_unstable();
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "未知的 endpoint: {}，可用值: {:?}",
                        trimmed, known
                    )));
                }
                cfg.default_endpoint = trimmed.to_string();
            }

            if let Some(extract) = req.extract_thinking {
                cfg.extract_thinking = extract;
            }

            if let Some(c) = &req.compression {
                Self::apply_compression_fields(&mut cfg.compression, c);
            }

            // 凭据队列等待超时（秒）：无单独 setter，运行时 acquire 路径每次读 config 即生效
            if let Some(v) = req.acquire_wait_timeout_secs {
                cfg.acquire_wait_timeout_secs = v;
            }

            // 单凭据最大并发数 0 无意义（setter 内部也会 bail），提前拦截返回 400。
            // 注：setter 已改双参（old, new），old 由函数顶部 snapshot 传入，
            // 故此处可放心写 cfg —— setter 不再读 config，写入时机不影响 setter。
            if let Some(v) = req.per_credential_concurrency {
                if v == 0 {
                    return Err(AdminServiceError::InvalidCredential(
                        "单凭据最大并发数不能为 0".to_string(),
                    ));
                }
                cfg.per_credential_concurrency = v;
            }

            // 全局最大并发数：0 表示不限，合法
            if let Some(v) = req.global_concurrency {
                cfg.global_concurrency = v;
            }

            // 余额刷新三连：写入后统一 clamp（min 间隔 180s, 并发 1..=10）
            if let Some(v) = req.balance_refresh_enabled {
                cfg.balance_refresh_enabled = v;
            }
            if let Some(v) = req.balance_refresh_interval_secs {
                cfg.balance_refresh_interval_secs = v;
            }
            if let Some(v) = req.balance_refresh_concurrency {
                cfg.balance_refresh_concurrency = v;
            }
            cfg.clamp_balance_refresh();

            if let Some(v) = req.session_affinity_enabled {
                cfg.session_affinity_enabled = v;
            }

            if let Some(v) = req.privacy_mode {
                cfg.privacy_mode = v;
            }

            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        // 2. 持久化成功后再应用运行时变更
        let config = self.token_manager.config();

        // 关闭 session 亲和后清空已有绑定，避免残留
        if let Some(false) = req.session_affinity_enabled {
            self.token_manager.clear_session_affinity();
        }

        // 热更新 region（注：xkiro 已剔除 credential_rpm，故不存在 update_credential_rpm 同步）
        if req.region.is_some() {
            self.token_manager.update_region(config.region.clone());
        }

        // 热更新 default_endpoint
        // 贴合 BK admin/service.rs:910-925：token_manager 先 + provider 后双层同步
        if req.default_endpoint.is_some() {
            self.token_manager
                .update_default_endpoint(config.default_endpoint.clone());
            if let Some(provider) = &self.kiro_provider {
                if let Err(e) = provider.update_default_endpoint(config.default_endpoint.clone()) {
                    tracing::warn!("provider.update_default_endpoint 失败（已持久化）: {}", e);
                }
            }
        }

        // 热更新 Prompt Cache 运行时配置
        if req.prompt_cache_ttl_seconds.is_some() || req.prompt_cache_accounting_enabled.is_some() {
            self.prompt_cache_runtime.write().update(
                req.prompt_cache_ttl_seconds,
                req.prompt_cache_accounting_enabled,
            );
        }

        // 热更新压缩配置到运行时 Arc<RwLock<CompressionConfig>>
        if let Some(c) = &req.compression {
            let mut runtime = self.compression_config.write();
            Self::apply_compression_fields(&mut runtime, c);
        }

        // 热更新单凭据最大并发数（0 不允许，setter 内部 bail）
        // old 由函数顶部 snapshot 传入，setter 内部不读 config，避免 noop。
        if let Some(v) = req.per_credential_concurrency {
            self.token_manager
                .set_per_credential_concurrency(old_per_credential, v)
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        }

        // 热更新全局并发数（0 表示不限）
        if let Some(v) = req.global_concurrency {
            self.token_manager
                .set_global_concurrency(old_global, v)
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        }

        Ok(self.get_global_config())
    }

    // ============ 系统提示注入 ============

    /// 取系统提示注入快照（含 builtin + user preset 列表 + 启用状态）
    pub fn get_system_prompt(&self) -> SystemPromptResponse {
        let rt = self.prompt_runtime.read();
        let mut presets: Vec<PresetItem> = Vec::new();

        for p in crate::anthropic::prompt_presets::PRESETS {
            let enabled = rt.enabled_presets.iter().any(|id| id == p.id);
            presets.push(PresetItem {
                id: p.id.to_string(),
                name: p.name.to_string(),
                description: p.description.to_string(),
                source: "builtin".to_string(),
                enabled,
                content: None,
            });
        }
        for up in &rt.user_presets {
            let enabled = rt.enabled_presets.iter().any(|id| id == &up.id);
            presets.push(PresetItem {
                id: up.id.clone(),
                name: up.name.clone(),
                description: up.description.clone(),
                source: "user".to_string(),
                enabled,
                content: Some(up.content.clone()),
            });
        }

        SystemPromptResponse {
            enabled: rt.enabled,
            position: match rt.position {
                SystemPromptPosition::Prepend => "prepend".to_string(),
                SystemPromptPosition::Append => "append".to_string(),
            },
            custom_content: rt.custom_content.clone(),
            presets,
        }
    }

    /// 更新系统提示注入配置（部分字段更新；持久化到 config.json）
    pub fn update_system_prompt(
        &self,
        req: UpdateSystemPromptRequest,
    ) -> Result<SystemPromptResponse, AdminServiceError> {
        let position = if let Some(pos) = req.position.as_deref() {
            match pos {
                "prepend" => Some(SystemPromptPosition::Prepend),
                "append" => Some(SystemPromptPosition::Append),
                _ => {
                    return Err(AdminServiceError::InvalidCredential(
                        "position 仅允许 'prepend' 或 'append'".to_string(),
                    ));
                }
            }
        } else {
            None
        };

        if let Some(ref ids) = req.enabled_presets {
            let user_ids: Vec<String> = self
                .prompt_runtime
                .read()
                .user_presets
                .iter()
                .map(|p| p.id.clone())
                .collect();
            for id in ids {
                let known = crate::anthropic::prompt_presets::is_builtin(id)
                    || user_ids.iter().any(|u| u == id);
                if !known {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "未知 preset id: {}",
                        id
                    )));
                }
            }
        }

        self.token_manager.with_config_mut(|cfg| {
            if let Some(v) = req.enabled {
                cfg.system_prompt_enabled = v;
            }
            if let Some(p) = position {
                cfg.system_prompt_position = p;
            }
            if let Some(c) = req.custom_content.clone() {
                let trimmed = c.trim();
                cfg.system_prompt = if trimmed.is_empty() { None } else { Some(c) };
            }
            if let Some(ids) = req.enabled_presets.clone() {
                cfg.enabled_presets = ids;
            }
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        // 持久化成功 → 同步运行时
        {
            let mut rt = self.prompt_runtime.write();
            if let Some(v) = req.enabled {
                rt.enabled = v;
            }
            if let Some(p) = position {
                rt.position = p;
            }
            if let Some(c) = req.custom_content {
                let trimmed = c.trim();
                rt.custom_content = if trimmed.is_empty() { None } else { Some(c) };
            }
            if let Some(ids) = req.enabled_presets {
                rt.enabled_presets = ids;
            }
        }

        Ok(self.get_system_prompt())
    }

    /// 新增/覆盖用户预设（id 已存在则覆盖）；不允许与内置 id 冲突
    pub fn upsert_user_preset(
        &self,
        req: UpsertUserPresetRequest,
    ) -> Result<SystemPromptResponse, AdminServiceError> {
        let id = req.id.trim().to_string();
        if id.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "preset id 不能为空".to_string(),
            ));
        }
        if id.len() > 32
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(AdminServiceError::InvalidCredential(
                "preset id 仅允许 [a-z0-9_-]，长度 1-32".to_string(),
            ));
        }
        if crate::anthropic::prompt_presets::is_builtin(&id) {
            return Err(AdminServiceError::InvalidCredential(format!(
                "id '{}' 与内置预设冲突",
                id
            )));
        }

        let preset = UserPreset {
            id: id.clone(),
            name: req.name,
            description: req.description,
            content: req.content,
        };

        self.token_manager.with_config_mut(|cfg| {
            if let Some(existing) = cfg.user_presets.iter_mut().find(|p| p.id == id) {
                *existing = preset.clone();
            } else {
                cfg.user_presets.push(preset.clone());
            }
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        {
            let mut rt = self.prompt_runtime.write();
            if let Some(existing) = rt.user_presets.iter_mut().find(|p| p.id == id) {
                *existing = preset;
            } else {
                rt.user_presets.push(preset);
            }
        }

        Ok(self.get_system_prompt())
    }

    /// 删除用户预设；同时从 enabled_presets 移除
    pub fn delete_user_preset(&self, id: &str) -> Result<SystemPromptResponse, AdminServiceError> {
        let id_owned = id.to_string();

        let existed = self
            .prompt_runtime
            .read()
            .user_presets
            .iter()
            .any(|p| p.id == id_owned);
        if !existed {
            return Err(AdminServiceError::InvalidCredential(format!(
                "未找到用户预设 id: {}",
                id_owned
            )));
        }

        self.token_manager.with_config_mut(|cfg| {
            cfg.user_presets.retain(|p| p.id != id_owned);
            cfg.enabled_presets.retain(|x| x != &id_owned);
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        {
            let mut rt = self.prompt_runtime.write();
            rt.user_presets.retain(|p| p.id != id_owned);
            rt.enabled_presets.retain(|x| x != &id_owned);
        }

        Ok(self.get_system_prompt())
    }

    /// 将更新请求中的压缩字段应用到目标 CompressionConfig
    ///
    fn apply_compression_fields(
        target: &mut CompressionConfig,
        src: &UpdateCompressionConfigRequest,
    ) {
        if let Some(v) = src.max_request_body_bytes {
            target.max_request_body_bytes = v;
        }
    }

    // ============ 导出 token.json ============

    /// 按 ID 列表导出凭据为 token.json 兼容格式
    ///
    /// - API Key 凭据（无 refreshToken）跳过
    /// - 不存在的 ID 跳过
    /// - 输出顺序与 `ids` 一致；可被 `import_token_json` 直接吃回
    pub fn export_credentials_to_token_json(&self, ids: &[u64]) -> Vec<ExportTokenJsonItem> {
        let creds = self.token_manager.export_credentials_by_ids(ids);
        creds
            .into_iter()
            .filter_map(|c| {
                let refresh_token = c.refresh_token.clone()?;
                if refresh_token.is_empty() {
                    return None;
                }
                let auth_method = match c.auth_method.as_deref() {
                    Some(m) => {
                        let lower = m.to_lowercase();
                        match lower.as_str() {
                            "builder-id" | "builderid" | "iam" | "idc" => "idc".to_string(),
                            "api_key" => return None, // API Key 不可导出为 token.json
                            other => other.to_string(),
                        }
                    }
                    None => "social".to_string(),
                };
                let provider = c
                    .provider
                    .clone()
                    .unwrap_or_else(|| match auth_method.as_str() {
                        "idc" => "BuilderId".to_string(),
                        _ => "Social".to_string(),
                    });
                Some(ExportTokenJsonItem {
                    provider,
                    refresh_token,
                    client_id: c.client_id,
                    client_secret: c.client_secret,
                    auth_method,
                    priority: c.priority,
                    weight: c.weight,
                    region: c.region,
                    api_region: c.api_region,
                    machine_id: c.machine_id,
                })
            })
            .collect()
    }

    /// 按 ID 列表导出 KAM 兼容格式（`kiro-account-manager` 可直接 import）
    ///
    /// - API Key 凭据跳过（KAM 仅支持 OAuth）
    /// - `id` 用 UUIDv4 派生（KAM 用字符串 ID，xkiro 用 u64，需重映射避免冲突）
    /// - `label` 用 email 优先，否则用 `Kiro #{id}` 占位
    /// - `provider` 优先级：subscription_title 启发 → start_url → email 域名 → 默认
    ///   - idc + start_url 含 `awsapps.com` → `Enterprise`
    ///   - idc → `BuilderId`
    ///   - social + email 含 `gmail` → `Google`
    ///   - social + email 含 `github` → `Github`
    ///   - social → `Google`（默认）
    /// - `authMethod` 取大写 `IdC` / 小写 `social`（KAM 约定）
    /// - `addedAt` 用 RFC3339 当前时间（xkiro 不存添加时间）
    pub fn export_credentials_to_kam(&self, ids: &[u64]) -> Vec<ExportKamItem> {
        let creds = self.token_manager.export_credentials_with_state_by_ids(ids);
        let now = chrono::Local::now().to_rfc3339();
        creds
            .into_iter()
            .filter_map(|(c, enabled)| {
                let refresh_token = c.refresh_token.clone()?;
                if refresh_token.is_empty() {
                    return None;
                }
                let auth_method_lower = c
                    .auth_method
                    .as_deref()
                    .map(|m| m.to_lowercase())
                    .unwrap_or_else(|| "social".to_string());
                if auth_method_lower == "api_key" {
                    return None;
                }
                let is_idc = matches!(
                    auth_method_lower.as_str(),
                    "idc" | "builder-id" | "builderid" | "iam"
                );
                let auth_method = if is_idc {
                    "IdC".to_string()
                } else {
                    "social".to_string()
                };
                let provider = c.provider.clone().unwrap_or_else(|| {
                    if is_idc {
                        if c.start_url
                            .as_deref()
                            .map(|s| {
                                let url = s.trim().trim_end_matches('/');
                                !url.is_empty() && url != "https://view.awsapps.com/start"
                            })
                            .unwrap_or(false)
                            || c.client_secret
                                .as_deref()
                                .map(|s| {
                                    s.contains("awsapps.com") || s.contains("initiateLoginUri")
                                })
                                .unwrap_or(false)
                        {
                            "Enterprise".to_string()
                        } else {
                            "BuilderId".to_string()
                        }
                    } else if let Some(email) = c.email.as_deref() {
                        if email.contains("gmail") {
                            "Google".to_string()
                        } else if email.contains("github") {
                            "Github".to_string()
                        } else {
                            "Google".to_string()
                        }
                    } else {
                        "Google".to_string()
                    }
                });
                let user_id = c.user_id.clone().or_else(|| c.email.clone());
                let id_str =
                    c.id.map(|n| format!("xkiro-{}", n))
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let label = c.email.clone().unwrap_or_else(|| match c.id {
                    Some(n) => format!("Kiro #{}", n),
                    None => "Kiro Account".to_string(),
                });
                let status = if enabled { "active" } else { "disabled" };
                Some(ExportKamItem {
                    id: id_str,
                    email: c.email.clone(),
                    label,
                    status: status.to_string(),
                    added_at: now.clone(),
                    access_token: c.access_token,
                    refresh_token: Some(refresh_token),
                    expires_at: c.expires_at,
                    provider: Some(provider),
                    user_id,
                    auth_method: Some(auth_method),
                    client_id: c.client_id,
                    client_secret: c.client_secret,
                    region: c.region,
                    client_id_hash: c.client_id_hash,
                    sso_session_id: c.sso_session_id,
                    id_token: c.id_token,
                    start_url: c.start_url,
                    profile_arn: c.profile_arn,
                    machine_id: c.machine_id,
                    enabled,
                })
            })
            .collect()
    }

    // ============ 批量导入 token.json ============

    /// 批量导入 token.json
    ///
    /// 解析官方 token.json 格式，按 provider 字段自动映射 authMethod：
    /// - BuilderId/builder-id/idc → idc
    /// - Social/social → social
    pub async fn import_token_json(&self, req: ImportTokenJsonRequest) -> ImportTokenJsonResponse {
        let items = req.items.into_vec();
        let dry_run = req.dry_run;

        let mut results = Vec::with_capacity(items.len());
        let mut added = 0usize;
        let mut skipped = 0usize;
        let mut invalid = 0usize;

        for (index, item) in items.into_iter().enumerate() {
            let result = self.process_token_json_item(index, item, dry_run).await;
            match result.action {
                ImportAction::Added => added += 1,
                ImportAction::Skipped => skipped += 1,
                ImportAction::Invalid => invalid += 1,
            }
            results.push(result);
        }

        ImportTokenJsonResponse {
            summary: ImportSummary {
                parsed: results.len(),
                added,
                skipped,
                invalid,
            },
            items: results,
        }
    }

    /// 处理单个 token.json 项
    async fn process_token_json_item(
        &self,
        index: usize,
        item: TokenJsonItem,
        dry_run: bool,
    ) -> ImportItemResult {
        // 生成指纹（用于识别和去重）
        let fingerprint = Self::generate_fingerprint(&item);

        // 验证必填字段
        let refresh_token = match &item.refresh_token {
            Some(rt) if !rt.is_empty() => rt.clone(),
            _ => {
                return ImportItemResult {
                    index,
                    fingerprint,
                    action: ImportAction::Invalid,
                    reason: Some("缺少 refreshToken".to_string()),
                    credential_id: None,
                };
            }
        };

        // 映射 authMethod
        let auth_method = Self::map_auth_method(&item);

        // IdC 需要 clientId 和 clientSecret
        if auth_method == "idc" && (item.client_id.is_none() || item.client_secret.is_none()) {
            return ImportItemResult {
                index,
                fingerprint,
                action: ImportAction::Invalid,
                reason: Some(format!("{} 认证需要 clientId 和 clientSecret", auth_method)),
                credential_id: None,
            };
        }

        // 检查是否已存在（通过 refreshToken 前缀匹配）
        if self.token_manager.has_refresh_token_prefix(&refresh_token) {
            return ImportItemResult {
                index,
                fingerprint,
                action: ImportAction::Skipped,
                reason: Some("凭据已存在".to_string()),
                credential_id: None,
            };
        }

        // dry-run 模式只返回预览
        if dry_run {
            return ImportItemResult {
                index,
                fingerprint,
                action: ImportAction::Added,
                reason: Some("预览模式".to_string()),
                credential_id: None,
            };
        }

        // 实际添加凭据（trim + 空字符串转 None，与 set_region 逻辑一致）
        let region = item
            .region
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let api_region = item
            .api_region
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some(refresh_token),
            kiro_api_key: None,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(auth_method),
            provider: item.provider,
            user_id: None,
            client_id: item.client_id,
            client_secret: item.client_secret,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            start_url: None,
            client_id_hash: None,
            id_token: None,
            sso_session_id: None,
            priority: item.priority,
            weight: item.weight,
            region,
            auth_region: None,
            api_region,
            machine_id: item.machine_id,
            endpoint: None,
            email: None,
            subscription_title: None,
            overage_status: None,
            legacy_allow_overage: false,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            concurrency: None,
        };

        match self.token_manager.add_credential(new_cred).await {
            Ok(credential_id) => ImportItemResult {
                index,
                fingerprint,
                action: ImportAction::Added,
                reason: None,
                credential_id: Some(credential_id),
            },
            Err(e) => ImportItemResult {
                index,
                fingerprint,
                action: ImportAction::Invalid,
                reason: Some(e.to_string()),
                credential_id: None,
            },
        }
    }

    /// 生成凭据指纹（用于识别）
    ///
    /// 使用 refreshToken 前 16 字符作为指纹，floor_char_boundary 安全截断
    fn generate_fingerprint(item: &TokenJsonItem) -> String {
        item.refresh_token
            .as_ref()
            .map(|rt| {
                if rt.len() >= 16 {
                    let end = floor_char_boundary(rt, 16);
                    format!("{}...", &rt[..end])
                } else {
                    rt.clone()
                }
            })
            .unwrap_or_else(|| "(empty)".to_string())
    }

    /// 映射 provider/authMethod 到标准 authMethod
    ///
    /// 优先级：authMethod > provider > 默认 social
    fn map_auth_method(item: &TokenJsonItem) -> String {
        // 优先使用 authMethod 字段
        if let Some(auth) = &item.auth_method {
            let auth_lower = auth.to_lowercase();
            return match auth_lower.as_str() {
                "idc" | "builder-id" | "builderid" => "idc".to_string(),
                "social" => "social".to_string(),
                _ => auth_lower,
            };
        }

        // 回退到 provider 字段
        if let Some(provider) = &item.provider {
            let provider_lower = provider.to_lowercase();
            return match provider_lower.as_str() {
                "builderid" | "builder-id" | "idc" => "idc".to_string(),
                "social" => "social".to_string(),
                _ => "social".to_string(),
            };
        }

        // 默认 social
        "social".to_string()
    }
}

use crate::kiro::token_manager::CreditUsageObserver;

impl CreditUsageObserver for AdminService {
    fn on_credit_usage(
        &self,
        id: u64,
        credit: f64,
        new_primary_remaining: f64,
        new_overage_remaining: f64,
    ) {
        if !credit.is_finite() || credit <= 0.0 {
            return;
        }

        let mutated = {
            let mut cache = self.balance_cache.lock();
            let Some(entry) = cache.get_mut(&id) else {
                return;
            };
            let data = &mut entry.data;
            data.current_usage = (data.current_usage + credit).max(0.0);
            data.remaining = new_primary_remaining;
            data.usage_percentage = if data.usage_limit > 0.0 {
                ((data.current_usage / data.usage_limit) * 100.0).min(9_999.0)
            } else {
                0.0
            };
            entry.cached_at = Utc::now().timestamp() as f64;
            tracing::debug!(
                credential_id = id,
                credit,
                new_remaining = data.remaining,
                new_overage = new_overage_remaining,
                "AdminService disk cache 已同步 metering 扣减"
            );
            true
        };

        if mutated {
            self.save_balance_cache();
        }
    }
}

impl AdminService {
    // ── Social OAuth 登录 ────────────────────────────────────────────────────

    pub async fn start_social_login(
        &self,
        req: StartSocialLoginRequest,
    ) -> Result<StartSocialLoginResponse, AdminServiceError> {
        let provider = match req.provider.trim() {
            "Google" => "Google",
            "Github" => "Github",
            other => {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "不支持的 Social 提供方: {}",
                    other
                )));
            }
        };

        let global_proxy = {
            let config = self.token_manager.config();
            config.proxy_url.as_deref().map(ProxyConfig::new)
        };
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let auth_endpoint = req
            .auth_endpoint
            .unwrap_or_else(|| social::KIRO_AUTH_ENDPOINT.to_string());

        let is_helper = req.mode.as_deref() == Some("helper");

        let expires_at = Utc::now() + chrono::Duration::minutes(10);
        let session_id = uuid::Uuid::new_v4().to_string();

        let machine_id = if is_helper {
            None
        } else {
            Some(self.generate_machine_id().machine_id)
        };

        let cred_template = KiroCredentials {
            auth_method: Some("social".to_string()),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            machine_id,
            ..Default::default()
        };

        let (mode, portal_url, session) = if is_helper {
            let session = SocialAuthSession {
                kind: SocialAuthSessionKind::Helper,
                auth_endpoint,
                state: String::new(),
                code_verifier: String::new(),
                redirect_uri: String::new(),
                expires_at,
                cred_template,
                proxy,
                helper_result: None,
                helper_completing: false,
            };
            ("helper".to_string(), None, session)
        } else {
            let (code_verifier, code_challenge) = social::generate_pkce();
            let state = uuid::Uuid::new_v4().to_string();
            let redirect_uri = social::manual_redirect_uri();
            let portal_url = social::build_login_url(
                &auth_endpoint,
                provider,
                &state,
                &code_challenge,
                &redirect_uri,
            );
            let session = SocialAuthSession {
                kind: SocialAuthSessionKind::Manual,
                auth_endpoint,
                state,
                code_verifier,
                redirect_uri,
                expires_at,
                cred_template,
                proxy,
                helper_result: None,
                helper_completing: false,
            };
            ("manual".to_string(), Some(portal_url), session)
        };

        self.social_sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartSocialLoginResponse {
            session_id,
            mode,
            portal_url,
            expires_at: expires_at.to_rfc3339(),
        })
    }

    pub async fn poll_social_login(
        &self,
        session_id: &str,
    ) -> Result<PollSocialLoginResponse, AdminServiceError> {
        enum PollOutcome {
            Waiting,
            Expired,
            HelperSucceeded(u64),
            HelperFailed(String),
        }

        let outcome = {
            let mut sessions = self.social_sessions.lock();
            let s = match sessions.get_mut(session_id) {
                Some(s) => s,
                None => return Ok(PollSocialLoginResponse::Expired),
            };
            match s.kind {
                SocialAuthSessionKind::Helper => match s.helper_result.take() {
                    Some(Ok(id)) => PollOutcome::HelperSucceeded(id),
                    Some(Err(msg)) => PollOutcome::HelperFailed(msg),
                    None if s.helper_completing => PollOutcome::Waiting,
                    None if Utc::now() >= s.expires_at => PollOutcome::Expired,
                    None => PollOutcome::Waiting,
                },
                SocialAuthSessionKind::Manual if Utc::now() >= s.expires_at => PollOutcome::Expired,
                SocialAuthSessionKind::Manual => PollOutcome::Waiting,
            }
        };

        match outcome {
            PollOutcome::Waiting => Ok(PollSocialLoginResponse::Waiting),
            PollOutcome::Expired => {
                self.social_sessions.lock().remove(session_id);
                Ok(PollSocialLoginResponse::Expired)
            }
            PollOutcome::HelperSucceeded(credential_id) => {
                self.social_sessions.lock().remove(session_id);
                Ok(PollSocialLoginResponse::Success { credential_id })
            }
            PollOutcome::HelperFailed(message) => {
                self.social_sessions.lock().remove(session_id);
                Ok(PollSocialLoginResponse::Error { message })
            }
        }
    }

    pub async fn complete_social_login_callback(
        &self,
        session_id: &str,
        req: CompleteSocialCallbackRequest,
    ) -> Result<PollSocialLoginResponse, AdminServiceError> {
        if req.callback_url.trim().is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "callbackUrl 不能为空".to_string(),
            ));
        }
        let callback = social::callback_from_input(&req.callback_url)
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;
        self.do_complete_social_login(session_id, callback).await
    }

    async fn do_complete_social_login(
        &self,
        session_id: &str,
        callback: social::OAuthCallbackData,
    ) -> Result<PollSocialLoginResponse, AdminServiceError> {
        {
            let sessions = self.social_sessions.lock();
            let session = sessions
                .get(session_id)
                .ok_or(AdminServiceError::NotFound { id: 0 })?;
            if session.kind == SocialAuthSessionKind::Helper {
                return Ok(PollSocialLoginResponse::Error {
                    message: "该会话是 helper 模式，不能提交浏览器回调 URL".to_string(),
                });
            }
            if Utc::now() >= session.expires_at {
                drop(sessions);
                self.social_sessions.lock().remove(session_id);
                return Ok(PollSocialLoginResponse::Expired);
            }
            if callback.state != session.state {
                return Ok(PollSocialLoginResponse::Error {
                    message: "OAuth state 不匹配，请重新发起登录".to_string(),
                });
            }
        }

        let session = self
            .social_sessions
            .lock()
            .remove(session_id)
            .ok_or(AdminServiceError::NotFound { id: 0 })?;
        let machine_id = session
            .cred_template
            .machine_id
            .clone()
            .unwrap_or_else(|| self.generate_machine_id().machine_id);

        let config = self.token_manager.config();
        let token_resp = social::exchange_code_for_token(
            &session.auth_endpoint,
            &callback.code,
            &session.code_verifier,
            &session.redirect_uri,
            &machine_id,
            &config,
            session.proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let mut new_cred = session.cred_template;
        if new_cred.machine_id.is_none() {
            new_cred.machine_id = Some(machine_id);
        }
        new_cred.access_token = Some(token_resp.access_token);
        new_cred.refresh_token = token_resp.refresh_token;
        new_cred.profile_arn = token_resp.profile_arn;

        if let Some(expires_at) = token_resp.expires_at {
            new_cred.expires_at = Some(expires_at);
        } else if let Some(expires_in) = token_resp.expires_in {
            let ea = Utc::now() + chrono::Duration::seconds(expires_in);
            new_cred.expires_at = Some(ea.to_rfc3339());
        }

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        tracing::info!("Social 登录成功，已添加凭据 #{}", credential_id);
        Ok(PollSocialLoginResponse::Success { credential_id })
    }

    pub async fn complete_social_login(
        &self,
        session_id: &str,
        req: CompleteSocialLoginRequest,
    ) -> Result<(), AdminServiceError> {
        let cred_template = {
            let mut sessions = self.social_sessions.lock();
            let session = sessions
                .get_mut(session_id)
                .ok_or(AdminServiceError::NotFound { id: 0 })?;
            if session.kind != SocialAuthSessionKind::Helper {
                return Err(AdminServiceError::InvalidCredential(
                    "该会话不是 helper 模式，无法接收回传".to_string(),
                ));
            }
            if session.helper_completing || session.helper_result.is_some() {
                return Err(AdminServiceError::InvalidCredential(
                    "该会话已在处理回传，请勿重复提交".to_string(),
                ));
            }
            if Utc::now() >= session.expires_at {
                return Err(AdminServiceError::InvalidCredential(
                    "登录会话已过期".to_string(),
                ));
            }
            session.helper_completing = true;
            session.cred_template.clone()
        };

        let mut new_cred = cred_template;
        new_cred.access_token = Some(req.access_token);
        new_cred.refresh_token = req.refresh_token;
        new_cred.profile_arn = req.profile_arn;
        if req.machine_id.is_some() {
            new_cred.machine_id = req.machine_id;
        }
        if let Some(expires_at) = req.expires_at {
            new_cred.expires_at = Some(expires_at);
        } else if let Some(expires_in) = req.expires_in {
            // 来自请求体的 i64，需防溢出：chrono::Duration::seconds 与日期加法都会 panic
            if let Some(ea) = chrono::Duration::try_seconds(expires_in)
                .and_then(|d| Utc::now().checked_add_signed(d))
            {
                new_cred.expires_at = Some(ea.to_rfc3339());
            }
        }

        let result = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| e.to_string());

        // 写回结果并释放占用；会话此时必然仍在（helper_completing 阻止了 poll 清除）
        if let Some(session) = self.social_sessions.lock().get_mut(session_id) {
            session.helper_completing = false;
            session.helper_result = Some(result.clone());
        }

        match result {
            Ok(credential_id) => {
                tracing::info!("Social helper 回传成功，已添加凭据 #{}", credential_id);
                Ok(())
            }
            Err(e) => Err(AdminServiceError::InternalError(e)),
        }
    }

    pub async fn start_idc_login(
        &self,
        req: StartIdcLoginRequest,
    ) -> Result<StartIdcLoginResponse, AdminServiceError> {
        let global_proxy = {
            let config = self.token_manager.config();
            config.proxy_url.as_deref().map(ProxyConfig::new)
        };
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let region = req.region.trim().to_string();
        if region.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "region 不能为空".to_string(),
            ));
        }

        let start_url = req
            .start_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(idc::BUILDER_ID_START_URL);

        let config = self.token_manager.config();
        let registered = idc::register_client(&region, start_url, &config, proxy.as_ref())
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        let device = idc::start_device_authorization(
            &region,
            start_url,
            &registered.client_id,
            &registered.client_secret,
            &config,
            proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        drop(config);

        let expires_at = Utc::now() + chrono::Duration::seconds(device.expires_in);
        let session_id = uuid::Uuid::new_v4().to_string();

        let cred_template = KiroCredentials {
            auth_method: Some("idc".to_string()),
            client_id: Some(registered.client_id.clone()),
            client_secret: Some(registered.client_secret.clone()),
            region: Some(region.clone()),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };

        let session = IdcAuthSession {
            region,
            client_id: registered.client_id,
            client_secret: registered.client_secret,
            device_code: device.device_code,
            expires_at,
            poll_interval: device.interval.max(5),
            cred_template,
            proxy,
        };

        let poll_interval = session.poll_interval;
        self.idc_sessions.lock().insert(session_id.clone(), session);

        Ok(StartIdcLoginResponse {
            session_id,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            expires_at: expires_at.to_rfc3339(),
            poll_interval,
        })
    }

    pub async fn start_iam_sso_login(
        &self,
        req: StartIdcLoginRequest,
    ) -> Result<StartIamSsoLoginResponse, AdminServiceError> {
        let global_proxy = {
            let config = self.token_manager.config();
            config.proxy_url.as_deref().map(ProxyConfig::new)
        };
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let region = req.region.trim().to_string();
        if region.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "region 不能为空".to_string(),
            ));
        }
        let start_url = req
            .start_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AdminServiceError::InvalidCredential("startUrl is required".to_string())
            })?;

        let config = self.token_manager.config();
        let started =
            idc::start_iam_sso_code_authorization(&region, start_url, &config, proxy.as_ref())
                .await
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        drop(config);
        let authorize_url = started.authorize_url.trim().to_string();
        if authorize_url.is_empty() {
            return Err(AdminServiceError::InternalError(
                "IAM SSO 授权链接为空".to_string(),
            ));
        }

        let expires_at = Utc::now() + chrono::Duration::seconds(started.expires_in);
        let session_id = uuid::Uuid::new_v4().to_string();
        let cred_template = KiroCredentials {
            auth_method: Some("idc".to_string()),
            client_id: Some(started.client_id.clone()),
            client_secret: Some(started.client_secret.clone()),
            region: Some(region.clone()),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };

        self.iam_sso_code_sessions.lock().insert(
            session_id.clone(),
            IamSsoCodeAuthSession {
                region,
                client_id: started.client_id,
                client_secret: started.client_secret,
                code_verifier: started.code_verifier,
                state: started.state,
                redirect_uri: started.redirect_uri,
                expires_at,
                cred_template,
                proxy,
            },
        );

        Ok(StartIamSsoLoginResponse {
            session_id,
            authorize_url,
            expires_in: started.expires_in,
        })
    }

    pub async fn complete_iam_sso_login(
        &self,
        req: CompleteIamSsoLoginRequest,
    ) -> Result<serde_json::Value, AdminServiceError> {
        enum SessionState {
            Missing,
            Expired,
            Active {
                region: String,
                client_id: String,
                client_secret: String,
                code_verifier: String,
                state: String,
                redirect_uri: String,
                proxy: Option<ProxyConfig>,
                cred_template: KiroCredentials,
            },
        }

        let session_state = {
            let sessions = self.iam_sso_code_sessions.lock();
            match sessions.get(&req.session_id) {
                None => SessionState::Missing,
                Some(session) if Utc::now() >= session.expires_at => SessionState::Expired,
                Some(session) => SessionState::Active {
                    region: session.region.clone(),
                    client_id: session.client_id.clone(),
                    client_secret: session.client_secret.clone(),
                    code_verifier: session.code_verifier.clone(),
                    state: session.state.clone(),
                    redirect_uri: session.redirect_uri.clone(),
                    proxy: session.proxy.clone(),
                    cred_template: session.cred_template.clone(),
                },
            }
        };

        let (
            region,
            client_id,
            client_secret,
            code_verifier,
            expected_state,
            redirect_uri,
            proxy,
            cred_template,
        ) = match session_state {
            SessionState::Missing => {
                return Err(AdminServiceError::NotFound { id: 0 });
            }
            SessionState::Expired => {
                self.iam_sso_code_sessions.lock().remove(&req.session_id);
                return Err(AdminServiceError::InvalidCredential(
                    "会话已过期".to_string(),
                ));
            }
            SessionState::Active {
                region,
                client_id,
                client_secret,
                code_verifier,
                state,
                redirect_uri,
                proxy,
                cred_template,
            } => (
                region,
                client_id,
                client_secret,
                code_verifier,
                state,
                redirect_uri,
                proxy,
                cred_template,
            ),
        };

        let params = parse_aws_sso_callback_params(&req.callback_url)?;
        if let Some(error) = params.get("error").filter(|v| !v.is_empty()) {
            return Err(AdminServiceError::InvalidCredential(format!(
                "授权失败: {}",
                error
            )));
        }
        let state = params.get("state").cloned().unwrap_or_default();
        if state != expected_state {
            return Err(AdminServiceError::InvalidCredential(
                "状态不匹配，可能存在安全风险".to_string(),
            ));
        }
        let code = params
            .get("code")
            .filter(|v| !v.trim().is_empty())
            .cloned()
            .ok_or_else(|| AdminServiceError::InvalidCredential("未收到授权码".to_string()))?;

        let config = self.token_manager.config();
        let token = idc::exchange_iam_sso_code(
            &region,
            &client_id,
            &client_secret,
            &code,
            &code_verifier,
            &redirect_uri,
            &config,
            proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        drop(config);
        self.iam_sso_code_sessions.lock().remove(&req.session_id);

        let mut new_cred = cred_template;
        new_cred.access_token = Some(token.access_token);
        new_cred.refresh_token = token.refresh_token;
        if let Some(expires_in) = token.expires_in {
            let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);
            new_cred.expires_at = Some(expires_at.to_rfc3339());
        }
        if let Some(email) = crate::kiro::auth::kiro_sso::extract_email_from_jwt(
            new_cred.access_token.as_deref().unwrap_or_default(),
        ) {
            new_cred.email = Some(email);
        }

        let email = new_cred.email.clone();
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(serde_json::json!({
            "success": true,
            "account": {
                "id": credential_id,
                "email": email,
            },
        }))
    }

    pub async fn poll_idc_login(
        &self,
        session_id: &str,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        enum SessionState {
            Missing,
            Expired,
            Active {
                region: String,
                client_id: String,
                client_secret: String,
                device_code: String,
                proxy: Option<ProxyConfig>,
                cred_template: KiroCredentials,
            },
        }

        let session_state = {
            let sessions = self.idc_sessions.lock();
            match sessions.get(session_id) {
                None => SessionState::Missing,
                Some(session) if Utc::now() >= session.expires_at => SessionState::Expired,
                Some(session) => SessionState::Active {
                    region: session.region.clone(),
                    client_id: session.client_id.clone(),
                    client_secret: session.client_secret.clone(),
                    device_code: session.device_code.clone(),
                    proxy: session.proxy.clone(),
                    cred_template: session.cred_template.clone(),
                },
            }
        };

        let (region, client_id, client_secret, device_code, proxy, cred_template) =
            match session_state {
                SessionState::Missing => return Ok(PollIdcLoginResponse::Expired),
                SessionState::Expired => {
                    self.idc_sessions.lock().remove(session_id);
                    return Ok(PollIdcLoginResponse::Expired);
                }
                SessionState::Active {
                    region,
                    client_id,
                    client_secret,
                    device_code,
                    proxy,
                    cred_template,
                } => (
                    region,
                    client_id,
                    client_secret,
                    device_code,
                    proxy,
                    cred_template,
                ),
            };

        let config = self.token_manager.config();
        let outcome = idc::poll_token(
            &region,
            &client_id,
            &client_secret,
            &device_code,
            &config,
            proxy.as_ref(),
        )
        .await;
        drop(config);

        match outcome {
            idc::PollResult::Pending => Ok(PollIdcLoginResponse::Pending),
            idc::PollResult::SlowDown => Ok(PollIdcLoginResponse::Pending),
            idc::PollResult::Expired => {
                self.idc_sessions.lock().remove(session_id);
                Ok(PollIdcLoginResponse::Expired)
            }
            idc::PollResult::Error(error) => {
                self.idc_sessions.lock().remove(session_id);
                Err(AdminServiceError::InternalError(error.to_string()))
            }
            idc::PollResult::Success(token) => {
                self.idc_sessions.lock().remove(session_id);

                let mut new_cred = cred_template;
                new_cred.access_token = Some(token.access_token);
                new_cred.refresh_token = token.refresh_token;
                if let Some(expires_in) = token.expires_in {
                    let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);
                    new_cred.expires_at = Some(expires_at.to_rfc3339());
                }

                let credential_id = self
                    .token_manager
                    .add_credential(new_cred)
                    .await
                    .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

                tracing::info!("IdC 登录成功，已添加凭据 #{}", credential_id);
                Ok(PollIdcLoginResponse::Success { credential_id })
            }
        }
    }

    // ========================================================================
    // 请求日志和统计
    // ========================================================================

    /// 获取请求统计引用（用于外部记录）
    pub fn request_stats(&self) -> super::stats::SharedRequestStats {
        self.request_stats.clone()
    }

    /// 获取请求日志（最新在前）
    pub fn get_request_logs(&self) -> super::types::RequestLogsResponse {
        let logs = self.request_stats.get_logs();
        let total = logs.len();
        super::types::RequestLogsResponse { logs, total }
    }

    /// 清空请求日志
    pub fn clear_request_logs(&self) {
        self.request_stats.clear_logs();
    }

    /// 获取系统状态
    pub fn get_system_status(&self) -> super::types::SystemStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let available = snapshot.entries.iter().filter(|e| !e.disabled).count();

        super::types::SystemStatusResponse {
            status: "ok".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime: self.request_stats.uptime(),
            total_requests: self.request_stats.total_requests(),
            success_requests: self.request_stats.success_requests(),
            failed_requests: self.request_stats.failed_requests(),
            total_tokens: self.request_stats.total_tokens(),
            total_credits: self.request_stats.total_credits(),
            credentials_total: snapshot.entries.len(),
            credentials_available: available,
        }
    }

    /// 获取详细统计
    pub fn get_stats(&self) -> super::types::StatsResponse {
        let snapshot = self.token_manager.snapshot();
        let available = snapshot.entries.iter().filter(|e| !e.disabled).count();

        super::types::StatsResponse {
            total_requests: self.request_stats.total_requests(),
            success_requests: self.request_stats.success_requests(),
            failed_requests: self.request_stats.failed_requests(),
            total_tokens: self.request_stats.total_tokens(),
            total_credits: self.request_stats.total_credits(),
            uptime: self.request_stats.uptime(),
            credentials_total: snapshot.entries.len(),
            credentials_available: available,
        }
    }

    /// 重置统计
    pub fn reset_stats(&self) {
        self.request_stats.reset();
    }

    /// 获取版本信息
    pub fn get_version(&self) -> super::types::VersionResponse {
        super::types::VersionResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            name: "xkiro.rs".to_string(),
        }
    }

    /// 生成 Machine ID
    pub fn generate_machine_id(&self) -> super::types::GenerateMachineIdResponse {
        let uuid = uuid::Uuid::new_v4().to_string();
        // 格式化为 64 位十六进制字符串（与 Kiro IDE 一致）
        let machine_id = format!("{:0>64}", uuid.replace('-', ""));
        super::types::GenerateMachineIdResponse { machine_id }
    }

    // ========================================================================
    // 凭据连通性测试
    // ========================================================================

    /// 测试凭据连通性
    pub async fn test_credential(
        &self,
        id: u64,
    ) -> Result<super::types::CredentialTestResponse, AdminServiceError> {
        let snapshot = self.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| AdminServiceError::NotFound { id })?;

        if entry.disabled {
            return Ok(super::types::CredentialTestResponse {
                success: false,
                message: "凭据已禁用".to_string(),
                error: Some("credential_disabled".to_string()),
            });
        }

        // 尝试获取 usage limits 作为连通性测试
        match self.token_manager.get_usage_limits_for(id).await {
            Ok(_) => Ok(super::types::CredentialTestResponse {
                success: true,
                message: "连通性测试成功".to_string(),
                error: None,
            }),
            Err(e) => Ok(super::types::CredentialTestResponse {
                success: false,
                message: "连通性测试失败".to_string(),
                error: Some(e.to_string()),
            }),
        }
    }

    // ========================================================================
    // 批量操作
    // ========================================================================

    /// 批量操作凭据
    pub async fn batch_operation(
        &self,
        request: super::types::BatchOperationRequest,
    ) -> Result<super::types::BatchOperationResponse, AdminServiceError> {
        let mut results = Vec::new();
        let mut success_count = 0;
        let mut failure_count = 0;

        for id in request.ids {
            let result = match request.action.as_str() {
                "enable" => match self.set_disabled(id, false) {
                    Ok(_) => {
                        success_count += 1;
                        super::types::BatchOperationResultItem {
                            id,
                            success: true,
                            error: None,
                        }
                    }
                    Err(e) => {
                        failure_count += 1;
                        super::types::BatchOperationResultItem {
                            id,
                            success: false,
                            error: Some(e.to_string()),
                        }
                    }
                },
                "disable" => match self.set_disabled(id, true) {
                    Ok(_) => {
                        success_count += 1;
                        super::types::BatchOperationResultItem {
                            id,
                            success: true,
                            error: None,
                        }
                    }
                    Err(e) => {
                        failure_count += 1;
                        super::types::BatchOperationResultItem {
                            id,
                            success: false,
                            error: Some(e.to_string()),
                        }
                    }
                },
                "refresh" => match self.force_refresh_token(id).await {
                    Ok(_) => {
                        success_count += 1;
                        super::types::BatchOperationResultItem {
                            id,
                            success: true,
                            error: None,
                        }
                    }
                    Err(e) => {
                        failure_count += 1;
                        super::types::BatchOperationResultItem {
                            id,
                            success: false,
                            error: Some(e.to_string()),
                        }
                    }
                },
                _ => {
                    failure_count += 1;
                    super::types::BatchOperationResultItem {
                        id,
                        success: false,
                        error: Some(format!("未知操作: {}", request.action)),
                    }
                }
            };
            results.push(result);
        }

        Ok(super::types::BatchOperationResponse {
            results,
            success_count,
            failure_count,
        })
    }

    // ========================================================================
    // SSO Token 导入
    // ========================================================================

    /// 从 SSO Token 导入凭据
    pub async fn import_sso_token(
        &self,
        request: super::types::ImportSsoTokenRequest,
    ) -> Result<super::types::ImportSsoTokenResponse, AdminServiceError> {
        let tokens: Vec<&str> = request
            .token
            .split('\n')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect();

        if tokens.is_empty() {
            return Err(AdminServiceError::InvalidRequest(
                "未提供有效的 SSO Token".to_string(),
            ));
        }

        let mut results = Vec::new();
        let mut success_count = 0;
        let mut failure_count = 0;

        for (index, token) in tokens.iter().enumerate() {
            match self
                .import_single_sso_token(
                    token,
                    &request.region,
                    request.priority,
                    request.email.as_deref(),
                    request.proxy_url.as_deref(),
                )
                .await
            {
                Ok(credential_id) => {
                    success_count += 1;
                    results.push(super::types::SsoTokenImportResultItem {
                        index,
                        success: true,
                        credential_id: Some(credential_id),
                        email: None,
                        error: None,
                    });
                }
                Err(e) => {
                    failure_count += 1;
                    results.push(super::types::SsoTokenImportResultItem {
                        index,
                        success: false,
                        credential_id: None,
                        email: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        Ok(super::types::ImportSsoTokenResponse {
            results,
            success_count,
            failure_count,
        })
    }

    /// 导入单个 SSO Token
    async fn import_single_sso_token(
        &self,
        bearer_token: &str,
        region: &str,
        priority: u32,
        email: Option<&str>,
        proxy_url: Option<&str>,
    ) -> Result<u64, AdminServiceError> {
        let proxy_config = proxy_url.map(|url| {
            let proxy = crate::http_client::ProxyConfig::new(url);
            proxy
        });

        // 使用完整的 7 步 SSO Token 导入流程
        let token = crate::kiro::auth::idc::import_sso_token(
            bearer_token,
            region,
            &self.token_manager.config(),
            proxy_config.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(format!("SSO Token 导入失败: {}", e)))?;

        let new_cred = Self::sso_token_credential_from_import(token, region, priority, email);

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        tracing::info!("SSO Token 导入成功，已添加凭据 #{}", credential_id);
        Ok(credential_id)
    }

    fn sso_token_credential_from_import(
        token: idc::ImportedSsoToken,
        region: &str,
        priority: u32,
        email: Option<&str>,
    ) -> KiroCredentials {
        let mut new_cred = KiroCredentials {
            access_token: Some(token.access_token),
            refresh_token: token.refresh_token,
            auth_method: Some("idc".to_string()),
            client_id: Some(token.client_id),
            client_secret: Some(token.client_secret),
            priority,
            region: Some(region.to_string()),
            ..Default::default()
        };

        if let Some(expires_in) = token.expires_in {
            let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);
            new_cred.expires_at = Some(expires_at.to_rfc3339());
        }

        if let Some(email) = email {
            new_cred.email = Some(email.to_string());
        }

        new_cred
    }

    // ========================================================================
    // Builder ID 登录
    // ========================================================================

    fn builder_id_credential_template(
        client_id: String,
        client_secret: String,
        region: String,
        priority: u32,
        email: Option<String>,
        proxy_url: Option<String>,
    ) -> KiroCredentials {
        KiroCredentials {
            auth_method: Some("idc".to_string()),
            client_id: Some(client_id),
            client_secret: Some(client_secret),
            priority,
            region: Some(region),
            email,
            proxy_url,
            ..Default::default()
        }
    }

    fn builder_id_expires_in(expires_in: i64) -> i64 {
        if expires_in > 0 { expires_in } else { 600 }
    }

    /// 启动 Builder ID 登录
    pub async fn start_builder_id_login(
        &self,
        request: super::types::StartBuilderIdLoginRequest,
    ) -> Result<super::types::StartBuilderIdLoginResponse, AdminServiceError> {
        let region = request.region.trim();
        let region = if region.is_empty() {
            "us-east-1".to_string()
        } else {
            region.to_string()
        };
        let proxy_config = request.proxy_url.as_ref().map(|url| {
            let proxy = crate::http_client::ProxyConfig::new(url);
            proxy
        });

        let config = self.token_manager.config();
        let registered = crate::kiro::auth::idc::register_client(
            &region,
            crate::kiro::auth::idc::BUILDER_ID_START_URL,
            &config,
            proxy_config.as_ref(),
        )
        .await
        .map_err(|e| {
            AdminServiceError::InternalError(format!("注册 Builder ID 客户端失败: {}", e))
        })?;
        let device = crate::kiro::auth::idc::start_device_authorization(
            &region,
            crate::kiro::auth::idc::BUILDER_ID_START_URL,
            &registered.client_id,
            &registered.client_secret,
            &config,
            proxy_config.as_ref(),
        )
        .await
        .map_err(|e| {
            AdminServiceError::InternalError(format!("启动 Builder ID 设备授权失败: {}", e))
        })?;
        drop(config);

        let session_id = uuid::Uuid::new_v4().to_string();
        let expires_in = Self::builder_id_expires_in(device.expires_in);
        let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);
        let verification_uri = device
            .verification_uri_complete
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| device.verification_uri.clone());

        let cred_template = Self::builder_id_credential_template(
            registered.client_id.clone(),
            registered.client_secret.clone(),
            region.clone(),
            request.priority,
            request.email,
            request.proxy_url,
        );

        self.builder_id_sessions.lock().insert(
            session_id.clone(),
            BuilderIdAuthSession {
                region: region.clone(),
                client_id: registered.client_id,
                client_secret: registered.client_secret,
                device_code: device.device_code,
                user_code: device.user_code.clone(),
                verification_uri: verification_uri.clone(),
                verification_uri_complete: device.verification_uri_complete.clone(),
                expires_at,
                poll_interval: device.interval.max(5),
                cred_template,
                proxy: proxy_config,
            },
        );

        Ok(super::types::StartBuilderIdLoginResponse {
            session_id,
            user_code: device.user_code,
            verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            poll_interval: device.interval.max(5),
            expires_in,
        })
    }

    pub async fn complete_builder_id_login(
        &self,
        _req: CompleteIamSsoLoginRequest,
    ) -> Result<serde_json::Value, AdminServiceError> {
        Err(AdminServiceError::InvalidRequest(
            "Builder ID 使用设备码轮询流程，请调用 /auth/builderid/poll".to_string(),
        ))
    }

    /// 轮询 Builder ID 登录状态
    pub async fn poll_builder_id_login(
        &self,
        session_id: &str,
    ) -> Result<super::types::PollBuilderIdLoginResponse, AdminServiceError> {
        let session = {
            let sessions = self.builder_id_sessions.lock();
            sessions.get(session_id).cloned().ok_or_else(|| {
                AdminServiceError::InvalidRequest(format!("Builder ID 会话 {} 不存在", session_id))
            })?
        };

        if Utc::now() > session.expires_at {
            self.builder_id_sessions.lock().remove(session_id);
            return Ok(super::types::PollBuilderIdLoginResponse::Expired);
        }

        match crate::kiro::auth::idc::poll_token(
            &session.region,
            &session.client_id,
            &session.client_secret,
            &session.device_code,
            &self.token_manager.config(),
            session.proxy.as_ref(),
        )
        .await
        {
            crate::kiro::auth::idc::PollResult::Success(token) => {
                self.builder_id_sessions.lock().remove(session_id);

                let mut new_cred = session.cred_template;
                new_cred.access_token = Some(token.access_token);
                new_cred.refresh_token = token.refresh_token;
                if let Some(expires_in) = token.expires_in {
                    let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);
                    new_cred.expires_at = Some(expires_at.to_rfc3339());
                }
                if let Some(email) = crate::kiro::auth::kiro_sso::extract_email_from_jwt(
                    new_cred.access_token.as_deref().unwrap_or_default(),
                ) {
                    new_cred.email = Some(email);
                }
                let email = new_cred.email.clone();

                let credential_id = self
                    .token_manager
                    .add_credential(new_cred)
                    .await
                    .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

                tracing::info!("Builder ID 登录成功，已添加凭据 #{}", credential_id);
                Ok(super::types::PollBuilderIdLoginResponse::Success {
                    credential_id,
                    email,
                })
            }
            crate::kiro::auth::idc::PollResult::Pending => {
                Ok(super::types::PollBuilderIdLoginResponse::Pending {
                    interval: session.poll_interval,
                })
            }
            crate::kiro::auth::idc::PollResult::SlowDown => {
                let mut interval = session.poll_interval + 5;
                if let Some(session) = self.builder_id_sessions.lock().get_mut(session_id) {
                    session.poll_interval = interval;
                } else {
                    interval = 5;
                }
                Ok(super::types::PollBuilderIdLoginResponse::Pending { interval })
            }
            crate::kiro::auth::idc::PollResult::Expired => {
                self.builder_id_sessions.lock().remove(session_id);
                Ok(super::types::PollBuilderIdLoginResponse::Expired)
            }
            crate::kiro::auth::idc::PollResult::Error(e) => {
                self.builder_id_sessions.lock().remove(session_id);
                Ok(super::types::PollBuilderIdLoginResponse::Error {
                    message: e.to_string(),
                })
            }
        }
    }

    // ========================================================================
    // Kiro hosted SSO（Microsoft 365 / Entra ID）
    // ========================================================================

    pub async fn start_kiro_sso_login(
        &self,
        request: StartKiroSsoLoginRequest,
    ) -> Result<StartKiroSsoLoginResponse, AdminServiceError> {
        let global_proxy = {
            let config = self.token_manager.config();
            config.proxy_url.as_deref().map(ProxyConfig::new)
        };
        let config = self.token_manager.config();
        let started = kiro_sso::start_login_manual(&config, global_proxy.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        drop(config);
        let sign_in_url = started.sign_in_url.trim().to_string();
        if sign_in_url.is_empty() {
            return Err(AdminServiceError::InternalError(
                "Microsoft SSO 登录链接为空".to_string(),
            ));
        }

        let session_id = uuid::Uuid::new_v4().to_string();
        let expires_at = Utc::now() + chrono::Duration::minutes(10);
        let region = request
            .region
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("us-east-1")
            .to_string();

        let session = KiroSsoAuthSession {
            callback_rx: tokio::sync::Mutex::new(started.callback_rx),
            manual_callback_tx: started.manual_callback_tx,
            expires_at,
            cred_template: KiroCredentials {
                region: Some(region),
                machine_id: Some(self.generate_machine_id().machine_id),
                ..Default::default()
            },
            proxy: global_proxy,
            _server_handle: started.server_handle,
        };
        self.kiro_sso_sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartKiroSsoLoginResponse {
            session_id,
            sign_in_url,
            interval: 2,
        })
    }

    pub async fn cancel_kiro_sso_login(&self, session_id: &str) {
        self.kiro_sso_sessions.lock().remove(session_id);
    }

    pub async fn complete_kiro_sso_login_callback(
        &self,
        request: CompleteKiroSsoLoginRequest,
    ) -> Result<CompleteKiroSsoLoginResponse, AdminServiceError> {
        if request.callback_url.trim().is_empty() {
            return Err(AdminServiceError::InvalidRequest(
                "callbackUrl 不能为空".to_string(),
            ));
        }

        let manual_callback_tx = {
            let sessions = self.kiro_sso_sessions.lock();
            let Some(session) = sessions.get(&request.session_id) else {
                return Err(AdminServiceError::InvalidRequest(
                    "Kiro SSO 会话不存在".to_string(),
                ));
            };
            if Utc::now() >= session.expires_at {
                drop(sessions);
                self.kiro_sso_sessions.lock().remove(&request.session_id);
                return Ok(CompleteKiroSsoLoginResponse {
                    success: false,
                    status: "expired".to_string(),
                    redirect_url: None,
                    error: Some("SSO login timed out".to_string()),
                });
            }
            session.manual_callback_tx.clone()
        };

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        manual_callback_tx
            .send(kiro_sso::ManualCallbackRequest {
                callback_url: request.callback_url,
                response_tx,
            })
            .await
            .map_err(|_| AdminServiceError::InvalidRequest("Kiro SSO 会话已结束".to_string()))?;

        let result = response_rx.await.map_err(|_| {
            AdminServiceError::InvalidRequest("Kiro SSO 回调处理已结束".to_string())
        })?;

        Ok(match result {
            kiro_sso::ManualCallbackResult::Pending => CompleteKiroSsoLoginResponse {
                success: true,
                status: "pending".to_string(),
                redirect_url: None,
                error: None,
            },
            kiro_sso::ManualCallbackResult::Redirect(url) => {
                let redirect_url = url.trim().to_string();
                if redirect_url.is_empty() {
                    return Err(AdminServiceError::InternalError(
                        "Microsoft SSO 下一步登录链接为空".to_string(),
                    ));
                }
                CompleteKiroSsoLoginResponse {
                    success: true,
                    status: "redirect".to_string(),
                    redirect_url: Some(redirect_url),
                    error: None,
                }
            }
            kiro_sso::ManualCallbackResult::Submitted => CompleteKiroSsoLoginResponse {
                success: true,
                status: "submitted".to_string(),
                redirect_url: None,
                error: None,
            },
            kiro_sso::ManualCallbackResult::Failed(error) => CompleteKiroSsoLoginResponse {
                success: false,
                status: "error".to_string(),
                redirect_url: None,
                error: Some(error),
            },
        })
    }

    pub async fn poll_kiro_sso_login(
        &self,
        session_id: &str,
    ) -> Result<PollKiroSsoLoginResponse, AdminServiceError> {
        use tokio::sync::oneshot::error::TryRecvError;

        enum Outcome {
            Waiting,
            Expired,
            Cancelled,
            Received(kiro_sso::KiroSsoCapture),
        }

        let outcome = {
            let sessions = self.kiro_sso_sessions.lock();
            let Some(session) = sessions.get(session_id) else {
                return Ok(PollKiroSsoLoginResponse {
                    success: false,
                    completed: false,
                    status: None,
                    error: Some("session not found or expired".to_string()),
                    account: None,
                });
            };
            if Utc::now() >= session.expires_at {
                Outcome::Expired
            } else {
                match session.callback_rx.try_lock() {
                    Err(_) => Outcome::Waiting,
                    Ok(mut rx) => match rx.try_recv() {
                        Err(TryRecvError::Empty) => Outcome::Waiting,
                        Err(TryRecvError::Closed) => Outcome::Cancelled,
                        Ok(capture) => Outcome::Received(capture),
                    },
                }
            }
        };

        match outcome {
            Outcome::Waiting => Ok(PollKiroSsoLoginResponse {
                success: true,
                completed: false,
                status: Some("pending".to_string()),
                error: None,
                account: None,
            }),
            Outcome::Expired => {
                self.kiro_sso_sessions.lock().remove(session_id);
                Ok(PollKiroSsoLoginResponse {
                    success: false,
                    completed: false,
                    status: None,
                    error: Some("SSO login timed out".to_string()),
                    account: None,
                })
            }
            Outcome::Cancelled => {
                self.kiro_sso_sessions.lock().remove(session_id);
                Ok(PollKiroSsoLoginResponse {
                    success: false,
                    completed: false,
                    status: None,
                    error: Some("登录已被取消".to_string()),
                    account: None,
                })
            }
            Outcome::Received(capture) => self.complete_kiro_sso_login(session_id, capture).await,
        }
    }

    async fn complete_kiro_sso_login(
        &self,
        session_id: &str,
        capture: kiro_sso::KiroSsoCapture,
    ) -> Result<PollKiroSsoLoginResponse, AdminServiceError> {
        let session = self
            .kiro_sso_sessions
            .lock()
            .remove(session_id)
            .ok_or_else(|| AdminServiceError::InvalidRequest("Kiro SSO 会话不存在".to_string()))?;

        let config = self.token_manager.config();
        let mut new_cred = session.cred_template;
        match capture.kind {
            kiro_sso::KiroSsoCaptureKind::Social => {
                let machine_id = new_cred
                    .machine_id
                    .clone()
                    .unwrap_or_else(|| self.generate_machine_id().machine_id);
                new_cred.machine_id = Some(machine_id.clone());
                let token = kiro_sso::exchange_social_code(
                    &capture.code,
                    &capture.code_verifier,
                    &capture.redirect_uri,
                    &machine_id,
                    &config,
                    session.proxy.as_ref(),
                )
                .await
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
                let email = kiro_sso::extract_email_from_jwt(&token.access_token);
                new_cred.access_token = Some(token.access_token);
                new_cred.refresh_token = token.refresh_token;
                new_cred.profile_arn = token.profile_arn;
                new_cred.auth_method = Some("social".to_string());
                new_cred.email = email;
                if let Some(expires_at) = token.expires_at {
                    new_cred.expires_at = Some(expires_at);
                } else if let Some(expires_in) = token.expires_in {
                    new_cred.expires_at =
                        Some((Utc::now() + chrono::Duration::seconds(expires_in)).to_rfc3339());
                }
            }
            kiro_sso::KiroSsoCaptureKind::ExternalIdp => {
                let token_endpoint = capture.token_endpoint.as_deref().ok_or_else(|| {
                    AdminServiceError::InvalidRequest("缺少 tokenEndpoint".to_string())
                })?;
                let client_id = capture.client_id.as_deref().ok_or_else(|| {
                    AdminServiceError::InvalidRequest("缺少 clientId".to_string())
                })?;
                let token = kiro_sso::exchange_external_idp_code(
                    token_endpoint,
                    client_id,
                    &capture.code,
                    &capture.code_verifier,
                    &capture.redirect_uri,
                    capture.scopes.as_deref(),
                    &config,
                    session.proxy.as_ref(),
                )
                .await
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
                let email = kiro_sso::extract_email_from_jwt(&token.access_token);
                new_cred.access_token = Some(token.access_token);
                new_cred.refresh_token = token.refresh_token;
                new_cred.auth_method = Some("external_idp".to_string());
                new_cred.client_id = Some(client_id.to_string());
                new_cred.token_endpoint = capture.token_endpoint;
                new_cred.issuer_url = capture.issuer_url;
                new_cred.scopes = capture.scopes;
                new_cred.email = email;
                if let Some(expires_in) = token.expires_in {
                    new_cred.expires_at =
                        Some((Utc::now() + chrono::Duration::seconds(expires_in)).to_rfc3339());
                }
            }
        }
        drop(config);

        let email = new_cred.email.clone();
        let auth_method = new_cred.auth_method.clone();
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(PollKiroSsoLoginResponse {
            success: true,
            completed: true,
            status: None,
            error: None,
            account: Some(KiroSsoAccountResponse {
                id: credential_id,
                email,
                auth_method,
            }),
        })
    }

    // ========================================================================
    // API Key 管理
    // ========================================================================

    /// 从磁盘加载 API Keys
    fn load_api_keys_from(path: &Option<PathBuf>) -> Vec<super::types::ApiKeyEntry> {
        if let Some(path) = path {
            if path.exists() {
                match std::fs::read_to_string(path) {
                    Ok(data) => match serde_json::from_str(&data) {
                        Ok(keys) => return keys,
                        Err(e) => {
                            tracing::warn!("解析 API Keys 文件失败: {}", e);
                        }
                    },
                    Err(e) => {
                        tracing::warn!("读取 API Keys 文件失败: {}", e);
                    }
                }
            }
        }
        Vec::new()
    }

    pub fn load_api_keys_runtime(cache_dir: Option<&std::path::Path>) -> SharedApiKeys {
        let path = cache_dir.map(|d| d.join("kiro_api_keys.json"));
        Arc::new(RwLock::new(Self::load_api_keys_from(&path)))
    }

    pub fn load_api_keys_runtime_with_legacy(
        cache_dir: Option<&std::path::Path>,
        legacy_api_key: Option<&str>,
        require_api_key: bool,
    ) -> SharedApiKeys {
        let path = cache_dir.map(|d| d.join("kiro_api_keys.json"));
        let mut keys = Self::load_api_keys_from(&path);
        let legacy_api_key = legacy_api_key.map(str::trim).filter(|key| !key.is_empty());

        if keys.is_empty() {
            if let Some(legacy_api_key) = legacy_api_key {
                keys.push(super::types::ApiKeyEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: Some("legacy-api-key".to_string()),
                    key: legacy_api_key.to_string(),
                    enabled: require_api_key,
                    created_at: Utc::now().timestamp(),
                    last_used_at: None,
                    token_limit: 0,
                    credit_limit: 0.0,
                    tokens_used: 0,
                    credits_used: 0.0,
                    requests_count: 0,
                });

                if let Some(path) = &path {
                    match serde_json::to_string_pretty(&keys) {
                        Ok(data) => {
                            if let Err(e) = std::fs::write(path, data) {
                                tracing::warn!(error = %e, "持久化迁移 API Key 失败");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "序列化迁移 API Key 失败");
                        }
                    }
                }
            }
        }

        Arc::new(RwLock::new(keys))
    }

    /// 保存 API Keys 到磁盘
    fn save_api_keys(&self) -> Result<(), AdminServiceError> {
        let path = self
            .token_manager
            .cache_dir()
            .map(|d| d.join("kiro_api_keys.json"));
        if let Some(path) = path {
            let keys = self.api_keys.read();
            let data = serde_json::to_string_pretty(&*keys)
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
            std::fs::write(&path, data)
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        }
        Ok(())
    }

    /// 获取所有 API Keys（脱敏）
    pub fn get_api_keys(&self) -> super::types::ApiKeyListResponse {
        let keys = self.api_keys.read();
        let api_keys = keys.iter().map(to_api_key_view).collect();
        super::types::ApiKeyListResponse { api_keys }
    }

    /// 创建 API Key
    pub fn create_api_key(
        &self,
        request: super::types::CreateApiKeyRequest,
    ) -> Result<super::types::CreateApiKeyResponse, AdminServiceError> {
        let key_value = request
            .key
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .unwrap_or_else(generate_api_key_value);
        let id = uuid::Uuid::new_v4().to_string();
        if self.api_keys.read().iter().any(|k| k.key == key_value) {
            return Err(AdminServiceError::InvalidRequest(
                "api key already exists".to_string(),
            ));
        }

        let entry = super::types::ApiKeyEntry {
            id: id.clone(),
            name: request.name,
            key: key_value.clone(),
            enabled: request.enabled.unwrap_or(true),
            created_at: Utc::now().timestamp(),
            last_used_at: None,
            token_limit: request.token_limit,
            credit_limit: request.credit_limit,
            tokens_used: 0,
            credits_used: 0.0,
            requests_count: 0,
        };

        self.api_keys.write().push(entry.clone());
        if let Err(e) = self.save_api_keys() {
            self.api_keys.write().retain(|k| k.id != id);
            return Err(e);
        }

        Ok(super::types::CreateApiKeyResponse {
            success: true,
            id,
            key: key_value,
            api_key: to_api_key_view(&entry),
        })
    }

    /// 获取单个 API Key
    pub fn get_api_key(&self, id: &str) -> Result<super::types::ApiKeyView, AdminServiceError> {
        let keys = self.api_keys.read();
        keys.iter()
            .find(|k| k.id == id)
            .map(to_api_key_view)
            .ok_or_else(|| AdminServiceError::ResourceNotFound("API key not found".to_string()))
    }

    /// 更新 API Key
    pub fn update_api_key(
        &self,
        id: &str,
        request: super::types::UpdateApiKeyRequest,
    ) -> Result<super::types::ApiKeyView, AdminServiceError> {
        let mut keys = self.api_keys.write();
        let index = keys
            .iter()
            .position(|k| k.id == id)
            .ok_or_else(|| AdminServiceError::ResourceNotFound("API key not found".to_string()))?;

        if let Some(name) = request.name {
            keys[index].name = name;
        }
        if let Some(key) = request.key {
            let key = key.trim().to_string();
            if !key.is_empty() {
                if keys
                    .iter()
                    .enumerate()
                    .any(|(i, entry)| i != index && entry.key == key)
                {
                    return Err(AdminServiceError::InvalidRequest(
                        "api key value collides with existing entry".to_string(),
                    ));
                }
                keys[index].key = key;
            }
        }
        if let Some(enabled) = request.enabled {
            keys[index].enabled = enabled;
        }
        if let Some(token_limit) = request.token_limit {
            keys[index].token_limit = token_limit;
        }
        if let Some(credit_limit) = request.credit_limit {
            keys[index].credit_limit = credit_limit;
        }

        let view = to_api_key_view(&keys[index]);
        drop(keys);
        self.save_api_keys()?;
        Ok(view)
    }

    /// 删除 API Key
    pub fn delete_api_key(&self, id: &str) -> Result<(), AdminServiceError> {
        let mut keys = self.api_keys.write();
        if let Some(index) = keys.iter().position(|k| k.id == id) {
            keys.remove(index);
            drop(keys);
            self.save_api_keys()?;
        }
        Ok(())
    }

    /// 重置 API Key 使用量
    pub fn reset_api_key_usage(
        &self,
        id: &str,
    ) -> Result<super::types::ApiKeyView, AdminServiceError> {
        let mut keys = self.api_keys.write();
        let entry = keys
            .iter_mut()
            .find(|k| k.id == id)
            .ok_or_else(|| AdminServiceError::ResourceNotFound("API key not found".to_string()))?;

        entry.tokens_used = 0;
        entry.credits_used = 0.0;
        entry.requests_count = 0;
        let view = to_api_key_view(entry);

        drop(keys);
        self.save_api_keys()?;
        Ok(view)
    }

    /// 验证 API Key（用于认证中间件）
    pub fn validate_api_key(&self, key: &str) -> Option<super::types::ApiKeyEntry> {
        let keys = self.api_keys.read();
        keys.iter().find(|k| k.key == key && k.enabled).cloned()
    }

    /// 记录 API Key 使用量
    pub fn record_api_key_usage(&self, key_id: &str, tokens: i64, credits: f64) {
        let mut keys = self.api_keys.write();
        if let Some(entry) = keys.iter_mut().find(|k| k.id == key_id) {
            if tokens > 0 {
                entry.tokens_used += tokens;
            }
            if credits > 0.0 {
                entry.credits_used += credits;
            }
            entry.requests_count += 1;
            entry.last_used_at = Some(Utc::now().timestamp());
        }
        drop(keys);
        if let Err(e) = self.save_api_keys() {
            tracing::warn!(error = %e, "保存 API Key 使用量失败");
        }
    }
}

fn to_api_key_view(entry: &super::types::ApiKeyEntry) -> super::types::ApiKeyView {
    super::types::ApiKeyView {
        id: entry.id.clone(),
        name: entry.name.clone(),
        key_masked: mask_api_key(&entry.key),
        enabled: entry.enabled,
        created_at: entry.created_at,
        last_used_at: entry.last_used_at,
        token_limit: entry.token_limit,
        credit_limit: entry.credit_limit,
        tokens_used: entry.tokens_used,
        credits_used: entry.credits_used,
        requests_count: entry.requests_count,
    }
}

/// 生成 API Key 值
fn generate_api_key_value() -> String {
    let bytes: Vec<u8> = (0..32).map(|_| fastrand::u8(..)).collect();
    format!("sk-{}", hex::encode(bytes))
}

/// 脱敏 API Key
fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    if key.len() <= 10 {
        return key.to_string();
    }
    format!("{}****{}", &key[..6], &key[key.len() - 4..])
}

fn parse_aws_sso_callback_params(
    callback_url: &str,
) -> Result<HashMap<String, String>, AdminServiceError> {
    oauth_callback::parse_input(callback_url)
        .map(|callback| callback.params)
        .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))
}

#[cfg(test)]
mod builder_id_login_tests {
    use super::*;

    #[test]
    fn builder_id_template_preserves_oidc_registration_for_refresh() {
        let cred = AdminService::builder_id_credential_template(
            "client-id".to_string(),
            "client-secret".to_string(),
            "us-east-1".to_string(),
            3,
            Some("user@example.com".to_string()),
            Some("direct".to_string()),
        );

        assert_eq!(cred.auth_method.as_deref(), Some("idc"));
        assert_eq!(cred.client_id.as_deref(), Some("client-id"));
        assert_eq!(cred.client_secret.as_deref(), Some("client-secret"));
        assert_eq!(cred.region.as_deref(), Some("us-east-1"));
        assert_eq!(cred.priority, 3);
        assert_eq!(cred.email.as_deref(), Some("user@example.com"));
        assert_eq!(cred.proxy_url.as_deref(), Some("direct"));
    }

    #[test]
    fn builder_id_expires_in_falls_back_like_kiro_go() {
        assert_eq!(AdminService::builder_id_expires_in(0), 600);
        assert_eq!(AdminService::builder_id_expires_in(-1), 600);
        assert_eq!(AdminService::builder_id_expires_in(900), 900);
    }
}

#[cfg(test)]
mod sso_token_import_tests {
    use super::*;

    #[test]
    fn sso_token_import_preserves_oidc_registration_for_refresh() {
        let cred = AdminService::sso_token_credential_from_import(
            idc::ImportedSsoToken {
                access_token: "access-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                expires_in: Some(3600),
                client_id: "client-id".to_string(),
                client_secret: "client-secret".to_string(),
            },
            "us-east-1",
            7,
            Some("user@example.com"),
        );

        assert_eq!(cred.auth_method.as_deref(), Some("idc"));
        assert_eq!(cred.access_token.as_deref(), Some("access-token"));
        assert_eq!(cred.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(cred.client_id.as_deref(), Some("client-id"));
        assert_eq!(cred.client_secret.as_deref(), Some("client-secret"));
        assert_eq!(cred.region.as_deref(), Some("us-east-1"));
        assert_eq!(cred.priority, 7);
        assert_eq!(cred.email.as_deref(), Some("user@example.com"));
        assert!(cred.expires_at.is_some());
    }
}

#[cfg(test)]
mod kiro_go_import_tests {
    use super::*;

    #[test]
    fn normalizes_external_idp_before_social_or_idc_fallbacks() {
        assert_eq!(
            AdminService::normalize_kiro_go_import_auth_method(
                Some("AzureAD"),
                Some("client"),
                None,
                None,
            ),
            "external_idp"
        );
        assert_eq!(
            AdminService::normalize_kiro_go_import_auth_method(
                None,
                Some("client"),
                None,
                Some("https://login.microsoftonline.com/t/oauth2/v2.0/token"),
            ),
            "external_idp"
        );
        assert_eq!(
            AdminService::normalize_kiro_go_import_auth_method(
                Some("enterprise"),
                Some("client"),
                Some("secret"),
                None,
            ),
            "idc"
        );
        assert_eq!(
            AdminService::normalize_kiro_go_import_auth_method(None, Some("client"), None, None),
            "idc"
        );
        assert_eq!(
            AdminService::normalize_kiro_go_import_auth_method(
                Some("weird"),
                Some("client"),
                None,
                None,
            ),
            "social"
        );
    }

    #[test]
    fn parses_numeric_kiro_go_import_id_only() {
        assert_eq!(
            AdminService::parse_kiro_go_import_id(Some(&serde_json::json!(42))),
            Some(42)
        );
        assert_eq!(
            AdminService::parse_kiro_go_import_id(Some(&serde_json::json!("43"))),
            Some(43)
        );
        assert_eq!(
            AdminService::parse_kiro_go_import_id(Some(&serde_json::json!("account-1"))),
            None
        );
    }

    #[test]
    fn maps_kiro_go_import_enabled_and_disabled_flags() {
        assert!(!AdminService::kiro_go_import_disabled(None, None));
        assert!(AdminService::kiro_go_import_disabled(
            Some(true),
            Some(true)
        ));
        assert!(!AdminService::kiro_go_import_disabled(
            Some(false),
            Some(false)
        ));
        assert!(AdminService::kiro_go_import_disabled(None, Some(false)));
        assert!(!AdminService::kiro_go_import_disabled(None, Some(true)));
    }

    #[test]
    fn kiro_go_import_request_accepts_reference_metadata_fields() {
        let req: KiroGoImportCredentialsRequest = serde_json::from_value(serde_json::json!({
            "refreshToken": "r",
            "provider": "Enterprise",
            "userId": "user-1",
            "startUrl": "https://d-123.awsapps.com/start",
            "clientIdHash": "hash-1",
            "idToken": "id-token-1",
            "ssoSessionId": "session-1",
            "weight": 5
        }))
        .unwrap();

        assert_eq!(req.provider.as_deref(), Some("Enterprise"));
        assert_eq!(req.user_id.as_deref(), Some("user-1"));
        assert_eq!(
            req.start_url.as_deref(),
            Some("https://d-123.awsapps.com/start")
        );
        assert_eq!(req.client_id_hash.as_deref(), Some("hash-1"));
        assert_eq!(req.id_token.as_deref(), Some("id-token-1"));
        assert_eq!(req.sso_session_id.as_deref(), Some("session-1"));
        assert_eq!(req.weight, 5);
    }

    #[test]
    fn token_json_item_accepts_kiro_go_weight_field() {
        let item: TokenJsonItem = serde_json::from_value(serde_json::json!({
            "provider": "Social",
            "refreshToken": "r",
            "authMethod": "social",
            "weight": 4
        }))
        .unwrap();

        assert_eq!(item.weight, 4);
    }
}

#[cfg(test)]
mod aws_sso_callback_tests {
    use super::*;

    #[test]
    fn parses_full_callback_url() {
        let params =
            parse_aws_sso_callback_params("http://127.0.0.1/oauth/callback?code=abc&state=xyz")
                .unwrap();

        assert_eq!(params.get("code").map(String::as_str), Some("abc"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
    }

    #[test]
    fn parses_path_and_query_callback_like_kiro_account_manager() {
        let params =
            parse_aws_sso_callback_params("/oauth/callback?code=abc%2Fdef&state=xyz").unwrap();

        assert_eq!(params.get("code").map(String::as_str), Some("abc/def"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn temp_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xkiro-admin-settings-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_service(
        config: crate::model::config::Config,
        credentials_path: PathBuf,
    ) -> (
        AdminService,
        Arc<RwLock<String>>,
        Arc<AtomicBool>,
        Arc<RwLock<String>>,
    ) {
        let token_manager = Arc::new(
            MultiTokenManager::new(
                config.clone(),
                Vec::new(),
                None,
                Some(credentials_path),
                true,
            )
            .unwrap(),
        );
        let client_api_key_runtime =
            Arc::new(RwLock::new(config.api_key.clone().unwrap_or_default()));
        let require_api_key_runtime = Arc::new(AtomicBool::new(config.require_api_key));
        let admin_api_key_runtime = Arc::new(RwLock::new(
            config.admin_api_key.clone().unwrap_or_default(),
        ));

        let service = AdminService::new(
            token_manager,
            None,
            Arc::new(RwLock::new(config.compression.clone())),
            client_api_key_runtime.clone(),
            require_api_key_runtime.clone(),
            admin_api_key_runtime.clone(),
            Arc::new(RwLock::new(config.prompt_filter.clone())),
            Arc::new(RwLock::new(ThinkingRuntimeConfig {
                suffix: config.thinking_suffix.clone(),
                openai_format: config.openai_thinking_format.clone(),
                claude_format: config.claude_thinking_format.clone(),
            })),
            Arc::new(RwLock::new(PromptCacheRuntime::new(
                config.prompt_cache_ttl_seconds,
                config.prompt_cache_accounting_enabled,
            ))),
            crate::model::runtime::shared_from_config(&config),
            Arc::new(RwLock::new(Vec::new())),
            Vec::<String>::new(),
        );

        (
            service,
            client_api_key_runtime,
            require_api_key_runtime,
            admin_api_key_runtime,
        )
    }

    fn jwt_with_exp(exp: i64) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_kiro_go_credential_persists_weight_without_refresh_network() {
        let dir = temp_test_dir("kiro-go-weight-import");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let access_token = jwt_with_exp((Utc::now() + chrono::Duration::hours(1)).timestamp());

        let req: KiroGoImportCredentialsRequest = serde_json::from_value(serde_json::json!({
            "refreshToken": "r".repeat(150),
            "accessToken": access_token,
            "authMethod": "external_idp",
            "clientId": "client-1",
            "tokenEndpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
            "issuerUrl": "https://login.microsoftonline.com/tenant/v2.0",
            "priority": 2,
            "weight": 7,
            "concurrency": 3,
            "disabled": true
        }))
        .unwrap();

        let added = service.import_kiro_go_credential(req).await.unwrap();
        let snapshot = service.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == added.credential_id)
            .unwrap();

        assert_eq!(entry.priority, 2);
        assert_eq!(entry.weight, 7);
        assert_eq!(entry.concurrency, Some(3));
        assert!(entry.disabled);
    }

    #[test]
    fn update_kiro_go_account_weight_does_not_change_priority() {
        let dir = temp_test_dir("kiro-go-weight-update");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        cred.priority = 9;
        cred.weight = 1;
        let id = service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();

        service
            .update_kiro_go_account(
                id,
                KiroGoUpdateAccountRequest {
                    enabled: None,
                    weight: Some(6),
                    proxy_url: None,
                },
            )
            .unwrap();

        let snapshot = service.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .unwrap();
        assert_eq!(entry.priority, 9);
        assert_eq!(entry.weight, 6);
    }

    #[test]
    fn export_token_json_preserves_weight() {
        let dir = temp_test_dir("token-json-weight-export");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        cred.auth_method = Some("social".to_string());
        cred.priority = 2;
        cred.weight = 5;
        let id = service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();

        let exported = service.export_credentials_to_token_json(&[id]);

        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].priority, 2);
        assert_eq!(exported[0].weight, 5);
    }

    #[test]
    fn export_kam_preserves_reference_account_metadata() {
        let dir = temp_test_dir("kam-metadata-export");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.access_token = Some("access-token".to_string());
        cred.refresh_token = Some("r".repeat(150));
        cred.auth_method = Some("idc".to_string());
        cred.provider = Some("Enterprise".to_string());
        cred.user_id = Some("user-1".to_string());
        cred.client_id = Some("client-1".to_string());
        cred.client_secret = Some("secret-1".to_string());
        cred.region = Some("us-east-1".to_string());
        cred.start_url = Some("https://d-123.awsapps.com/start".to_string());
        cred.client_id_hash = Some("hash-1".to_string());
        cred.id_token = Some("id-token-1".to_string());
        cred.sso_session_id = Some("session-1".to_string());

        let id = service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();
        let exported = service.export_credentials_to_kam(&[id]);

        assert_eq!(exported.len(), 1);
        let item = &exported[0];
        assert_eq!(item.provider.as_deref(), Some("Enterprise"));
        assert_eq!(item.user_id.as_deref(), Some("user-1"));
        assert_eq!(
            item.start_url.as_deref(),
            Some("https://d-123.awsapps.com/start")
        );
        assert_eq!(item.client_id_hash.as_deref(), Some("hash-1"));
        assert_eq!(item.id_token.as_deref(), Some("id-token-1"));
        assert_eq!(item.sso_session_id.as_deref(), Some("session-1"));
    }

    #[tokio::test]
    async fn update_settings_patch_empty_password_preserves_admin_key_like_kiro_go() {
        let dir = temp_test_dir("empty-password");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let mut config = crate::model::config::Config::load(&config_path).unwrap();
        config.api_key = Some("proxy-api-key".to_string());
        config.require_api_key = true;
        config.admin_api_key = Some("admin-password".to_string());
        config.save().unwrap();

        let (service, client_runtime, require_runtime, admin_runtime) =
            test_service(config, credentials_path);

        service
            .update_settings(UpdateSettingsRequest {
                api_key: Some(String::new()),
                require_api_key: Some(false),
                password: Some("   ".to_string()),
                allow_over_usage: None,
            })
            .await
            .unwrap();

        let reloaded = crate::model::config::Config::load(&config_path).unwrap();
        assert_eq!(reloaded.api_key, None);
        assert!(!reloaded.require_api_key);
        assert_eq!(reloaded.admin_api_key.as_deref(), Some("admin-password"));
        assert_eq!(client_runtime.read().as_str(), "");
        assert!(!require_runtime.load(Ordering::Relaxed));
        assert_eq!(admin_runtime.read().as_str(), "admin-password");

        service
            .update_settings(UpdateSettingsRequest {
                api_key: None,
                require_api_key: None,
                password: Some(" new-admin-password ".to_string()),
                allow_over_usage: None,
            })
            .await
            .unwrap();

        let reloaded = crate::model::config::Config::load(&config_path).unwrap();
        assert_eq!(reloaded.api_key, None);
        assert!(!reloaded.require_api_key);
        assert_eq!(
            reloaded.admin_api_key.as_deref(),
            Some("new-admin-password")
        );
        assert_eq!(admin_runtime.read().as_str(), "new-admin-password");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn update_api_key_empty_key_preserves_existing_value_like_kiro_go() {
        let dir = temp_test_dir("api-key-empty-patch");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let config = crate::model::config::Config::load(&config_path).unwrap();
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let first = service
            .create_api_key(CreateApiKeyRequest {
                name: Some("alpha".to_string()),
                key: Some("sk-alpha".to_string()),
                enabled: Some(true),
                token_limit: 1000,
                credit_limit: 1.0,
            })
            .unwrap();
        let second = service
            .create_api_key(CreateApiKeyRequest {
                name: Some("beta".to_string()),
                key: Some("sk-beta".to_string()),
                enabled: Some(true),
                token_limit: 0,
                credit_limit: 0.0,
            })
            .unwrap();

        let updated = service
            .update_api_key(
                &first.id,
                UpdateApiKeyRequest {
                    name: Some(Some("alpha-renamed".to_string())),
                    key: Some("   ".to_string()),
                    enabled: Some(false),
                    token_limit: Some(2000),
                    credit_limit: Some(5.5),
                },
            )
            .unwrap();

        assert_eq!(updated.name.as_deref(), Some("alpha-renamed"));
        assert!(!updated.enabled);
        assert_eq!(updated.token_limit, 2000);
        assert_eq!(updated.credit_limit, 5.5);
        assert_eq!(updated.key_masked, mask_api_key("sk-alpha"));
        let first_entry = service
            .api_keys
            .read()
            .iter()
            .find(|entry| entry.id == first.id)
            .cloned()
            .unwrap();
        assert_eq!(first_entry.key, "sk-alpha");
        assert!(!first_entry.enabled);

        let err = service
            .update_api_key(
                &first.id,
                UpdateApiKeyRequest {
                    name: None,
                    key: Some("sk-beta".to_string()),
                    enabled: None,
                    token_limit: None,
                    credit_limit: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, AdminServiceError::InvalidRequest(_)));
        let first_entry = service
            .api_keys
            .read()
            .iter()
            .find(|entry| entry.id == first.id)
            .cloned()
            .unwrap();
        assert_eq!(first_entry.key, "sk-alpha");
        assert_eq!(service.validate_api_key("sk-beta").unwrap().id, second.id);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_api_keys_migrates_legacy_api_key_like_kiro_go() {
        let dir = temp_test_dir("api-key-legacy-migration");

        let keys = AdminService::load_api_keys_runtime_with_legacy(
            Some(&dir),
            Some(" legacy-secret "),
            true,
        );
        let snapshot = keys.read().clone();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].key, "legacy-secret");
        assert!(snapshot[0].enabled);
        assert_eq!(snapshot[0].name.as_deref(), Some("legacy-api-key"));

        let persisted: Vec<ApiKeyEntry> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("kiro_api_keys.json")).unwrap())
                .unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].key, "legacy-secret");

        let reloaded = AdminService::load_api_keys_runtime_with_legacy(
            Some(&dir),
            Some("legacy-secret"),
            true,
        );
        assert_eq!(reloaded.read().len(), 1);
        assert_eq!(reloaded.read()[0].id, snapshot[0].id);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_api_keys_migrates_public_legacy_api_key_disabled_like_kiro_go() {
        let dir = temp_test_dir("api-key-public-migration");

        let keys = AdminService::load_api_keys_runtime_with_legacy(
            Some(&dir),
            Some("legacy-secret"),
            false,
        );
        let snapshot = keys.read().clone();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].key, "legacy-secret");
        assert!(!snapshot[0].enabled);

        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod api_key_tests {
    use super::*;

    #[test]
    fn mask_api_key_matches_kiro_go() {
        assert_eq!(mask_api_key(""), "");
        assert_eq!(mask_api_key("short"), "short");
        assert_eq!(mask_api_key("sk-1234567890abcdef"), "sk-123****cdef");
    }

    #[test]
    fn api_key_view_exposes_masked_key_only() {
        let view = to_api_key_view(&super::super::types::ApiKeyEntry {
            id: "id-1".to_string(),
            name: Some("main".to_string()),
            key: "sk-1234567890abcdef".to_string(),
            enabled: true,
            created_at: 100,
            last_used_at: Some(200),
            token_limit: 10,
            credit_limit: 1.5,
            tokens_used: 3,
            credits_used: 0.5,
            requests_count: 2,
        });

        let value = serde_json::to_value(view).expect("view should serialize");
        assert_eq!(value["keyMasked"], "sk-123****cdef");
        assert!(value.get("key").is_none());
        assert_eq!(value["lastUsedAt"], 200);
    }

    #[test]
    fn prompt_filter_dto_matches_kiro_go_fields() {
        let response = PromptFilterConfigResponse {
            filter_claude_code: false,
            filter_env_noise: false,
            filter_strip_boundaries: false,
            rules: Vec::new(),
        };

        let value = serde_json::to_value(response).expect("response should serialize");
        assert!(value.get("filterStripRestrictions").is_none());

        let request: UpdatePromptFilterConfigRequest = serde_json::from_value(serde_json::json!({
            "filterClaudeCode": false,
            "filterEnvNoise": false,
            "filterStripBoundaries": false,
            "rules": []
        }))
        .expect("Kiro-Go-shaped prompt filter request should parse");
        assert!(!request.filter_strip_boundaries);
    }
}
