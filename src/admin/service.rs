//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::anthropic::middleware::PromptCacheRuntime;
use crate::common::utf8::floor_char_boundary;
use crate::http_client::ProxyConfig;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::{LOW_BALANCE_THRESHOLD, MultiTokenManager};
use crate::model::config::{CompressionConfig, SystemPromptPosition, UserPreset};
use crate::model::runtime::SharedPromptConfig;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, BatchRefreshBalanceResponse,
    BatchRefreshBalanceResultItem, BatchRefreshResponse, BatchRefreshResultItem,
    CachedBalanceItem, CachedBalancesResponse, CompressionConfigResponse, CredentialStatusItem,
    CredentialsStatusResponse, ExportKamItem, ExportTokenJsonItem, GlobalConfigResponse,
    ImportAction, ImportItemResult, ImportSummary, ImportTokenJsonRequest, ImportTokenJsonResponse,
    PresetItem,
    ProxyConfigResponse, RuntimeBalanceSnapshot, RuntimeStatsItem, RuntimeStatsResponse,
    SystemPromptResponse, TokenJsonItem, UpdateCompressionConfigRequest,
    UpdateGlobalConfigRequest, UpdateProxyConfigRequest, UpdateSystemPromptRequest,
    UpsertUserPresetRequest,
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
    /// Prompt Cache 运行时（共享引用，支持 ttl/accounting 热更新）
    prompt_cache_runtime: Arc<RwLock<PromptCacheRuntime>>,
    /// 系统提示注入运行时（共享引用，支持热更新）
    prompt_runtime: SharedPromptConfig,
    /// 截断恢复识别开关（与 AppState 共享，admin 写入即热生效）
    truncation_recovery_notice: Arc<std::sync::atomic::AtomicBool>,
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
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        kiro_provider: Option<Arc<KiroProvider>>,
        compression_config: Arc<RwLock<CompressionConfig>>,
        prompt_cache_runtime: Arc<RwLock<PromptCacheRuntime>>,
        prompt_runtime: SharedPromptConfig,
        truncation_recovery_notice: Arc<std::sync::atomic::AtomicBool>,
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
            prompt_cache_runtime,
            prompt_runtime,
            truncation_recovery_notice,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            balance_semaphore: Arc::new(Semaphore::new(8)),
            models_cache: Mutex::new(HashMap::new()),
        }
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
            let mut next_sleep =
                self.token_manager.config().balance_refresh_interval_secs.max(
                    crate::model::config::MIN_BALANCE_REFRESH_INTERVAL_SECS,
                );
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
                    let exhausted = remaining < LOW_BALANCE_THRESHOLD
                        && overage_rem < LOW_BALANCE_THRESHOLD;
                    if exhausted {
                        self.token_manager.mark_insufficient_balance(id);
                        stats.low_balance_disabled += 1;
                        tracing::warn!(
                            "凭据 #{} 额度耗尽（正式 {:.2}, 超额 remaining={:.2}），已自动禁用",
                            id,
                            remaining,
                            overage_rem
                        );
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
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
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
                let balance = disk_cache.get(&entry.id).map(|cached| RuntimeBalanceSnapshot {
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
        let (active_ids, skipped_ids): (Vec<u64>, Vec<u64>) = ids
            .into_iter()
            .partition(|id| !disabled_ids.contains(id));

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
    pub async fn force_refresh_balances_batch(
        &self,
        ids: Vec<u64>,
    ) -> BatchRefreshBalanceResponse {
        // 源头过滤：禁用的凭据直接跳过查询，不占用并发槽位
        let snapshot = self.token_manager.snapshot();
        let disabled_ids: HashSet<u64> = snapshot
            .entries
            .iter()
            .filter(|e| e.disabled)
            .map(|e| e.id)
            .collect();
        let (active_ids, skipped_ids): (Vec<u64>, Vec<u64>) = ids
            .into_iter()
            .partition(|id| !disabled_ids.contains(id));

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
                self.token_manager
                    .update_balance_cache_full(item.id, balance.remaining, overage_remaining(balance));
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

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = crate::common::io::atomic_write_string(path, &json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
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
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据") {
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
    pub fn set_endpoint(
        &self,
        id: u64,
        endpoint: Option<String>,
    ) -> Result<(), AdminServiceError> {
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
            has_credentials: config.proxy_username.is_some()
                && config.proxy_password.is_some(),
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
                    if let (Some(u), Some(p)) =
                        (&config.proxy_username, &config.proxy_password)
                    {
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
            truncation_recovery_system_notice: config.truncation_recovery_system_notice,
            privacy_mode: config.privacy_mode,
            compression: CompressionConfigResponse {
                enabled: c.enabled,
                whitespace_compression: c.whitespace_compression,
                thinking_strategy: c.thinking_strategy.clone(),
                tool_result_max_chars: c.tool_result_max_chars,
                tool_result_head_lines: c.tool_result_head_lines,
                tool_result_tail_lines: c.tool_result_tail_lines,
                tool_use_input_max_chars: c.tool_use_input_max_chars,
                tool_description_max_chars: c.tool_description_max_chars,
                max_history_turns: c.max_history_turns,
                max_history_chars: c.max_history_chars,
                image_max_long_edge: c.image_max_long_edge,
                image_max_pixels_single: c.image_max_pixels_single,
                image_max_pixels_multi: c.image_max_pixels_multi,
                image_multi_threshold: c.image_multi_threshold,
                image_compression_enabled: c.image_compression_enabled,
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

            if let Some(v) = req.truncation_recovery_system_notice {
                cfg.truncation_recovery_system_notice = v;
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

        // 截断恢复识别开关：直接同步到共享 atomic（converter 下次调用即生效）
        if let Some(v) = req.truncation_recovery_system_notice {
            self.truncation_recovery_notice
                .store(v, std::sync::atomic::Ordering::Relaxed);
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
                if let Err(e) =
                    provider.update_default_endpoint(config.default_endpoint.clone())
                {
                    tracing::warn!(
                        "provider.update_default_endpoint 失败（已持久化）: {}",
                        e
                    );
                }
            }
        }

        // 热更新 Prompt Cache 运行时配置
        if req.prompt_cache_ttl_seconds.is_some() || req.prompt_cache_accounting_enabled.is_some()
        {
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

        self.token_manager
            .with_config_mut(|cfg| {
                if let Some(v) = req.enabled {
                    cfg.system_prompt_enabled = v;
                }
                if let Some(p) = position {
                    cfg.system_prompt_position = p;
                }
                if let Some(c) = req.custom_content.clone() {
                    let trimmed = c.trim();
                    cfg.system_prompt = if trimmed.is_empty() {
                        None
                    } else {
                        Some(c)
                    };
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
                rt.custom_content = if trimmed.is_empty() {
                    None
                } else {
                    Some(c)
                };
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
    pub fn delete_user_preset(
        &self,
        id: &str,
    ) -> Result<SystemPromptResponse, AdminServiceError> {
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
    /// 兼容 BK 11 字段 + xkiro 独有 5 字段（image_*  + max_request_body_bytes）。
    fn apply_compression_fields(
        target: &mut CompressionConfig,
        src: &UpdateCompressionConfigRequest,
    ) {
        if let Some(v) = src.enabled {
            target.enabled = v;
        }
        if let Some(v) = src.whitespace_compression {
            target.whitespace_compression = v;
        }
        if let Some(ref v) = src.thinking_strategy {
            target.thinking_strategy = v.clone();
        }
        if let Some(v) = src.tool_result_max_chars {
            target.tool_result_max_chars = v;
        }
        if let Some(v) = src.tool_result_head_lines {
            target.tool_result_head_lines = v;
        }
        if let Some(v) = src.tool_result_tail_lines {
            target.tool_result_tail_lines = v;
        }
        if let Some(v) = src.tool_use_input_max_chars {
            target.tool_use_input_max_chars = v;
        }
        if let Some(v) = src.tool_description_max_chars {
            target.tool_description_max_chars = v;
        }
        if let Some(v) = src.max_history_turns {
            target.max_history_turns = v;
        }
        if let Some(v) = src.max_history_chars {
            target.max_history_chars = v;
        }
        // xkiro 独有 5 字段
        if let Some(v) = src.image_max_long_edge {
            target.image_max_long_edge = v;
        }
        if let Some(v) = src.image_max_pixels_single {
            target.image_max_pixels_single = v;
        }
        if let Some(v) = src.image_max_pixels_multi {
            target.image_max_pixels_multi = v;
        }
        if let Some(v) = src.image_multi_threshold {
            target.image_multi_threshold = v;
        }
        if let Some(v) = src.image_compression_enabled {
            target.image_compression_enabled = v;
        }
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
                let provider = match auth_method.as_str() {
                    "idc" => "BuilderId".to_string(),
                    _ => "Social".to_string(),
                };
                Some(ExportTokenJsonItem {
                    provider,
                    refresh_token,
                    client_id: c.client_id,
                    client_secret: c.client_secret,
                    auth_method,
                    priority: c.priority,
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
        let creds = self
            .token_manager
            .export_credentials_with_state_by_ids(ids);
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
                let provider = if is_idc {
                    if c.client_secret
                        .as_deref()
                        .map(|s| s.contains("awsapps.com") || s.contains("initiateLoginUri"))
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
                };
                let id_str = c
                    .id
                    .map(|n| format!("xkiro-{}", n))
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let label = c
                    .email
                    .clone()
                    .unwrap_or_else(|| match c.id {
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
                    user_id: c.email.clone(),
                    auth_method: Some(auth_method),
                    client_id: c.client_id,
                    client_secret: c.client_secret,
                    region: c.region,
                    start_url: None,
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
            client_id: item.client_id,
            client_secret: item.client_secret,
            priority: item.priority,
            region,
            auth_region: None,
            api_region,
            machine_id: item.machine_id,
            endpoint: None,
            email: None,
            subscription_title: None,
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
