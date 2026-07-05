//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::anthropic::converter::convert_request_with_thinking_suffix;
use crate::anthropic::middleware::{PromptCacheRuntime, SharedApiKeys, ThinkingRuntimeConfig};
use crate::anthropic::types::{Message, MessagesRequest, Metadata};
use crate::common::utf8::floor_char_boundary;
use crate::http_client::ProxyConfig;
use crate::kiro::auth::{idc, kiro_sso, oauth_callback, social};
use crate::kiro::endpoint::{
    AMAZONQ_ENDPOINT_NAME, CLI_ENDPOINT_NAME, CODEWHISPERER_ENDPOINT_NAME, IDE_ENDPOINT_NAME,
};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::{CredentialSourceMetadata, KiroCredentials};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::{InferenceConfig, KiroRequest};
use crate::kiro::model::usage_limits::{UsageLimitsResponse, normalize_subscription_type};
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::kiro::proxy_manager::{ProxyEntry, ProxyManager};
use crate::kiro::token_manager::{
    LOW_BALANCE_THRESHOLD, MultiTokenManager, get_usage_limits, refresh_token,
};
use crate::model::config::{
    CompressionConfig, CredentialMachineIdStrategy, PromptFilterConfig, SystemPromptPosition,
    UserPreset,
};
use crate::model::runtime::{SharedModelMappingConfig, SharedPromptConfig};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::error::AdminServiceError;
use super::types::{
    AccessSettingsResponse, AddCredentialRequest, AddCredentialResponse, ApiKeyEntry,
    ApiKeyListResponse, BalanceResponse, BatchOperationRequest, BatchOperationResponse,
    BatchOperationResultItem, BatchRefreshBalanceResponse, BatchRefreshBalanceResultItem,
    BatchRefreshResponse, BatchRefreshResultItem, CachedBalanceItem, CachedBalancesResponse,
    CachedCredentialModelsResponse, CommonConfigResponse, CompleteIamSsoLoginRequest,
    CompleteIamSsoLoginResponse, CompleteKiroSsoLoginRequest, CompleteKiroSsoLoginResponse,
    CompleteSocialCallbackRequest, CompleteSocialLoginRequest, CompressionConfigResponse,
    CreateApiKeyRequest, CreateApiKeyResponse, CredentialAliasUpdateRequest,
    CredentialAliasViewItem, CredentialBackup, CredentialBackupEntry, CredentialBackupSource,
    CredentialBatchRequest, CredentialBatchResponse, CredentialCacheItem, CredentialExportMaterial,
    CredentialFullExportResponse, CredentialImportAction, CredentialImportItem,
    CredentialImportMode, CredentialImportSummary, CredentialLoginDetailsResponse,
    CredentialModelsResponse, CredentialOverageResponse, CredentialProbeResponse,
    CredentialRefreshInfo, CredentialRefreshResponse, CredentialSnapshotExportData,
    CredentialSnapshotExportItem, CredentialSnapshotExportSubscription,
    CredentialSnapshotExportUsage, CredentialStatusItem, CredentialTestResponse,
    CredentialsStatusResponse, EndpointConfigResponse, GenerateMachineIdResponse,
    GlobalConfigResponse, ImportCredentialRecordRequest, ImportCredentialsRequest,
    ImportCredentialsResponse, ImportSsoTokenRequest, ImportSsoTokenResponse,
    ModelMappingsResponse, PollBuilderIdLoginResponse, PollIdcLoginResponse,
    PollKiroSsoLoginResponse, PollSocialLoginResponse, PresetItem, PromptFilterConfigResponse,
    PromptFilterRuleDto, ProxyAutoAssignRequest, ProxyAutoAssignResponse, ProxyConfigResponse,
    ProxyImportRequest, ProxyImportResponse, ProxyItem, ProxyListResponse, ProxyTestResponse,
    ProxyUpsertRequest, ProxyUrlConfigResponse, RequestLogsResponse, RuntimeBalanceSnapshot,
    RuntimeStatsItem, RuntimeStatsResponse, SetCredentialProxyByRegionRequest,
    SsoTokenImportResultItem, StartBuilderIdLoginRequest, StartBuilderIdLoginResponse,
    StartIamSsoLoginResponse, StartIdcLoginRequest, StartIdcLoginResponse,
    StartKiroSsoLoginRequest, StartKiroSsoLoginResponse, StartSocialLoginRequest,
    StartSocialLoginResponse, StatsResponse, SystemPromptResponse, SystemStatusResponse,
    ThinkingConfigResponse, UpdateAccessSettingsRequest, UpdateApiKeyRequest,
    UpdateCommonConfigRequest, UpdateCompressionConfigRequest, UpdateEndpointConfigRequest,
    UpdateGlobalConfigRequest, UpdateModelMappingsRequest, UpdatePromptFilterConfigRequest,
    UpdateProxyConfigRequest, UpdateSystemPromptRequest, UpdateThinkingConfigRequest,
    UpsertUserPresetRequest, VersionResponse,
};
use crate::kiro::token_manager::{CachedBalanceInfo, CredentialEntrySnapshot};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;
const CREDENTIAL_BACKUP_FORMAT: &str = "xkiro.credentials.bundle";
const CREDENTIAL_BACKUP_VERSION: u32 = 1;
const CREDENTIAL_BACKUP_SCHEMA: &str = "native-credential-bundle";
const SOURCE_FORMAT_CREDENTIAL_SNAPSHOT: &str = "external.account-export";
const SOURCE_FORMAT_CACHED_CREDENTIAL: &str = "compatible.cache-record";
const SOURCE_FORMAT_FLAT_CREDENTIAL: &str = "flat.credentials";
const KIRO_BUILDER_ID_CLIENT_ID_HASH: &str = "e909a0580879b06ece1202964fbe9dda95ea4ce3";

struct ParsedCredentialImport {
    source_format: String,
    credentials: Vec<KiroCredentials>,
}

struct ExistingCredentialMatch {
    id: u64,
    reason: String,
}

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

