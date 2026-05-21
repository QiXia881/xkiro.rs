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
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::CompressionConfig;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, CachedBalanceItem,
    CachedBalancesResponse, CompressionConfigResponse, CredentialStatusItem,
    CredentialsStatusResponse, GlobalConfigResponse, ImportAction, ImportItemResult,
    ImportSummary, ImportTokenJsonRequest, ImportTokenJsonResponse, LoadBalancingModeResponse,
    ProxyConfigResponse, SetLoadBalancingModeRequest, TokenJsonItem,
    UpdateCompressionConfigRequest, UpdateGlobalConfigRequest, UpdateProxyConfigRequest,
};
use crate::kiro::token_manager::CachedBalanceInfo;

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

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
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        kiro_provider: Option<Arc<KiroProvider>>,
        compression_config: Arc<RwLock<CompressionConfig>>,
        prompt_cache_runtime: Arc<RwLock<PromptCacheRuntime>>,
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
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
        }
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
                is_current: entry.id == snapshot.current_id,
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
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
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

        // 更新缓存
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

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
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
        })
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

        // 从 AdminService 磁盘缓存获取完整余额信息
        let disk_cache = self.balance_cache.lock();

        let balances = runtime_balances
            .into_iter()
            .map(|(id, info)| {
                // 优先从磁盘缓存获取完整快照（保证字段一致性）
                if let Some(cached) = disk_cache.get(&id) {
                    CachedBalanceItem {
                        id,
                        remaining: cached.data.remaining,
                        usage_limit: cached.data.usage_limit,
                        usage_percentage: cached.data.usage_percentage,
                        subscription_title: cached.data.subscription_title.clone(),
                        cached_at: info.cached_at,
                        ttl_secs: info.ttl_secs,
                    }
                } else {
                    CachedBalanceItem {
                        id,
                        remaining: info.remaining,
                        usage_limit: 0.0,
                        usage_percentage: 0.0,
                        subscription_title: None,
                        cached_at: info.cached_at,
                        ttl_secs: info.ttl_secs,
                    }
                }
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

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
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
                if let Err(e) = std::fs::write(path, json) {
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
                max_request_body_bytes: c.max_request_body_bytes,
            },
        }
    }

    /// 更新全局配置（热更新）
    pub async fn update_global_config(
        &self,
        req: UpdateGlobalConfigRequest,
    ) -> Result<(), AdminServiceError> {
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

            cfg.save()
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))
        })?;

        // 2. 持久化成功后再应用运行时变更
        let config = self.token_manager.config();

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

        Ok(())
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
        if let Some(v) = src.max_request_body_bytes {
            target.max_request_body_bytes = v;
        }
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