fn balance_response_from_usage(id: u64, usage: &UsageLimitsResponse) -> BalanceResponse {
    let current_usage = usage.current_usage();
    let usage_limit = usage.usage_limit();
    let remaining = usage.primary_remaining();
    let usage_percentage = (usage.usage_ratio() * 100.0).min(100.0);
    BalanceResponse {
        id,
        subscription_title: usage.subscription_title().map(str::to_string),
        subscription_type: usage.subscription_type(),
        current_usage,
        usage_limit,
        remaining,
        usage_percentage,
        next_reset_at: usage.next_date_reset,
        overage_cap: usage.overage_cap(),
        overage_capability: usage.overage_capability().map(str::to_string),
        overage_status: usage.overage_status().map(str::to_string),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocialAuthSessionKind {
    Manual,
    Helper,
}

/// 社交 OAuth 登录进行中的会话状态
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
    proxy_manager: Arc<ProxyManager>,
    /// Kiro Provider 引用，用于区域/端点/global_proxy 热更新双层同步
    kiro_provider: Option<Arc<KiroProvider>>,
    /// 共享压缩配置，与 AppState 同源（运行时热更新）
    compression_config: Arc<RwLock<CompressionConfig>>,
    /// 客户端 API 密钥运行时状态
    client_api_key_runtime: Arc<RwLock<String>>,
    /// 客户端 API 密钥强制开关运行时状态
    require_api_key_runtime: Arc<std::sync::atomic::AtomicBool>,
    /// Admin 密钥运行时状态
    admin_api_key_runtime: Arc<RwLock<String>>,
    /// 共享提示过滤配置，与 AppState 同源（运行时热更新）
    prompt_filter_config: Arc<RwLock<PromptFilterConfig>>,
    /// 用户模型映射运行时（共享引用，OpenAI 路径 OVERRIDE 层，支持热更新）
    model_mapping_config: SharedModelMappingConfig,
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
    /// 进行中的社交 OAuth 登录会话（session_id → SocialAuthSession）
    social_sessions: Mutex<HashMap<String, SocialAuthSession>>,
    /// 进行中的 IAM Identity Center 设备授权会话（session_id → IdcAuthSession）
    idc_sessions: Mutex<HashMap<String, IdcAuthSession>>,
    /// 进行中的 IAM SSO authorization-code 会话
    iam_sso_code_sessions: Mutex<HashMap<String, IamSsoCodeAuthSession>>,
    /// 请求日志和统计
    request_stats: super::stats::SharedRequestStats,
    /// 进行中的 Builder ID 登录会话（session_id → BuilderIdAuthSession）
    builder_id_sessions: Mutex<HashMap<String, BuilderIdAuthSession>>,
    /// 进行中的 Kiro hosted SSO 登录会话（Microsoft 365 / Entra ID）
    kiro_sso_sessions: Mutex<HashMap<String, KiroSsoAuthSession>>,
    /// API 密钥列表（多 API 密钥系统）
    api_keys: SharedApiKeys,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        proxy_manager: Arc<ProxyManager>,
        kiro_provider: Option<Arc<KiroProvider>>,
        compression_config: Arc<RwLock<CompressionConfig>>,
        client_api_key_runtime: Arc<RwLock<String>>,
        require_api_key_runtime: Arc<std::sync::atomic::AtomicBool>,
        admin_api_key_runtime: Arc<RwLock<String>>,
        prompt_filter_config: Arc<RwLock<PromptFilterConfig>>,
        model_mapping_config: SharedModelMappingConfig,
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
            proxy_manager,
            kiro_provider,
            compression_config,
            client_api_key_runtime,
            require_api_key_runtime,
            admin_api_key_runtime,
            prompt_filter_config,
            model_mapping_config,
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

    pub fn export_credentials_by_ids(&self, ids: &[u64]) -> Vec<KiroCredentials> {
        self.token_manager.export_credentials_by_ids(ids)
    }

    fn validate_proxy_id_exists(&self, proxy_id: Option<u64>) -> Result<(), AdminServiceError> {
        if let Some(proxy_id) = proxy_id
            && self.proxy_manager.get(proxy_id).is_none()
        {
            return Err(AdminServiceError::InvalidCredential(format!(
                "代理 #{} 不存在",
                proxy_id
            )));
        }
        Ok(())
    }

    fn assign_proxy_before_validation(
        &self,
        credential: &mut KiroCredentials,
    ) -> Result<(), AdminServiceError> {
        if credential
            .proxy_url
            .as_deref()
            .is_some_and(|url| url.trim().is_empty())
        {
            credential.proxy_url = None;
            credential.proxy_username = None;
            credential.proxy_password = None;
        }

        if let Some(proxy_id) = credential.proxy_id {
            if self.proxy_manager.get(proxy_id).is_none() {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "代理 #{} 不存在",
                    proxy_id
                )));
            }
            if !self.proxy_manager.is_usable(proxy_id) {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "代理 #{} 不可用",
                    proxy_id
                )));
            }
            return Ok(());
        }

        if credential
            .proxy_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .is_some()
        {
            return Ok(());
        }

        let Some(proxy_id) = self.pick_proxy_for_new_credential(credential) else {
            tracing::warn!(
                region = credential.region.as_deref().unwrap_or("<none>"),
                "新凭据验活前未找到可用代理，降级使用全局代理或直连"
            );
            return Ok(());
        };
        credential.proxy_id = Some(proxy_id);
        tracing::info!(
            proxy_id,
            region = credential.region.as_deref().unwrap_or("<none>"),
            "新凭据验活前已自动分配代理"
        );
        Ok(())
    }

    fn pick_proxy_for_new_credential(&self, credential: &KiroCredentials) -> Option<u64> {
        let mut load: HashMap<u64, usize> = HashMap::new();
        for (_, _, proxy_id, disabled) in self.token_manager.credential_region_bindings() {
            if disabled {
                continue;
            }
            if let Some(proxy_id) = proxy_id {
                *load.entry(proxy_id).or_insert(0) += 1;
            }
        }

        let credential_region = credential.region.as_deref();
        let mut candidates: Vec<_> = self
            .proxy_manager
            .list()
            .into_iter()
            .filter(|view| !view.entry.disabled && !view.health.dead)
            .filter_map(|view| {
                let proxy_id = view.entry.id?;
                let region_mismatch = match (credential_region, view.entry.region.as_deref()) {
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
        candidates.first().map(|(proxy_id, _, _, _)| *proxy_id)
    }

    fn proxy_for_validation(
        &self,
        credential: &KiroCredentials,
        fallback_proxy: Option<&ProxyConfig>,
    ) -> Result<Option<ProxyConfig>, AdminServiceError> {
        if let Some(proxy_id) = credential.proxy_id {
            if !self.proxy_manager.is_usable(proxy_id) {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "代理 #{} 不可用",
                    proxy_id
                )));
            }
            let entry = self.proxy_manager.get(proxy_id).ok_or_else(|| {
                AdminServiceError::InvalidCredential(format!("代理 #{} 不存在", proxy_id))
            })?;
            return Ok(Some(entry.to_proxy_config()));
        }

        Ok(credential.effective_proxy(fallback_proxy))
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

                let active_ids: Vec<u64> = self.token_manager.active_credential_ids();
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
                        let resp = balance_response_from_usage(id, &usage);
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
                provider: entry.provider,
                user_id: entry.user_id,
                source_account_id: entry.source_account_id,
                label: entry.label,
                status: entry.status,
                added_at: entry.added_at,
                nickname: entry.nickname,
                group_id: entry.group_id,
                tag_links: entry.tag_links,
                usage_data: entry.usage_data,
                has_available_models_cache: entry.has_available_models_cache,
                source_failure_count: entry.source_failure_count,
                source_last_failure_at: entry.source_last_failure_at,
                source_disabled_reason: entry.source_disabled_reason,
                source_success_count: entry.source_success_count,
                has_profile_arn: entry.has_profile_arn,
                has_token: entry.has_token,
                has_refresh_token: entry.has_refresh_token,
                has_client_id: entry.has_client_id,
                has_client_secret: entry.has_client_secret,
                has_id_token: entry.has_id_token,
                has_api_key: entry.has_api_key,
                has_proxy_credentials: entry.has_proxy_credentials,
                region: entry.region,
                auth_region: entry.auth_region,
                api_region: entry.api_region,
                machine_id: entry.machine_id,
                start_url: entry.start_url,
                client_id_hash: entry.client_id_hash,
                sso_session_id: entry.sso_session_id,
                token_endpoint: entry.token_endpoint,
                issuer_url: entry.issuer_url,
                scopes: entry.scopes,
                subscription_type: entry.subscription_type,
                subscription_title: entry.subscription_title,
                days_remaining: entry.days_remaining,
                overage_status: entry.overage_status,
                overage_capability: entry.overage_capability,
                overage_cap: entry.overage_cap,
                overage_rate: entry.overage_rate,
                current_overages: entry.current_overages,
                overage_checked_at: entry.overage_checked_at,
                ban_status: entry.ban_status,
                ban_reason: entry.ban_reason,
                ban_time: entry.ban_time,
                usage_current: entry.usage_current,
                usage_limit: entry.usage_limit,
                usage_percent: entry.usage_percent,
                next_reset_date: entry.next_reset_date,
                last_refresh: entry.last_refresh,
                trial_usage_current: entry.trial_usage_current,
                trial_usage_limit: entry.trial_usage_limit,
                trial_usage_percent: entry.trial_usage_percent,
                trial_status: entry.trial_status,
                trial_expires_at: entry.trial_expires_at,
                request_count: entry.request_count,
                error_count: entry.error_count,
                total_tokens: entry.total_tokens,
                total_credits: entry.total_credits,
                last_used: entry.last_used,
                created_at: entry.created_at,
                tags: entry.tags,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                proxy_id: entry.proxy_id,
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

    pub fn list_credential_alias_views(&self) -> Vec<CredentialAliasViewItem> {
        self.token_manager
            .snapshot()
            .entries
            .into_iter()
            .map(|entry| {
                let id = entry
                    .source_account_id
                    .clone()
                    .unwrap_or_else(|| entry.id.to_string());
                CredentialAliasViewItem {
                    id,
                    email: entry.email.unwrap_or_default(),
                    user_id: entry.user_id.unwrap_or_default(),
                    nickname: entry.nickname.unwrap_or_default(),
                    auth_method: entry.auth_method.unwrap_or_default(),
                    provider: entry.provider.unwrap_or_default(),
                    region: entry.region.unwrap_or_default(),
                    enabled: !entry.disabled,
                    ban_status: entry.ban_status.unwrap_or_default(),
                    ban_reason: entry.ban_reason.unwrap_or_default(),
                    ban_time: entry.ban_time.unwrap_or_default(),
                    expires_at: Self::rfc3339_seconds(entry.expires_at.as_deref()),
                    has_token: entry.has_token,
                    machine_id: entry.machine_id.unwrap_or_default(),
                    weight: entry.weight,
                    overage_status: entry.overage_status.unwrap_or_default(),
                    overage_capability: entry.overage_capability.unwrap_or_default(),
                    overage_cap: entry.overage_cap.unwrap_or_default(),
                    overage_rate: entry.overage_rate.unwrap_or_default(),
                    current_overages: entry.current_overages.unwrap_or_default(),
                    overage_checked_at: entry.overage_checked_at.unwrap_or_default(),
                    proxy_url: entry.proxy_url.unwrap_or_default(),
                    subscription_type: entry.subscription_type.unwrap_or_default(),
                    subscription_title: entry.subscription_title.unwrap_or_default(),
                    days_remaining: entry.days_remaining.unwrap_or_default(),
                    usage_current: entry.usage_current.unwrap_or_default(),
                    usage_limit: entry.usage_limit.unwrap_or_default(),
                    usage_percent: entry.usage_percent.unwrap_or_default(),
                    next_reset_date: entry.next_reset_date.unwrap_or_default(),
                    last_refresh: entry.last_refresh.unwrap_or_default(),
                    trial_usage_current: entry.trial_usage_current.unwrap_or_default(),
                    trial_usage_limit: entry.trial_usage_limit.unwrap_or_default(),
                    trial_usage_percent: entry.trial_usage_percent.unwrap_or_default(),
                    trial_status: entry.trial_status.unwrap_or_default(),
                    trial_expires_at: entry.trial_expires_at.unwrap_or_default(),
                    request_count: entry.request_count.unwrap_or_default(),
                    error_count: entry.error_count.unwrap_or_default(),
                    total_tokens: entry.total_tokens.unwrap_or_default(),
                    total_credits: entry.total_credits.unwrap_or_default(),
                    last_used: entry
                        .last_used
                        .or_else(|| Self::rfc3339_seconds_opt(entry.last_used_at.as_deref()))
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    pub fn get_credential_full_export_by_path_id(
        &self,
        path_id: &str,
    ) -> Result<CredentialFullExportResponse, AdminServiceError> {
        let id = self.resolve_credential_id(path_id)?;
        let snapshot = self.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or(AdminServiceError::NotFound { id })?;
        let credentials = self
            .export_credentials_by_ids(&[id])
            .into_iter()
            .next()
            .ok_or_else(|| {
                AdminServiceError::InvalidCredential("credential is not exportable".to_string())
            })?;
        let refresh_token = credentials
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                AdminServiceError::InvalidCredential("credential is not exportable".to_string())
            })?
            .to_string();
        let auth_method = credentials
            .auth_method
            .clone()
            .unwrap_or_else(|| "social".to_string());
        if auth_method.eq_ignore_ascii_case("api_key") {
            return Err(AdminServiceError::InvalidCredential(
                "credential is not exportable".to_string(),
            ));
        }
        let export_id = credentials
            .meta
            .source_account_id
            .clone()
            .unwrap_or_else(|| id.to_string());
        let nickname = credentials
            .meta
            .nickname
            .clone()
            .or_else(|| credentials.email.clone())
            .unwrap_or_else(|| format!("凭据 #{}", id));
        let user_id = credentials
            .user_id
            .clone()
            .or_else(|| credentials.email.clone());

        Ok(CredentialFullExportResponse {
            id: export_id,
            email: credentials.email.clone(),
            user_id,
            nickname,
            access_token: credentials.access_token.clone(),
            refresh_token,
            client_id: credentials.client_id.clone(),
            client_secret: credentials.client_secret.clone(),
            auth_method,
            provider: credentials.provider.clone(),
            region: credentials.region.clone(),
            start_url: credentials.start_url.clone(),
            expires_at: Self::rfc3339_seconds_opt(credentials.expires_at.as_deref()),
            machine_id: credentials.machine_id.clone(),
            weight: entry.weight,
            profile_arn: credentials.profile_arn.clone(),
            token_endpoint: credentials.token_endpoint.clone(),
            issuer_url: credentials.issuer_url.clone(),
            scopes: credentials.scopes.clone(),
            client_id_hash: credentials.client_id_hash.clone(),
            id_token: credentials.id_token.clone(),
            sso_session_id: credentials.sso_session_id.clone(),
            proxy_url: entry.proxy_url.clone(),
            proxy_id: entry.proxy_id,
            overage_status: credentials.meta.overage_status.clone(),
            overage_capability: credentials.meta.overage_capability.clone(),
            overage_cap: credentials.meta.overage_cap.unwrap_or_default(),
            overage_rate: credentials.meta.overage_rate.unwrap_or_default(),
            current_overages: credentials.meta.current_overages.unwrap_or_default(),
            overage_checked_at: credentials.meta.overage_checked_at.unwrap_or_default(),
            enabled: !entry.disabled,
            ban_status: credentials.meta.ban_status.clone(),
            ban_reason: credentials.meta.ban_reason.clone(),
            ban_time: credentials.meta.ban_time.unwrap_or_default(),
            subscription_type: credentials.meta.subscription_type.clone(),
            subscription_title: credentials.meta.subscription_title.clone(),
            days_remaining: credentials.meta.days_remaining.unwrap_or_default(),
            usage_current: credentials.meta.usage_current.unwrap_or_default(),
            usage_limit: credentials.meta.usage_limit.unwrap_or_default(),
            usage_percent: credentials.meta.usage_percent.unwrap_or_default(),
            next_reset_date: credentials.meta.next_reset_date.clone(),
            last_refresh: credentials.meta.last_refresh.unwrap_or_default(),
            trial_usage_current: credentials.meta.trial_usage_current.unwrap_or_default(),
            trial_usage_limit: credentials.meta.trial_usage_limit.unwrap_or_default(),
            trial_usage_percent: credentials.meta.trial_usage_percent.unwrap_or_default(),
            trial_status: credentials.meta.trial_status.clone(),
            trial_expires_at: credentials.meta.trial_expires_at.unwrap_or_default(),
            request_count: credentials
                .meta
                .request_count
                .unwrap_or(entry.success_count),
            error_count: credentials
                .meta
                .error_count
                .unwrap_or(entry.failure_count as u64),
            total_tokens: credentials.meta.total_tokens.unwrap_or_default(),
            total_credits: credentials.meta.total_credits.unwrap_or_default(),
            last_used: credentials
                .meta
                .last_used_at
                .or_else(|| Self::rfc3339_seconds_opt(entry.last_used_at.as_deref()))
                .unwrap_or_default(),
        })
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

    pub fn update_credential_alias_fields(
        &self,
        id: u64,
        req: CredentialAliasUpdateRequest,
    ) -> Result<(), AdminServiceError> {
        let nickname = Self::optional_trimmed_string(req.nickname);
        let machine_id = Self::optional_trimmed_string(req.machine_id)
            .map(machine_id::normalize_optional_machine_id);
        let proxy_url = req.proxy_url.map(|v| {
            let trimmed = v.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
        self.token_manager
            .update_credential_fields(id, req.enabled, nickname, machine_id, req.weight, proxy_url)
            .map_err(|e| self.classify_error(e, id))
    }

    pub fn update_credential_alias_by_path_id(
        &self,
        path_id: &str,
        req: CredentialAliasUpdateRequest,
    ) -> Result<(), AdminServiceError> {
        let id = self.resolve_credential_id(path_id)?;
        self.update_credential_alias_fields(id, req)
    }

    pub fn delete_credential_by_path_id(&self, path_id: &str) -> Result<(), AdminServiceError> {
        let id = self.resolve_credential_id(path_id)?;
        self.set_disabled(id, true)?;
        self.delete_credential(id)
    }

    fn optional_trimmed_string(value: Option<String>) -> Option<Option<String>> {
        value.map(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
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

        Ok(balance_response_from_usage(id, &usage))
    }

    /// 拉取指定凭据可用模型列表（30 分钟内存缓存；force=true 跳过缓存）
    ///
    /// API 密钥凭据 / 不存在 / 被禁用 → InvalidCredential / NotFound；
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

    pub async fn refresh_credential_models_by_path_id(
        &self,
        path_id: &str,
    ) -> Result<CredentialModelsResponse, AdminServiceError> {
        let id = self.resolve_credential_id(path_id)?;
        let response = self.list_available_models(id, None, true).await?;
        Ok(CredentialModelsResponse {
            success: true,
            models: response.available_models,
        })
    }

    pub fn get_cached_credential_models_by_path_id(
        &self,
        path_id: &str,
    ) -> CachedCredentialModelsResponse {
        let models = self
            .resolve_credential_id(path_id)
            .map(|id| self.token_manager.get_model_list(id))
            .unwrap_or_default();
        CachedCredentialModelsResponse {
            success: true,
            models,
        }
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
                        subscription_type: cached.data.subscription_type.clone(),
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
                        subscription_type: None,
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
        self.validate_proxy_id_exists(req.proxy_id)?;
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

        let token_endpoint = Self::clean_import_string(req.token_endpoint);
        let issuer_url = Self::clean_import_string(req.issuer_url);
        let scopes = Self::clean_import_string(req.scopes);
        let auth_method = Self::normalize_import_auth_method(
            Some(&req.auth_method),
            req.provider.as_deref(),
            req.client_id.as_deref(),
            req.client_secret.as_deref(),
            token_endpoint.as_deref(),
            req.api_key.as_deref(),
        );
        let provider = Self::normalize_provider_for_auth_method(
            Self::clean_import_string(req.provider),
            &auth_method,
        );

        // 构建凭据对象
        let email = req.email.clone();
        let mut new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(auth_method),
            provider,
            user_id: Self::clean_import_string(req.user_id),
            client_id: req.client_id,
            client_secret: req.client_secret,
            token_endpoint,
            issuer_url,
            scopes,
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
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            proxy_id: req.proxy_id,
            disabled: false, // 新添加的凭据默认启用
            api_key: req.api_key,
            endpoint: req.endpoint,
            concurrency: req.concurrency,
            meta: CredentialSourceMetadata {
                subscription_title: None,
                // 将在首次获取使用额度时自动更新
                overage_status: None,
                allow_overage_import: false,
                ..Default::default()
            },
        };
        self.assign_machine_id_for_new_credential(&mut new_cred)?;
        self.assign_proxy_before_validation(&mut new_cred)?;

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 订阅绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(self.add_credential_response_from_stored(
            credential_id,
            format!("凭据添加成功，ID: {}", credential_id),
            email,
        ))
    }

    pub async fn import_credential_record(
        &self,
        req: ImportCredentialRecordRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        self.validate_proxy_id_exists(req.proxy_id)?;
        let access_token = Self::clean_import_string(req.access_token);
        let api_key = Self::clean_import_string(req.api_key);
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
        let mut profile_arn = Self::clean_import_string(req.profile_arn);
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
        let preferred_id = Self::parse_credential_record_id(req.id.as_ref());
        let disabled = Self::credential_record_import_disabled(req.disabled, req.enabled);
        let concurrency = req.concurrency.filter(|value| *value > 0);

        let mut auth_method = Self::normalize_credential_record_auth_method(
            req.auth_method.as_deref(),
            provider.as_deref(),
            client_id.as_deref(),
            client_secret.as_deref(),
            token_endpoint.as_deref(),
            api_key.as_deref(),
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
        let import_refresh_token = if auth_method == "api_key" {
            if api_key.is_none() {
                return Err(AdminServiceError::InvalidRequest(
                    "apiKey is required".to_string(),
                ));
            }
            None
        } else {
            Some(Self::clean_import_string(req.refresh_token).ok_or_else(|| {
                AdminServiceError::InvalidRequest("refreshToken is required".to_string())
            })?)
        };
        let provider = Self::normalize_provider_for_auth_method(provider, &auth_method);
        profile_arn = KiroCredentials::clean_profile_arn(profile_arn);
        let source_account_id = Self::clean_import_string(req.source_account_id)
            .or_else(|| Self::credential_record_import_source_id(req.id.as_ref()));

        let mut credential = KiroCredentials {
            id: preferred_id,
            access_token: None,
            refresh_token: import_refresh_token,
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
            proxy_url,
            proxy_username,
            proxy_password,
            proxy_id: req.proxy_id,
            disabled,
            api_key,
            endpoint,
            meta: CredentialSourceMetadata {
                source_account_id,
                label: Self::clean_import_string(req.label),
                status: Self::clean_import_string(req.status),
                added_at: Self::clean_import_string(req.added_at),
                password: Self::clean_import_string(req.password),
                subscription_title: Self::clean_import_string(req.subscription_title),
                overage_status,
                usage_data: req.usage_data,
                group_id: Self::clean_import_string(req.group_id),
                tag_links: req.tag_links,
                available_models_cache: req.available_models_cache,
                failure_count: req.failure_count,
                last_failure_at: Self::clean_import_string(req.last_failure_at),
                disabled_reason: Self::clean_import_string(req.disabled_reason),
                success_count: req.success_count,
                csrf_token: Self::clean_import_string(req.csrf_token),
                nickname: Self::clean_import_string(req.nickname),
                ban_status: Self::clean_import_string(req.ban_status),
                ban_reason: Self::clean_import_string(req.ban_reason),
                ban_time: req.ban_time,
                subscription_type: Self::clean_import_string(req.subscription_type),
                days_remaining: req.days_remaining,
                usage_current: req.usage_current,
                usage_limit: req.usage_limit,
                usage_percent: req.usage_percent,
                next_reset_date: Self::clean_import_string(req.next_reset_date),
                last_refresh: req.last_refresh,
                trial_usage_current: req.trial_usage_current,
                trial_usage_limit: req.trial_usage_limit,
                trial_usage_percent: req.trial_usage_percent,
                trial_status: Self::clean_import_string(req.trial_status),
                trial_expires_at: req.trial_expires_at,
                overage_capability: Self::clean_import_string(req.overage_capability),
                overage_cap: req.overage_cap,
                overage_rate: req.overage_rate,
                current_overages: req.current_overages,
                overage_checked_at: req.overage_checked_at,
                request_count: req.request_count,
                error_count: req.error_count,
                total_tokens: req.total_tokens,
                total_credits: req.total_credits,
                last_used_at: req.last_used_at,
                created_at: req.created_at,
                tags: req.tags,
                allow_overage_import: false,
                ..Default::default()
            },
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
        self.assign_machine_id_for_new_credential(&mut credential)?;
        self.assign_proxy_before_validation(&mut credential)?;

        if auth_method != "api_key" && credential.access_token.is_none() {
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
            tracing::warn!("导入凭据记录后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(self.add_credential_response_from_stored(
            credential_id,
            format!("凭据添加成功，ID: {}", credential_id),
            email,
        ))
    }

    fn add_credential_response_from_stored(
        &self,
        credential_id: u64,
        message: String,
        email_hint: Option<String>,
    ) -> AddCredentialResponse {
        let details =
            self.credential_login_details_from_stored_with_email_hint(credential_id, email_hint);
        AddCredentialResponse::from_login_details(message, details)
    }

    fn clean_import_string(value: Option<String>) -> Option<String> {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn has_import_string(value: &Option<String>) -> bool {
        value
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
    }

    fn normalize_credential_record_auth_method(
        auth_method: Option<&str>,
        provider: Option<&str>,
        client_id: Option<&str>,
        client_secret: Option<&str>,
        token_endpoint: Option<&str>,
        api_key: Option<&str>,
    ) -> String {
        Self::normalize_import_auth_method(
            auth_method,
            provider,
            client_id,
            client_secret,
            token_endpoint,
            api_key,
        )
    }

    fn canonical_explicit_auth_method(
        auth_method: Option<&str>,
        allow_api_key: bool,
    ) -> Option<String> {
        let method = auth_method?.trim();
        if method.is_empty() {
            return None;
        }
        let canonical = KiroCredentials::canonical_auth_method_name(method);
        match canonical {
            "social" | "idc" | "external_idp" => Some(canonical.to_string()),
            "api_key" if allow_api_key => Some(canonical.to_string()),
            _ => None,
        }
    }

    fn is_external_idp_provider_alias(value: &str) -> bool {
        KiroCredentials::is_external_idp_provider_alias(value)
    }

    fn is_external_idp_provider_alias_option(value: Option<&str>) -> bool {
        KiroCredentials::provider_implies_external_idp(value)
    }

    fn normalize_provider_for_auth_method(
        provider: Option<String>,
        auth_method: &str,
    ) -> Option<String> {
        KiroCredentials::normalize_provider_for_auth_method(provider, auth_method)
    }

    fn parse_credential_record_id(value: Option<&serde_json::Value>) -> Option<u64> {
        match value {
            Some(serde_json::Value::Number(number)) => number.as_u64().filter(|id| *id > 0),
            Some(serde_json::Value::String(value)) => {
                value.trim().parse::<u64>().ok().filter(|id| *id > 0)
            }
            _ => None,
        }
    }

    fn credential_record_import_source_id(value: Option<&serde_json::Value>) -> Option<String> {
        match value {
            Some(serde_json::Value::String(value)) => {
                let value = value.trim();
                (!value.is_empty() && value.parse::<u64>().is_err()).then(|| value.to_string())
            }
            _ => None,
        }
    }

    fn credential_record_import_disabled(disabled: Option<bool>, enabled: Option<bool>) -> bool {
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

    pub async fn refresh_credential_by_path_id(
        &self,
        path_id: &str,
    ) -> Result<CredentialRefreshResponse, AdminServiceError> {
        let id = self.resolve_credential_id(path_id)?;
        self.force_refresh_token(id).await?;
        self.get_balance(id, true).await?;
        let snapshot = self.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or(AdminServiceError::NotFound { id })?;
        Ok(CredentialRefreshResponse {
            success: true,
            info: Self::credential_refresh_info_from_entry(entry),
        })
    }

    fn credential_refresh_info_from_entry(
        entry: &CredentialEntrySnapshot,
    ) -> CredentialRefreshInfo {
        CredentialRefreshInfo {
            email: entry.email.clone().unwrap_or_default(),
            user_id: entry.user_id.clone().unwrap_or_default(),
            subscription_type: entry.subscription_type.clone().unwrap_or_default(),
            subscription_title: entry.subscription_title.clone().unwrap_or_default(),
            days_remaining: entry.days_remaining.unwrap_or_default(),
            usage_current: entry.usage_current.unwrap_or_default(),
            usage_limit: entry.usage_limit.unwrap_or_default(),
            usage_percent: entry.usage_percent.unwrap_or_default(),
            next_reset_date: entry.next_reset_date.clone().unwrap_or_default(),
            last_refresh: entry.last_refresh.unwrap_or_default(),
            trial_usage_current: entry.trial_usage_current.unwrap_or_default(),
            trial_usage_limit: entry.trial_usage_limit.unwrap_or_default(),
            trial_usage_percent: entry.trial_usage_percent.unwrap_or_default(),
            trial_status: entry.trial_status.clone().unwrap_or_default(),
            trial_expires_at: entry.trial_expires_at.unwrap_or_default(),
        }
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

    pub async fn get_credential_overage_by_path_id(
        &self,
        path_id: &str,
    ) -> Result<CredentialOverageResponse, AdminServiceError> {
        let id = self.resolve_credential_id(path_id)?;
        let balance = self.get_balance(id, true).await?;
        Ok(self.credential_overage_response(id, Some(&balance), None))
    }

    pub async fn set_credential_overage_by_path_id(
        &self,
        path_id: &str,
        enabled: bool,
    ) -> Result<CredentialOverageResponse, AdminServiceError> {
        let id = self.resolve_credential_id(path_id)?;
        self.set_overage_status(id, enabled).await?;
        let status = if enabled { "ENABLED" } else { "DISABLED" }.to_string();
        let balance = self.get_balance(id, true).await.ok();
        Ok(self.credential_overage_response(id, balance.as_ref(), Some(status)))
    }

    fn credential_overage_response(
        &self,
        id: u64,
        balance: Option<&BalanceResponse>,
        status_override: Option<String>,
    ) -> CredentialOverageResponse {
        let snapshot = self
            .token_manager
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.id == id);

        let overage_status = status_override
            .or_else(|| balance.and_then(|b| b.overage_status.clone()))
            .or_else(|| {
                snapshot
                    .as_ref()
                    .and_then(|entry| entry.overage_status.clone())
            });
        let overage_capability = balance
            .and_then(|b| b.overage_capability.clone())
            .or_else(|| {
                snapshot
                    .as_ref()
                    .and_then(|entry| entry.overage_capability.clone())
            });
        let subscription_title = balance
            .and_then(|b| b.subscription_title.clone())
            .or_else(|| {
                snapshot
                    .as_ref()
                    .and_then(|entry| entry.subscription_title.clone())
            });
        let overage_cap = balance
            .map(|b| b.overage_cap)
            .filter(|cap| *cap > 0.0)
            .or_else(|| snapshot.as_ref().and_then(|entry| entry.overage_cap))
            .unwrap_or_default();
        let current_overages = snapshot
            .as_ref()
            .and_then(|entry| entry.current_overages)
            .or_else(|| balance.map(|b| (b.current_usage - b.usage_limit).max(0.0)))
            .unwrap_or_default();

        CredentialOverageResponse {
            success: true,
            overage_status,
            overage_capability,
            subscription_title,
            overage_cap,
            overage_rate: snapshot
                .as_ref()
                .and_then(|entry| entry.overage_rate)
                .unwrap_or_default(),
            current_overages,
            overage_checked_at: snapshot
                .as_ref()
                .and_then(|entry| entry.overage_checked_at)
                .unwrap_or_else(|| Utc::now().timestamp()),
        }
    }

    /// 轻量运行时状态快照（高频轮询用，纯内存读取，不触发任何 IO）
    ///
    /// 字段精简至 dashboard 实时需要的 5 项：
    /// - `id`：凭据主键
    /// - `last_used_at`：最近一次被选中的时间戳（RFC3339）
    /// - `available_permits` / `max_permits`：用于渲染 K/N 并发占用
    /// - `disabled`：手动禁用标记
    pub fn get_runtime_stats(&self) -> RuntimeStatsResponse {
        let entries = self.token_manager.runtime_snapshot();
        let disk_cache = self.balance_cache.lock();
        let credentials = entries
            .into_iter()
            .map(|entry| {
                let balance = disk_cache
                    .get(&entry.id)
                    .map(|cached| RuntimeBalanceSnapshot {
                        subscription_title: cached.data.subscription_title.clone(),
                        subscription_type: cached.data.subscription_type.clone(),
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
                    last_error_code: entry.last_error_code.map(str::to_string),
                    last_error_at: entry.last_error_at.clone(),
                }
            })
            .collect();
        RuntimeStatsResponse { credentials }
    }

    /// 批量强制刷新令牌（B 端点）
    ///
    /// 用 `Semaphore(8)` 限制并发，`JoinSet` 收集结果。
    /// 单个失败不影响其他凭据，全部完成后返回 `BatchRefreshResponse`。
    /// 内部调用 `force_refresh_token_for(id)`，对 API 密钥凭据会 `bail` 走 Err 分支。
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
                    Ok(usage) => BatchRefreshBalanceResultItem {
                        id,
                        success: true,
                        balance: Some(balance_response_from_usage(id, &usage)),
                        error: None,
                    },
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

        // JSON 对象键使用字符串，加载后再解析为凭据 ID。
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

        // 2. API 密钥凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API 密钥凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭据已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("令牌刷新失败") ||
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
            || msg.contains("apiKey 重复")
            || msg.contains("缺少 apiKey")
            || msg.contains("apiKey 为空")
            || msg.contains("凭据已过期或无效")
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

    /// 设置凭据区域（凭据级 region/api_region 覆盖）
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

    /// 设置凭据端点（凭据级 endpoint 覆盖，须命中已注册端点）
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
                "端点必须是已注册值，已注册: {:?}，收到: {}",
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

        // 3. 持久化成功后再应用运行时变更：先更新 token_manager，再同步 provider。
        self.token_manager.update_proxy(new_proxy.clone());
        if let Some(provider) = &self.kiro_provider {
            if let Err(e) = provider.update_global_proxy(new_proxy) {
                tracing::warn!("provider.update_global_proxy 失败（已持久化）: {}", e);
            }
        }

        Ok(())
    }

    pub fn get_access_settings(&self) -> AccessSettingsResponse {
        let config = self.token_manager.config();
        AccessSettingsResponse {
            api_key: config.api_key.clone(),
            require_api_key: config.require_api_key,
            port: config.port,
            host: config.host.clone(),
            allow_over_usage: config.allow_over_usage,
        }
    }

    pub async fn update_access_settings(
        &self,
        req: UpdateAccessSettingsRequest,
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

    pub fn get_common_config(&self) -> Result<CommonConfigResponse, AdminServiceError> {
        let machine_id = self.ensure_local_machine_id()?;
        let config = self.token_manager.config();
        Ok(CommonConfigResponse {
            machine_id,
            credential_machine_id_strategy: config.credential_machine_id_strategy,
        })
    }

    pub fn update_common_config(
        &self,
        req: UpdateCommonConfigRequest,
    ) -> Result<CommonConfigResponse, AdminServiceError> {
        self.token_manager.with_config_mut(|cfg| {
            if let Some(strategy) = req.credential_machine_id_strategy {
                cfg.credential_machine_id_strategy = strategy;
            }
            let machine_id = cfg
                .machine_id
                .as_deref()
                .and_then(machine_id::normalize_machine_id)
                .unwrap_or_else(machine_id::generate_account_machine_id);
            cfg.machine_id = Some(machine_id);
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        self.get_common_config()
    }

    fn ensure_local_machine_id(&self) -> Result<String, AdminServiceError> {
        self.token_manager.with_config_mut(|cfg| {
            let old = cfg.machine_id.clone();
            let machine_id = old
                .as_deref()
                .and_then(machine_id::normalize_machine_id)
                .unwrap_or_else(machine_id::generate_account_machine_id);
            let changed = old.as_deref() != Some(machine_id.as_str());
            cfg.machine_id = Some(machine_id.clone());
            if changed {
                cfg.save()
                    .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
            }
            Ok(machine_id)
        })
    }

    fn assign_machine_id_for_new_credential(
        &self,
        credential: &mut KiroCredentials,
    ) -> Result<(), AdminServiceError> {
        if let Some(machine_id) = credential
            .machine_id
            .as_deref()
            .and_then(machine_id::normalize_machine_id)
        {
            credential.machine_id = Some(machine_id);
            return Ok(());
        }

        let machine_id = self.machine_id_for_new_credential()?;
        credential.machine_id = Some(machine_id);
        Ok(())
    }

    fn machine_id_for_new_credential(&self) -> Result<String, AdminServiceError> {
        match self.token_manager.config().credential_machine_id_strategy {
            CredentialMachineIdStrategy::Local => self.ensure_local_machine_id(),
            CredentialMachineIdStrategy::Random => Ok(machine_id::generate_account_machine_id()),
        }
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
        fn resolve_format(
            value: Option<String>,
            field: &str,
            default_value: &str,
        ) -> Result<String, AdminServiceError> {
            let value = value.unwrap_or_default();
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(default_value.to_string());
            }
            if matches!(trimmed, "reasoning_content" | "thinking" | "think") {
                return Ok(trimmed.to_string());
            }
            Err(AdminServiceError::InvalidRequest(format!(
                "{} 必须是 reasoning_content、thinking 或 think",
                field
            )))
        }

        let suffix = {
            let value = req.suffix.unwrap_or_default();
            let trimmed = value.trim();
            if trimmed.is_empty() {
                "-thinking".to_string()
            } else {
                trimmed.to_string()
            }
        };
        let openai_format = resolve_format(req.openai_format, "openaiFormat", "reasoning_content")?;
        let claude_format = resolve_format(req.claude_format, "claudeFormat", "thinking")?;

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
        let preferred_endpoint = req.preferred_endpoint.unwrap_or_default();
        let internal_endpoint = Self::external_endpoint_alias_to_internal(&preferred_endpoint)?;
        let endpoint_fallback = req.endpoint_fallback;

        self.token_manager.with_config_mut(|cfg| {
            cfg.preferred_endpoint = preferred_endpoint.clone();
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

    pub fn get_proxy_url_config(&self) -> ProxyUrlConfigResponse {
        let config = self.token_manager.config();
        ProxyUrlConfigResponse {
            proxy_url: config.proxy_url.unwrap_or_default(),
        }
    }

    pub async fn update_proxy_url_config(
        &self,
        req: UpdateProxyConfigRequest,
    ) -> Result<(), AdminServiceError> {
        if let Some(proxy_url) = &req.proxy_url {
            Self::validate_proxy_url(proxy_url)?;
        }
        self.update_proxy_config(req).await
    }

    pub fn list_proxies(&self) -> ProxyListResponse {
        let proxies = self
            .proxy_manager
            .list()
            .into_iter()
            .map(|view| {
                let id = view.entry.id.unwrap_or(0);
                ProxyItem {
                    id,
                    url: view.entry.url,
                    username: view.entry.username,
                    region: view.entry.region,
                    country: view.entry.country,
                    max_concurrency: view.entry.max_concurrency,
                    disabled: view.entry.disabled,
                    note: view.entry.note,
                    dead: view.health.dead,
                    consecutive_failures: view.health.consecutive_failures,
                    last_error: view.health.last_error,
                    last_checked: view.health.last_checked.map(|dt| dt.to_rfc3339()),
                    available_permits: view.available_permits,
                    bound_credentials: self.token_manager.credentials_bound_to_proxy(id).len(),
                }
            })
            .collect();
        ProxyListResponse { proxies }
    }

    pub async fn add_proxy(&self, req: ProxyUpsertRequest) -> Result<u64, AdminServiceError> {
        let entry = proxy_entry_from_req(req)?;
        let id = self
            .proxy_manager
            .add(entry)
            .map_err(|e| AdminServiceError::InternalError(format!("新增代理失败: {}", e)))?;
        if let Some(entry) = self.proxy_manager.get(id)
            && entry.region.is_none()
            && let Some(geo) = probe_proxy_geo(&entry).await
            && let Err(e) = self.proxy_manager.set_geo(id, geo.region, geo.country)
        {
            tracing::warn!("新增代理 #{} 回填地理信息失败: {}", id, e);
        }
        Ok(id)
    }

    pub fn update_proxy(&self, id: u64, req: ProxyUpsertRequest) -> Result<(), AdminServiceError> {
        let entry = proxy_entry_from_req(req)?;
        self.proxy_manager
            .update(id, entry)
            .map_err(|e| AdminServiceError::InternalError(format!("更新代理失败: {}", e)))
    }

    pub fn delete_proxy(&self, id: u64) -> Result<usize, AdminServiceError> {
        let bound = self.token_manager.credentials_bound_to_proxy(id);
        for credential_id in &bound {
            if let Err(e) = self.token_manager.set_proxy_id(*credential_id, None) {
                tracing::warn!("删除代理 #{} 时解绑凭据 #{} 失败: {}", id, credential_id, e);
            }
        }
        self.proxy_manager
            .delete(id)
            .map_err(|e| AdminServiceError::InternalError(format!("删除代理失败: {}", e)))?;
        Ok(bound.len())
    }

    pub fn set_credential_proxy(
        &self,
        credential_id: u64,
        proxy_id: Option<u64>,
    ) -> Result<(), AdminServiceError> {
        if let Some(id) = proxy_id
            && self.proxy_manager.get(id).is_none()
        {
            return Err(AdminServiceError::InvalidCredential(format!(
                "代理 #{} 不存在",
                id
            )));
        }
        self.token_manager
            .set_proxy_id(credential_id, proxy_id)
            .map_err(|e| self.classify_error(e, credential_id))
    }

    pub fn set_credential_proxy_by_region(
        &self,
        credential_id: u64,
        region: Option<&str>,
    ) -> Result<Option<u64>, AdminServiceError> {
        let Some(region) = region.map(str::trim).filter(|value| !value.is_empty()) else {
            self.token_manager
                .set_proxy_id(credential_id, None)
                .map_err(|e| self.classify_error(e, credential_id))?;
            return Ok(None);
        };

        let mut load: HashMap<u64, usize> = HashMap::new();
        for (_, _, proxy_id, _) in self.token_manager.credential_region_bindings() {
            if let Some(proxy_id) = proxy_id {
                *load.entry(proxy_id).or_insert(0) += 1;
            }
        }

        let mut candidates: Vec<u64> = self
            .proxy_manager
            .list()
            .into_iter()
            .filter(|view| !view.entry.disabled && !view.health.dead)
            .filter(|view| match view.entry.region.as_deref() {
                Some(proxy_region) => proxy_region == region,
                None => true,
            })
            .filter_map(|view| view.entry.id)
            .collect();

        if candidates.is_empty() {
            return Err(AdminServiceError::InvalidCredential(format!(
                "region '{}' 下无可用代理",
                region
            )));
        }
        candidates.sort_by_key(|id| *load.get(id).unwrap_or(&0));
        let proxy_id = candidates[0];
        self.token_manager
            .set_proxy_id(credential_id, Some(proxy_id))
            .map_err(|e| self.classify_error(e, credential_id))?;
        Ok(Some(proxy_id))
    }

    pub async fn test_proxy(&self, id: u64) -> ProxyTestResponse {
        let Some(entry) = self.proxy_manager.get(id) else {
            return ProxyTestResponse {
                ok: false,
                exit_ip: None,
                latency_ms: None,
                error: Some(format!("代理 #{} 不存在", id)),
            };
        };
        let result = probe_proxy(&entry).await;
        self.proxy_manager
            .record_health(id, result.ok, result.error.clone());
        result
    }

    pub async fn import_proxies(&self, req: ProxyImportRequest) -> ProxyImportResponse {
        let mut added = 0usize;
        let mut failed = 0usize;
        let mut errors = Vec::new();
        let mut new_ids = Vec::new();

        for (lineno, raw) in req.text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (url, username, password) = match parse_proxy_line(line) {
                Ok(parsed) => parsed,
                Err(error) => {
                    failed += 1;
                    errors.push(format!("第{}行: {}", lineno + 1, error));
                    continue;
                }
            };
            let entry = ProxyEntry {
                id: None,
                url,
                username,
                password,
                region: clean_proxy_string(req.region.clone()),
                country: None,
                max_concurrency: req.max_concurrency.filter(|value| *value > 0),
                disabled: false,
                note: None,
            };
            match self.proxy_manager.add(entry) {
                Ok(id) => {
                    added += 1;
                    new_ids.push(id);
                }
                Err(error) => {
                    failed += 1;
                    errors.push(format!("第{}行: {}", lineno + 1, error));
                }
            }
        }

        for id in new_ids {
            if let Some(entry) = self.proxy_manager.get(id)
                && let Some(geo) = probe_proxy_geo(&entry).await
            {
                let region = if entry.region.is_some() {
                    None
                } else {
                    geo.region
                };
                if let Err(error) = self.proxy_manager.set_geo(id, region, geo.country) {
                    tracing::warn!("代理 #{} 回填地理信息失败: {}", id, error);
                }
            }
        }

        ProxyImportResponse {
            added,
            failed,
            errors,
        }
    }

    pub fn auto_assign_proxies(&self, req: ProxyAutoAssignRequest) -> ProxyAutoAssignResponse {
        let bindings = self.token_manager.credential_region_bindings();
        let mut load: HashMap<u64, usize> = HashMap::new();
        for (_, _, proxy_id, _) in &bindings {
            if let Some(proxy_id) = proxy_id {
                *load.entry(*proxy_id).or_insert(0) += 1;
            }
        }
        let proxies = self.proxy_manager.list();
        let mut assigned = Vec::new();
        let mut skipped = Vec::new();

        for (credential_id, region, current_proxy_id, disabled) in bindings {
            if !req.credential_ids.is_empty() && !req.credential_ids.contains(&credential_id) {
                continue;
            }
            if disabled || (current_proxy_id.is_some() && !req.reassign_bound) {
                continue;
            }

            let mut candidates: Vec<u64> = proxies
                .iter()
                .filter(|view| !view.entry.disabled && !view.health.dead)
                .filter(
                    |view| match (region.as_deref(), view.entry.region.as_deref()) {
                        (Some(credential_region), Some(proxy_region)) => {
                            credential_region == proxy_region
                        }
                        (_, None) => true,
                        (None, Some(_)) => true,
                    },
                )
                .filter_map(|view| view.entry.id)
                .collect();
            candidates.sort_by_key(|id| *load.get(id).unwrap_or(&0));

            match candidates.first().copied() {
                Some(proxy_id) => match self
                    .token_manager
                    .set_proxy_id(credential_id, Some(proxy_id))
                {
                    Ok(()) => {
                        *load.entry(proxy_id).or_insert(0) += 1;
                        assigned.push((credential_id, proxy_id));
                    }
                    Err(error) => {
                        tracing::warn!(
                            "自动分配代理: 凭据 #{} 绑定代理 #{} 失败: {}",
                            credential_id,
                            proxy_id,
                            error
                        );
                        skipped.push(credential_id);
                    }
                },
                None => skipped.push(credential_id),
            }
        }

        ProxyAutoAssignResponse { assigned, skipped }
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
            if let Some(value) = req.filter_claude_code {
                cfg.prompt_filter.filter_claude_code = value;
            }
            if let Some(value) = req.filter_env_noise {
                cfg.prompt_filter.filter_env_noise = value;
            }
            if let Some(value) = req.filter_strip_boundaries {
                cfg.prompt_filter.filter_strip_boundaries = value;
            }
            if let Some(rules) = req.rules {
                cfg.prompt_filter.rules = rules
                    .iter()
                    .map(Self::prompt_filter_rule_from_dto)
                    .collect();
            }
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        let config = self.token_manager.config();
        *self.prompt_filter_config.write() = config.prompt_filter.clone();
        Ok(())
    }

    fn external_endpoint_alias_to_internal(value: &str) -> Result<&'static str, AdminServiceError> {
        match value {
            "auto" | "kiro" => Ok(IDE_ENDPOINT_NAME),
            "codewhisperer" => Ok(CODEWHISPERER_ENDPOINT_NAME),
            "amazonq" => Ok(AMAZONQ_ENDPOINT_NAME),
            _ => Err(AdminServiceError::InvalidRequest(
                "preferredEndpoint 必须是 auto、kiro、codewhisperer 或 amazonq".to_string(),
            )),
        }
    }

    fn validate_proxy_url(value: &str) -> Result<(), AdminServiceError> {
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
                "proxyUrl must start with http://, https://, socks5://, or socks5h://".to_string(),
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
            prompt_cache_max_ratio: config.prompt_cache_max_ratio,
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
                        "区域不能为空".to_string(),
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

            if let Some(max_ratio) = req.prompt_cache_max_ratio {
                if !(max_ratio > 0.0 && max_ratio <= 1.0) {
                    return Err(AdminServiceError::InvalidCredential(
                        "Prompt Cache max ratio 必须大于 0 且不超过 1".to_string(),
                    ));
                }
                cfg.prompt_cache_max_ratio = max_ratio;
            }

            if let Some(ref endpoint) = req.default_endpoint {
                let trimmed = endpoint.trim();
                if trimmed.is_empty() {
                    return Err(AdminServiceError::InvalidCredential(
                        "默认端点不能为空".to_string(),
                    ));
                }
                if !self.known_endpoints.contains(trimmed) {
                    let mut known: Vec<&str> =
                        self.known_endpoints.iter().map(|s| s.as_str()).collect();
                    known.sort_unstable();
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "未知的端点: {}，可用值: {:?}",
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

        // 关闭调度亲和后清空已有绑定，避免残留
        if let Some(false) = req.session_affinity_enabled {
            self.token_manager.clear_session_affinity();
        }

        // 热更新 region（注：xkiro 已剔除 credential_rpm，故不存在 update_credential_rpm 同步）
        if req.region.is_some() {
            self.token_manager.update_region(config.region.clone());
        }

        // 热更新 default_endpoint：先更新 token_manager，再同步 provider。
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
        if req.prompt_cache_ttl_seconds.is_some()
            || req.prompt_cache_accounting_enabled.is_some()
            || req.prompt_cache_max_ratio.is_some()
        {
            self.prompt_cache_runtime.write().update(
                req.prompt_cache_ttl_seconds,
                req.prompt_cache_accounting_enabled,
                req.prompt_cache_max_ratio,
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

    /// 获取用户模型映射规则（仅 OpenAI 路径生效）
    pub fn get_model_mappings(&self) -> ModelMappingsResponse {
        ModelMappingsResponse {
            rules: self.model_mapping_config.read().rules().to_vec(),
        }
    }

    /// 更新用户模型映射规则（全量替换；持久化到 config.json 后同步运行时）
    pub async fn update_model_mappings(
        &self,
        req: UpdateModelMappingsRequest,
    ) -> Result<ModelMappingsResponse, AdminServiceError> {
        let rules = req.rules;

        self.token_manager.with_config_mut(|cfg| {
            cfg.model_mappings = rules.clone();
            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        // 持久化成功 → 同步运行时（丢弃旧轮询计数器）
        self.model_mapping_config.write().replace(rules);

        Ok(self.get_model_mappings())
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

    pub fn export_credential_snapshot(
        &self,
        ids: &[serde_json::Value],
    ) -> CredentialSnapshotExportData {
        let requested_ids = Self::credential_export_id_filter(ids);
        let snapshot = self.token_manager.snapshot();
        let selected_ids: Vec<u64> = snapshot
            .entries
            .iter()
            .filter(|entry| {
                requested_ids.is_empty()
                    || requested_ids.contains(&entry.id.to_string())
                    || entry
                        .source_account_id
                        .as_deref()
                        .is_some_and(|id| requested_ids.contains(id))
            })
            .map(|entry| entry.id)
            .collect();
        let now = Utc::now().timestamp_millis();
        let credentials: Vec<_> = self
            .token_manager
            .export_credentials_with_state_by_ids(&selected_ids)
            .into_iter()
            .map(|(credential, enabled)| {
                Self::credential_snapshot_export_item(credential, enabled, now)
            })
            .collect();
        CredentialSnapshotExportData {
            version: env!("CARGO_PKG_VERSION").to_string(),
            exported_at: now,
            credentials,
            groups: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn credential_export_id_filter(ids: &[serde_json::Value]) -> HashSet<String> {
        ids.iter()
            .filter_map(|id| match id {
                serde_json::Value::String(value) => {
                    let value = value.trim();
                    (!value.is_empty()).then(|| value.to_string())
                }
                serde_json::Value::Number(value) => value.as_u64().map(|id| id.to_string()),
                _ => None,
            })
            .collect()
    }

    fn credential_snapshot_export_item(
        credential: KiroCredentials,
        enabled: bool,
        now: i64,
    ) -> CredentialSnapshotExportItem {
        let auth_method = credential
            .auth_method
            .as_deref()
            .map(KiroCredentials::canonical_auth_method_name)
            .map(str::to_string);
        let provider = credential
            .provider
            .clone()
            .or_else(|| {
                credential.auth_method.as_deref().map(|method| {
                    if method.eq_ignore_ascii_case("social") {
                        "Google".to_string()
                    } else {
                        "BuilderId".to_string()
                    }
                })
            })
            .unwrap_or_else(|| "BuilderId".to_string());
        let id = credential
            .meta
            .source_account_id
            .clone()
            .or_else(|| credential.id.map(|id| id.to_string()))
            .unwrap_or_default();
        let status = credential.meta.status.clone().unwrap_or_else(|| {
            if enabled {
                "active".to_string()
            } else {
                "disabled".to_string()
            }
        });

        CredentialSnapshotExportItem {
            id,
            email: credential.email.unwrap_or_default(),
            nickname: credential.meta.nickname.unwrap_or_default(),
            provider,
            user_id: credential.user_id,
            machine_id: credential.machine_id,
            credentials: CredentialExportMaterial {
                access_token: credential.access_token.unwrap_or_default(),
                csrf_token: credential.meta.csrf_token.unwrap_or_default(),
                refresh_token: credential.refresh_token.unwrap_or_default(),
                client_id: credential.client_id,
                client_secret: credential.client_secret,
                region: credential.region,
                expires_at: Self::rfc3339_millis(credential.expires_at.as_deref()),
                auth_method,
                provider: credential.provider,
            },
            subscription: CredentialSnapshotExportSubscription {
                subscription_type: Self::credential_export_subscription_type(
                    credential
                        .meta
                        .subscription_type
                        .as_deref()
                        .or(credential.meta.subscription_title.as_deref()),
                ),
                title: credential.meta.subscription_title,
            },
            usage: CredentialSnapshotExportUsage {
                current: credential.meta.usage_current.unwrap_or_default(),
                limit: credential.meta.usage_limit.unwrap_or_default(),
                percent_used: credential.meta.usage_percent.unwrap_or_default(),
                last_updated: credential.meta.last_refresh.unwrap_or(now),
            },
            tags: Self::credential_export_tags(credential.meta.tags.as_ref()),
            status,
            created_at: credential.meta.created_at.unwrap_or(now),
            last_used_at: credential.meta.last_used_at.unwrap_or(now),
        }
    }

    fn credential_export_subscription_type(raw: Option<&str>) -> String {
        match normalize_subscription_type(raw.unwrap_or_default()).as_str() {
            "PRO_PLUS" | "POWER" => "Pro_Plus".to_string(),
            "PRO" => "Pro".to_string(),
            "ENTERPRISE" => "Enterprise".to_string(),
            "TEAMS" => "Teams".to_string(),
            _ => "Free".to_string(),
        }
    }

    fn credential_export_tags(value: Option<&serde_json::Value>) -> Vec<String> {
        value
            .and_then(serde_json::Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(|tag| tag.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn rfc3339_millis(value: Option<&str>) -> i64 {
        value
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or_default()
    }

    fn rfc3339_seconds(value: Option<&str>) -> i64 {
        Self::rfc3339_seconds_opt(value).unwrap_or_default()
    }

    fn rfc3339_seconds_opt(value: Option<&str>) -> Option<i64> {
        value
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|dt| dt.timestamp())
    }

    pub fn export_credential_backup(&self, ids: &[u64]) -> CredentialBackup {
        let credentials: Vec<CredentialBackupEntry> = self
            .token_manager
            .export_credentials_with_state_by_ids(ids)
            .into_iter()
            .map(|(mut credential, enabled)| {
                credential.canonicalize_auth_method();
                credential.disabled = !enabled;
                CredentialBackupEntry { credential }
            })
            .collect();

        CredentialBackup {
            format: CREDENTIAL_BACKUP_FORMAT.to_string(),
            version: CREDENTIAL_BACKUP_VERSION,
            exported_at: Utc::now().to_rfc3339(),
            source: CredentialBackupSource {
                app: "xkiro.rs".to_string(),
                schema: CREDENTIAL_BACKUP_SCHEMA.to_string(),
                credential_count: credentials.len(),
            },
            credentials,
        }
    }

    pub fn import_credentials(&self, req: ImportCredentialsRequest) -> ImportCredentialsResponse {
        let parsed = match self.parse_credential_import(req.input) {
            Ok(parsed) => parsed,
            Err(reason) => {
                return ImportCredentialsResponse::invalid_import(reason);
            }
        };

        let source_format = parsed.source_format;
        let parsed_credentials = parsed.credentials;
        let mut normalized = Vec::with_capacity(parsed_credentials.len());
        for credential in parsed_credentials {
            normalized.push(self.normalize_imported_credential(credential));
        }

        let mut machine_counts = HashMap::<String, usize>::new();
        for credential in normalized.iter().filter_map(|result| result.as_ref().ok()) {
            if let Some(machine_id) = credential
                .machine_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                *machine_counts
                    .entry(machine_id.to_ascii_lowercase())
                    .or_default() += 1;
            }
        }

        let existing_ids: Vec<u64> = self
            .token_manager
            .snapshot()
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect();
        let existing_credentials = self.token_manager.export_credentials_by_ids(&existing_ids);

        let mut summary = CredentialImportSummary {
            parsed: normalized.len(),
            added: 0,
            skipped: 0,
            merged: 0,
            replaced: 0,
            invalid: 0,
        };
        let mut items = Vec::with_capacity(normalized.len());

        for (index, result) in normalized.into_iter().enumerate() {
            let mut credential = match result {
                Ok(credential) => credential,
                Err(reason) => {
                    summary.invalid += 1;
                    items.push(CredentialImportItem::invalid(
                        index,
                        source_format.clone(),
                        "(invalid)",
                        reason,
                    ));
                    continue;
                }
            };
            if !req.dry_run
                && let Err(e) = self.assign_machine_id_for_new_credential(&mut credential)
            {
                summary.invalid += 1;
                items.push(CredentialImportItem::invalid(
                    index,
                    source_format.clone(),
                    "(invalid)",
                    e.to_string(),
                ));
                continue;
            }

            let mut warnings = self.credential_import_warnings(&credential, &existing_credentials);
            if let Some(machine_id) = credential
                .machine_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                && machine_counts
                    .get(&machine_id.to_ascii_lowercase())
                    .copied()
                    .unwrap_or_default()
                    > 1
            {
                warnings.push("导入包内存在重复 machineId，默认保留以便真实机器模拟".to_string());
            }

            let fingerprint = Self::credential_import_fingerprint(&credential);
            let existing_match =
                Self::find_existing_import_match(&credential, &existing_credentials);
            let action_result: Result<
                (CredentialImportAction, Option<u64>, Option<String>),
                String,
            > = match (existing_match, req.mode) {
                (Some(existing), CredentialImportMode::SkipExisting) => Ok((
                    CredentialImportAction::Skipped,
                    Some(existing.id),
                    Some(existing.reason),
                )),
                (Some(existing), CredentialImportMode::MergeMissing) => {
                    if req.dry_run {
                        Ok((
                            CredentialImportAction::Merged,
                            Some(existing.id),
                            Some(existing.reason),
                        ))
                    } else {
                        self.token_manager
                            .merge_imported_credential_missing(existing.id, credential.clone())
                            .map(|_| {
                                (
                                    CredentialImportAction::Merged,
                                    Some(existing.id),
                                    Some(existing.reason),
                                )
                            })
                            .map_err(|e| e.to_string())
                    }
                }
                (Some(existing), CredentialImportMode::ReplaceExisting) => {
                    if req.dry_run {
                        Ok((
                            CredentialImportAction::Replaced,
                            Some(existing.id),
                            Some(existing.reason),
                        ))
                    } else {
                        self.token_manager
                            .replace_imported_credential(existing.id, credential.clone())
                            .map(|_| {
                                (
                                    CredentialImportAction::Replaced,
                                    Some(existing.id),
                                    Some(existing.reason),
                                )
                            })
                            .map_err(|e| e.to_string())
                    }
                }
                (None, _) => {
                    if req.dry_run {
                        Ok((CredentialImportAction::Added, credential.id, None))
                    } else {
                        self.token_manager
                            .add_imported_credential(credential.clone())
                            .map(|id| (CredentialImportAction::Added, Some(id), None))
                            .map_err(|e| e.to_string())
                    }
                }
            };

            match action_result {
                Ok((action, credential_id, reason)) => {
                    match action {
                        CredentialImportAction::Added => summary.added += 1,
                        CredentialImportAction::Skipped => summary.skipped += 1,
                        CredentialImportAction::Merged => summary.merged += 1,
                        CredentialImportAction::Replaced => summary.replaced += 1,
                        CredentialImportAction::Invalid => summary.invalid += 1,
                    }
                    items.push(Self::credential_import_item(
                        index,
                        action,
                        source_format.clone(),
                        fingerprint,
                        credential_id,
                        reason,
                        &credential,
                        warnings,
                    ));
                }
                Err(reason) => {
                    summary.invalid += 1;
                    items.push(Self::credential_import_item(
                        index,
                        CredentialImportAction::Invalid,
                        source_format.clone(),
                        fingerprint,
                        None,
                        Some(reason),
                        &credential,
                        warnings,
                    ));
                }
            }
        }

        ImportCredentialsResponse { summary, items }
    }

    fn credential_import_item(
        index: usize,
        action: CredentialImportAction,
        source_format: String,
        fingerprint: String,
        credential_id: Option<u64>,
        reason: Option<String>,
        credential: &KiroCredentials,
        warnings: Vec<String>,
    ) -> CredentialImportItem {
        CredentialImportItem {
            index,
            action,
            source_format,
            fingerprint,
            credential_id,
            reason,
            auth_method: credential.auth_method.clone(),
            provider: credential.provider.clone(),
            email: credential.email.clone(),
            user_id: credential.user_id.clone(),
            machine_id: credential.machine_id.clone(),
            group_id: credential.meta.group_id.clone(),
            tag_links: credential.meta.tag_links.clone(),
            has_usage_data: credential.meta.usage_data.is_some(),
            has_available_models_cache: credential.meta.available_models_cache.is_some(),
            source_failure_count: credential.meta.failure_count,
            source_last_failure_at: credential.meta.last_failure_at.clone(),
            source_disabled_reason: credential.meta.disabled_reason.clone(),
            source_success_count: credential.meta.success_count,
            region: credential.region.clone(),
            auth_region: credential.auth_region.clone(),
            api_region: credential.api_region.clone(),
            start_url: credential.start_url.clone(),
            client_id_hash: credential.client_id_hash.clone(),
            sso_session_id: credential.sso_session_id.clone(),
            token_endpoint: credential.token_endpoint.clone(),
            issuer_url: credential.issuer_url.clone(),
            scopes: credential.scopes.clone(),
            endpoint: credential.endpoint.clone(),
            will_refresh: false,
            has_profile_arn: credential.profile_arn_trimmed().is_some(),
            has_token: Self::has_import_string(&credential.access_token),
            has_refresh_token: Self::has_import_string(&credential.refresh_token),
            has_client_id: Self::has_import_string(&credential.client_id),
            has_client_secret: Self::has_import_string(&credential.client_secret),
            has_id_token: Self::has_import_string(&credential.id_token),
            has_api_key: Self::has_import_string(&credential.api_key),
            has_proxy_credentials: Self::has_import_string(&credential.proxy_username)
                || Self::has_import_string(&credential.proxy_password),
            warnings,
        }
    }

    fn parse_credential_import(
        &self,
        value: serde_json::Value,
    ) -> Result<ParsedCredentialImport, String> {
        if let Some(object) = value.as_object() {
            if let Some(format) = Self::get_clean_string(object, &["format"]) {
                if format != CREDENTIAL_BACKUP_FORMAT {
                    return Err(format!("不支持的完整备份格式: {}", format));
                }
                let version = Self::get_u32(object, &["version"]).unwrap_or(0);
                if version != CREDENTIAL_BACKUP_VERSION {
                    return Err(format!("不支持的完整备份版本: {}", version));
                }
                let entries = object
                    .get("credentials")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| "xkiro.rs 完整备份缺少 credentials 数组".to_string())?;
                let mut credentials = Vec::with_capacity(entries.len());
                for entry in entries {
                    let credential_value = entry.get("credential").unwrap_or(entry);
                    let credential_object = credential_value
                        .as_object()
                        .ok_or_else(|| "xkiro.rs 完整备份 credential 必须是对象".to_string())?;
                    credentials.push(Self::credential_from_import_object(
                        credential_object,
                        false,
                    )?);
                }
                return Ok(ParsedCredentialImport {
                    source_format: CREDENTIAL_BACKUP_FORMAT.to_string(),
                    credentials,
                });
            }

            if let Some(snapshot_items) = Self::credential_snapshot_items(object) {
                let mut credentials = Vec::with_capacity(snapshot_items.len());
                for snapshot_item in snapshot_items {
                    let snapshot_item = snapshot_item
                        .as_object()
                        .ok_or_else(|| "凭据快照条目必须是对象".to_string())?;
                    credentials.push(Self::credential_from_snapshot_item(snapshot_item)?);
                }
                return Ok(ParsedCredentialImport {
                    source_format: SOURCE_FORMAT_CREDENTIAL_SNAPSHOT.to_string(),
                    credentials,
                });
            }

            if let Some(items) = object.get("items") {
                return Self::parse_cached_credentials(items);
            }

            if Self::looks_like_cached_credential(object) {
                return Self::parse_cached_credentials(&value);
            }

            return Ok(ParsedCredentialImport {
                source_format: SOURCE_FORMAT_FLAT_CREDENTIAL.to_string(),
                credentials: vec![Self::credential_from_import_object(object, false)?],
            });
        }

        if let Some(array) = value.as_array() {
            if array.iter().all(|item| {
                item.as_object()
                    .is_some_and(Self::looks_like_cached_credential)
            }) {
                return Self::parse_cached_credentials(&value);
            }

            let mut credentials = Vec::with_capacity(array.len());
            for item in array {
                let object = item
                    .as_object()
                    .ok_or_else(|| "凭据数组元素必须是对象".to_string())?;
                credentials.push(Self::credential_from_import_object(object, false)?);
            }
            return Ok(ParsedCredentialImport {
                source_format: SOURCE_FORMAT_FLAT_CREDENTIAL.to_string(),
                credentials,
            });
        }

        Err("导入内容必须是对象或数组".to_string())
    }

    fn credential_snapshot_items(
        object: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<&Vec<serde_json::Value>> {
        object
            .get("credentials")
            .and_then(serde_json::Value::as_array)
            .or_else(|| object.get("accounts").and_then(serde_json::Value::as_array))
    }

    fn looks_like_cached_credential(object: &serde_json::Map<String, serde_json::Value>) -> bool {
        if Self::get_value(object, &["refreshToken", "refresh_token"]).is_none() {
            return false;
        }
        let cached_credential_keys = [
            "provider",
            "authMethod",
            "auth_method",
            "clientId",
            "client_id",
            "clientSecret",
            "client_secret",
            "priority",
            "weight",
            "region",
            "apiRegion",
            "api_region",
            "machineId",
            "machine_id",
        ];
        let full_credential_keys = [
            "accessToken",
            "access_token",
            "profileArn",
            "profile_arn",
            "expiresAt",
            "expires_at",
            "tokenEndpoint",
            "token_endpoint",
            "issuerUrl",
            "issuer_url",
            "startUrl",
            "start_url",
            "idToken",
            "id_token",
            "ssoSessionId",
            "sso_session_id",
            "apiKey",
            "api_key",
            "kiroApiKey",
            "kiro_api_key",
            "proxyUrl",
            "proxyURL",
            "proxy_url",
            "endpoint",
            "concurrency",
            "disabled",
            "subscriptionTitle",
            "subscription_title",
            "usageData",
            "usage_data",
        ];
        cached_credential_keys
            .iter()
            .any(|key| object.contains_key(*key))
            && !full_credential_keys
                .iter()
                .any(|key| object.contains_key(*key))
    }

    fn parse_cached_credentials(
        value: &serde_json::Value,
    ) -> Result<ParsedCredentialImport, String> {
        let item_values: Vec<&serde_json::Value> = if let Some(items) = value.as_array() {
            items.iter().collect()
        } else {
            vec![value]
        };
        let mut credentials = Vec::with_capacity(item_values.len());
        for item in item_values {
            let object = item
                .as_object()
                .ok_or_else(|| "缓存凭据条目必须是对象".to_string())?;
            credentials.push(Self::credential_from_cached_credential(object));
        }
        Ok(ParsedCredentialImport {
            source_format: SOURCE_FORMAT_CACHED_CREDENTIAL.to_string(),
            credentials,
        })
    }

    fn credential_from_cached_credential(
        object: &serde_json::Map<String, serde_json::Value>,
    ) -> KiroCredentials {
        let mut credential = Self::credential_from_normalized_import_object(object, false);
        if credential.auth_method.is_none()
            && let Some(provider) = credential.provider.as_deref()
            && let Some(auth_method) = Self::canonical_explicit_auth_method(Some(provider), false)
        {
            credential.auth_method = Some(auth_method);
        }
        credential
    }

    fn credential_from_snapshot_item(
        snapshot_item: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<KiroCredentials, String> {
        let numeric_expires_at_is_millis = snapshot_item
            .get("credentials")
            .and_then(serde_json::Value::as_object)
            .is_some();
        Self::credential_from_import_object(snapshot_item, numeric_expires_at_is_millis)
    }

    fn credential_from_import_object(
        object: &serde_json::Map<String, serde_json::Value>,
        numeric_expires_at_is_millis: bool,
    ) -> Result<KiroCredentials, String> {
        let mut merged = if let Some(credentials) = object
            .get("credentials")
            .and_then(serde_json::Value::as_object)
        {
            let mut merged = credentials.clone();
            for (key, value) in object {
                if key == "credentials" {
                    continue;
                }
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
            merged
        } else {
            object.clone()
        };

        if let Some(value) = object.get("idp") {
            merged
                .entry("provider".to_string())
                .or_insert_with(|| value.clone());
        }
        if let Some(subscription) = object
            .get("subscription")
            .and_then(serde_json::Value::as_object)
        {
            if let Some(title) = subscription.get("title") {
                merged
                    .entry("subscriptionTitle".to_string())
                    .or_insert_with(|| title.clone());
            }
            if let Some(subscription_type) = subscription.get("type") {
                merged
                    .entry("subscriptionType".to_string())
                    .or_insert_with(|| subscription_type.clone());
            }
        }
        if let Some(usage) = object.get("usage").and_then(serde_json::Value::as_object) {
            for (target, source) in [
                ("usageCurrent", "current"),
                ("usageLimit", "limit"),
                ("usagePercent", "percentUsed"),
                ("lastRefresh", "lastUpdated"),
            ] {
                if let Some(value) = usage.get(source) {
                    merged
                        .entry(target.to_string())
                        .or_insert_with(|| value.clone());
                }
            }
        }
        Ok(Self::credential_from_normalized_import_object(
            &merged,
            numeric_expires_at_is_millis,
        ))
    }

    fn credential_from_normalized_import_object(
        object: &serde_json::Map<String, serde_json::Value>,
        numeric_expires_at_is_millis: bool,
    ) -> KiroCredentials {
        let mut credential = KiroCredentials {
            id: Self::get_u64(object, &["id"]),
            access_token: Self::get_clean_string(object, &["accessToken", "access_token"]),
            refresh_token: Self::get_clean_string(object, &["refreshToken", "refresh_token"]),
            profile_arn: Self::get_clean_string(object, &["profileArn", "profile_arn"]),
            expires_at: Self::parse_import_expires_at(
                Self::get_value(object, &["expiresAt", "expires_at"]),
                numeric_expires_at_is_millis,
            ),
            auth_method: Self::get_clean_string(object, &["authMethod", "auth_method"]),
            provider: Self::get_clean_string(object, &["provider", "idp"]),
            user_id: Self::get_clean_string(object, &["userId", "user_id"]),
            client_id: Self::get_clean_string(object, &["clientId", "client_id"]),
            client_secret: Self::get_clean_string(object, &["clientSecret", "client_secret"]),
            token_endpoint: Self::get_clean_string(object, &["tokenEndpoint", "token_endpoint"]),
            issuer_url: Self::get_clean_string(object, &["issuerUrl", "issuer_url"]),
            scopes: Self::get_clean_string(object, &["scopes"]),
            start_url: Self::get_clean_string(object, &["startUrl", "start_url"]),
            client_id_hash: Self::get_clean_string(object, &["clientIdHash", "client_id_hash"]),
            id_token: Self::get_clean_string(object, &["idToken", "id_token"]),
            sso_session_id: Self::get_clean_string(object, &["ssoSessionId", "sso_session_id"]),
            priority: Self::get_u32(object, &["priority"]).unwrap_or_default(),
            weight: Self::get_u32(object, &["weight"]).unwrap_or_default(),
            concurrency: Self::get_u32(object, &["concurrency"]),
            region: Self::get_clean_string(object, &["region"]),
            auth_region: Self::get_clean_string(object, &["authRegion", "auth_region"]),
            api_region: Self::get_clean_string(object, &["apiRegion", "api_region"]),
            machine_id: Self::get_clean_string(object, &["machineId", "machine_id"]),
            email: Self::get_clean_string(object, &["email"]),
            proxy_url: Self::get_clean_string(object, &["proxyUrl", "proxyURL", "proxy_url"]),
            proxy_username: Self::get_clean_string(object, &["proxyUsername", "proxy_username"]),
            proxy_password: Self::get_clean_string(object, &["proxyPassword", "proxy_password"]),
            proxy_id: Self::get_u64(object, &["proxyId", "proxy_id"]),
            disabled: Self::get_bool(object, &["disabled"]).unwrap_or_else(|| {
                Self::get_bool(object, &["enabled"])
                    .map(|enabled| !enabled)
                    .unwrap_or(false)
            }),
            api_key: Self::get_clean_string(
                object,
                &["apiKey", "api_key", "kiroApiKey", "kiro_api_key"],
            ),
            endpoint: Self::get_clean_string(object, &["endpoint"]),
            meta: CredentialSourceMetadata {
                source_account_id: Self::get_import_source_id(object),
                label: Self::get_clean_string(object, &["label"]),
                status: Self::get_clean_string(object, &["status"]),
                added_at: Self::get_clean_string(object, &["addedAt", "added_at"]),
                password: Self::get_clean_string(object, &["password"]),
                subscription_title: Self::get_clean_string(
                    object,
                    &["subscriptionTitle", "subscription_title"],
                ),
                overage_status: Self::get_clean_string(
                    object,
                    &["overageStatus", "overage_status"],
                ),
                usage_data: Self::get_cloned_non_null_value(object, &["usageData", "usage_data"]),
                group_id: Self::get_clean_string(object, &["groupId", "group_id"]),
                tag_links: Self::get_cloned_non_null_value(object, &["tagLinks", "tag_links"]),
                available_models_cache: Self::get_cloned_non_null_value(
                    object,
                    &["availableModelsCache", "available_models_cache"],
                ),
                failure_count: Self::get_u32(object, &["failureCount", "failure_count"]),
                last_failure_at: Self::get_clean_string(
                    object,
                    &["lastFailureAt", "last_failure_at"],
                ),
                disabled_reason: Self::get_clean_string(
                    object,
                    &["disabledReason", "disabled_reason"],
                ),
                success_count: Self::get_u64(object, &["successCount", "success_count"]),
                csrf_token: Self::get_clean_string(object, &["csrfToken", "csrf_token"]),
                nickname: Self::get_clean_string(object, &["nickname"]),
                ban_status: Self::get_clean_string(object, &["banStatus", "ban_status"]),
                ban_reason: Self::get_clean_string(object, &["banReason", "ban_reason"]),
                ban_time: Self::get_i64(object, &["banTime", "ban_time"]),
                subscription_type: Self::get_clean_string(
                    object,
                    &["subscriptionType", "subscription_type"],
                ),
                days_remaining: Self::get_i64(object, &["daysRemaining", "days_remaining"]),
                usage_current: Self::get_f64(object, &["usageCurrent", "usage_current"]),
                usage_limit: Self::get_f64(object, &["usageLimit", "usage_limit"]),
                usage_percent: Self::get_f64(object, &["usagePercent", "usage_percent"]),
                next_reset_date: Self::get_clean_string(
                    object,
                    &["nextResetDate", "next_reset_date"],
                ),
                last_refresh: Self::get_i64(object, &["lastRefresh", "last_refresh"]),
                trial_usage_current: Self::get_f64(
                    object,
                    &["trialUsageCurrent", "trial_usage_current"],
                ),
                trial_usage_limit: Self::get_f64(object, &["trialUsageLimit", "trial_usage_limit"]),
                trial_usage_percent: Self::get_f64(
                    object,
                    &["trialUsagePercent", "trial_usage_percent"],
                ),
                trial_status: Self::get_clean_string(object, &["trialStatus", "trial_status"]),
                trial_expires_at: Self::get_i64(object, &["trialExpiresAt", "trial_expires_at"]),
                overage_capability: Self::get_clean_string(
                    object,
                    &["overageCapability", "overage_capability"],
                ),
                overage_cap: Self::get_f64(object, &["overageCap", "overage_cap"]),
                overage_rate: Self::get_f64(object, &["overageRate", "overage_rate"]),
                current_overages: Self::get_f64(object, &["currentOverages", "current_overages"]),
                overage_checked_at: Self::get_i64(
                    object,
                    &["overageCheckedAt", "overage_checked_at"],
                ),
                request_count: Self::get_u64(object, &["requestCount", "request_count"]),
                error_count: Self::get_u64(object, &["errorCount", "error_count"]),
                total_tokens: Self::get_u64(object, &["totalTokens", "total_tokens"]),
                total_credits: Self::get_f64(object, &["totalCredits", "total_credits"]),
                last_used_at: Self::get_i64(object, &["lastUsedAt", "last_used_at", "lastUsed"]),
                created_at: Self::get_i64(object, &["createdAt", "created_at"]),
                tags: Self::get_cloned_non_null_value(object, &["tags"]),
                allow_overage_import: false,
                ..Default::default()
            },
        };

        if credential.proxy_url.is_none()
            && let Some(proxy_config) = object
                .get("proxyConfig")
                .or_else(|| object.get("proxy_config"))
                .and_then(serde_json::Value::as_object)
        {
            let (proxy_url, username, password) = Self::proxy_from_source_config(proxy_config);
            credential.proxy_url = proxy_url;
            credential.proxy_username = credential.proxy_username.or(username);
            credential.proxy_password = credential.proxy_password.or(password);
        }
        if credential.meta.overage_status.is_none()
            && Self::get_bool(object, &["allowOverage"]).unwrap_or(false)
        {
            credential.meta.overage_status = Some("ENABLED".to_string());
        }
        credential
    }

    fn normalize_imported_credential(
        &self,
        mut credential: KiroCredentials,
    ) -> Result<KiroCredentials, String> {
        Self::clean_import_credential_strings(&mut credential);
        let derived_external_idp = kiro_sso::derive_external_idp_endpoints(
            credential.user_id.as_deref().unwrap_or_default(),
            credential.client_id.as_deref().unwrap_or_default(),
            credential.access_token.as_deref().unwrap_or_default(),
        );
        let mut auth_method = Self::normalize_import_auth_method(
            credential.auth_method.as_deref(),
            credential.provider.as_deref(),
            credential.client_id.as_deref(),
            credential.client_secret.as_deref(),
            credential.token_endpoint.as_deref(),
            credential.api_key.as_deref(),
        );
        if let Some((derived_endpoint, _, _)) = derived_external_idp.as_ref()
            && kiro_sso::validate_external_idp_endpoint(derived_endpoint).is_ok()
            && auth_method != "external_idp"
        {
            auth_method = "external_idp".to_string();
        }
        credential.auth_method = Some(auth_method.clone());
        if credential.provider.is_none() {
            credential.provider = Self::infer_import_provider(&credential, &auth_method);
        }
        credential.provider =
            Self::normalize_provider_for_auth_method(credential.provider.take(), &auth_method);
        if auth_method == "idc" {
            Self::normalize_import_idc_metadata(&mut credential)?;
        }
        if auth_method == "external_idp" {
            if let Some((token_endpoint, issuer_url, scopes)) = derived_external_idp {
                if credential.token_endpoint.is_none() {
                    credential.token_endpoint = Some(token_endpoint);
                }
                if credential.issuer_url.is_none() {
                    credential.issuer_url = Some(issuer_url);
                }
                if credential.scopes.is_none() {
                    credential.scopes = Some(scopes);
                }
            }
            if credential.client_id.is_none() || credential.token_endpoint.is_none() {
                return Err(
                    "external_idp requires clientId and tokenEndpoint (or userId/accessToken to derive it)"
                        .to_string(),
                );
            }
            if let Some(endpoint) = credential.token_endpoint.as_deref() {
                kiro_sso::validate_external_idp_endpoint(endpoint)
                    .map_err(|e| format!("external IdP endpoint rejected: {}", e))?;
            }
            if let Some(issuer) = credential.issuer_url.as_deref() {
                kiro_sso::validate_external_idp_endpoint(issuer)
                    .map_err(|e| format!("external IdP issuer rejected: {}", e))?;
            }
            if credential.expires_at.is_none()
                && let Some(access_token) = credential.access_token.as_deref()
                && let Some(exp) = kiro_sso::exp_from_access_token_jwt(access_token)
                && let Some(expires_at) = chrono::DateTime::<Utc>::from_timestamp(exp, 0)
            {
                credential.expires_at = Some(expires_at.to_rfc3339());
            }
        }
        if let Some(endpoint) = credential.endpoint.as_deref()
            && !self.known_endpoints.contains(endpoint)
        {
            let mut known: Vec<&str> = self.known_endpoints.iter().map(String::as_str).collect();
            known.sort();
            return Err(format!(
                "未知端点 \"{}\"，已注册端点: {:?}",
                endpoint, known
            ));
        }
        if credential.concurrency == Some(0) {
            return Err("concurrency 必须 >= 1".to_string());
        }
        credential.machine_id = machine_id::normalize_optional_machine_id(credential.machine_id);
        credential.canonicalize_auth_method();
        credential.profile_arn = KiroCredentials::clean_profile_arn(credential.profile_arn.take());
        Ok(credential)
    }

    fn clean_import_credential_strings(credential: &mut KiroCredentials) {
        credential.access_token = Self::clean_import_string(credential.access_token.take());
        credential.refresh_token = Self::clean_import_string(credential.refresh_token.take());
        credential.profile_arn = Self::clean_import_string(credential.profile_arn.take());
        credential.expires_at = Self::clean_import_string(credential.expires_at.take());
        credential.auth_method = Self::clean_import_string(credential.auth_method.take());
        credential.provider = Self::clean_import_string(credential.provider.take());
        credential.user_id = Self::clean_import_string(credential.user_id.take());
        credential.client_id = Self::clean_import_string(credential.client_id.take());
        credential.client_secret = Self::clean_import_string(credential.client_secret.take());
        credential.token_endpoint = Self::clean_import_string(credential.token_endpoint.take());
        credential.issuer_url = Self::clean_import_string(credential.issuer_url.take());
        credential.scopes = Self::clean_import_string(credential.scopes.take());
        credential.start_url = Self::clean_import_string(credential.start_url.take());
        credential.client_id_hash = Self::clean_import_string(credential.client_id_hash.take());
        credential.id_token = Self::clean_import_string(credential.id_token.take());
        credential.sso_session_id = Self::clean_import_string(credential.sso_session_id.take());
        credential.region = Self::clean_import_string(credential.region.take());
        credential.auth_region = Self::clean_import_string(credential.auth_region.take());
        credential.api_region = Self::clean_import_string(credential.api_region.take());
        credential.machine_id = Self::clean_import_string(credential.machine_id.take());
        credential.email = Self::clean_import_string(credential.email.take());
        credential.meta.source_account_id =
            Self::clean_import_string(credential.meta.source_account_id.take());
        credential.meta.label = Self::clean_import_string(credential.meta.label.take());
        credential.meta.status = Self::clean_import_string(credential.meta.status.take());
        credential.meta.added_at = Self::clean_import_string(credential.meta.added_at.take());
        credential.meta.password = Self::clean_import_string(credential.meta.password.take());
        credential.meta.subscription_title =
            Self::clean_import_string(credential.meta.subscription_title.take());
        credential.meta.overage_status =
            Self::clean_import_string(credential.meta.overage_status.take());
        credential.meta.group_id = Self::clean_import_string(credential.meta.group_id.take());
        credential.meta.last_failure_at =
            Self::clean_import_string(credential.meta.last_failure_at.take());
        credential.meta.disabled_reason =
            Self::clean_import_string(credential.meta.disabled_reason.take());
        credential.meta.csrf_token = Self::clean_import_string(credential.meta.csrf_token.take());
        credential.meta.nickname = Self::clean_import_string(credential.meta.nickname.take());
        credential.meta.ban_status = Self::clean_import_string(credential.meta.ban_status.take());
        credential.meta.ban_reason = Self::clean_import_string(credential.meta.ban_reason.take());
        credential.meta.subscription_type =
            Self::clean_import_string(credential.meta.subscription_type.take());
        credential.meta.next_reset_date =
            Self::clean_import_string(credential.meta.next_reset_date.take());
        credential.meta.trial_status =
            Self::clean_import_string(credential.meta.trial_status.take());
        credential.meta.overage_capability =
            Self::clean_import_string(credential.meta.overage_capability.take());
        credential.proxy_url = Self::clean_import_string(credential.proxy_url.take());
        credential.proxy_username = Self::clean_import_string(credential.proxy_username.take());
        credential.proxy_password = Self::clean_import_string(credential.proxy_password.take());
        credential.api_key = Self::clean_import_string(credential.api_key.take());
        credential.endpoint = Self::clean_import_string(credential.endpoint.take());
    }

    fn normalize_import_auth_method(
        auth_method: Option<&str>,
        provider: Option<&str>,
        client_id: Option<&str>,
        client_secret: Option<&str>,
        token_endpoint: Option<&str>,
        api_key: Option<&str>,
    ) -> String {
        if api_key.is_some() {
            return "api_key".to_string();
        }
        let method_is_empty = auth_method
            .map(str::trim)
            .filter(|method| !method.is_empty())
            .is_none();
        let explicit = Self::canonical_explicit_auth_method(auth_method, true);
        let provider_implies_external_idp = Self::is_external_idp_provider_alias_option(provider);
        if explicit.as_deref() == Some("api_key") {
            "api_key".to_string()
        } else if explicit.as_deref() == Some("external_idp") || token_endpoint.is_some() {
            "external_idp".to_string()
        } else if let Some(method @ ("social" | "idc")) = explicit.as_deref() {
            method.to_string()
        } else if method_is_empty && provider_implies_external_idp {
            "external_idp".to_string()
        } else if method_is_empty && client_id.is_some() {
            "idc".to_string()
        } else if method_is_empty {
            "social".to_string()
        } else if client_id.is_some() && client_secret.is_some() {
            "idc".to_string()
        } else {
            "social".to_string()
        }
    }

    fn infer_import_provider(credential: &KiroCredentials, auth_method: &str) -> Option<String> {
        match auth_method {
            "external_idp" => Some("AzureAD".to_string()),
            "idc" => {
                if let Some(start_url) = credential.start_url.as_deref() {
                    Some(Self::idc_provider_for_start_url(start_url).to_string())
                } else {
                    Some("BuilderId".to_string())
                }
            }
            "social" => {
                if let Some(email) = credential.email.as_deref() {
                    let email = email.to_ascii_lowercase();
                    if email.contains("github") {
                        return Some("GitHub".to_string());
                    }
                    if email.contains("gmail") || email.contains("google") {
                        return Some("Google".to_string());
                    }
                }
                Some("Google".to_string())
            }
            _ => None,
        }
    }

    fn normalize_import_idc_metadata(credential: &mut KiroCredentials) -> Result<(), String> {
        credential.start_url = credential
            .start_url
            .take()
            .map(|value| Self::normalize_start_url(&value))
            .filter(|value| !value.is_empty());

        if credential.start_url.is_none()
            && let Some(start_url) = credential
                .client_secret
                .as_deref()
                .and_then(Self::extract_start_url_from_client_secret)
        {
            credential.start_url = Some(start_url);
        }

        let provider = Self::canonical_idc_provider(
            credential.provider.as_deref(),
            credential.start_url.as_deref(),
        );
        credential.provider = Some(provider.to_string());
        credential.client_id_hash = Some(Self::resolve_idc_client_id_hash(
            provider,
            credential.client_id_hash.as_deref(),
            credential.start_url.as_deref(),
        )?);
        Ok(())
    }

    fn canonical_idc_provider(provider: Option<&str>, start_url: Option<&str>) -> &'static str {
        if let Some(start_url) = start_url
            && Self::idc_provider_for_start_url(start_url) == "Enterprise"
        {
            return "Enterprise";
        }

        match provider
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "enterprise" | "iam" | "identitycenter" | "identity-center" | "identity_center"
            | "aws-sso" | "aws_sso" => "Enterprise",
            _ => "BuilderId",
        }
    }

    fn resolve_idc_client_id_hash(
        provider: &str,
        client_id_hash: Option<&str>,
        start_url: Option<&str>,
    ) -> Result<String, String> {
        let hash = if let Some(hash) = client_id_hash
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            hash.to_string()
        } else {
            match provider {
                "BuilderId" => start_url
                    .map(Self::calculate_client_id_hash)
                    .unwrap_or_else(|| KIRO_BUILDER_ID_CLIENT_ID_HASH.to_string()),
                "Enterprise" => {
                    let start_url = start_url
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .ok_or("Enterprise 凭据必须提供 startUrl 或 clientIdHash")?;
                    Self::calculate_client_id_hash(start_url)
                }
                other => return Err(format!("未知的 IAM Identity Center provider: {}", other)),
            }
        };

        if provider == "Enterprise" && hash.eq_ignore_ascii_case(KIRO_BUILDER_ID_CLIENT_ID_HASH) {
            return Err(
                "Enterprise 凭据不能使用 BuilderId 默认 clientIdHash，请检查 startUrl/clientIdHash"
                    .to_string(),
            );
        }
        Ok(hash)
    }

    fn normalize_start_url(start_url: &str) -> String {
        start_url.trim().trim_end_matches('/').to_string()
    }

    fn calculate_client_id_hash(start_url: &str) -> String {
        let normalized = Self::normalize_start_url(start_url);
        let input = serde_json::json!({ "startUrl": normalized }).to_string();
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(input.as_bytes());
        hasher.digest().to_string()
    }

    fn extract_start_url_from_client_secret(client_secret: &str) -> Option<String> {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let payload = client_secret.split('.').nth(1)?;
        let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let payload_json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
        let serialized = payload_json.get("serialized")?.as_str()?;
        let serialized_json: serde_json::Value = serde_json::from_str(serialized).ok()?;
        serialized_json
            .get("initiateLoginUri")?
            .as_str()
            .map(Self::normalize_start_url)
            .filter(|value| !value.is_empty())
    }

    fn credential_import_warnings(
        &self,
        credential: &KiroCredentials,
        existing_credentials: &[KiroCredentials],
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(machine_id) = credential
            .machine_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let incoming_id = credential.id;
            if existing_credentials.iter().any(|existing| {
                existing.id != incoming_id
                    && existing
                        .machine_id
                        .as_deref()
                        .map(str::trim)
                        .map(|value| value.eq_ignore_ascii_case(machine_id))
                        .unwrap_or(false)
            }) {
                warnings.push("machineId 与已有凭据重复，默认保留以便真实机器模拟".to_string());
            }
        }
        warnings
    }

    fn find_existing_import_match(
        credential: &KiroCredentials,
        existing_credentials: &[KiroCredentials],
    ) -> Option<ExistingCredentialMatch> {
        if let Some(id) = credential.id.filter(|id| *id > 0)
            && existing_credentials
                .iter()
                .any(|existing| existing.id == Some(id))
        {
            return Some(ExistingCredentialMatch {
                id,
                reason: "id 已存在".to_string(),
            });
        }

        if let Some(api_key) = credential
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && let Some(existing) = existing_credentials.iter().find(|existing| {
                existing
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .map(|value| value == api_key)
                    .unwrap_or(false)
            })
            && let Some(id) = existing.id
        {
            return Some(ExistingCredentialMatch {
                id,
                reason: "apiKey 已存在".to_string(),
            });
        }

        if let Some(refresh_token) = credential
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && let Some(existing) = existing_credentials.iter().find(|existing| {
                existing
                    .refresh_token
                    .as_deref()
                    .map(str::trim)
                    .map(|value| value == refresh_token)
                    .unwrap_or(false)
            })
            && let Some(id) = existing.id
        {
            return Some(ExistingCredentialMatch {
                id,
                reason: "refreshToken 已存在".to_string(),
            });
        }

        if let Some(user_id) = credential
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && let Some(existing) = existing_credentials.iter().find(|existing| {
                existing
                    .user_id
                    .as_deref()
                    .map(str::trim)
                    .map(|value| value == user_id)
                    .unwrap_or(false)
            })
            && let Some(id) = existing.id
        {
            return Some(ExistingCredentialMatch {
                id,
                reason: "userId 已存在".to_string(),
            });
        }

        if Self::imported_credential_is_social(credential)
            && let Some(email) = credential
                .email
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            && let Some(existing) = existing_credentials.iter().find(|existing| {
                Self::imported_credential_is_social(existing)
                    && existing
                        .email
                        .as_deref()
                        .map(str::trim)
                        .map(|value| value.eq_ignore_ascii_case(email))
                        .unwrap_or(false)
                    && Self::provider_matches(
                        credential.provider.as_deref(),
                        existing.provider.as_deref(),
                    )
            })
            && let Some(id) = existing.id
        {
            return Some(ExistingCredentialMatch {
                id,
                reason: "social provider + email 已存在".to_string(),
            });
        }

        None
    }

    fn imported_credential_is_social(credential: &KiroCredentials) -> bool {
        credential
            .canonical_auth_method()
            .is_some_and(|method| method == "social")
            || matches!(
                Self::normalize_provider_for_match(credential.provider.as_deref(), "social")
                    .as_deref(),
                Some("Google") | Some("GitHub")
            )
    }

    fn provider_matches(left: Option<&str>, right: Option<&str>) -> bool {
        match (left, right) {
            (Some(left), Some(right)) => {
                let left = Self::normalize_provider_for_match(Some(left), "social");
                let right = Self::normalize_provider_for_match(Some(right), "social");
                left.as_deref()
                    .zip(right.as_deref())
                    .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
            }
            _ => true,
        }
    }

    fn normalize_provider_for_match(provider: Option<&str>, auth_method: &str) -> Option<String> {
        KiroCredentials::normalize_provider_for_auth_method(
            provider.map(str::to_string),
            auth_method,
        )
    }

    fn credential_import_fingerprint(credential: &KiroCredentials) -> String {
        if let Some(email) = credential.email.as_deref() {
            return email.to_string();
        }
        if let Some(user_id) = credential.user_id.as_deref() {
            return user_id.to_string();
        }
        if let Some(refresh_token) = credential.refresh_token.as_deref() {
            let end = floor_char_boundary(refresh_token, refresh_token.len().min(16));
            return format!("{}...", &refresh_token[..end]);
        }
        if let Some(api_key) = credential.api_key.as_deref() {
            let end = floor_char_boundary(api_key, api_key.len().min(10));
            return format!("{}...", &api_key[..end]);
        }
        credential
            .id
            .map(|id| format!("id:{}", id))
            .unwrap_or_else(|| "(unknown)".to_string())
    }

    fn get_value<'a>(
        object: &'a serde_json::Map<String, serde_json::Value>,
        keys: &[&str],
    ) -> Option<&'a serde_json::Value> {
        keys.iter().find_map(|key| object.get(*key))
    }

    fn get_clean_string(
        object: &serde_json::Map<String, serde_json::Value>,
        keys: &[&str],
    ) -> Option<String> {
        Self::get_value(object, keys).and_then(|value| match value {
            serde_json::Value::String(value) => {
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_string())
            }
            serde_json::Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
    }

    fn get_cloned_non_null_value(
        object: &serde_json::Map<String, serde_json::Value>,
        keys: &[&str],
    ) -> Option<serde_json::Value> {
        Self::get_value(object, keys)
            .filter(|value| !value.is_null())
            .cloned()
    }

    fn get_import_source_id(object: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
        if let Some(value) =
            Self::get_clean_string(object, &["sourceAccountId", "source_account_id"])
        {
            return Some(value);
        }

        match object.get("id") {
            Some(serde_json::Value::String(value)) => {
                let value = value.trim();
                (!value.is_empty() && value.parse::<u64>().is_err()).then(|| value.to_string())
            }
            _ => None,
        }
    }

    fn get_bool(
        object: &serde_json::Map<String, serde_json::Value>,
        keys: &[&str],
    ) -> Option<bool> {
        Self::get_value(object, keys).and_then(|value| match value {
            serde_json::Value::Bool(value) => Some(*value),
            serde_json::Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "y" => Some(true),
                "false" | "0" | "no" | "n" => Some(false),
                _ => None,
            },
            _ => None,
        })
    }

    fn get_u32(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u32> {
        Self::get_value(object, keys).and_then(|value| match value {
            serde_json::Value::Number(value) => value.as_u64().and_then(|v| u32::try_from(v).ok()),
            serde_json::Value::String(value) => value.trim().parse::<u32>().ok(),
            _ => None,
        })
    }

    fn get_u64(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u64> {
        Self::get_value(object, keys).and_then(|value| match value {
            serde_json::Value::Number(value) => value.as_u64(),
            serde_json::Value::String(value) => value.trim().parse::<u64>().ok(),
            _ => None,
        })
    }

    fn get_i64(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<i64> {
        Self::get_value(object, keys).and_then(|value| match value {
            serde_json::Value::Number(value) => value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok())),
            serde_json::Value::String(value) => value.trim().parse::<i64>().ok(),
            _ => None,
        })
    }

    fn get_f64(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<f64> {
        Self::get_value(object, keys).and_then(|value| match value {
            serde_json::Value::Number(value) => value.as_f64(),
            serde_json::Value::String(value) => value.trim().parse::<f64>().ok(),
            _ => None,
        })
    }

    fn parse_import_expires_at(
        value: Option<&serde_json::Value>,
        numeric_is_millis: bool,
    ) -> Option<String> {
        match value? {
            serde_json::Value::String(value) => {
                let value = value.trim();
                if value.is_empty() {
                    None
                } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
                    Some(dt.with_timezone(&Utc).to_rfc3339())
                } else {
                    Some(value.to_string())
                }
            }
            serde_json::Value::Number(value) => {
                let raw = value
                    .as_i64()
                    .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))?;
                let use_millis = numeric_is_millis || raw.abs() >= 1_000_000_000_000;
                if use_millis {
                    let seconds = raw.div_euclid(1000);
                    let nanos = (raw.rem_euclid(1000) as u32) * 1_000_000;
                    chrono::DateTime::<Utc>::from_timestamp(seconds, nanos)
                        .map(|dt| dt.to_rfc3339())
                } else {
                    chrono::DateTime::<Utc>::from_timestamp(raw, 0).map(|dt| dt.to_rfc3339())
                }
            }
            _ => None,
        }
    }

    fn proxy_from_source_config(
        object: &serde_json::Map<String, serde_json::Value>,
    ) -> (Option<String>, Option<String>, Option<String>) {
        if !Self::get_bool(object, &["enabled"]).unwrap_or(false) {
            return (None, None, None);
        }
        let host = match Self::get_clean_string(object, &["host"]) {
            Some(host) => host,
            None => return (None, None, None),
        };
        let port = match Self::get_u32(object, &["port"]) {
            Some(port) if port > 0 => port,
            _ => return (None, None, None),
        };
        let protocol = Self::get_clean_string(object, &["protocol"])
            .unwrap_or_else(|| "http".to_string())
            .to_ascii_lowercase();
        let proxy_url = Some(format!("{}://{}:{}", protocol, host, port));
        let username = Self::get_clean_string(object, &["username"]);
        let password = Self::get_clean_string(object, &["password"]);
        (proxy_url, username, password)
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
    // ── 社交 OAuth 登录 ────────────────────────────────────────────────────

    fn clean_social_profile_arn(profile_arn: Option<String>) -> Option<String> {
        KiroCredentials::clean_profile_arn(profile_arn)
    }

    async fn enrich_social_login_credential(
        &self,
        credential: &mut KiroCredentials,
        proxy: Option<&ProxyConfig>,
    ) -> Result<(), AdminServiceError> {
        let access_token = credential
            .access_token
            .as_deref()
            .ok_or_else(|| {
                AdminServiceError::InvalidCredential("社交登录缺少 accessToken".to_string())
            })?
            .to_string();
        let config = self.token_manager.config();
        let usage = get_usage_limits(credential, &config, &access_token, proxy, true)
            .await
            .map_err(|e| {
                AdminServiceError::InternalError(format!("社交登录后获取身份信息失败: {}", e))
            })?;

        if let Some(user_id) = usage
            .user_info
            .as_ref()
            .and_then(|user| user.user_id.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            credential.user_id = Some(user_id.to_string());
        }

        if credential
            .email
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
            && let Some(email) = usage
                .user_info
                .as_ref()
                .and_then(|user| user.email.as_deref())
                .or(usage.email.as_deref())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        {
            credential.email = Some(email.to_string());
        }

        credential.meta.subscription_title = usage.subscription_title().map(str::to_string);
        credential.meta.overage_status = usage.overage_status().map(str::to_string);

        match crate::kiro::models::fetch_all_available_models(
            credential,
            &config,
            &access_token,
            proxy,
            None,
        )
        .await
        {
            Ok(_) => {}
            Err(message) if message.starts_with("BANNED:") => {
                return Err(AdminServiceError::InvalidCredential(
                    "BANNED: 凭据已被封禁".to_string(),
                ));
            }
            Err(message) => {
                tracing::warn!("社交登录后检测可用模型失败（不影响凭据添加）: {}", message);
            }
        }

        Ok(())
    }

    pub async fn start_social_login(
        &self,
        req: StartSocialLoginRequest,
    ) -> Result<StartSocialLoginResponse, AdminServiceError> {
        let provider = social::resolve_provider_names(&req.provider)
            .map_err(|err| AdminServiceError::InvalidCredential(err.to_string()))?;
        let credential_provider = provider.credential_provider.to_string();

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

        let mut cred_template = KiroCredentials {
            auth_method: Some("social".to_string()),
            provider: Some(credential_provider),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };
        if !is_helper {
            self.assign_machine_id_for_new_credential(&mut cred_template)?;
        }

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
                provider.portal_idp,
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
                let details = self.credential_login_details_from_stored(credential_id);
                Ok(PollSocialLoginResponse::success(credential_id, details))
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
            .as_deref()
            .and_then(machine_id::normalize_machine_id)
            .map(Ok)
            .unwrap_or_else(|| self.machine_id_for_new_credential())?;

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

        let SocialAuthSession {
            cred_template,
            proxy,
            ..
        } = session;
        let mut new_cred = cred_template;
        if new_cred.machine_id.is_none() {
            new_cred.machine_id = Some(machine_id);
        }
        new_cred.access_token = Some(token_resp.access_token);
        new_cred.refresh_token = token_resp.refresh_token;
        new_cred.profile_arn = Self::clean_social_profile_arn(token_resp.profile_arn);

        if let Some(expires_at) = token_resp.expires_at {
            new_cred.expires_at = Some(expires_at);
        } else if let Some(expires_in) = token_resp.expires_in {
            let ea = Utc::now() + chrono::Duration::seconds(expires_in);
            new_cred.expires_at = Some(ea.to_rfc3339());
        }
        self.assign_proxy_before_validation(&mut new_cred)?;
        let validation_proxy = self.proxy_for_validation(&new_cred, proxy.as_ref())?;

        self.enrich_social_login_credential(&mut new_cred, validation_proxy.as_ref())
            .await?;

        let credential_id = self
            .token_manager
            .upsert_prevalidated_social_credential(new_cred)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        tracing::info!("社交登录成功，已添加凭据 #{}", credential_id);
        let details = self.credential_login_details_from_stored(credential_id);
        Ok(PollSocialLoginResponse::success(credential_id, details))
    }

    pub async fn complete_social_login(
        &self,
        session_id: &str,
        req: CompleteSocialLoginRequest,
    ) -> Result<(), AdminServiceError> {
        let (cred_template, proxy) = {
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
            (session.cred_template.clone(), session.proxy.clone())
        };

        let mut new_cred = cred_template;
        new_cred.access_token = Some(req.access_token);
        new_cred.refresh_token = req.refresh_token;
        new_cred.profile_arn = Self::clean_social_profile_arn(req.profile_arn);
        if req.machine_id.is_some() {
            new_cred.machine_id = req.machine_id;
        }
        self.assign_machine_id_for_new_credential(&mut new_cred)?;
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

        if let Err(e) = self.assign_proxy_before_validation(&mut new_cred) {
            if let Some(session) = self.social_sessions.lock().get_mut(session_id) {
                session.helper_completing = false;
                session.helper_result = Some(Err(e.to_string()));
            }
            return Err(e);
        }

        let validation_proxy = match self.proxy_for_validation(&new_cred, proxy.as_ref()) {
            Ok(proxy) => proxy,
            Err(e) => {
                if let Some(session) = self.social_sessions.lock().get_mut(session_id) {
                    session.helper_completing = false;
                    session.helper_result = Some(Err(e.to_string()));
                }
                return Err(e);
            }
        };

        let enrich_result = self
            .enrich_social_login_credential(&mut new_cred, validation_proxy.as_ref())
            .await;
        if let Err(e) = enrich_result {
            if let Some(session) = self.social_sessions.lock().get_mut(session_id) {
                session.helper_completing = false;
                session.helper_result = Some(Err(e.to_string()));
            }
            return Err(e);
        }

        let result = self
            .token_manager
            .upsert_prevalidated_social_credential(new_cred)
            .map_err(|e| e.to_string());

        // 写回结果并释放占用；会话此时必然仍在（helper_completing 阻止了 poll 清除）
        if let Some(session) = self.social_sessions.lock().get_mut(session_id) {
            session.helper_completing = false;
            session.helper_result = Some(result.clone());
        }

        match result {
            Ok(credential_id) => {
                tracing::info!("社交登录 helper 回传成功，已添加凭据 #{}", credential_id);
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
        let provider = Self::idc_provider_for_start_url(start_url);
        let start_url_metadata = Self::idc_start_url_metadata(start_url);
        let client_id_hash = Self::resolve_idc_client_id_hash(provider, None, Some(start_url))
            .map_err(AdminServiceError::InvalidCredential)?;

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
            provider: Some(provider.to_string()),
            client_id: Some(registered.client_id.clone()),
            client_secret: Some(registered.client_secret.clone()),
            start_url: start_url_metadata,
            client_id_hash: Some(client_id_hash),
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
        let provider = Self::idc_provider_for_start_url(start_url);
        let start_url_metadata = Self::idc_start_url_metadata(start_url);
        let client_id_hash = Self::resolve_idc_client_id_hash(provider, None, Some(start_url))
            .map_err(AdminServiceError::InvalidCredential)?;

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
            provider: Some(provider.to_string()),
            client_id: Some(started.client_id.clone()),
            client_secret: Some(started.client_secret.clone()),
            start_url: start_url_metadata,
            client_id_hash: Some(client_id_hash),
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
    ) -> Result<CompleteIamSsoLoginResponse, AdminServiceError> {
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
        self.assign_machine_id_for_new_credential(&mut new_cred)?;
        self.assign_proxy_before_validation(&mut new_cred)?;

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
        let details = self.credential_login_details_from_stored(credential_id);

        Ok(CompleteIamSsoLoginResponse::success(details))
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
                self.assign_machine_id_for_new_credential(&mut new_cred)?;
                self.assign_proxy_before_validation(&mut new_cred)?;

                let credential_id = self
                    .token_manager
                    .add_credential(new_cred)
                    .await
                    .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

                tracing::info!(
                    "IAM Identity Center 登录成功，已添加凭据 #{}",
                    credential_id
                );
                let details = self.credential_login_details_from_stored(credential_id);
                Ok(PollIdcLoginResponse::success(credential_id, details))
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
        let success = logs
            .iter()
            .filter(|log| log.status == super::stats::REQUEST_LOG_STATUS_SUCCESS)
            .count();
        let errors = total.saturating_sub(success);
        super::types::RequestLogsResponse {
            logs,
            total,
            success,
            errors,
        }
    }

    /// 清空请求日志
    pub fn clear_request_logs(&self) {
        self.request_stats.clear_logs();
    }

    /// 获取系统状态
    pub fn get_system_status(&self) -> super::types::SystemStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let available = snapshot.entries.iter().filter(|e| !e.disabled).count();

        super::types::SystemStatusResponse::new(
            "ok",
            env!("CARGO_PKG_VERSION"),
            self.request_stats.uptime(),
            self.request_stats.total_requests(),
            self.request_stats.success_requests(),
            self.request_stats.failed_requests(),
            self.request_stats.total_tokens(),
            self.request_stats.total_credits(),
            snapshot.entries.len(),
            available,
        )
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

    /// 生成机器 ID
    pub fn generate_machine_id(&self) -> super::types::GenerateMachineIdResponse {
        let machine_id = machine_id::generate_account_machine_id();
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

    pub async fn test_credential_by_path_id(
        &self,
        path_id: &str,
        model: Option<String>,
    ) -> Result<CredentialProbeResponse, AdminServiceError> {
        let id = self.resolve_credential_id(path_id)?;
        let model =
            Self::clean_import_string(model).unwrap_or_else(|| "claude-sonnet-4".to_string());
        let request_body = self.credential_test_request_body(&model)?;
        let provider = self.kiro_provider.as_ref().ok_or_else(|| {
            AdminServiceError::UpstreamError("Kiro provider not configured".to_string())
        })?;
        let api_result = provider
            .call_api_with_credential(id, &request_body)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;
        let body = api_result
            .response
            .bytes()
            .await
            .map_err(|e| AdminServiceError::UpstreamError(format!("读取响应失败: {}", e)))?;
        let (reply, credits) = Self::extract_credential_probe_reply(&body)?;
        if credits > 0.0 {
            self.token_manager.apply_credit_usage(id, credits);
        }

        Ok(CredentialProbeResponse {
            success: true,
            reply,
            model,
        })
    }

    fn credential_test_request_body(&self, model: &str) -> Result<String, AdminServiceError> {
        let request = MessagesRequest {
            model: model.to_string(),
            max_tokens: 5,
            temperature: None,
            top_p: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String("say ok".to_string()),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: None,
                preserve_tool_names: true,
            }),
        };
        let conversion = convert_request_with_thinking_suffix(
            &request,
            &self.compression_config.read().clone(),
            &self.prompt_filter_config.read().clone(),
            false,
            &self.thinking_config.read().suffix,
        )
        .map_err(|e| AdminServiceError::InvalidRequest(format!("构建测试请求失败: {}", e)))?;
        let kiro_request = KiroRequest {
            conversation_state: conversion.conversation_state,
            inference_config: Some(InferenceConfig {
                max_tokens: Some(5),
                temperature: None,
                top_p: None,
            }),
            profile_arn: None,
        };
        serde_json::to_string(&kiro_request)
            .map_err(|e| AdminServiceError::InternalError(format!("序列化测试请求失败: {}", e)))
    }

    fn extract_credential_probe_reply(body: &[u8]) -> Result<(String, f64), AdminServiceError> {
        let mut decoder = EventStreamDecoder::new();
        decoder
            .feed(body)
            .map_err(|e| AdminServiceError::UpstreamError(format!("解析响应失败: {}", e)))?;

        let mut reply = String::new();
        let mut last_assistant_content = String::new();
        let mut credits = 0.0;
        for frame in decoder.decode_iter() {
            let frame = frame
                .map_err(|e| AdminServiceError::UpstreamError(format!("解析响应失败: {}", e)))?;
            let Ok(event) = Event::from_frame(frame) else {
                continue;
            };
            match event {
                Event::AssistantResponse(resp) => {
                    Self::append_cumulative_delta(
                        &mut reply,
                        &mut last_assistant_content,
                        &resp.content,
                    );
                }
                Event::Metering(metering) => {
                    credits = metering.usage;
                }
                _ => {}
            }
        }
        Ok((reply, credits))
    }

    fn append_cumulative_delta(output: &mut String, previous: &mut String, content: &str) {
        if content.is_empty() {
            return;
        }
        if previous.is_empty() {
            output.push_str(content);
            *previous = content.to_string();
            return;
        }
        if let Some(delta) = content.strip_prefix(previous.as_str()) {
            output.push_str(delta);
            *previous = content.to_string();
        } else if !previous.starts_with(content) {
            output.push_str(content);
            *previous = content.to_string();
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

    pub async fn batch_credentials(
        &self,
        request: CredentialBatchRequest,
    ) -> Result<CredentialBatchResponse, AdminServiceError> {
        if request.ids.is_empty() {
            return Err(AdminServiceError::InvalidRequest(
                "No credential IDs provided".to_string(),
            ));
        }

        match request.action.as_str() {
            "enable" | "disable" => {
                let disabled = request.action == "disable";
                for id in self
                    .resolve_credential_ids(&request.ids)
                    .into_iter()
                    .flatten()
                {
                    let _ = self.set_disabled(id, disabled);
                }
                Ok(CredentialBatchResponse {
                    success: true,
                    count: Some(request.ids.len()),
                    refreshed: None,
                    failed: None,
                })
            }
            "refresh" => {
                let mut refreshed = 0;
                let mut failed = 0;
                for id in self.resolve_credential_ids(&request.ids) {
                    let Some(id) = id else {
                        failed += 1;
                        continue;
                    };
                    match self.force_refresh_token(id).await {
                        Ok(_) => refreshed += 1,
                        Err(_) => failed += 1,
                    }
                }
                Ok(CredentialBatchResponse {
                    success: true,
                    count: None,
                    refreshed: Some(refreshed),
                    failed: Some(failed),
                })
            }
            _ => Err(AdminServiceError::InvalidRequest(format!(
                "Invalid action: {}",
                request.action
            ))),
        }
    }

    fn resolve_credential_ids(&self, ids: &[serde_json::Value]) -> Vec<Option<u64>> {
        let requested = Self::credential_request_id_strings(ids);
        let entries = self.token_manager.snapshot().entries;
        let lookup = Self::credential_request_id_lookup(&entries);
        requested
            .iter()
            .map(|requested_id| lookup.get(requested_id).copied())
            .collect()
    }

    fn resolve_credential_id(&self, path_id: &str) -> Result<u64, AdminServiceError> {
        let normalized = path_id.trim();
        if normalized.is_empty() {
            return Err(AdminServiceError::ResourceNotFound(
                "Credential not found".to_string(),
            ));
        }
        let entries = self.token_manager.snapshot().entries;
        Self::resolve_credential_entry_by_request_id(&entries, normalized)
            .map(|entry| entry.id)
            .ok_or_else(|| AdminServiceError::ResourceNotFound("Credential not found".to_string()))
    }

    fn resolve_credential_entry_by_request_id<'a>(
        entries: &'a [crate::kiro::token_manager::CredentialEntrySnapshot],
        requested_id: &str,
    ) -> Option<&'a crate::kiro::token_manager::CredentialEntrySnapshot> {
        let requested_local_id = requested_id.parse::<u64>().ok();
        entries.iter().find(|entry| {
            requested_local_id == Some(entry.id)
                || entry.source_account_id.as_deref() == Some(requested_id)
        })
    }

    fn credential_request_id_strings(ids: &[serde_json::Value]) -> Vec<String> {
        ids.iter()
            .filter_map(|id| match id {
                serde_json::Value::String(value) => {
                    let value = value.trim();
                    (!value.is_empty()).then(|| value.to_string())
                }
                serde_json::Value::Number(value) => value.as_u64().map(|id| id.to_string()),
                _ => None,
            })
            .collect()
    }

    fn credential_request_id_lookup(
        entries: &[crate::kiro::token_manager::CredentialEntrySnapshot],
    ) -> HashMap<String, u64> {
        let mut lookup = HashMap::with_capacity(entries.len().saturating_mul(2));
        for entry in entries {
            lookup.entry(entry.id.to_string()).or_insert(entry.id);
            if let Some(source_id) = entry
                .source_account_id
                .as_deref()
                .map(str::trim)
                .filter(|source_id| !source_id.is_empty())
            {
                lookup.entry(source_id.to_string()).or_insert(entry.id);
            }
        }
        lookup
    }

    // ========================================================================
    // SSO 令牌导入
    // ========================================================================

    /// 从 SSO 令牌导入凭据
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
                "未提供有效的 SSO 令牌".to_string(),
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

    /// 导入单个 SSO 令牌
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

        // 使用完整的 7 步 SSO 令牌导入流程
        let token = crate::kiro::auth::idc::import_sso_token(
            bearer_token,
            region,
            &self.token_manager.config(),
            proxy_config.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(format!("SSO 令牌导入失败: {}", e)))?;

        let mut new_cred = Self::sso_token_credential_from_import(token, region, priority, email);
        self.assign_machine_id_for_new_credential(&mut new_cred)?;
        self.assign_proxy_before_validation(&mut new_cred)?;

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        tracing::info!("SSO 令牌导入成功，已添加凭据 #{}", credential_id);
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
            provider: Some("BuilderId".to_string()),
            client_id: Some(token.client_id),
            client_secret: Some(token.client_secret),
            client_id_hash: Some(KIRO_BUILDER_ID_CLIENT_ID_HASH.to_string()),
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
            provider: Some("BuilderId".to_string()),
            client_id: Some(client_id),
            client_secret: Some(client_secret),
            client_id_hash: Some(KIRO_BUILDER_ID_CLIENT_ID_HASH.to_string()),
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

    fn idc_provider_for_start_url(start_url: &str) -> &'static str {
        let value = start_url.trim().trim_end_matches('/');
        let builder_id = idc::BUILDER_ID_START_URL.trim_end_matches('/');
        if value.is_empty() || value == builder_id {
            "BuilderId"
        } else {
            "Enterprise"
        }
    }

    fn idc_start_url_metadata(start_url: &str) -> Option<String> {
        let value = Self::normalize_start_url(start_url);
        (Self::idc_provider_for_start_url(&value) == "Enterprise").then_some(value)
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
                self.assign_machine_id_for_new_credential(&mut new_cred)?;
                self.assign_proxy_before_validation(&mut new_cred)?;
                let credential_id = self
                    .token_manager
                    .add_credential(new_cred)
                    .await
                    .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

                tracing::info!("Builder ID 登录成功，已添加凭据 #{}", credential_id);
                let details = self.credential_login_details_from_stored(credential_id);
                Ok(super::types::PollBuilderIdLoginResponse::success(
                    credential_id,
                    details,
                ))
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
                machine_id: Some(self.machine_id_for_new_credential()?),
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
                    details: None,
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
                details: None,
            }),
            Outcome::Expired => {
                self.kiro_sso_sessions.lock().remove(session_id);
                Ok(PollKiroSsoLoginResponse {
                    success: false,
                    completed: false,
                    status: None,
                    error: Some("SSO login timed out".to_string()),
                    details: None,
                })
            }
            Outcome::Cancelled => {
                self.kiro_sso_sessions.lock().remove(session_id);
                Ok(PollKiroSsoLoginResponse {
                    success: false,
                    completed: false,
                    status: None,
                    error: Some("登录已被取消".to_string()),
                    details: None,
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
                    .map(Ok)
                    .unwrap_or_else(|| self.machine_id_for_new_credential())?;
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
                new_cred.profile_arn = KiroCredentials::clean_profile_arn(token.profile_arn);
                new_cred.auth_method = Some("social".to_string());
                new_cred.provider =
                    Some(Self::kiro_sso_provider(kiro_sso::KiroSsoCaptureKind::Social).to_string());
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
                new_cred.provider = Some(
                    Self::kiro_sso_provider(kiro_sso::KiroSsoCaptureKind::ExternalIdp).to_string(),
                );
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
        self.assign_proxy_before_validation(&mut new_cred)?;

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let details = self.credential_login_details_from_stored(credential_id);
        Ok(PollKiroSsoLoginResponse::success(details))
    }

    fn credential_login_details_from_stored(
        &self,
        credential_id: u64,
    ) -> CredentialLoginDetailsResponse {
        self.credential_login_details_from_stored_with_email_hint(credential_id, None)
    }

    fn credential_login_details_from_stored_with_email_hint(
        &self,
        credential_id: u64,
        email_hint: Option<String>,
    ) -> CredentialLoginDetailsResponse {
        let stored = self
            .token_manager
            .export_credentials_by_ids(&[credential_id])
            .into_iter()
            .next();
        CredentialLoginDetailsResponse {
            id: credential_id,
            email: stored
                .as_ref()
                .and_then(|credential| credential.email.clone())
                .or(email_hint),
            auth_method: stored
                .as_ref()
                .and_then(|credential| credential.auth_method.clone()),
            provider: stored
                .as_ref()
                .and_then(|credential| credential.provider.clone()),
            user_id: stored
                .as_ref()
                .and_then(|credential| credential.user_id.clone()),
            source_account_id: stored
                .as_ref()
                .and_then(|credential| credential.meta.source_account_id.clone()),
            label: stored
                .as_ref()
                .and_then(|credential| credential.meta.label.clone()),
            status: stored
                .as_ref()
                .and_then(|credential| credential.meta.status.clone()),
            added_at: stored
                .as_ref()
                .and_then(|credential| credential.meta.added_at.clone()),
            nickname: stored
                .as_ref()
                .and_then(|credential| credential.meta.nickname.clone()),
            group_id: stored
                .as_ref()
                .and_then(|credential| credential.meta.group_id.clone()),
            tag_links: stored
                .as_ref()
                .and_then(|credential| credential.meta.tag_links.clone()),
            has_usage_data: stored
                .as_ref()
                .is_some_and(|credential| credential.meta.usage_data.is_some()),
            has_available_models_cache: stored
                .as_ref()
                .is_some_and(|credential| credential.meta.available_models_cache.is_some()),
            source_failure_count: stored
                .as_ref()
                .and_then(|credential| credential.meta.failure_count),
            source_last_failure_at: stored
                .as_ref()
                .and_then(|credential| credential.meta.last_failure_at.clone()),
            source_disabled_reason: stored
                .as_ref()
                .and_then(|credential| credential.meta.disabled_reason.clone()),
            source_success_count: stored
                .as_ref()
                .and_then(|credential| credential.meta.success_count),
            has_profile_arn: stored
                .as_ref()
                .is_some_and(|credential| credential.profile_arn_trimmed().is_some()),
            has_token: stored
                .as_ref()
                .is_some_and(|credential| Self::has_import_string(&credential.access_token)),
            has_refresh_token: stored
                .as_ref()
                .is_some_and(|credential| Self::has_import_string(&credential.refresh_token)),
            has_client_id: stored
                .as_ref()
                .is_some_and(|credential| Self::has_import_string(&credential.client_id)),
            has_client_secret: stored
                .as_ref()
                .is_some_and(|credential| Self::has_import_string(&credential.client_secret)),
            has_id_token: stored
                .as_ref()
                .is_some_and(|credential| Self::has_import_string(&credential.id_token)),
            has_api_key: stored
                .as_ref()
                .is_some_and(|credential| Self::has_import_string(&credential.api_key)),
            has_proxy_credentials: stored.as_ref().is_some_and(|credential| {
                Self::has_import_string(&credential.proxy_username)
                    || Self::has_import_string(&credential.proxy_password)
            }),
            region: stored
                .as_ref()
                .and_then(|credential| credential.region.clone()),
            auth_region: stored
                .as_ref()
                .and_then(|credential| credential.auth_region.clone()),
            api_region: stored
                .as_ref()
                .and_then(|credential| credential.api_region.clone()),
            machine_id: stored
                .as_ref()
                .and_then(|credential| credential.machine_id.clone()),
            start_url: stored
                .as_ref()
                .and_then(|credential| credential.start_url.clone()),
            client_id_hash: stored
                .as_ref()
                .and_then(|credential| credential.client_id_hash.clone()),
            sso_session_id: stored
                .as_ref()
                .and_then(|credential| credential.sso_session_id.clone()),
            token_endpoint: stored
                .as_ref()
                .and_then(|credential| credential.token_endpoint.clone()),
            issuer_url: stored
                .as_ref()
                .and_then(|credential| credential.issuer_url.clone()),
            scopes: stored
                .as_ref()
                .and_then(|credential| credential.scopes.clone()),
            endpoint: stored
                .as_ref()
                .and_then(|credential| credential.endpoint.clone()),
        }
    }

    fn kiro_sso_provider(kind: kiro_sso::KiroSsoCaptureKind) -> &'static str {
        match kind {
            kiro_sso::KiroSsoCaptureKind::Social => "Kiro SSO",
            kiro_sso::KiroSsoCaptureKind::ExternalIdp => "AzureAD",
        }
    }

    // ========================================================================
    // API 密钥管理
    // ========================================================================

    /// 从磁盘加载 API 密钥
    fn load_api_keys_from(path: &Option<PathBuf>) -> Vec<super::types::ApiKeyEntry> {
        if let Some(path) = path {
            if path.exists() {
                match std::fs::read_to_string(path) {
                    Ok(data) => match serde_json::from_str(&data) {
                        Ok(keys) => return keys,
                        Err(e) => {
                            tracing::warn!("解析 API 密钥文件失败: {}", e);
                        }
                    },
                    Err(e) => {
                        tracing::warn!("读取 API 密钥文件失败: {}", e);
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

    pub fn load_api_keys_runtime_with_config_key(
        cache_dir: Option<&std::path::Path>,
        config_api_key: Option<&str>,
        require_api_key: bool,
    ) -> SharedApiKeys {
        let path = cache_dir.map(|d| d.join("kiro_api_keys.json"));
        let mut keys = Self::load_api_keys_from(&path);
        let config_api_key = config_api_key.map(str::trim).filter(|key| !key.is_empty());

        if keys.is_empty() {
            if let Some(config_api_key) = config_api_key {
                keys.push(super::types::ApiKeyEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: Some("config-api-key".to_string()),
                    key: config_api_key.to_string(),
                    enabled: require_api_key,
                    migrated: true,
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
                                tracing::warn!(error = %e, "持久化迁移 API 密钥失败");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "序列化迁移 API 密钥失败");
                        }
                    }
                }
            }
        }

        Arc::new(RwLock::new(keys))
    }

    /// 保存 API 密钥到磁盘
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

    /// 获取所有 API 密钥（脱敏）
    pub fn get_api_keys(&self) -> super::types::ApiKeyListResponse {
        let keys = self.api_keys.read();
        let api_keys = keys.iter().map(to_api_key_view).collect();
        super::types::ApiKeyListResponse { api_keys }
    }

    /// 创建 API 密钥
    pub fn create_api_key(
        &self,
        request: super::types::CreateApiKeyRequest,
    ) -> Result<super::types::CreateApiKeyResponse, AdminServiceError> {
        let key_value = resolve_create_api_key_value(request.key.as_deref())?;
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
            migrated: false,
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

    /// 获取单个 API 密钥
    pub fn get_api_key(&self, id: &str) -> Result<super::types::ApiKeyView, AdminServiceError> {
        let keys = self.api_keys.read();
        keys.iter()
            .find(|k| k.id == id)
            .map(to_api_key_view)
            .ok_or_else(|| AdminServiceError::ResourceNotFound("API key not found".to_string()))
    }

    /// 更新 API 密钥
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

    /// 删除 API 密钥
    pub fn delete_api_key(&self, id: &str) -> Result<(), AdminServiceError> {
        let mut keys = self.api_keys.write();
        if let Some(index) = keys.iter().position(|k| k.id == id) {
            keys.remove(index);
            drop(keys);
            self.save_api_keys()?;
        }
        Ok(())
    }

    /// 重置 API 密钥使用量
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

    /// 验证 API 密钥（用于认证中间件）
    pub fn validate_api_key(&self, key: &str) -> Option<super::types::ApiKeyEntry> {
        let keys = self.api_keys.read();
        keys.iter().find(|k| k.key == key && k.enabled).cloned()
    }

    /// 记录 API 密钥使用量
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
            tracing::warn!(error = %e, "保存 API 密钥使用量失败");
        }
    }
}

struct ProxyGeo {
    region: Option<String>,
    country: Option<String>,
}

fn clean_proxy_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn proxy_entry_from_req(req: ProxyUpsertRequest) -> Result<ProxyEntry, AdminServiceError> {
    let url = req.url.trim();
    if url.is_empty() {
        return Err(AdminServiceError::InvalidRequest(
            "代理 URL 不能为空".to_string(),
        ));
    }
    let url = normalize_proxy_scheme(url);
    AdminService::validate_proxy_url(&url)?;
    Ok(ProxyEntry {
        id: None,
        url,
        username: clean_proxy_string(req.username),
        password: clean_proxy_string(req.password),
        region: clean_proxy_string(req.region),
        country: None,
        max_concurrency: req.max_concurrency.filter(|value| *value > 0),
        disabled: req.disabled,
        note: clean_proxy_string(req.note),
    })
}

fn normalize_proxy_scheme(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("sock5://") {
        return format!("socks5://{}", &url[7..]);
    }
    if lower.starts_with("sock4://") {
        return format!("socks4://{}", &url[7..]);
    }
    if lower.starts_with("sock://") {
        return format!("socks5://{}", &url[7..]);
    }
    url.to_string()
}

fn parse_proxy_line(line: &str) -> Result<(String, Option<String>, Option<String>), String> {
    let normalized = normalize_proxy_scheme(line.trim());
    let line = normalized.as_str();

    if line.contains(',') {
        let parts: Vec<&str> = line.splitn(3, ',').map(str::trim).collect();
        let url = parts.first().copied().unwrap_or_default();
        if url.is_empty() {
            return Err("url 为空".to_string());
        }
        return Ok((
            normalize_proxy_scheme(url),
            parts
                .get(1)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            parts
                .get(2)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        ));
    }

    if let Some(scheme_index) = line.find("://") {
        let scheme = &line[..scheme_index];
        let authority = &line[scheme_index + 3..];
        let authority = authority.split('/').next().unwrap_or(authority);
        if authority.contains('@') {
            return Ok((line.to_string(), None, None));
        }
        let parts: Vec<&str> = authority.split(':').collect();
        return match parts.as_slice() {
            [host, port] if !host.is_empty() && !port.is_empty() => {
                Ok((line.to_string(), None, None))
            }
            [host, port, user, pass] if !host.is_empty() && !port.is_empty() => Ok((
                format!("{}://{}:{}", scheme, host, port),
                (!user.is_empty()).then(|| user.to_string()),
                (!pass.is_empty()).then(|| pass.to_string()),
            )),
            _ => Err(format!(
                "无法识别的代理格式(authority 段应为 host:port 或 host:port:user:pass): {}",
                line
            )),
        };
    }

    let parts: Vec<&str> = line.split(':').map(str::trim).collect();
    match parts.as_slice() {
        [host, port] if !host.is_empty() && !port.is_empty() => {
            Ok((format!("socks5://{}:{}", host, port), None, None))
        }
        [host, port, user, pass] if !host.is_empty() && !port.is_empty() => Ok((
            format!("socks5://{}:{}", host, port),
            (!user.is_empty()).then(|| user.to_string()),
            (!pass.is_empty()).then(|| pass.to_string()),
        )),
        _ => Err(format!(
            "无法识别的代理格式(支持 ip:port[:user:pass] / scheme://host:port[:user:pass] / url,user,pass): {}",
            line
        )),
    }
}

async fn probe_proxy(entry: &ProxyEntry) -> ProxyTestResponse {
    let proxy_config = entry.to_proxy_config();
    let client = match crate::http_client::build_client(
        Some(&proxy_config),
        10,
        crate::model::config::TlsBackend::Rustls,
    ) {
        Ok(client) => client,
        Err(error) => {
            return ProxyTestResponse {
                ok: false,
                exit_ip: None,
                latency_ms: None,
                error: Some(format!("构建代理客户端失败: {}", error)),
            };
        }
    };

    let start = std::time::Instant::now();
    let response = client.get("https://api.ipify.org?format=text").send().await;
    let latency_ms = start.elapsed().as_millis() as u64;

    match response {
        Ok(response) if response.status().is_success() => ProxyTestResponse {
            ok: true,
            exit_ip: response
                .text()
                .await
                .ok()
                .map(|value| value.trim().to_string()),
            latency_ms: Some(latency_ms),
            error: None,
        },
        Ok(response) => ProxyTestResponse {
            ok: false,
            exit_ip: None,
            latency_ms: Some(latency_ms),
            error: Some(format!("探测端点返回 HTTP {}", response.status())),
        },
        Err(error) => ProxyTestResponse {
            ok: false,
            exit_ip: None,
            latency_ms: Some(latency_ms),
            error: Some(format!("代理连接失败: {}", error)),
        },
    }
}

async fn probe_proxy_geo(entry: &ProxyEntry) -> Option<ProxyGeo> {
    let proxy_config = entry.to_proxy_config();
    let client = crate::http_client::build_client(
        Some(&proxy_config),
        10,
        crate::model::config::TlsBackend::Rustls,
    )
    .ok()?;
    let response = client.get("https://ipinfo.io/json").send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let json: serde_json::Value = response.json().await.ok()?;
    let region_raw = json
        .get("region")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let country = json
        .get("country")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if region_raw.is_none() && country.is_none() {
        return None;
    }
    let region = match (&country, &region_raw) {
        (Some(country), Some(region)) => Some(format!("{}:{}", country, region)),
        (Some(country), None) => Some(country.clone()),
        (None, Some(region)) => Some(region.clone()),
        (None, None) => None,
    };
    Some(ProxyGeo { region, country })
}

fn to_api_key_view(entry: &super::types::ApiKeyEntry) -> super::types::ApiKeyView {
    super::types::ApiKeyView {
        id: entry.id.clone(),
        name: entry.name.clone(),
        key_masked: mask_api_key(&entry.key),
        enabled: entry.enabled,
        migrated: entry.migrated,
        created_at: entry.created_at,
        last_used_at: entry.last_used_at,
        token_limit: entry.token_limit,
        credit_limit: entry.credit_limit,
        tokens_used: entry.tokens_used,
        credits_used: entry.credits_used,
        requests_count: entry.requests_count,
    }
}

/// 生成 API 密钥值
fn generate_api_key_value() -> String {
    let bytes: Vec<u8> = (0..32).map(|_| fastrand::u8(..)).collect();
    format!("sk-{}", hex::encode(bytes))
}

fn resolve_create_api_key_value(key: Option<&str>) -> Result<String, AdminServiceError> {
    match key {
        None | Some("") => Ok(generate_api_key_value()),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(AdminServiceError::InvalidRequest(
                    "api key value must not be empty".to_string(),
                ));
            }
            Ok(trimmed.to_string())
        }
    }
}

/// 脱敏 API 密钥
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
        assert_eq!(
            cred.client_id_hash.as_deref(),
            Some(KIRO_BUILDER_ID_CLIENT_ID_HASH)
        );
        assert_eq!(cred.region.as_deref(), Some("us-east-1"));
        assert_eq!(cred.priority, 3);
        assert_eq!(cred.email.as_deref(), Some("user@example.com"));
        assert_eq!(cred.proxy_url.as_deref(), Some("direct"));
    }

    #[test]
    fn builder_id_expires_in_uses_default_when_missing() {
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
        assert_eq!(
            cred.client_id_hash.as_deref(),
            Some(KIRO_BUILDER_ID_CLIENT_ID_HASH)
        );
        assert_eq!(cred.region.as_deref(), Some("us-east-1"));
        assert_eq!(cred.priority, 7);
        assert_eq!(cred.email.as_deref(), Some("user@example.com"));
        assert!(cred.expires_at.is_some());
    }
}

#[cfg(test)]
mod credential_record_import_tests {
    use super::*;

    #[test]
    fn normalizes_external_idp_before_social_or_idc_fallbacks() {
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                Some("AzureAD"),
                None,
                Some("client"),
                None,
                None,
                None,
            ),
            "external_idp"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                None,
                None,
                Some("client"),
                None,
                Some("https://login.microsoftonline.com/t/oauth2/v2.0/token"),
                None,
            ),
            "external_idp"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                None,
                Some("Microsoft"),
                Some("client"),
                None,
                None,
                None,
            ),
            "external_idp"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                Some("enterprise"),
                None,
                Some("client"),
                Some("secret"),
                None,
                None,
            ),
            "idc"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                Some("builder_id"),
                None,
                Some("client"),
                Some("secret"),
                None,
                None,
            ),
            "idc"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                Some("GitHub"),
                None,
                None,
                None,
                None,
                None,
            ),
            "social"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                Some("social"),
                None,
                None,
                None,
                Some("https://login.microsoftonline.com/t/oauth2/v2.0/token"),
                None,
            ),
            "external_idp"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                None,
                None,
                Some("client"),
                None,
                None,
                None
            ),
            "idc"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                Some("weird"),
                None,
                Some("client"),
                None,
                None,
                None,
            ),
            "social"
        );
        assert_eq!(
            AdminService::normalize_credential_record_auth_method(
                Some("api-key"),
                None,
                None,
                None,
                None,
                Some("ksk_record_key"),
            ),
            "api_key"
        );
    }

    #[test]
    fn normalizes_external_idp_provider_to_azuread() {
        assert_eq!(
            AdminService::normalize_provider_for_auth_method(None, "external_idp").as_deref(),
            Some("AzureAD")
        );
        assert_eq!(
            AdminService::normalize_provider_for_auth_method(
                Some("Microsoft".to_string()),
                "external_idp"
            )
            .as_deref(),
            Some("AzureAD")
        );
        assert_eq!(
            AdminService::normalize_provider_for_auth_method(Some("Google".to_string()), "social")
                .as_deref(),
            Some("Google")
        );
    }

    #[test]
    fn import_auth_method_uses_core_alias_canonicalization() {
        assert_eq!(
            AdminService::normalize_import_auth_method(
                Some("api-key"),
                None,
                None,
                None,
                None,
                None,
            ),
            "api_key"
        );
        assert_eq!(
            AdminService::normalize_import_auth_method(
                Some("builder_id"),
                None,
                Some("client"),
                Some("secret"),
                None,
                None,
            ),
            "idc"
        );
        assert_eq!(
            AdminService::normalize_import_auth_method(
                Some("social"),
                None,
                None,
                None,
                Some("https://login.microsoftonline.com/t/oauth2/v2.0/token"),
                None,
            ),
            "external_idp"
        );
    }

    #[test]
    fn cached_credential_provider_only_auth_method_uses_core_alias_canonicalization() {
        for (provider, expected) in [
            ("GitHub", "social"),
            ("BuilderId", "idc"),
            ("Enterprise", "idc"),
            ("Microsoft", "external_idp"),
        ] {
            let mut object = serde_json::Map::new();
            object.insert(
                "provider".to_string(),
                serde_json::Value::String(provider.to_string()),
            );

            let credential = AdminService::credential_from_cached_credential(&object);
            assert_eq!(
                credential.auth_method.as_deref(),
                Some(expected),
                "{provider}"
            );
        }
    }

    #[test]
    fn imported_credential_social_matching_uses_core_provider_canonicalization() {
        let mut provider_only = KiroCredentials {
            provider: Some("Github".to_string()),
            ..Default::default()
        };
        assert!(AdminService::imported_credential_is_social(&provider_only));

        provider_only.provider = Some("google".to_string());
        assert!(AdminService::imported_credential_is_social(&provider_only));

        let auth_method_alias = KiroCredentials {
            auth_method: Some("GitHub".to_string()),
            ..Default::default()
        };
        assert!(AdminService::imported_credential_is_social(
            &auth_method_alias
        ));

        assert!(AdminService::provider_matches(
            Some("Github"),
            Some("GitHub")
        ));
        assert!(AdminService::provider_matches(
            Some("google"),
            Some("Google")
        ));
        assert!(!AdminService::provider_matches(
            Some("Google"),
            Some("GitHub")
        ));
    }

    #[test]
    fn parses_numeric_credential_record_id_only() {
        assert_eq!(
            AdminService::parse_credential_record_id(Some(&serde_json::json!(42))),
            Some(42)
        );
        assert_eq!(
            AdminService::parse_credential_record_id(Some(&serde_json::json!("43"))),
            Some(43)
        );
        assert_eq!(
            AdminService::parse_credential_record_id(Some(&serde_json::json!(
                "source-credential-1"
            ))),
            None
        );
    }

    #[test]
    fn maps_credential_record_import_enabled_and_disabled_flags() {
        assert!(!AdminService::credential_record_import_disabled(None, None));
        assert!(AdminService::credential_record_import_disabled(
            Some(true),
            Some(true)
        ));
        assert!(!AdminService::credential_record_import_disabled(
            Some(false),
            Some(false)
        ));
        assert!(AdminService::credential_record_import_disabled(
            None,
            Some(false)
        ));
        assert!(!AdminService::credential_record_import_disabled(
            None,
            Some(true)
        ));
    }

    #[test]
    fn credential_record_request_accepts_reference_metadata_fields() {
        let req: ImportCredentialRecordRequest = serde_json::from_value(serde_json::json!({
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
    fn credential_cache_item_accepts_weight_field() {
        let item: CredentialCacheItem = serde_json::from_value(serde_json::json!({
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
    fn parses_path_and_query_callback_with_encoded_code() {
        let params =
            parse_aws_sso_callback_params("/oauth/callback?code=abc%2Fdef&state=xyz").unwrap();

        assert_eq!(params.get("code").map(String::as_str), Some("abc/def"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
    }
}

#[cfg(test)]
mod kiro_sso_admin_tests {
    use super::*;

    #[test]
    fn kiro_sso_provider_matches_login_result_metadata() {
        assert_eq!(
            AdminService::kiro_sso_provider(kiro_sso::KiroSsoCaptureKind::Social),
            "Kiro SSO"
        );
        assert_eq!(
            AdminService::kiro_sso_provider(kiro_sso::KiroSsoCaptureKind::ExternalIdp),
            "AzureAD"
        );
    }
}

#[cfg(test)]
mod idc_auth_metadata_tests {
    use super::*;

    #[test]
    fn idc_provider_metadata_matches_reference_auth_results() {
        assert_eq!(
            AdminService::idc_provider_for_start_url(idc::BUILDER_ID_START_URL),
            "BuilderId"
        );
        assert_eq!(
            AdminService::idc_start_url_metadata(idc::BUILDER_ID_START_URL),
            None
        );

        let enterprise_start_url = "https://d-1234567890.awsapps.com/start";
        assert_eq!(
            AdminService::idc_provider_for_start_url(enterprise_start_url),
            "Enterprise"
        );
        assert_eq!(
            AdminService::idc_start_url_metadata(enterprise_start_url).as_deref(),
            Some(enterprise_start_url)
        );
        assert_eq!(
            AdminService::idc_start_url_metadata(&format!("{enterprise_start_url}/")).as_deref(),
            Some(enterprise_start_url)
        );
        assert_eq!(
            AdminService::resolve_idc_client_id_hash(
                "Enterprise",
                None,
                Some(enterprise_start_url)
            )
            .unwrap(),
            AdminService::calculate_client_id_hash(enterprise_start_url)
        );
    }

    #[test]
    fn builder_id_and_sso_token_credentials_persist_builderid_provider() {
        let builder = AdminService::builder_id_credential_template(
            "client-1".to_string(),
            "secret-1".to_string(),
            "us-east-1".to_string(),
            0,
            None,
            None,
        );
        assert_eq!(builder.auth_method.as_deref(), Some("idc"));
        assert_eq!(builder.provider.as_deref(), Some("BuilderId"));
        assert_eq!(
            builder.client_id_hash.as_deref(),
            Some(KIRO_BUILDER_ID_CLIENT_ID_HASH)
        );

        let sso_token = AdminService::sso_token_credential_from_import(
            idc::ImportedSsoToken {
                access_token: "access-1".to_string(),
                refresh_token: Some("refresh-1".to_string()),
                expires_in: None,
                client_id: "client-2".to_string(),
                client_secret: "secret-2".to_string(),
            },
            "us-east-1",
            0,
            None,
        );
        assert_eq!(sso_token.auth_method.as_deref(), Some("idc"));
        assert_eq!(sso_token.provider.as_deref(), Some("BuilderId"));
        assert_eq!(
            sso_token.client_id_hash.as_deref(),
            Some(KIRO_BUILDER_ID_CLIENT_ID_HASH)
        );
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
        let proxy_manager = Arc::new(ProxyManager::new(Vec::new(), None).unwrap());
        token_manager.set_proxy_manager(Some(proxy_manager.clone()));
        let client_api_key_runtime =
            Arc::new(RwLock::new(config.api_key.clone().unwrap_or_default()));
        let require_api_key_runtime = Arc::new(AtomicBool::new(config.require_api_key));
        let admin_api_key_runtime = Arc::new(RwLock::new(
            config.admin_api_key.clone().unwrap_or_default(),
        ));

        let service = AdminService::new(
            token_manager,
            proxy_manager,
            None,
            Arc::new(RwLock::new(config.compression.clone())),
            client_api_key_runtime.clone(),
            require_api_key_runtime.clone(),
            admin_api_key_runtime.clone(),
            Arc::new(RwLock::new(config.prompt_filter.clone())),
            crate::model::runtime::model_mapping_from_config(&config),
            Arc::new(RwLock::new(ThinkingRuntimeConfig {
                suffix: config.thinking_suffix.clone(),
                openai_format: config.openai_thinking_format.clone(),
                claude_format: config.claude_thinking_format.clone(),
            })),
            Arc::new(RwLock::new(PromptCacheRuntime::new(
                config.prompt_cache_ttl_seconds,
                config.prompt_cache_accounting_enabled,
                config.prompt_cache_max_ratio,
            ))),
            crate::model::runtime::shared_from_config(&config),
            Arc::new(RwLock::new(Vec::new())),
            vec![
                IDE_ENDPOINT_NAME.to_string(),
                CLI_ENDPOINT_NAME.to_string(),
                CODEWHISPERER_ENDPOINT_NAME.to_string(),
            ],
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

    fn add_test_proxy(service: &AdminService, region: Option<&str>) -> u64 {
        service
            .proxy_manager
            .add(ProxyEntry {
                url: "http://proxy.local:8080".to_string(),
                region: region.map(str::to_string),
                max_concurrency: Some(2),
                ..Default::default()
            })
            .unwrap()
    }

    fn jwt_with_iss_exp(iss: &str, exp: i64) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let payload =
            URL_SAFE_NO_PAD.encode(serde_json::json!({ "iss": iss, "exp": exp }).to_string());
        format!("header.{payload}.sig")
    }

    fn client_secret_with_start_url(start_url: &str) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let serialized = serde_json::json!({ "initiateLoginUri": start_url }).to_string();
        let payload = serde_json::json!({ "serialized": serialized }).to_string();
        let payload = URL_SAFE_NO_PAD.encode(payload);
        format!("header.{payload}.sig")
    }

    #[test]
    fn proxy_url_config_emits_canonical_and_compat_fields() {
        let dir = temp_test_dir("proxy-url-config-fields");
        let mut config = crate::model::config::Config::default();
        config.proxy_url = Some("http://127.0.0.1:8080".to_string());
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let value = serde_json::to_value(service.get_proxy_url_config()).unwrap();

        assert_eq!(value["proxyUrl"], "http://127.0.0.1:8080");
        assert_eq!(value["proxyURL"], "http://127.0.0.1:8080");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn common_config_generates_and_persists_local_machine_id() {
        let dir = temp_test_dir("common-machine-id");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let config = crate::model::config::Config::load(&config_path).unwrap();
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let common = service.get_common_config().unwrap();
        let saved = crate::model::config::Config::load(&config_path).unwrap();

        assert_eq!(
            saved.machine_id.as_deref(),
            Some(common.machine_id.as_str())
        );
        assert!(uuid::Uuid::parse_str(&common.machine_id).is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_machine_id_strategy_reuses_config_machine_id_for_new_imports() {
        let dir = temp_test_dir("local-machine-id-strategy");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let mut config = crate::model::config::Config::load(&config_path).unwrap();
        config.machine_id = Some("2582956E-CC88-4669-B546-07ADBFFCB894".to_string());
        config.credential_machine_id_strategy =
            crate::model::config::CredentialMachineIdStrategy::Local;
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let req: ImportCredentialRecordRequest = serde_json::from_value(serde_json::json!({
            "authMethod": "api-key",
            "apiKey": "ksk_local_machine_id",
            "disabled": true
        }))
        .unwrap();

        let added = service.import_credential_record(req).await.unwrap();
        let imported = service
            .token_manager
            .export_credentials_by_ids(&[added.credential_id])
            .pop()
            .unwrap();

        assert_eq!(
            imported.machine_id.as_deref(),
            Some("2582956e-cc88-4669-b546-07adbffcb894")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn random_machine_id_strategy_generates_distinct_machine_ids_for_new_imports() {
        let dir = temp_test_dir("random-machine-id-strategy");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let mut config = crate::model::config::Config::load(&config_path).unwrap();
        config.machine_id = Some("2582956e-cc88-4669-b546-07adbffcb894".to_string());
        config.credential_machine_id_strategy =
            crate::model::config::CredentialMachineIdStrategy::Random;
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let first: ImportCredentialRecordRequest = serde_json::from_value(serde_json::json!({
            "authMethod": "api-key",
            "apiKey": "ksk_random_machine_id_1",
            "disabled": true
        }))
        .unwrap();
        let second: ImportCredentialRecordRequest = serde_json::from_value(serde_json::json!({
            "authMethod": "api-key",
            "apiKey": "ksk_random_machine_id_2",
            "disabled": true
        }))
        .unwrap();

        let first = service.import_credential_record(first).await.unwrap();
        let second = service.import_credential_record(second).await.unwrap();
        let credentials = service
            .token_manager
            .export_credentials_by_ids(&[first.credential_id, second.credential_id]);
        let first_machine_id = credentials[0].machine_id.as_deref().unwrap();
        let second_machine_id = credentials[1].machine_id.as_deref().unwrap();

        assert!(uuid::Uuid::parse_str(first_machine_id).is_ok());
        assert!(uuid::Uuid::parse_str(second_machine_id).is_ok());
        assert_ne!(first_machine_id, second_machine_id);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn request_logs_response_includes_summary_counts() {
        let dir = temp_test_dir("request-logs-summary");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let stats = service.request_stats();
        stats.record_success("claude", "claude-sonnet-4.5", "7", 1, 2, 0.1, 10);
        stats.record_failure("openai", "gpt", "8", "HTTP 429", "quota", 3);

        let response = service.get_request_logs();
        let value = serde_json::to_value(&response).unwrap();

        assert_eq!(value["total"], 2);
        assert_eq!(value["success"], 1);
        assert_eq!(value["errors"], 1);
        assert_eq!(value["logs"][0]["credentialId"], "8");
        assert_eq!(value["logs"][0]["accountId"], "8");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn social_manual_login_template_preserves_selected_provider_metadata() {
        let dir = temp_test_dir("social-provider-template");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let config = crate::model::config::Config::load(&config_path).unwrap();
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let response = service
            .start_social_login(StartSocialLoginRequest {
                priority: 0,
                email: None,
                proxy_url: None,
                auth_endpoint: None,
                provider: "GitHub".to_string(),
                mode: Some("manual".to_string()),
            })
            .await
            .unwrap();

        let sessions = service.social_sessions.lock();
        let session = sessions.get(&response.session_id).unwrap();
        assert_eq!(response.mode, "manual");
        assert!(
            response
                .portal_url
                .as_deref()
                .unwrap()
                .contains("idp=Github")
        );
        assert_eq!(session.cred_template.auth_method.as_deref(), Some("social"));
        assert_eq!(session.cred_template.provider.as_deref(), Some("GitHub"));

        drop(sessions);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_credential_record_missing_refresh_token_reaches_service_validation() {
        let dir = temp_test_dir("credential-record-missing-refresh");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let req: ImportCredentialRecordRequest = serde_json::from_value(serde_json::json!({}))
            .expect("zero-value credential-record import should parse");

        let err = service.import_credential_record(req).await.unwrap_err();
        match err {
            AdminServiceError::InvalidRequest(message) => {
                assert_eq!(message, "refreshToken is required");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_credential_record_accepts_api_key_without_refresh_token() {
        let dir = temp_test_dir("credential-record-api-key-import");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let req: ImportCredentialRecordRequest = serde_json::from_value(serde_json::json!({
            "authMethod": "api-key",
            "apiKey": "ksk_record_import_key",
            "endpoint": "ide",
            "disabled": true,
            "concurrency": 2,
            "machineId": "machine-api-record",
            "email": "api@example.com"
        }))
        .unwrap();

        let added = service.import_credential_record(req).await.unwrap();

        assert_eq!(added.auth_method.as_deref(), Some("api_key"));
        assert!(added.has_api_key);
        assert!(!added.has_refresh_token);
        assert_eq!(added.endpoint.as_deref(), Some("ide"));

        let imported = service
            .token_manager
            .export_credentials_by_ids(&[added.credential_id])
            .pop()
            .unwrap();
        assert_eq!(imported.auth_method.as_deref(), Some("api_key"));
        assert_eq!(imported.api_key.as_deref(), Some("ksk_record_import_key"));
        assert!(imported.refresh_token.is_none());
        assert!(imported.disabled);
        assert_eq!(imported.concurrency, Some(2));
        assert_eq!(imported.machine_id.as_deref(), Some("machine-api-record"));

        let status = service.get_all_credentials();
        let item = status
            .credentials
            .iter()
            .find(|item| item.id == added.credential_id)
            .unwrap();
        assert_eq!(item.auth_method.as_deref(), Some("api_key"));
        assert!(item.has_api_key);
        assert!(!item.has_refresh_token);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_credential_record_persists_weight_without_refresh_network() {
        let dir = temp_test_dir("credential-record-weight-import");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let access_token = jwt_with_exp((Utc::now() + chrono::Duration::hours(1)).timestamp());

        let req: ImportCredentialRecordRequest = serde_json::from_value(serde_json::json!({
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

        let added = service.import_credential_record(req).await.unwrap();
        let snapshot = service.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == added.credential_id)
            .unwrap();

        assert_eq!(entry.priority, 2);
        assert_eq!(entry.weight, 7);
        assert_eq!(entry.concurrency, Some(3));
        assert_eq!(entry.provider.as_deref(), Some("AzureAD"));
        assert_eq!(
            entry.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token")
        );
        assert_eq!(
            entry.issuer_url.as_deref(),
            Some("https://login.microsoftonline.com/tenant/v2.0")
        );
        assert!(entry.disabled);

        let status = service.get_all_credentials();
        assert_eq!(status.credentials[0].provider.as_deref(), Some("AzureAD"));
        assert!(status.credentials[0].has_token);
        assert!(status.credentials[0].has_refresh_token);
        assert!(status.credentials[0].has_client_id);
        assert!(!status.credentials[0].has_client_secret);
        assert_eq!(
            status.credentials[0].token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token")
        );
        assert_eq!(
            status.credentials[0].issuer_url.as_deref(),
            Some("https://login.microsoftonline.com/tenant/v2.0")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_credential_record_assigns_proxy_before_prevalidation() {
        let dir = temp_test_dir("credential-record-proxy-prevalidation");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let proxy_id = add_test_proxy(&service, Some("us-east-1"));
        let access_token = jwt_with_exp((Utc::now() + chrono::Duration::hours(1)).timestamp());

        let req: ImportCredentialRecordRequest = serde_json::from_value(serde_json::json!({
            "refreshToken": "r".repeat(150),
            "accessToken": access_token,
            "authMethod": "external_idp",
            "clientId": "client-1",
            "tokenEndpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
            "issuerUrl": "https://login.microsoftonline.com/tenant/v2.0",
            "region": "us-east-1"
        }))
        .unwrap();

        let added = service.import_credential_record(req).await.unwrap();

        let snapshot = service.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == added.credential_id)
            .unwrap();
        assert_eq!(entry.proxy_id, Some(proxy_id));
        assert!(entry.proxy_url.is_none());
    }

    #[test]
    fn new_oauth_credential_assigns_proxy_before_validation() {
        let dir = temp_test_dir("oauth-proxy-prevalidation");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let proxy_id = add_test_proxy(&service, Some("us-east-1"));
        let mut credential = KiroCredentials {
            region: Some("us-east-1".to_string()),
            auth_method: Some("social".to_string()),
            provider: Some("GitHub".to_string()),
            refresh_token: Some("r".repeat(150)),
            ..Default::default()
        };

        service
            .assign_proxy_before_validation(&mut credential)
            .unwrap();

        assert_eq!(credential.proxy_id, Some(proxy_id));
    }

    #[test]
    fn assigned_pool_proxy_is_used_for_validation_before_global_proxy() {
        let dir = temp_test_dir("validation-proxy-resolution");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let proxy_id = add_test_proxy(&service, Some("us-east-1"));
        let global_proxy = ProxyConfig::new("http://global-proxy.local:8080");
        let mut credential = KiroCredentials {
            region: Some("us-east-1".to_string()),
            auth_method: Some("social".to_string()),
            provider: Some("GitHub".to_string()),
            refresh_token: Some("r".repeat(150)),
            ..Default::default()
        };

        service
            .assign_proxy_before_validation(&mut credential)
            .unwrap();
        let validation_proxy = service
            .proxy_for_validation(&credential, Some(&global_proxy))
            .unwrap()
            .unwrap();

        assert_eq!(credential.proxy_id, Some(proxy_id));
        assert_eq!(validation_proxy.url, "http://proxy.local:8080");
    }

    #[test]
    fn proxy_preassignment_preserves_explicit_direct_proxy() {
        let dir = temp_test_dir("proxy-prevalidation-direct");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        add_test_proxy(&service, Some("us-east-1"));
        let mut credential = KiroCredentials {
            region: Some("us-east-1".to_string()),
            proxy_url: Some(KiroCredentials::PROXY_DIRECT.to_string()),
            ..Default::default()
        };

        service
            .assign_proxy_before_validation(&mut credential)
            .unwrap();

        assert!(credential.proxy_id.is_none());
        assert_eq!(
            credential.proxy_url.as_deref(),
            Some(KiroCredentials::PROXY_DIRECT)
        );
    }

    #[test]
    fn update_credential_alias_weight_does_not_change_priority() {
        let dir = temp_test_dir("credential-alias-weight-update");
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
            .update_credential_alias_fields(
                id,
                CredentialAliasUpdateRequest {
                    enabled: None,
                    nickname: None,
                    machine_id: None,
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
    fn update_credential_alias_accepts_source_id_and_profile_fields() {
        let dir = temp_test_dir("credential-alias-source-update");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.meta.source_account_id = Some("source-credential".to_string());
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        let id = service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();

        service
            .update_credential_alias_by_path_id(
                "source-credential",
                CredentialAliasUpdateRequest {
                    enabled: Some(false),
                    nickname: Some("Work Credential".to_string()),
                    machine_id: Some("machine-updated".to_string()),
                    weight: Some(8),
                    proxy_url: Some("http://proxy.local:8080".to_string()),
                },
            )
            .unwrap();

        let snapshot = service.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .unwrap();
        assert!(entry.disabled);
        assert_eq!(entry.nickname.as_deref(), Some("Work Credential"));
        assert_eq!(entry.machine_id.as_deref(), Some("machine-updated"));
        assert_eq!(entry.weight, 8);
        assert_eq!(entry.proxy_url.as_deref(), Some("http://proxy.local:8080"));
    }

    #[test]
    fn delete_credential_by_path_id_accepts_source_id() {
        let dir = temp_test_dir("credential-source-delete");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.meta.source_account_id = Some("source-credential".to_string());
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();

        service
            .delete_credential_by_path_id("source-credential")
            .unwrap();

        assert!(service.token_manager.snapshot().entries.is_empty());
    }

    #[test]
    fn single_credential_id_resolution_preserves_snapshot_order_on_source_id_conflict() {
        let dir = temp_test_dir("single-credential-id-conflict");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut first = KiroCredentials::default();
        first.refresh_token = Some("r".repeat(150));
        first.access_token = Some("access-1".to_string());
        let first_id = service
            .token_manager
            .add_prevalidated_credential(first)
            .unwrap();

        let mut second = KiroCredentials::default();
        second.meta.source_account_id = Some(first_id.to_string());
        second.refresh_token = Some("s".repeat(150));
        second.access_token = Some("access-2".to_string());
        let second_id = service
            .token_manager
            .add_prevalidated_credential(second)
            .unwrap();

        assert_eq!(
            service
                .resolve_credential_id(&first_id.to_string())
                .unwrap(),
            first_id
        );
        assert_ne!(
            service
                .resolve_credential_id(&first_id.to_string())
                .unwrap(),
            second_id
        );
    }

    #[test]
    fn list_credential_alias_views_returns_source_metadata_items() {
        let dir = temp_test_dir("credential-alias-view-list");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.meta.source_account_id = Some("source-credential".to_string());
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        cred.expires_at = Some("2026-06-29T00:00:00Z".to_string());
        cred.auth_method = Some("idc".to_string());
        cred.provider = Some("BuilderId".to_string());
        cred.user_id = Some("user-1".to_string());
        cred.region = Some("us-east-1".to_string());
        cred.machine_id = Some("machine-1".to_string());
        cred.email = Some("user@example.com".to_string());
        cred.meta.nickname = Some("User".to_string());
        cred.weight = 4;
        cred.meta.overage_status = Some("ENABLED".to_string());
        cred.meta.overage_capability = Some("OVERAGE_CAPABLE".to_string());
        cred.meta.overage_cap = Some(50.0);
        cred.meta.overage_rate = Some(0.04);
        cred.meta.current_overages = Some(1.5);
        cred.meta.overage_checked_at = Some(1_782_691_200);
        cred.proxy_url = Some("http://proxy.local:8080".to_string());
        cred.meta.subscription_type = Some("PRO_PLUS".to_string());
        cred.meta.subscription_title = Some("KIRO PRO+".to_string());
        cred.meta.days_remaining = Some(20);
        cred.meta.usage_current = Some(3.0);
        cred.meta.usage_limit = Some(10.0);
        cred.meta.usage_percent = Some(30.0);
        cred.meta.next_reset_date = Some("2026-07-01".to_string());
        cred.meta.last_refresh = Some(1_782_691_201);
        cred.meta.trial_usage_current = Some(1.0);
        cred.meta.trial_usage_limit = Some(5.0);
        cred.meta.trial_usage_percent = Some(20.0);
        cred.meta.trial_status = Some("ACTIVE".to_string());
        cred.meta.trial_expires_at = Some(1_785_283_200);
        cred.meta.request_count = Some(7);
        cred.meta.error_count = Some(2);
        cred.meta.total_tokens = Some(1234);
        cred.meta.total_credits = Some(12.5);
        cred.meta.last_used_at = Some(1_782_691_202);
        service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();

        let alias_views = service.list_credential_alias_views();

        assert_eq!(alias_views.len(), 1);
        let alias_view = &alias_views[0];
        assert_eq!(alias_view.id, "source-credential");
        assert_eq!(alias_view.email, "user@example.com");
        assert_eq!(alias_view.user_id, "user-1");
        assert_eq!(alias_view.nickname, "User");
        assert_eq!(alias_view.auth_method, "idc");
        assert_eq!(alias_view.provider, "BuilderId");
        assert_eq!(alias_view.region, "us-east-1");
        assert!(alias_view.enabled);
        assert_eq!(alias_view.expires_at, 1_782_691_200);
        assert!(alias_view.has_token);
        assert_eq!(alias_view.machine_id, "machine-1");
        assert_eq!(alias_view.weight, 4);
        assert_eq!(alias_view.overage_status, "ENABLED");
        assert_eq!(alias_view.overage_capability, "OVERAGE_CAPABLE");
        assert_eq!(alias_view.overage_cap, 50.0);
        assert_eq!(alias_view.overage_rate, 0.04);
        assert_eq!(alias_view.current_overages, 1.5);
        assert_eq!(alias_view.overage_checked_at, 1_782_691_200);
        assert_eq!(alias_view.proxy_url, "http://proxy.local:8080");
        assert_eq!(alias_view.subscription_type, "PRO_PLUS");
        assert_eq!(alias_view.subscription_title, "KIRO PRO+");
        assert_eq!(alias_view.days_remaining, 20);
        assert_eq!(alias_view.usage_current, 3.0);
        assert_eq!(alias_view.usage_limit, 10.0);
        assert_eq!(alias_view.usage_percent, 30.0);
        assert_eq!(alias_view.next_reset_date, "2026-07-01");
        assert_eq!(alias_view.last_refresh, 1_782_691_201);
        assert_eq!(alias_view.trial_usage_current, 1.0);
        assert_eq!(alias_view.trial_usage_limit, 5.0);
        assert_eq!(alias_view.trial_usage_percent, 20.0);
        assert_eq!(alias_view.trial_status, "ACTIVE");
        assert_eq!(alias_view.trial_expires_at, 1_785_283_200);
        assert_eq!(alias_view.request_count, 7);
        assert_eq!(alias_view.error_count, 2);
        assert_eq!(alias_view.total_tokens, 1234);
        assert_eq!(alias_view.total_credits, 12.5);
        assert_eq!(alias_view.last_used, 1_782_691_202);

        let serialized = serde_json::to_value(alias_view).unwrap();
        assert_eq!(serialized["proxyUrl"], "http://proxy.local:8080");
        assert_eq!(serialized["proxyURL"], "http://proxy.local:8080");
    }

    #[test]
    fn credential_refresh_info_uses_source_refresh_shape() {
        let dir = temp_test_dir("credential-refresh-info");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        cred.email = Some("user@example.com".to_string());
        cred.user_id = Some("user-1".to_string());
        cred.meta.subscription_type = Some("PRO_PLUS".to_string());
        cred.meta.subscription_title = Some("KIRO PRO+".to_string());
        cred.meta.days_remaining = Some(12);
        cred.meta.usage_current = Some(4.0);
        cred.meta.usage_limit = Some(20.0);
        cred.meta.usage_percent = Some(20.0);
        cred.meta.next_reset_date = Some("2026-07-01".to_string());
        cred.meta.last_refresh = Some(1_782_691_201);
        cred.meta.trial_usage_current = Some(1.0);
        cred.meta.trial_usage_limit = Some(5.0);
        cred.meta.trial_usage_percent = Some(20.0);
        cred.meta.trial_status = Some("ACTIVE".to_string());
        cred.meta.trial_expires_at = Some(1_785_283_200);
        let id = service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();
        let snapshot = service.token_manager.snapshot();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .unwrap();

        let info = AdminService::credential_refresh_info_from_entry(entry);
        let value = serde_json::to_value(&info).unwrap();

        assert_eq!(value["Email"], "user@example.com");
        assert_eq!(value["UserId"], "user-1");
        assert_eq!(value["SubscriptionType"], "PRO_PLUS");
        assert_eq!(value["SubscriptionTitle"], "KIRO PRO+");
        assert_eq!(value["DaysRemaining"], 12);
        assert_eq!(value["UsageCurrent"], 4.0);
        assert_eq!(value["UsageLimit"], 20.0);
        assert_eq!(value["UsagePercent"], 20.0);
        assert_eq!(value["NextResetDate"], "2026-07-01");
        assert_eq!(value["LastRefresh"], 1_782_691_201);
        assert_eq!(value["TrialUsageCurrent"], 1.0);
        assert_eq!(value["TrialUsageLimit"], 5.0);
        assert_eq!(value["TrialUsagePercent"], 20.0);
        assert_eq!(value["TrialStatus"], "ACTIVE");
        assert_eq!(value["TrialExpiresAt"], 1_785_283_200);
    }

    #[test]
    fn get_credential_full_export_accepts_source_id_and_returns_string_id() {
        let dir = temp_test_dir("credential-full-source-id");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.meta.source_account_id = Some("source-credential".to_string());
        cred.access_token = Some("access-token".to_string());
        cred.refresh_token = Some("r".repeat(150));
        cred.client_id = Some("client-id".to_string());
        cred.client_secret = Some("client-secret".to_string());
        cred.auth_method = Some("idc".to_string());
        cred.provider = Some("BuilderId".to_string());
        cred.email = Some("user@example.com".to_string());
        cred.user_id = Some("user-1".to_string());
        cred.meta.nickname = Some("Work".to_string());
        cred.region = Some("us-east-1".to_string());
        cred.expires_at = Some("2026-06-29T00:00:00Z".to_string());
        cred.machine_id = Some("machine-1".to_string());
        cred.profile_arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/test".to_string());
        cred.proxy_url = Some("http://proxy.local:8080".to_string());
        cred.weight = 5;
        cred.meta.request_count = Some(7);
        cred.meta.error_count = Some(2);
        cred.meta.total_tokens = Some(1234);
        cred.meta.total_credits = Some(12.5);
        cred.meta.last_used_at = Some(1_782_691_202);
        service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();

        let credential_export = service
            .get_credential_full_export_by_path_id("source-credential")
            .unwrap();
        let value = serde_json::to_value(&credential_export).unwrap();

        assert_eq!(credential_export.id, "source-credential");
        assert_eq!(credential_export.email.as_deref(), Some("user@example.com"));
        assert_eq!(credential_export.user_id.as_deref(), Some("user-1"));
        assert_eq!(credential_export.nickname, "Work");
        assert_eq!(
            credential_export.access_token.as_deref(),
            Some("access-token")
        );
        assert_eq!(credential_export.refresh_token, "r".repeat(150));
        assert_eq!(credential_export.client_id.as_deref(), Some("client-id"));
        assert_eq!(
            credential_export.client_secret.as_deref(),
            Some("client-secret")
        );
        assert_eq!(credential_export.auth_method, "idc");
        assert_eq!(credential_export.provider.as_deref(), Some("BuilderId"));
        assert_eq!(credential_export.region.as_deref(), Some("us-east-1"));
        assert_eq!(credential_export.expires_at, Some(1_782_691_200));
        assert_eq!(credential_export.machine_id.as_deref(), Some("machine-1"));
        assert_eq!(
            credential_export.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:123:profile/test")
        );
        assert_eq!(
            credential_export.proxy_url.as_deref(),
            Some("http://proxy.local:8080")
        );
        assert_eq!(credential_export.weight, 5);
        assert_eq!(credential_export.request_count, 7);
        assert_eq!(credential_export.error_count, 2);
        assert_eq!(credential_export.total_tokens, 1234);
        assert_eq!(credential_export.total_credits, 12.5);
        assert_eq!(credential_export.last_used, 1_782_691_202);
        assert_eq!(value["id"], "source-credential");
        assert!(value["tokenEndpoint"].is_null());
        assert_eq!(value["proxyUrl"], "http://proxy.local:8080");
        assert_eq!(value["proxyURL"], "http://proxy.local:8080");
    }

    #[test]
    fn cached_credential_models_accepts_source_id_and_returns_model_ids() {
        let dir = temp_test_dir("credential-cached-models");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.meta.source_account_id = Some("source-credential".to_string());
        cred.access_token = Some("access-token".to_string());
        cred.refresh_token = Some("r".repeat(150));
        let id = service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();
        service.token_manager.set_model_list(
            id,
            [
                " Claude-Sonnet-4.5 ".to_string(),
                "claude-haiku-4.5".to_string(),
            ],
        );

        let response = service.get_cached_credential_models_by_path_id("source-credential");
        assert!(response.success);
        assert_eq!(
            response.models,
            vec![
                "claude-haiku-4-5".to_string(),
                "claude-sonnet-4-5".to_string()
            ]
        );

        let missing = service.get_cached_credential_models_by_path_id("missing");
        assert!(missing.success);
        assert!(missing.models.is_empty());
    }

    #[test]
    fn credential_overage_uses_source_id_snapshot_shape() {
        let dir = temp_test_dir("credential-overage-source-id");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.meta.source_account_id = Some("source-credential".to_string());
        cred.access_token = Some("access-token".to_string());
        cred.refresh_token = Some("r".repeat(150));
        cred.meta.subscription_title = Some("KIRO PRO+".to_string());
        cred.meta.overage_status = Some("ENABLED".to_string());
        cred.meta.overage_capability = Some("OVERAGE_CAPABLE".to_string());
        cred.meta.overage_cap = Some(50.0);
        cred.meta.overage_rate = Some(0.04);
        cred.meta.current_overages = Some(2.5);
        cred.meta.overage_checked_at = Some(1_782_691_200);
        let id = service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();

        assert_eq!(
            service.resolve_credential_id("source-credential").unwrap(),
            id
        );

        let balance = BalanceResponse {
            id,
            subscription_title: Some("KIRO PRO+".to_string()),
            subscription_type: Some("PRO_PLUS".to_string()),
            current_usage: 12.5,
            usage_limit: 10.0,
            remaining: 0.0,
            usage_percentage: 125.0,
            next_reset_at: None,
            overage_cap: 50.0,
            overage_capability: Some("OVERAGE_CAPABLE".to_string()),
            overage_status: Some("ENABLED".to_string()),
        };
        let response = service.credential_overage_response(id, Some(&balance), None);
        let value = serde_json::to_value(&response).unwrap();

        assert!(response.success);
        assert_eq!(value["overageStatus"], "ENABLED");
        assert_eq!(value["overageCapability"], "OVERAGE_CAPABLE");
        assert_eq!(value["subscriptionTitle"], "KIRO PRO+");
        assert_eq!(value["overageCap"], 50.0);
        assert_eq!(value["overageRate"], 0.04);
        assert_eq!(value["currentOverages"], 2.5);
        assert_eq!(value["overageCheckedAt"], 1_782_691_200);
    }

    #[test]
    fn credential_test_request_body_uses_minimal_chat() {
        let dir = temp_test_dir("credential-test-body");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let body = service
            .credential_test_request_body("claude-sonnet-4")
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(
            value["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "say ok"
        );
        assert_eq!(value["inferenceConfig"]["maxTokens"], 5);
    }

    #[test]
    fn credential_probe_reply_aggregation_normalizes_cumulative_text() {
        let mut output = String::new();
        let mut previous = String::new();

        AdminService::append_cumulative_delta(&mut output, &mut previous, "o");
        AdminService::append_cumulative_delta(&mut output, &mut previous, "ok");
        AdminService::append_cumulative_delta(&mut output, &mut previous, "ok");
        AdminService::append_cumulative_delta(&mut output, &mut previous, "okay");

        assert_eq!(output, "okay");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_credentials_accepts_source_ids_and_returns_account_wire_shape() {
        let dir = temp_test_dir("credential-batch");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.meta.source_account_id = Some("source-credential".to_string());
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        let local_id = service
            .token_manager
            .add_prevalidated_credential(cred)
            .unwrap();

        let response = service
            .batch_credentials(CredentialBatchRequest {
                ids: vec![
                    serde_json::json!("source-credential"),
                    serde_json::json!("missing"),
                ],
                action: "disable".to_string(),
            })
            .await
            .unwrap();

        assert!(response.success);
        assert_eq!(response.count, Some(2));
        assert_eq!(response.refreshed, None);
        assert_eq!(response.failed, None);
        let snapshot = service.token_manager.snapshot();
        assert!(
            snapshot
                .entries
                .iter()
                .find(|entry| entry.id == local_id)
                .unwrap()
                .disabled
        );

        let refresh_response = service
            .batch_credentials(CredentialBatchRequest {
                ids: vec![serde_json::json!("missing")],
                action: "refresh".to_string(),
            })
            .await
            .unwrap();
        assert!(refresh_response.success);
        assert_eq!(refresh_response.count, None);
        assert_eq!(refresh_response.refreshed, Some(0));
        assert_eq!(refresh_response.failed, Some(1));
    }

    #[test]
    fn batch_credential_id_lookup_preserves_snapshot_order_on_source_id_conflict() {
        let dir = temp_test_dir("credential-id-lookup-conflict");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut first = KiroCredentials::default();
        first.refresh_token = Some("r".repeat(150));
        first.access_token = Some("access-1".to_string());
        let first_id = service
            .token_manager
            .add_prevalidated_credential(first)
            .unwrap();

        let mut second = KiroCredentials::default();
        second.meta.source_account_id = Some(first_id.to_string());
        second.refresh_token = Some("s".repeat(150));
        second.access_token = Some("access-2".to_string());
        let second_id = service
            .token_manager
            .add_prevalidated_credential(second)
            .unwrap();

        let resolved = service.resolve_credential_ids(&[serde_json::json!(first_id.to_string())]);
        assert_eq!(resolved, vec![Some(first_id)]);
        assert_ne!(resolved, vec![Some(second_id)]);
    }

    #[test]
    fn export_credential_snapshot_matches_view_and_filters_by_source_id() {
        let dir = temp_test_dir("credential-export");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut first = KiroCredentials::default();
        first.meta.source_account_id = Some("source-1".to_string());
        first.refresh_token = Some("r".repeat(150));
        first.access_token = Some("access-1".to_string());
        first.meta.csrf_token = Some("csrf-1".to_string());
        first.expires_at = Some("2026-06-29T00:00:00Z".to_string());
        first.auth_method = Some("idc".to_string());
        first.provider = Some("BuilderId".to_string());
        first.client_id = Some("client-1".to_string());
        first.client_secret = Some("secret-1".to_string());
        first.region = Some("us-east-1".to_string());
        first.email = Some("first@example.com".to_string());
        first.meta.nickname = Some("First".to_string());
        first.user_id = Some("user-1".to_string());
        first.machine_id = Some("machine-1".to_string());
        first.meta.subscription_type = Some("POWER".to_string());
        first.meta.subscription_title = Some("KIRO POWER".to_string());
        first.meta.usage_current = Some(3.0);
        first.meta.usage_limit = Some(10.0);
        first.meta.usage_percent = Some(30.0);
        first.meta.last_refresh = Some(1_782_691_200_000);
        first.meta.created_at = Some(1_782_691_100_000);
        first.meta.last_used_at = Some(1_782_691_150_000);
        first.meta.tags = Some(serde_json::json!(["alpha", 7, "beta"]));
        let first_id = service
            .token_manager
            .add_prevalidated_credential(first)
            .unwrap();

        let mut second = KiroCredentials::default();
        second.meta.source_account_id = Some("source-2".to_string());
        second.refresh_token = Some("s".repeat(150));
        second.access_token = Some("access-2".to_string());
        service
            .token_manager
            .add_prevalidated_credential(second)
            .unwrap();

        let all = service.export_credential_snapshot(&[]);
        assert_eq!(all.credentials.len(), 2);
        let all_value = serde_json::to_value(&all).unwrap();
        assert_eq!(all_value["credentials"], all_value["accounts"]);
        assert!(all.exported_at > 0);
        assert!(all.groups.is_empty());
        assert!(all.tags.is_empty());

        let by_source = service.export_credential_snapshot(&[serde_json::json!("source-1")]);
        assert_eq!(by_source.credentials.len(), 1);
        let by_source_value = serde_json::to_value(&by_source).unwrap();
        assert_eq!(
            by_source_value["accounts"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(by_source_value["credentials"][0]["provider"], "BuilderId");
        assert_eq!(by_source_value["credentials"][0]["idp"], "BuilderId");
        let snapshot_item = &by_source.credentials[0];
        assert_eq!(snapshot_item.id, "source-1");
        assert_eq!(snapshot_item.email, "first@example.com");
        assert_eq!(snapshot_item.nickname, "First");
        assert_eq!(snapshot_item.provider, "BuilderId");
        assert_eq!(snapshot_item.user_id.as_deref(), Some("user-1"));
        assert_eq!(snapshot_item.machine_id.as_deref(), Some("machine-1"));
        assert_eq!(snapshot_item.credentials.access_token, "access-1");
        assert_eq!(snapshot_item.credentials.csrf_token, "csrf-1");
        assert_eq!(snapshot_item.credentials.refresh_token, "r".repeat(150));
        assert_eq!(
            snapshot_item.credentials.client_id.as_deref(),
            Some("client-1")
        );
        assert_eq!(
            snapshot_item.credentials.client_secret.as_deref(),
            Some("secret-1")
        );
        assert_eq!(
            snapshot_item.credentials.region.as_deref(),
            Some("us-east-1")
        );
        assert_eq!(snapshot_item.credentials.expires_at, 1_782_691_200_000);
        assert_eq!(
            snapshot_item.credentials.auth_method.as_deref(),
            Some("idc")
        );
        assert_eq!(
            snapshot_item.credentials.provider.as_deref(),
            Some("BuilderId")
        );
        assert_eq!(snapshot_item.subscription.subscription_type, "Pro_Plus");
        assert_eq!(
            snapshot_item.subscription.title.as_deref(),
            Some("KIRO POWER")
        );
        assert_eq!(snapshot_item.usage.current, 3.0);
        assert_eq!(snapshot_item.usage.limit, 10.0);
        assert_eq!(snapshot_item.usage.percent_used, 30.0);
        assert_eq!(snapshot_item.usage.last_updated, 1_782_691_200_000);
        assert_eq!(
            snapshot_item.tags,
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert_eq!(snapshot_item.status, "active");
        assert_eq!(snapshot_item.created_at, 1_782_691_100_000);
        assert_eq!(snapshot_item.last_used_at, 1_782_691_150_000);

        let by_local_id = service.export_credential_snapshot(&[serde_json::json!(first_id)]);
        assert_eq!(by_local_id.credentials.len(), 1);
        assert_eq!(by_local_id.credentials[0].id, "source-1");
        let by_local_id_value = serde_json::to_value(&by_local_id).unwrap();
        assert_eq!(by_local_id_value["accounts"][0]["id"], "source-1");
    }

    #[test]
    fn import_credentials_auto_detects_cached_credential_items_wrapper() {
        let dir = temp_test_dir("cached-credential-wrapper-import");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let import_payload = serde_json::json!({
            "items": [{
                "provider": "BuilderId",
                "refreshToken": "b".repeat(150),
                "clientId": "client-cached-credential",
                "clientSecret": "secret-cached-credential",
                "priority": 4,
                "weight": 2,
                "region": "us-east-1",
                "apiRegion": "us-west-2",
                "machineId": "machine-cached-credential"
            }]
        });

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: import_payload,
        });

        assert_eq!(response.summary.added, 1);
        assert_eq!(response.summary.invalid, 0);
        assert_eq!(
            response.items[0].source_format,
            SOURCE_FORMAT_CACHED_CREDENTIAL
        );
        assert_eq!(response.items[0].auth_method.as_deref(), Some("idc"));

        let snapshot = service.token_manager.snapshot();
        let exported = service
            .token_manager
            .export_credentials_by_ids(&[snapshot.entries[0].id]);
        let imported = exported.into_iter().next().unwrap();
        assert_eq!(imported.auth_method.as_deref(), Some("idc"));
        assert_eq!(imported.provider.as_deref(), Some("BuilderId"));
        assert_eq!(
            imported.client_id.as_deref(),
            Some("client-cached-credential")
        );
        assert_eq!(
            imported.client_secret.as_deref(),
            Some("secret-cached-credential")
        );
        assert_eq!(imported.priority, 4);
        assert_eq!(imported.weight, 2);
        assert_eq!(imported.region.as_deref(), Some("us-east-1"));
        assert_eq!(imported.api_region.as_deref(), Some("us-west-2"));
        assert_eq!(
            imported.machine_id.as_deref(),
            Some("machine-cached-credential")
        );
    }

    #[test]
    fn import_credentials_cached_credential_preserves_external_idp_material() {
        let dir = temp_test_dir("cached-credential-external-idp");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: serde_json::json!({
                "items": [{
                    "provider": "Microsoft",
                    "accessToken": "access-token-import",
                    "refreshToken": "e".repeat(150),
                    "profileArn": "arn:aws:codewhisperer:profile/imported",
                    "expiresAt": "2026-06-29T00:00:00Z",
                    "clientId": "client-import",
                    "authMethod": "microsoft",
                    "userId": "https://login.microsoftonline.com/tenant/v2.0",
                    "tokenEndpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
                    "issuerUrl": "https://login.microsoftonline.com/tenant/v2.0",
                    "scopes": "api://client-import/codewhisperer:conversations offline_access",
                    "clientIdHash": "client-hash-import",
                    "idToken": "id-token-import",
                    "ssoSessionId": "sso-session-import",
                    "priority": 8,
                    "weight": 4,
                    "concurrency": 2,
                    "region": "us-east-1",
                    "authRegion": "us-east-1",
                    "apiRegion": "eu-central-1",
                    "machineId": "machine-import",
                    "email": "import@example.com",
                    "proxyURL": "direct",
                    "overageStatus": "DISABLED",
                    "endpoint": "ide"
                }]
            }),
        });

        assert_eq!(response.summary.added, 1);
        assert_eq!(response.summary.invalid, 0);

        let snapshot = service.token_manager.snapshot();
        let exported = service
            .token_manager
            .export_credentials_by_ids(&[snapshot.entries[0].id]);
        let imported = exported.into_iter().next().unwrap();
        assert_eq!(imported.auth_method.as_deref(), Some("external_idp"));
        assert_eq!(imported.provider.as_deref(), Some("AzureAD"));
        assert_eq!(
            imported.access_token.as_deref(),
            Some("access-token-import")
        );
        assert_eq!(imported.client_id.as_deref(), Some("client-import"));
        assert_eq!(
            imported.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token")
        );
        assert_eq!(
            imported.issuer_url.as_deref(),
            Some("https://login.microsoftonline.com/tenant/v2.0")
        );
        assert_eq!(
            imported.scopes.as_deref(),
            Some("api://client-import/codewhisperer:conversations offline_access")
        );
        assert_eq!(
            imported.client_id_hash.as_deref(),
            Some("client-hash-import")
        );
        assert_eq!(imported.id_token.as_deref(), Some("id-token-import"));
        assert_eq!(
            imported.sso_session_id.as_deref(),
            Some("sso-session-import")
        );
        assert_eq!(imported.priority, 8);
        assert_eq!(imported.weight, 4);
        assert_eq!(imported.concurrency, Some(2));
        assert_eq!(imported.auth_region.as_deref(), Some("us-east-1"));
        assert_eq!(imported.api_region.as_deref(), Some("eu-central-1"));
        assert_eq!(imported.machine_id.as_deref(), Some("machine-import"));
        assert_eq!(imported.proxy_url.as_deref(), Some("direct"));
        assert_eq!(imported.meta.overage_status.as_deref(), Some("DISABLED"));
        assert_eq!(imported.endpoint.as_deref(), Some("ide"));
    }

    #[test]
    fn import_credentials_external_idp_drops_uuid_profile_arn() {
        let dir = temp_test_dir("external-idp-uuid-profile-arn");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: serde_json::json!({
                "items": [{
                    "provider": "Microsoft",
                    "refreshToken": "e".repeat(150),
                    "profileArn": "e3438419-4424-4e57-8990-ef76bd749a44",
                    "clientId": "client-import",
                    "authMethod": "microsoft",
                    "userId": "https://login.microsoftonline.com/tenant/v2.0",
                    "tokenEndpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
                    "issuerUrl": "https://login.microsoftonline.com/tenant/v2.0",
                    "scopes": "api://client-import/codewhisperer:conversations offline_access",
                    "region": "us-east-1"
                }]
            }),
        });

        assert_eq!(response.summary.added, 1);
        assert_eq!(response.summary.invalid, 0);
        assert!(!response.items[0].has_profile_arn);

        let snapshot = service.token_manager.snapshot();
        let imported = service
            .token_manager
            .export_credentials_by_ids(&[snapshot.entries[0].id])
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(imported.auth_method.as_deref(), Some("external_idp"));
        assert_eq!(imported.profile_arn, None);
    }

    #[test]
    fn import_credentials_accepts_single_cached_credential_item() {
        let dir = temp_test_dir("cached-credential-wrapper");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: serde_json::json!({
                "items": {
                    "provider": "Social",
                    "refreshToken": "s".repeat(150),
                    "authMethod": "social",
                    "priority": 3,
                    "weight": 6,
                    "region": "eu-west-1",
                    "apiRegion": "eu-central-1",
                    "machineId": "machine-cached-credential"
                }
            }),
        });

        assert_eq!(response.summary.added, 1);
        assert_eq!(response.summary.invalid, 0);

        let snapshot = service.token_manager.snapshot();
        let exported = service
            .token_manager
            .export_credentials_by_ids(&[snapshot.entries[0].id]);
        let imported = exported.into_iter().next().unwrap();
        assert_eq!(imported.auth_method.as_deref(), Some("social"));
        assert_eq!(imported.weight, 6);
        assert_eq!(imported.region.as_deref(), Some("eu-west-1"));
        assert_eq!(imported.api_region.as_deref(), Some("eu-central-1"));
        assert_eq!(
            imported.machine_id.as_deref(),
            Some("machine-cached-credential")
        );
    }

    #[test]
    fn export_complete_credential_backup_serializes_camel_case_and_preserves_full_fields() {
        let dir = temp_test_dir("native-backup-export-camel-case");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        cred.expires_at = Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
        cred.auth_method = Some("social".to_string());
        cred.provider = Some("GitHub".to_string());
        cred.user_id = Some("user-1".to_string());
        cred.profile_arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/backup".to_string());
        cred.machine_id = Some("machine-1".to_string());
        cred.region = Some("us-east-1".to_string());
        cred.auth_region = Some("us-west-2".to_string());
        cred.api_region = Some("us-east-2".to_string());
        cred.proxy_url = Some("direct".to_string());
        cred.endpoint = Some("ide".to_string());
        cred.concurrency = Some(3);
        let id = service.token_manager.add_imported_credential(cred).unwrap();

        let backup = service.export_credential_backup(&[id]);
        let json = serde_json::to_string(&backup).unwrap();
        assert!(json.contains("refreshToken"));
        assert!(json.contains("machineId"));
        assert!(json.contains("authRegion"));
        assert!(json.contains("apiRegion"));
        assert!(json.contains("proxyUrl"));
        assert!(!json.contains("refresh_token"));
        assert!(!json.contains("machine_id"));
        assert!(!json.contains("proxy_url"));

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["format"], CREDENTIAL_BACKUP_FORMAT);
        assert_eq!(value["source"]["credentialCount"], 1);
        let credential = &value["credentials"][0]["credential"];
        assert_eq!(credential["machineId"], "machine-1");
        assert_eq!(credential["endpoint"], "ide");
        assert_eq!(credential["concurrency"], 3);
    }

    #[test]
    fn credential_success_and_import_preview_summaries_preserve_source_metadata() {
        let dir = temp_test_dir("credential-summary-source-metadata");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("r".repeat(150));
        cred.access_token = Some("access-token".to_string());
        cred.auth_method = Some("external_idp".to_string());
        cred.provider = Some("AzureAD".to_string());
        cred.user_id = Some("https://login.microsoftonline.com/tenant/v2.0".to_string());
        cred.client_id = Some("client-id".to_string());
        cred.token_endpoint =
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token".to_string());
        cred.issuer_url = Some("https://login.microsoftonline.com/tenant/v2.0".to_string());
        cred.scopes =
            Some("api://client-id/codewhisperer:conversations offline_access".to_string());
        cred.start_url = Some("https://d-1234567890.awsapps.com/start".to_string());
        cred.client_id_hash = Some("client-hash".to_string());
        cred.sso_session_id = Some("sso-session".to_string());
        cred.region = Some("us-east-1".to_string());
        cred.auth_region = Some("us-west-2".to_string());
        cred.api_region = Some("eu-central-1".to_string());
        cred.machine_id = Some("machine-id".to_string());
        cred.endpoint = Some("ide".to_string());
        cred.proxy_username = Some("proxy-user".to_string());
        cred.meta.group_id = Some("group-1".to_string());
        cred.meta.tag_links = Some(serde_json::json!([{ "tagId": "tag-1", "tagName": "Tenant" }]));
        cred.meta.usage_data = Some(serde_json::json!({
            "userInfo": { "email": "user@example.com" },
            "usageBreakdownList": [{ "currentUsage": 1, "usageLimit": 10 }]
        }));
        cred.meta.available_models_cache = Some(serde_json::json!({
            "cachedAt": 1893456000,
            "response": { "availableModels": [] }
        }));
        cred.meta.failure_count = Some(2);
        cred.meta.last_failure_at = Some("2026-06-28T10:01:00Z".to_string());
        cred.meta.disabled_reason = Some("manual".to_string());
        cred.meta.success_count = Some(7);
        let id = service.token_manager.add_imported_credential(cred).unwrap();

        let added = service.add_credential_response_from_stored(id, "ok".to_string(), None);
        assert_eq!(added.auth_method.as_deref(), Some("external_idp"));
        assert_eq!(added.provider.as_deref(), Some("AzureAD"));
        assert_eq!(added.auth_region.as_deref(), Some("us-west-2"));
        assert_eq!(added.api_region.as_deref(), Some("eu-central-1"));
        assert_eq!(
            added.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token")
        );
        assert!(added.has_proxy_credentials);

        let details = service.credential_login_details_from_stored(id);
        assert_eq!(details.machine_id.as_deref(), Some("machine-id"));
        assert_eq!(details.client_id_hash.as_deref(), Some("client-hash"));
        assert_eq!(details.sso_session_id.as_deref(), Some("sso-session"));
        assert_eq!(details.api_region.as_deref(), Some("eu-central-1"));
        assert_eq!(details.group_id.as_deref(), Some("group-1"));
        assert!(details.tag_links.is_some());
        assert!(details.has_usage_data);
        assert!(details.has_available_models_cache);
        assert_eq!(details.source_failure_count, Some(2));
        assert_eq!(
            details.source_last_failure_at.as_deref(),
            Some("2026-06-28T10:01:00Z")
        );
        assert_eq!(details.source_disabled_reason.as_deref(), Some("manual"));
        assert_eq!(details.source_success_count, Some(7));

        let preview = service.import_credentials(ImportCredentialsRequest {
            dry_run: true,
            mode: CredentialImportMode::SkipExisting,
            input: serde_json::json!({
                "accessToken": "access-preview",
                "refreshToken": "p".repeat(150),
                "authMethod": "external_idp",
                "provider": "Microsoft",
                "clientId": "client-preview",
                "tokenEndpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
                "issuerUrl": "https://login.microsoftonline.com/tenant/v2.0",
                "scopes": "api://client-preview/codewhisperer:conversations offline_access",
                "region": "us-east-1",
                "authRegion": "us-west-2",
                "apiRegion": "eu-central-1",
                "machineId": "machine-preview",
                "groupId": "group-preview",
                "tagLinks": [{ "tagId": "tag-preview", "tagName": "Preview" }],
                "usageData": {
                    "userInfo": { "email": "preview@example.com" },
                    "usageBreakdownList": [{ "currentUsage": 2, "usageLimit": 20 }]
                },
                "availableModelsCache": {
                    "cachedAt": 1893456000,
                    "response": { "availableModels": [] }
                },
                "failureCount": 3,
                "lastFailureAt": "2026-06-28T10:02:00Z",
                "disabledReason": "imported",
                "successCount": 9,
                "startUrl": "https://d-1234567890.awsapps.com/start",
                "clientIdHash": "hash-preview",
                "ssoSessionId": "session-preview",
                "endpoint": "ide"
            }),
        });

        assert_eq!(preview.summary.parsed, 1);
        assert_eq!(preview.summary.invalid, 0);
        let item = &preview.items[0];
        assert_eq!(item.auth_method.as_deref(), Some("external_idp"));
        assert_eq!(item.provider.as_deref(), Some("AzureAD"));
        assert_eq!(item.region.as_deref(), Some("us-east-1"));
        assert_eq!(item.auth_region.as_deref(), Some("us-west-2"));
        assert_eq!(item.api_region.as_deref(), Some("eu-central-1"));
        assert_eq!(item.machine_id.as_deref(), Some("machine-preview"));
        assert_eq!(item.group_id.as_deref(), Some("group-preview"));
        assert!(item.tag_links.is_some());
        assert!(item.has_usage_data);
        assert!(item.has_available_models_cache);
        assert_eq!(item.source_failure_count, Some(3));
        assert_eq!(
            item.source_last_failure_at.as_deref(),
            Some("2026-06-28T10:02:00Z")
        );
        assert_eq!(item.source_disabled_reason.as_deref(), Some("imported"));
        assert_eq!(item.source_success_count, Some(9));
        assert_eq!(item.client_id_hash.as_deref(), Some("hash-preview"));
        assert_eq!(item.sso_session_id.as_deref(), Some("session-preview"));
        assert_eq!(
            item.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn credential_record_import_preserves_source_metadata_in_stored_model_and_summary() {
        let dir = temp_test_dir("credential-record-source-metadata");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let access_token = jwt_with_exp((Utc::now() + chrono::Duration::hours(1)).timestamp());

        let mut raw = serde_json::Map::new();
        macro_rules! field {
            ($key:literal, $value:expr) => {
                raw.insert($key.to_string(), serde_json::json!($value));
            };
        }
        field!("id", "source-single-credential");
        field!("accessToken", access_token);
        field!("refreshToken", "g".repeat(150));
        field!("authMethod", "microsoft");
        field!("provider", "Microsoft");
        field!("clientId", "client-single");
        field!(
            "tokenEndpoint",
            "https://login.microsoftonline.com/tenant/oauth2/v2.0/token"
        );
        field!("issuerUrl", "https://login.microsoftonline.com/tenant/v2.0");
        field!(
            "scopes",
            "api://client-single/codewhisperer:conversations offline_access"
        );
        field!("email", "single@example.com");
        field!("userId", "https://login.microsoftonline.com/tenant/v2.0");
        field!("machineId", "machine-single");
        field!("region", "us-east-1");
        field!("authRegion", "us-west-2");
        field!("apiRegion", "eu-central-1");
        field!("label", "Single import");
        field!("status", "active");
        field!("addedAt", "2026/06/29 10:00:00");
        field!("password", "source-password");
        field!("nickname", "Single Credential");
        field!(
            "usageData",
            serde_json::json!({ "userInfo": { "email": "single@example.com" } })
        );
        field!("groupId", "group-single");
        field!(
            "tagLinks",
            serde_json::json!([{ "tagId": "tag-single", "tagName": "Single" }])
        );
        field!(
            "availableModelsCache",
            serde_json::json!({ "response": { "models": ["claude-sonnet"] } })
        );
        field!("failureCount", 4);
        field!("lastFailureAt", "2026-06-29T01:02:03Z");
        field!("disabledReason", "manual");
        field!("successCount", 11);
        field!("subscriptionType", "Pro");
        field!("subscriptionTitle", "KIRO PRO");
        field!("usageCurrent", 12.5);
        field!("usageLimit", 100.0);
        field!("usagePercent", 12.5);
        field!("nextResetDate", "2026-07-01");
        field!("requestCount", 19);
        field!("errorCount", 2);
        field!("totalTokens", 12345);
        field!("totalCredits", 6.5);
        field!("lastUsedAt", 1893456000);
        field!("createdAt", 1893450000);
        field!("tags", serde_json::json!(["single", "source"]));
        field!("proxyURL", "direct");
        field!("endpoint", "ide");
        let req: ImportCredentialRecordRequest =
            serde_json::from_value(serde_json::Value::Object(raw)).unwrap();

        let response = service.import_credential_record(req).await.unwrap();

        assert_eq!(response.auth_method.as_deref(), Some("external_idp"));
        assert_eq!(response.provider.as_deref(), Some("AzureAD"));
        assert_eq!(
            response.source_account_id.as_deref(),
            Some("source-single-credential")
        );
        assert_eq!(response.group_id.as_deref(), Some("group-single"));
        assert!(response.tag_links.is_some());
        assert_eq!(response.has_usage_data, true);
        assert_eq!(response.has_available_models_cache, true);
        assert_eq!(response.source_failure_count, Some(4));
        assert_eq!(response.source_success_count, Some(11));
        assert_eq!(response.auth_region.as_deref(), Some("us-west-2"));
        assert_eq!(response.api_region.as_deref(), Some("eu-central-1"));

        let imported = service
            .token_manager
            .export_credentials_by_ids(&[response.credential_id])
            .pop()
            .unwrap();
        assert_eq!(
            imported.meta.source_account_id.as_deref(),
            Some("source-single-credential")
        );
        assert_eq!(imported.meta.label.as_deref(), Some("Single import"));
        assert_eq!(imported.meta.status.as_deref(), Some("active"));
        assert_eq!(
            imported.meta.added_at.as_deref(),
            Some("2026/06/29 10:00:00")
        );
        assert_eq!(imported.meta.password.as_deref(), Some("source-password"));
        assert_eq!(imported.meta.nickname.as_deref(), Some("Single Credential"));
        assert_eq!(imported.meta.group_id.as_deref(), Some("group-single"));
        assert!(imported.meta.tag_links.is_some());
        assert!(imported.meta.usage_data.is_some());
        assert!(imported.meta.available_models_cache.is_some());
        assert_eq!(imported.meta.failure_count, Some(4));
        assert_eq!(
            imported.meta.last_failure_at.as_deref(),
            Some("2026-06-29T01:02:03Z")
        );
        assert_eq!(imported.meta.disabled_reason.as_deref(), Some("manual"));
        assert_eq!(imported.meta.success_count, Some(11));
        assert_eq!(imported.meta.subscription_type.as_deref(), Some("Pro"));
        assert_eq!(
            imported.meta.subscription_title.as_deref(),
            Some("KIRO PRO")
        );
        assert_eq!(imported.meta.usage_current, Some(12.5));
        assert_eq!(imported.meta.usage_limit, Some(100.0));
        assert_eq!(imported.meta.usage_percent, Some(12.5));
        assert_eq!(imported.meta.request_count, Some(19));
        assert_eq!(imported.meta.error_count, Some(2));
        assert_eq!(imported.meta.total_tokens, Some(12345));
        assert_eq!(imported.meta.total_credits, Some(6.5));
        assert_eq!(imported.meta.last_used_at, Some(1893456000));
        assert_eq!(imported.meta.created_at, Some(1893450000));
        assert!(imported.meta.tags.is_some());

        let details = service.credential_login_details_from_stored(response.credential_id);
        assert_eq!(details.group_id.as_deref(), Some("group-single"));
        assert!(details.has_usage_data);
        assert!(details.has_available_models_cache);
        assert_eq!(details.source_failure_count, Some(4));
        assert_eq!(details.source_success_count, Some(11));
    }

    #[test]
    fn import_credentials_round_trips_multiple_auth_methods_without_refresh() {
        let dir = temp_test_dir("backup-round-trip");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let access_jwt = jwt_with_exp((Utc::now() + chrono::Duration::hours(1)).timestamp());

        let backup = serde_json::json!({
            "format": CREDENTIAL_BACKUP_FORMAT,
            "version": CREDENTIAL_BACKUP_VERSION,
            "exportedAt": Utc::now().to_rfc3339(),
            "source": { "app": "xkiro.rs", "schema": CREDENTIAL_BACKUP_SCHEMA, "credentialCount": 4 },
            "credentials": [
                { "credential": {
                    "accessToken": "access-social",
                    "refreshToken": "s".repeat(150),
                    "expiresAt": future,
                    "authMethod": "social",
                    "provider": "GitHub",
                    "email": "dev@github.local",
                    "profileArn": "arn:aws:codewhisperer:us-east-1:123:profile/social",
                    "machineId": "machine-social",
                    "region": "us-east-1",
                    "apiRegion": "us-east-2",
                    "proxyUrl": "direct"
                }},
                { "credential": {
                    "accessToken": "access-idc",
                    "refreshToken": "i".repeat(150),
                    "expiresAt": future,
                    "authMethod": "IdC",
                    "clientId": "client-idc",
                    "clientSecret": "secret-idc",
                    "startUrl": "https://d-123.awsapps.com/start",
                    "clientIdHash": "hash-idc",
                    "idToken": "id-token-idc",
                    "ssoSessionId": "session-idc",
                    "authRegion": "us-west-2",
                    "machineId": "machine-idc"
                }},
                { "credential": {
                    "accessToken": access_jwt,
                    "refreshToken": "e".repeat(150),
                    "authMethod": "external_idp",
                    "provider": "Microsoft",
                    "clientId": "client-external",
                    "tokenEndpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
                    "issuerUrl": "https://login.microsoftonline.com/tenant/v2.0",
                    "scopes": "openid profile offline_access",
                    "machineId": "machine-external"
                }},
                { "credential": {
                    "authMethod": "api_key",
                    "apiKey": "ksk_test_backup_key",
                    "endpoint": "ide",
                    "disabled": true,
                    "concurrency": 2,
                    "machineId": "machine-api"
                }}
            ]
        });

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: backup,
        });

        assert_eq!(response.summary.added, 4);
        assert_eq!(response.summary.invalid, 0);
        let snapshot = service.token_manager.snapshot();
        assert_eq!(snapshot.total, 4);

        let exported = service.token_manager.export_credentials_by_ids(
            &snapshot
                .entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
        );
        assert!(exported.iter().any(|cred| {
            cred.auth_method.as_deref() == Some("social")
                && cred.machine_id.as_deref() == Some("machine-social")
                && cred.api_region.as_deref() == Some("us-east-2")
                && cred.provider.as_deref() == Some("GitHub")
        }));
        assert!(exported.iter().any(|cred| {
            cred.auth_method.as_deref() == Some("idc")
                && cred.provider.as_deref() == Some("Enterprise")
                && cred.client_id_hash.as_deref() == Some("hash-idc")
                && cred.sso_session_id.as_deref() == Some("session-idc")
        }));
        assert!(exported.iter().any(|cred| {
            cred.auth_method.as_deref() == Some("external_idp")
                && cred.provider.as_deref() == Some("AzureAD")
                && cred
                    .token_endpoint
                    .as_deref()
                    .unwrap()
                    .contains("microsoftonline.com")
                && cred.expires_at.is_some()
        }));
        assert!(exported.iter().any(|cred| {
            cred.auth_method.as_deref() == Some("api_key")
                && cred.api_key.as_deref() == Some("ksk_test_backup_key")
                && cred.disabled
                && cred.concurrency == Some(2)
        }));

        let status = service.get_all_credentials();
        let api_key_item = status
            .credentials
            .iter()
            .find(|item| item.auth_method.as_deref() == Some("api_key"))
            .unwrap();
        assert!(api_key_item.has_api_key);
        assert!(!api_key_item.has_refresh_token);
    }

    #[test]
    fn import_credentials_derives_external_idp_from_provider_and_access_token_jwt() {
        let dir = temp_test_dir("backup-external-provider-jwt");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let exp = (Utc::now() + chrono::Duration::hours(1)).timestamp();
        let access_jwt = jwt_with_iss_exp("https://login.microsoftonline.com/tenant-1/v2.0", exp);

        let backup = serde_json::json!({
            "format": CREDENTIAL_BACKUP_FORMAT,
            "version": CREDENTIAL_BACKUP_VERSION,
            "exportedAt": Utc::now().to_rfc3339(),
            "source": { "app": "xkiro.rs", "schema": CREDENTIAL_BACKUP_SCHEMA, "credentialCount": 1 },
            "credentials": [{ "credential": {
                "accessToken": access_jwt,
                "refreshToken": "m".repeat(150),
                "provider": "Microsoft",
                "clientId": "client-microsoft",
                "machineId": "machine-microsoft"
            }}]
        });

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: backup,
        });

        assert_eq!(response.summary.added, 1);
        assert_eq!(response.summary.invalid, 0);
        let id = service.token_manager.snapshot().entries[0].id;
        let imported = service
            .token_manager
            .export_credentials_by_ids(&[id])
            .pop()
            .unwrap();

        assert_eq!(imported.auth_method.as_deref(), Some("external_idp"));
        assert_eq!(imported.provider.as_deref(), Some("AzureAD"));
        assert_eq!(
            imported.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant-1/oauth2/v2.0/token")
        );
        assert_eq!(
            imported.issuer_url.as_deref(),
            Some("https://login.microsoftonline.com/tenant-1/v2.0")
        );
        assert!(
            imported
                .scopes
                .as_deref()
                .unwrap_or_default()
                .contains("codewhisperer:conversations")
        );
        assert!(imported.expires_at.is_some());
    }

    #[test]
    fn import_credentials_dry_run_does_not_write_and_rejects_bad_external_endpoint() {
        let dir = temp_test_dir("backup-dry-run-security");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let dry_run_backup = serde_json::json!({
            "format": CREDENTIAL_BACKUP_FORMAT,
            "version": CREDENTIAL_BACKUP_VERSION,
            "exportedAt": Utc::now().to_rfc3339(),
            "source": { "app": "xkiro.rs", "schema": CREDENTIAL_BACKUP_SCHEMA, "credentialCount": 1 },
            "credentials": [{ "credential": {
                "accessToken": "access-dry-run",
                "refreshToken": "d".repeat(150),
                "authMethod": "social",
                "machineId": "machine-dry-run"
            }}]
        });

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: true,
            mode: CredentialImportMode::SkipExisting,
            input: dry_run_backup,
        });
        assert_eq!(response.summary.added, 1);
        assert_eq!(service.token_manager.snapshot().total, 0);

        let bad_external = serde_json::json!({
            "format": CREDENTIAL_BACKUP_FORMAT,
            "version": CREDENTIAL_BACKUP_VERSION,
            "exportedAt": Utc::now().to_rfc3339(),
            "source": { "app": "xkiro.rs", "schema": CREDENTIAL_BACKUP_SCHEMA, "credentialCount": 1 },
            "credentials": [{ "credential": {
                "refreshToken": "z".repeat(150),
                "authMethod": "external_idp",
                "clientId": "client-bad",
                "tokenEndpoint": "http://127.0.0.1/token"
            }}]
        });

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: true,
            mode: CredentialImportMode::SkipExisting,
            input: bad_external,
        });
        assert_eq!(response.summary.invalid, 1);
        assert!(
            response.items[0]
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("external IdP endpoint rejected")
        );
    }

    #[test]
    fn import_credentials_converts_credential_snapshot_and_flat_credential_shapes() {
        let dir = temp_test_dir("import-snapshot-shapes");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let expires_ms = 1_893_456_000_000_i64;

        let credential_snapshot = serde_json::json!({
            "version": "test",
            "exportedAt": expires_ms,
            "accounts": [{
                "id": "source-credential-1",
                "email": "builder@example.com",
                "nickname": "Builder backup",
                "idp": "BuilderId",
                "userId": "builder-user",
                "machineId": "machine-builder",
                "tags": ["primary", "builder"],
                "status": "active",
                "createdAt": 1_893_455_000_000_i64,
                "lastUsedAt": 1_893_455_500_000_i64,
                "credentials": {
                    "accessToken": "access-builder",
                    "csrfToken": "csrf-builder",
                    "refreshToken": "b".repeat(150),
                    "clientId": "client-builder",
                    "clientSecret": "secret-builder",
                    "region": "us-east-1",
                    "expiresAt": expires_ms,
                    "authMethod": "IdC"
                },
                "subscription": { "type": "Pro", "title": "KIRO PRO" },
                "usage": {
                    "current": 12.5,
                    "limit": 100.0,
                    "percentUsed": 0.125,
                    "lastUpdated": 1_893_455_900_000_i64
                }
            }]
        });
        let parsed = service
            .parse_credential_import(credential_snapshot)
            .unwrap();
        assert_eq!(parsed.source_format, SOURCE_FORMAT_CREDENTIAL_SNAPSHOT);
        let converted = service
            .normalize_imported_credential(parsed.credentials.into_iter().next().unwrap())
            .unwrap();
        assert_eq!(converted.auth_method.as_deref(), Some("idc"));
        assert_eq!(converted.provider.as_deref(), Some("BuilderId"));
        assert_eq!(converted.machine_id.as_deref(), Some("machine-builder"));
        assert_eq!(converted.meta.nickname.as_deref(), Some("Builder backup"));
        assert_eq!(converted.meta.csrf_token.as_deref(), Some("csrf-builder"));
        assert_eq!(converted.meta.status.as_deref(), Some("active"));
        assert_eq!(
            converted.meta.subscription_title.as_deref(),
            Some("KIRO PRO")
        );
        assert_eq!(converted.meta.subscription_type.as_deref(), Some("Pro"));
        assert_eq!(converted.meta.usage_current, Some(12.5));
        assert_eq!(converted.meta.usage_limit, Some(100.0));
        assert_eq!(converted.meta.usage_percent, Some(0.125));
        assert_eq!(converted.meta.last_refresh, Some(1_893_455_900_000_i64));
        assert_eq!(converted.meta.created_at, Some(1_893_455_000_000_i64));
        assert_eq!(converted.meta.last_used_at, Some(1_893_455_500_000_i64));
        assert_eq!(
            converted
                .meta
                .tags
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            converted.meta.source_account_id.as_deref(),
            Some("source-credential-1")
        );
        assert_eq!(
            converted.client_id_hash.as_deref(),
            Some(KIRO_BUILDER_ID_CLIENT_ID_HASH)
        );
        assert_eq!(
            converted.expires_at.as_deref(),
            Some("2030-01-01T00:00:00+00:00")
        );

        let flat_credential = serde_json::json!([{
            "id": "source-credential",
            "email": "user@gmail.com",
            "accessToken": "access-source",
            "refreshToken": "k".repeat(150),
            "authMethod": "IdC",
            "clientId": "client-source",
            "clientSecret": "secret-source",
            "clientIdHash": "hash-source",
            "ssoSessionId": "session-source",
            "idToken": "id-token-source",
            "region": "eu-west-1",
            "startUrl": "https://view.awsapps.com/start",
            "profileArn": "arn:aws:codewhisperer:eu-west-1:123:profile/source",
            "machineId": "machine-source",
            "enabled": false,
            "proxyConfig": {
                "enabled": true,
                "protocol": "socks5",
                "host": "127.0.0.1",
                "port": 1080,
                "username": "proxy-user",
                "password": "proxy-pass"
            }
        }]);
        let parsed = service.parse_credential_import(flat_credential).unwrap();
        let converted = service
            .normalize_imported_credential(parsed.credentials.into_iter().next().unwrap())
            .unwrap();
        assert_eq!(converted.auth_method.as_deref(), Some("idc"));
        assert_eq!(
            converted.proxy_url.as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        assert_eq!(converted.proxy_username.as_deref(), Some("proxy-user"));
        assert_eq!(converted.proxy_password.as_deref(), Some("proxy-pass"));
        assert!(converted.disabled);
        assert_eq!(converted.id, None);
        assert_eq!(
            converted.meta.source_account_id.as_deref(),
            Some("source-credential")
        );
        assert_eq!(converted.region.as_deref(), Some("eu-west-1"));
    }

    #[test]
    fn import_credentials_accepts_canonical_snapshot_credentials_collection() {
        let dir = temp_test_dir("import-canonical-snapshot-collection");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let parsed = service
            .parse_credential_import(serde_json::json!({
                "version": "test",
                "exportedAt": 1_893_456_000_000_i64,
                "credentials": [{
                    "id": "source-canonical-credential",
                    "email": "canonical@example.com",
                    "nickname": "Canonical snapshot",
                    "idp": "BuilderId",
                    "machineId": "machine-canonical",
                    "credentials": {
                        "refreshToken": "c".repeat(150),
                        "clientId": "client-canonical",
                        "clientSecret": "secret-canonical",
                        "authMethod": "IdC",
                        "expiresAt": 1_893_456_000_000_i64
                    }
                }]
            }))
            .unwrap();

        assert_eq!(parsed.source_format, SOURCE_FORMAT_CREDENTIAL_SNAPSHOT);
        let converted = service
            .normalize_imported_credential(parsed.credentials.into_iter().next().unwrap())
            .unwrap();
        assert_eq!(
            converted.meta.source_account_id.as_deref(),
            Some("source-canonical-credential")
        );
        assert_eq!(converted.auth_method.as_deref(), Some("idc"));
        assert_eq!(converted.provider.as_deref(), Some("BuilderId"));
        assert_eq!(
            converted.expires_at.as_deref(),
            Some("2030-01-01T00:00:00+00:00")
        );
    }

    #[test]
    fn import_credentials_accepts_credential_snapshot_items_without_credentials_wrapper() {
        let dir = temp_test_dir("import-credential-snapshot-items");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut snapshot_item = serde_json::Map::new();
        macro_rules! field {
            ($key:literal, $value:expr) => {
                snapshot_item.insert($key.to_string(), serde_json::json!($value));
            };
        }
        field!("id", "source-config-credential");
        field!("email", "config@example.com");
        field!("userId", "config-user");
        field!("nickname", "Config credential");
        field!("accessToken", "access-config");
        field!("refreshToken", "c".repeat(150));
        field!("clientId", "client-config");
        field!("clientSecret", "secret-config");
        field!("authMethod", "idc");
        field!("provider", "BuilderId");
        field!("region", "us-west-2");
        field!("expiresAt", 1_893_456_000_i64);
        field!("machineId", "machine-config");
        field!(
            "profileArn",
            "arn:aws:codewhisperer:us-west-2:123:profile/config"
        );
        field!("proxyURL", "direct");
        field!("weight", 4);
        field!("overageStatus", "ENABLED");
        field!("overageCapability", "OVERAGE_CAPABLE");
        field!("overageCap", 20.0);
        field!("overageRate", 0.2);
        field!("currentOverages", 3.5);
        field!("overageCheckedAt", 1_893_455_000_i64);
        field!("enabled", false);
        field!("banStatus", "SUSPENDED");
        field!("banReason", "manual-test");
        field!("banTime", 1_893_455_100_i64);
        field!("subscriptionType", "PRO_PLUS");
        field!("subscriptionTitle", "KIRO PRO+");
        field!("daysRemaining", 14);
        field!("usageCurrent", 42.0);
        field!("usageLimit", 100.0);
        field!("usagePercent", 0.42);
        field!("nextResetDate", "2030-02-01");
        field!("lastRefresh", 1_893_455_200_i64);
        field!("trialUsageCurrent", 1.0);
        field!("trialUsageLimit", 5.0);
        field!("trialUsagePercent", 0.2);
        field!("trialStatus", "ACTIVE");
        field!("trialExpiresAt", 1_893_555_000_i64);
        field!("requestCount", 11);
        field!("errorCount", 2);
        field!("totalTokens", 12345);
        field!("totalCredits", 6.75);
        field!("lastUsed", 1_893_455_300_i64);

        let import_payload = serde_json::json!({
            "accounts": [serde_json::Value::Object(snapshot_item)]
        });

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: import_payload,
        });

        assert_eq!(response.summary.added, 1);
        assert_eq!(response.summary.invalid, 0);
        let id = service.token_manager.snapshot().entries[0].id;
        let imported = service
            .token_manager
            .export_credentials_by_ids(&[id])
            .pop()
            .unwrap();

        assert_eq!(
            imported.meta.source_account_id.as_deref(),
            Some("source-config-credential")
        );
        assert_eq!(
            imported.expires_at.as_deref(),
            Some("2030-01-01T00:00:00+00:00")
        );
        assert_eq!(imported.meta.nickname.as_deref(), Some("Config credential"));
        assert_eq!(imported.region.as_deref(), Some("us-west-2"));
        assert_eq!(imported.weight, 4);
        assert!(imported.disabled);
        assert_eq!(imported.meta.overage_status.as_deref(), Some("ENABLED"));
        assert_eq!(
            imported.meta.overage_capability.as_deref(),
            Some("OVERAGE_CAPABLE")
        );
        assert_eq!(imported.meta.overage_cap, Some(20.0));
        assert_eq!(imported.meta.overage_rate, Some(0.2));
        assert_eq!(imported.meta.current_overages, Some(3.5));
        assert_eq!(imported.meta.overage_checked_at, Some(1_893_455_000_i64));
        assert_eq!(imported.meta.ban_status.as_deref(), Some("SUSPENDED"));
        assert_eq!(imported.meta.ban_reason.as_deref(), Some("manual-test"));
        assert_eq!(imported.meta.ban_time, Some(1_893_455_100_i64));
        assert_eq!(imported.meta.subscription_type.as_deref(), Some("PRO_PLUS"));
        assert_eq!(
            imported.meta.subscription_title.as_deref(),
            Some("KIRO PRO+")
        );
        assert_eq!(imported.meta.days_remaining, Some(14));
        assert_eq!(imported.meta.usage_current, Some(42.0));
        assert_eq!(imported.meta.usage_limit, Some(100.0));
        assert_eq!(imported.meta.usage_percent, Some(0.42));
        assert_eq!(imported.meta.next_reset_date.as_deref(), Some("2030-02-01"));
        assert_eq!(imported.meta.last_refresh, Some(1_893_455_200_i64));
        assert_eq!(imported.meta.trial_usage_current, Some(1.0));
        assert_eq!(imported.meta.trial_usage_limit, Some(5.0));
        assert_eq!(imported.meta.trial_usage_percent, Some(0.2));
        assert_eq!(imported.meta.trial_status.as_deref(), Some("ACTIVE"));
        assert_eq!(imported.meta.trial_expires_at, Some(1_893_555_000_i64));
        assert_eq!(imported.meta.request_count, Some(11));
        assert_eq!(imported.meta.error_count, Some(2));
        assert_eq!(imported.meta.total_tokens, Some(12345));
        assert_eq!(imported.meta.total_credits, Some(6.75));
        assert_eq!(imported.meta.last_used_at, Some(1_893_455_300_i64));

        let status = service.get_all_credentials();
        let item = &status.credentials[0];
        assert_eq!(
            item.source_account_id.as_deref(),
            Some("source-config-credential")
        );
        assert_eq!(item.nickname.as_deref(), Some("Config credential"));
        assert!(item.has_token);
        assert!(item.has_refresh_token);
        assert!(item.has_client_id);
        assert!(item.has_client_secret);
        assert!(!item.has_id_token);
        assert_eq!(item.ban_time, Some(1_893_455_100_i64));
        assert_eq!(item.overage_cap, Some(20.0));
        assert_eq!(item.overage_rate, Some(0.2));
        assert_eq!(item.current_overages, Some(3.5));
        assert_eq!(item.usage_current, Some(42.0));
        assert_eq!(item.usage_limit, Some(100.0));
        assert_eq!(item.usage_percent, Some(0.42));
        assert_eq!(item.trial_status.as_deref(), Some("ACTIVE"));
        assert_eq!(item.request_count, Some(11));
        assert_eq!(item.error_count, Some(2));
        assert_eq!(item.total_tokens, Some(12345));
        assert_eq!(item.total_credits, Some(6.75));
        assert_eq!(item.last_used, Some(1_893_455_300_i64));

        let exported = service.export_credential_backup(&[id]);
        let exported_credential = &exported.credentials[0].credential;
        assert_eq!(
            exported_credential.meta.source_account_id.as_deref(),
            Some("source-config-credential")
        );
        assert_eq!(
            exported_credential.meta.nickname.as_deref(),
            Some("Config credential")
        );
        assert_eq!(exported_credential.meta.overage_cap, Some(20.0));
        assert_eq!(exported_credential.meta.usage_current, Some(42.0));
        assert_eq!(
            exported_credential.meta.trial_status.as_deref(),
            Some("ACTIVE")
        );
        assert_eq!(exported_credential.meta.request_count, Some(11));
        assert_eq!(
            exported_credential.meta.last_used_at,
            Some(1_893_455_300_i64)
        );
    }

    #[test]
    fn import_credentials_preserves_source_shape_saved_credential_model() {
        let dir = temp_test_dir("import-source-shape-saved-credential");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let source_shape_input = serde_json::json!([{
            "id": "source-enterprise-credential",
            "email": "enterprise@example.com",
            "password": "source-password",
            "label": "Work tenant",
            "status": "active",
            "addedAt": "2026/06/28 10:00:00",
            "accessToken": "access-enterprise",
            "refreshToken": "r".repeat(150),
            "expiresAt": "2030-01-01T00:00:00Z",
            "provider": "Enterprise",
            "userId": "enterprise-user",
            "authMethod": "IdC",
            "clientId": "client-enterprise",
            "clientSecret": "secret-enterprise",
            "region": "eu-west-1",
            "clientIdHash": "enterprise-hash",
            "ssoSessionId": "session-enterprise",
            "idToken": "id-token-enterprise",
            "startUrl": "https://d-90660ceab3.awsapps.com/start/",
            "profileArn": "arn:aws:codewhisperer:eu-west-1:123:profile/enterprise",
            "usageData": {
                "userInfo": {
                    "email": "enterprise@example.com",
                    "userId": "enterprise-user"
                },
                "usageBreakdownList": [{ "current": 10, "limit": 100 }]
            },
            "groupId": "group-1",
            "tagLinks": [{
                "tagId": "tag-1",
                "tagName": "Tenant",
                "linkedAt": "2026-06-28 10:00"
            }],
            "machineId": "machine-enterprise",
            "availableModelsCache": {
                "response": { "models": ["claude-sonnet"] },
                "cachedAt": 1893456000
            },
            "failureCount": 2,
            "lastFailureAt": "2026-06-28T10:01:00Z",
            "disabledReason": "manual",
            "successCount": 7,
            "enabled": false,
            "proxyConfig": {
                "enabled": true,
                "protocol": "http",
                "host": "127.0.0.1",
                "port": 8080
            }
        }]);

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: source_shape_input,
        });

        assert_eq!(response.summary.added, 1);
        assert_eq!(response.summary.invalid, 0);
        let id = service.token_manager.snapshot().entries[0].id;
        let imported = service
            .token_manager
            .export_credentials_by_ids(&[id])
            .pop()
            .unwrap();

        assert_eq!(
            imported.meta.source_account_id.as_deref(),
            Some("source-enterprise-credential")
        );
        assert_eq!(imported.meta.label.as_deref(), Some("Work tenant"));
        assert_eq!(imported.meta.status.as_deref(), Some("active"));
        assert_eq!(
            imported.meta.added_at.as_deref(),
            Some("2026/06/28 10:00:00")
        );
        assert_eq!(imported.meta.password.as_deref(), Some("source-password"));
        assert_eq!(imported.auth_method.as_deref(), Some("idc"));
        assert_eq!(imported.provider.as_deref(), Some("Enterprise"));
        assert_eq!(imported.region.as_deref(), Some("eu-west-1"));
        assert_eq!(
            imported.start_url.as_deref(),
            Some("https://d-90660ceab3.awsapps.com/start")
        );
        assert_eq!(imported.client_id_hash.as_deref(), Some("enterprise-hash"));
        assert_eq!(
            imported.sso_session_id.as_deref(),
            Some("session-enterprise")
        );
        assert_eq!(imported.id_token.as_deref(), Some("id-token-enterprise"));
        assert_eq!(imported.machine_id.as_deref(), Some("machine-enterprise"));
        assert_eq!(imported.meta.group_id.as_deref(), Some("group-1"));
        assert_eq!(imported.meta.failure_count, Some(2));
        assert_eq!(
            imported.meta.last_failure_at.as_deref(),
            Some("2026-06-28T10:01:00Z")
        );
        assert_eq!(imported.meta.disabled_reason.as_deref(), Some("manual"));
        assert_eq!(imported.meta.success_count, Some(7));
        assert!(imported.disabled);
        assert_eq!(imported.proxy_url.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(
            imported
                .meta
                .usage_data
                .as_ref()
                .and_then(|value| value["userInfo"]["email"].as_str()),
            Some("enterprise@example.com")
        );
        assert_eq!(
            imported
                .meta
                .tag_links
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert!(imported.meta.available_models_cache.is_some());

        let exported = service.export_credential_backup(&[id]);
        let exported_credential = &exported.credentials[0].credential;
        assert_eq!(exported_credential.region.as_deref(), Some("eu-west-1"));
        assert_eq!(
            exported_credential.meta.source_account_id.as_deref(),
            Some("source-enterprise-credential")
        );
        assert_eq!(
            exported_credential.meta.usage_data.as_ref(),
            imported.meta.usage_data.as_ref()
        );

        let status = service.get_all_credentials();
        let item = &status.credentials[0];
        assert!(item.has_token);
        assert!(item.has_refresh_token);
        assert!(item.has_client_id);
        assert!(item.has_client_secret);
        assert!(item.has_id_token);
        assert!(item.has_profile_arn);
        assert_eq!(item.group_id.as_deref(), Some("group-1"));
        assert!(item.tag_links.is_some());
        assert!(item.usage_data.is_some());
        assert!(item.has_available_models_cache);
        assert_eq!(item.source_failure_count, Some(2));
        assert_eq!(
            item.source_last_failure_at.as_deref(),
            Some("2026-06-28T10:01:00Z")
        );
        assert_eq!(item.source_disabled_reason.as_deref(), Some("manual"));
        assert_eq!(item.source_success_count, Some(7));
    }

    #[test]
    fn import_credentials_derives_idc_metadata_from_nested_credentials_client_secret() {
        let dir = temp_test_dir("import-nested-idc-derive");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);
        let start_url = "https://d-derived.awsapps.com/start";
        let client_secret = client_secret_with_start_url(&format!("{start_url}/"));

        let raw_nested_credential = serde_json::json!({
            "id": "nested-enterprise-credential",
            "email": "nested@example.com",
            "provider": "Enterprise",
            "authMethod": "IdC",
            "machineId": "machine-nested",
            "credentials": {
                "accessToken": "access-nested",
                "refreshToken": "n".repeat(150),
                "clientId": "client-nested",
                "clientSecret": client_secret,
                "region": "ap-southeast-1"
            }
        });

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::SkipExisting,
            input: raw_nested_credential,
        });

        assert_eq!(response.summary.added, 1);
        assert_eq!(response.summary.invalid, 0);
        let id = service.token_manager.snapshot().entries[0].id;
        let imported = service
            .token_manager
            .export_credentials_by_ids(&[id])
            .pop()
            .unwrap();

        assert_eq!(
            imported.meta.source_account_id.as_deref(),
            Some("nested-enterprise-credential")
        );
        assert_eq!(imported.auth_method.as_deref(), Some("idc"));
        assert_eq!(imported.provider.as_deref(), Some("Enterprise"));
        assert_eq!(imported.start_url.as_deref(), Some(start_url));
        let expected_hash = AdminService::calculate_client_id_hash(start_url);
        assert_eq!(
            imported.client_id_hash.as_deref(),
            Some(expected_hash.as_str())
        );
        assert_eq!(imported.region.as_deref(), Some("ap-southeast-1"));
        assert_eq!(imported.machine_id.as_deref(), Some("machine-nested"));
    }

    #[test]
    fn import_credentials_merge_missing_uses_user_id_without_overwriting_refresh_token() {
        let dir = temp_test_dir("backup-merge-user-id");
        let config = crate::model::config::Config::default();
        let credentials_path = dir.join("credentials.json");
        let (service, _, _, _) = test_service(config, credentials_path);

        let mut existing = KiroCredentials::default();
        existing.refresh_token = Some("x".repeat(150));
        existing.access_token = Some("access-old".to_string());
        existing.auth_method = Some("social".to_string());
        existing.user_id = Some("same-user".to_string());
        existing.machine_id = Some("machine-old".to_string());
        let id = service
            .token_manager
            .add_imported_credential(existing)
            .unwrap();

        let backup = serde_json::json!({
            "format": CREDENTIAL_BACKUP_FORMAT,
            "version": CREDENTIAL_BACKUP_VERSION,
            "exportedAt": Utc::now().to_rfc3339(),
            "source": { "app": "xkiro.rs", "schema": CREDENTIAL_BACKUP_SCHEMA, "credentialCount": 1 },
            "credentials": [{ "credential": {
                "accessToken": "access-new",
                "refreshToken": "y".repeat(150),
                "authMethod": "social",
                "userId": "same-user",
                "machineId": "machine-new",
                "profileArn": "arn:aws:codewhisperer:profile/new"
            }}]
        });

        let response = service.import_credentials(ImportCredentialsRequest {
            dry_run: false,
            mode: CredentialImportMode::MergeMissing,
            input: backup,
        });

        assert_eq!(response.summary.merged, 1);
        assert_eq!(service.token_manager.snapshot().total, 1);
        let merged = service
            .token_manager
            .export_credentials_by_ids(&[id])
            .pop()
            .unwrap();
        let expected_refresh = "x".repeat(150);
        assert_eq!(
            merged.refresh_token.as_deref(),
            Some(expected_refresh.as_str())
        );
        assert_eq!(merged.machine_id.as_deref(), Some("machine-old"));
        assert_eq!(
            merged.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:profile/new")
        );
    }

    #[tokio::test]
    async fn update_access_settings_patch_empty_password_preserves_admin_key() {
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
            .update_access_settings(UpdateAccessSettingsRequest {
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
            .update_access_settings(UpdateAccessSettingsRequest {
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

    #[tokio::test]
    async fn thinking_update_accepts_missing_fields_as_zero_values() {
        let dir = temp_test_dir("thinking-partial");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let mut config = crate::model::config::Config::load(&config_path).unwrap();
        config.thinking_suffix = "-custom".to_string();
        config.openai_thinking_format = "reasoning_content".to_string();
        config.claude_thinking_format = "think".to_string();
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let request: UpdateThinkingConfigRequest = serde_json::from_value(serde_json::json!({
            "openaiFormat": "thinking"
        }))
        .expect("zero-value thinking update should parse");

        service.update_thinking_config(request).await.unwrap();

        let stored = service.token_manager.config();
        assert_eq!(stored.thinking_suffix, "-thinking");
        assert_eq!(stored.openai_thinking_format, "thinking");
        assert_eq!(stored.claude_thinking_format, "thinking");

        let runtime = service.thinking_config.read().clone();
        assert_eq!(runtime.suffix, "-thinking");
        assert_eq!(runtime.openai_format, "thinking");
        assert_eq!(runtime.claude_format, "thinking");

        let invalid: UpdateThinkingConfigRequest = serde_json::from_value(serde_json::json!({
            "openaiFormat": "bad-format",
            "claudeFormat": ""
        }))
        .unwrap();
        assert!(matches!(
            service.update_thinking_config(invalid).await,
            Err(AdminServiceError::InvalidRequest(_))
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn endpoint_update_missing_preferred_endpoint_reaches_service_validation() {
        let dir = temp_test_dir("endpoint-missing");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let config = crate::model::config::Config::load(&config_path).unwrap();
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let request: UpdateEndpointConfigRequest = serde_json::from_value(serde_json::json!({}))
            .expect("zero-value endpoint update should parse");

        assert!(matches!(
            service.update_endpoint_config(request).await,
            Err(AdminServiceError::InvalidRequest(_))
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn update_api_key_empty_key_preserves_existing_value() {
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
    fn create_api_key_whitespace_key_rejects() {
        let dir = temp_test_dir("api-key-whitespace-create");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let config = crate::model::config::Config::load(&config_path).unwrap();
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let generated = service
            .create_api_key(CreateApiKeyRequest {
                name: None,
                key: Some(String::new()),
                enabled: None,
                token_limit: 0,
                credit_limit: 0.0,
            })
            .unwrap();
        assert!(generated.key.starts_with("sk-"));

        let trimmed = service
            .create_api_key(CreateApiKeyRequest {
                name: None,
                key: Some("  sk-trimmed  ".to_string()),
                enabled: None,
                token_limit: 0,
                credit_limit: 0.0,
            })
            .unwrap();
        assert_eq!(trimmed.key, "sk-trimmed");

        let err = service
            .create_api_key(CreateApiKeyRequest {
                name: None,
                key: Some("   ".to_string()),
                enabled: None,
                token_limit: 0,
                credit_limit: 0.0,
            })
            .unwrap_err();
        assert!(matches!(err, AdminServiceError::InvalidRequest(_)));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_api_keys_migrates_config_api_key() {
        let dir = temp_test_dir("api-key-config-migration");

        let keys = AdminService::load_api_keys_runtime_with_config_key(
            Some(&dir),
            Some(" config-secret "),
            true,
        );
        let snapshot = keys.read().clone();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].key, "config-secret");
        assert!(snapshot[0].enabled);
        assert!(snapshot[0].migrated);
        assert_eq!(snapshot[0].name.as_deref(), Some("config-api-key"));
        assert!(to_api_key_view(&snapshot[0]).migrated);

        let persisted: Vec<ApiKeyEntry> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("kiro_api_keys.json")).unwrap())
                .unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].key, "config-secret");
        assert!(persisted[0].migrated);

        let reloaded = AdminService::load_api_keys_runtime_with_config_key(
            Some(&dir),
            Some("config-secret"),
            true,
        );
        assert_eq!(reloaded.read().len(), 1);
        assert_eq!(reloaded.read()[0].id, snapshot[0].id);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_api_keys_migrates_public_config_api_key_disabled() {
        let dir = temp_test_dir("api-key-public-migration");

        let keys = AdminService::load_api_keys_runtime_with_config_key(
            Some(&dir),
            Some("config-secret"),
            false,
        );
        let snapshot = keys.read().clone();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].key, "config-secret");
        assert!(!snapshot[0].enabled);
        assert!(snapshot[0].migrated);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn prompt_filter_update_accepts_partial_payload() {
        let dir = temp_test_dir("prompt-filter-partial");
        let config_path = dir.join("config.json");
        let credentials_path = dir.join("credentials.json");
        let mut config = crate::model::config::Config::load(&config_path).unwrap();
        config.prompt_filter.filter_claude_code = true;
        config.prompt_filter.filter_env_noise = true;
        config.prompt_filter.filter_strip_boundaries = true;
        config.prompt_filter.rules = vec![crate::model::config::PromptFilterRule {
            id: "rule-1".to_string(),
            name: "Rule 1".to_string(),
            enabled: true,
            rule_type: "lines-containing".to_string(),
            match_pattern: "secret".to_string(),
            replace: String::new(),
        }];
        config.save().unwrap();
        let (service, _, _, _) = test_service(config, credentials_path);

        let request: UpdatePromptFilterConfigRequest = serde_json::from_value(serde_json::json!({
            "filterEnvNoise": false
        }))
        .expect("partial prompt filter update should parse");

        service.update_prompt_filter_config(request).await.unwrap();

        let stored = service.token_manager.config().prompt_filter;
        assert!(stored.filter_claude_code);
        assert!(!stored.filter_env_noise);
        assert!(stored.filter_strip_boundaries);
        assert_eq!(stored.rules.len(), 1);
        assert_eq!(stored.rules[0].match_pattern, "secret");

        let runtime = service.prompt_filter_config.read().clone();
        assert_eq!(runtime.filter_env_noise, stored.filter_env_noise);
        assert_eq!(runtime.rules.len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod balance_response_tests {
    use super::*;

    #[test]
    fn balance_response_from_usage_maps_usage_limits_consistently() {
        let usage: UsageLimitsResponse = serde_json::from_value(serde_json::json!({
            "nextDateReset": 1234.0,
            "subscriptionInfo": {
                "subscriptionTitle": "KIRO PRO+",
                "overageCapability": "OVERAGE_CAPABLE"
            },
            "overageConfiguration": {
                "overageStatus": "ENABLED"
            },
            "usageBreakdownList": [{
                "currentUsageWithPrecision": 12.5,
                "usageLimitWithPrecision": 10.0,
                "overageCapWithPrecision": 50.0
            }]
        }))
        .expect("usage limits should parse");

        let balance = balance_response_from_usage(42, &usage);

        assert_eq!(balance.id, 42);
        assert_eq!(balance.subscription_title.as_deref(), Some("KIRO PRO+"));
        assert_eq!(balance.subscription_type.as_deref(), Some("PRO_PLUS"));
        assert_eq!(balance.current_usage, 12.5);
        assert_eq!(balance.usage_limit, 10.0);
        assert_eq!(balance.remaining, 0.0);
        assert_eq!(balance.usage_percentage, 100.0);
        assert_eq!(balance.next_reset_at, Some(1234.0));
        assert_eq!(balance.overage_cap, 50.0);
        assert_eq!(
            balance.overage_capability.as_deref(),
            Some("OVERAGE_CAPABLE")
        );
        assert_eq!(balance.overage_status.as_deref(), Some("ENABLED"));
    }
}

#[cfg(test)]
mod export_adapter_tests {
    use super::*;

    #[test]
    fn credential_snapshot_subscription_type_uses_canonical_classifier() {
        assert_eq!(
            AdminService::credential_export_subscription_type(Some("KIRO PRO+")),
            "Pro_Plus"
        );
        assert_eq!(
            AdminService::credential_export_subscription_type(Some("KIRO POWER")),
            "Pro_Plus"
        );
        assert_eq!(
            AdminService::credential_export_subscription_type(Some("enterprise")),
            "Enterprise"
        );
        assert_eq!(
            AdminService::credential_export_subscription_type(Some("KIRO TEAMS")),
            "Teams"
        );
        assert_eq!(
            AdminService::credential_export_subscription_type(None),
            "Free"
        );
    }
}

#[cfg(test)]
mod api_key_tests {
    use super::*;

    #[test]
    fn mask_api_key_keeps_short_values_and_masks_middle() {
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
            migrated: true,
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
        assert_eq!(value["migrated"], true);
        assert_eq!(value["lastUsedAt"], 200);
    }

    #[test]
    fn prompt_filter_dto_uses_canonical_fields() {
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
        .expect("prompt filter request should parse");
        assert_eq!(request.filter_strip_boundaries, Some(false));
    }
}
