//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::{Client, StatusCode};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::sleep;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TlsBackend;
use parking_lot::{Mutex, RwLock};

/// API 调用结果
///
/// `_credential_permit` / `_global_permit` 持有到上游响应消费完成（一次"上游来回"）：
/// - 流式：handler 在 SSE unfold 中读到上游 `body_stream` 返回 `None`/`Err` 即立即 drop。
/// - 非流式：`response.bytes().await` 完成即随 ApiCallResult 一起 drop。
///
/// 不绑客户端消费速度，避免慢客户端把凭据并发位长期占住。
pub struct ApiCallResult {
    pub response: reqwest::Response,
    pub credential_id: u64,
    pub _credential_permit: Option<OwnedSemaphorePermit>,
    pub _global_permit: Option<OwnedSemaphorePermit>,
}

/// MCP 调用结果
///
/// permit 语义同 [`ApiCallResult`]：随上游响应消费完成立即释放。
pub struct McpCallResult {
    pub response: reqwest::Response,
    pub credential_id: u64,
    pub _credential_permit: Option<OwnedSemaphorePermit>,
    pub _global_permit: Option<OwnedSemaphorePermit>,
}

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 单次 endpoint 尝试的错误分类
///
/// 用于区分瞬态错误（可重试/可 fallback）和致命错误（需切换凭据）。
enum EndpointError {
    /// 瞬态错误：可重试，也可 fallback 到备选 endpoint
    Transient(anyhow::Error),
    /// 致命错误（400/402/凭据问题）：不重试，需切换凭据
    Fatal(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialFailureAction {
    Generic,
    AuthenticationFailed,
    AccountSuspended,
    SoftCooldown,
}

/// 总重试次数硬上限（避免无限重试）
const MAX_TOTAL_RETRIES: usize = 9;

const STREAM_API_TIMEOUT_SECS: u64 = 5 * 60;
const REST_API_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ClientKind {
    Stream,
    Rest,
}

fn api_timeout_secs(kind: ClientKind) -> u64 {
    match kind {
        ClientKind::Stream => STREAM_API_TIMEOUT_SECS,
        ClientKind::Rest => REST_API_TIMEOUT_SECS,
    }
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    ///
    /// 使用 RwLock 包裹以支持运行时热更新（贴合 BK provider.rs:108）
    global_proxy: RwLock<Option<ProxyConfig>>,
    /// Client 缓存：key = (effective proxy config, request kind)
    /// 不同代理和 REST/stream timeout 需要独立 Client。
    client_cache: Mutex<HashMap<(Option<ProxyConfig>, ClientKind), Client>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    ///
    /// 使用 RwLock 包裹以支持运行时热更新（贴合 BK provider.rs:111）
    default_endpoint: RwLock<String>,
    /// 是否在首选 endpoint 瞬态失败时尝试其它 endpoint
    endpoint_fallback: RwLock<bool>,
}

impl KiroProvider {
    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client
        let mut cache = HashMap::new();
        for kind in [ClientKind::Rest, ClientKind::Stream] {
            let client = build_client(proxy.as_ref(), api_timeout_secs(kind), tls_backend)
                .expect("创建 HTTP 客户端失败");
            cache.insert((proxy.clone(), kind), client);
        }

        let endpoint_fallback = token_manager.config().endpoint_fallback;

        Self {
            token_manager,
            global_proxy: RwLock::new(proxy),
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint: RwLock::new(default_endpoint),
            endpoint_fallback: RwLock::new(endpoint_fallback),
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(
        &self,
        credentials: &KiroCredentials,
        kind: ClientKind,
    ) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.read().as_ref());
        let mut cache = self.client_cache.lock();
        let key = (effective, kind);
        if let Some(client) = cache.get(&key) {
            return Ok(client.clone());
        }
        let client = build_client(key.0.as_ref(), api_timeout_secs(kind), self.tls_backend)?;
        cache.insert(key, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let default = self.default_endpoint.read();
        let name = credentials.endpoint.as_deref().unwrap_or(default.as_str());
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 获取当前 endpoint 名称
    fn endpoint_name_for(&self, credentials: &KiroCredentials) -> String {
        let default = self.default_endpoint.read();
        credentials
            .endpoint
            .clone()
            .unwrap_or_else(|| default.clone())
    }

    /// 获取备选 endpoint 列表（排除当前 endpoint，按固定顺序）
    ///
    /// 对齐 Kiro-Go `getSortedEndpoints()` 的 fallback 逻辑：
    /// 固定顺序: ide → codewhisperer → cli，排除当前 endpoint。
    fn alternative_endpoints(&self, current_name: &str) -> Vec<Arc<dyn KiroEndpoint>> {
        // 对齐 Kiro-Go: 固定 endpoint 优先级顺序
        const ENDPOINT_ORDER: &[&str] = &["ide", "codewhisperer", "cli"];
        let mut result = Vec::new();
        for name in ENDPOINT_ORDER {
            if *name != current_name {
                if let Some(ep) = self.endpoints.get(*name) {
                    result.push(ep.clone());
                }
            }
        }
        // 补充不在固定顺序中的 endpoint（以防注册了自定义 endpoint）
        for (name, ep) in self.endpoints.iter() {
            if name.as_str() != current_name && !ENDPOINT_ORDER.contains(&name.as_str()) {
                result.push(ep.clone());
            }
        }
        result
    }

    /// 热更新全局代理配置
    ///
    /// 贴合 BK provider.rs:118-128：替换 global_proxy 后清空 client_cache，
    /// 让下次 [`Self::client_for`] 调用按需重建凭据级 Client。
    pub fn update_global_proxy(&self, proxy: Option<ProxyConfig>) -> anyhow::Result<()> {
        // 提前验证新代理配置是否能成功构建 Client，避免清空缓存后下次请求失败
        let rest_client = build_client(
            proxy.as_ref(),
            api_timeout_secs(ClientKind::Rest),
            self.tls_backend,
        )?;
        let stream_client = build_client(
            proxy.as_ref(),
            api_timeout_secs(ClientKind::Stream),
            self.tls_backend,
        )?;

        *self.global_proxy.write() = proxy.clone();
        // 清空缓存并预热全局代理对应的 Client（保留 xkiro 原 with_proxy 的预热语义）
        let mut cache = self.client_cache.lock();
        cache.clear();
        cache.insert((proxy.clone(), ClientKind::Rest), rest_client);
        cache.insert((proxy, ClientKind::Stream), stream_client);

        tracing::info!("全局代理配置已热更新，client_cache 已重建");
        Ok(())
    }

    /// 热更新默认 endpoint 名称
    ///
    /// 贴合 BK provider.rs:131-138：仅当目标端点已在注册表中时才生效。
    pub fn update_default_endpoint(&self, default_endpoint: String) -> anyhow::Result<()> {
        if !self.endpoints.contains_key(&default_endpoint) {
            return Err(anyhow::anyhow!("未知端点: {}", default_endpoint));
        }
        *self.default_endpoint.write() = default_endpoint;
        tracing::info!("默认 endpoint 已热更新");
        Ok(())
    }

    pub fn update_endpoint_fallback(&self, endpoint_fallback: bool) {
        *self.endpoint_fallback.write() = endpoint_fallback;
        tracing::info!("endpoint fallback 已热更新: {}", endpoint_fallback);
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）
    pub async fn call_api(
        &self,
        request_body: &str,
        user_id: Option<&str>,
    ) -> anyhow::Result<ApiCallResult> {
        self.call_api_with_retry(request_body, false, user_id).await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        user_id: Option<&str>,
    ) -> anyhow::Result<ApiCallResult> {
        self.call_api_with_retry(request_body, true, user_id).await
    }

    /// 获取内部 `MultiTokenManager` 引用（用于在请求生命周期外同步运行时缓存，
    /// 例如 metering 透传后 `apply_credit_usage`）
    pub fn token_manager(&self) -> &Arc<MultiTokenManager> {
        &self.token_manager
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(
        &self,
        request_body: &str,
        user_id: Option<&str>,
    ) -> anyhow::Result<McpCallResult> {
        self.call_mcp_with_retry(request_body, user_id).await
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(
        &self,
        request_body: &str,
        user_id: Option<&str>,
    ) -> anyhow::Result<McpCallResult> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();

        for attempt in 0..max_retries {
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let mut ctx = match self
                .token_manager
                .acquire_context_for_session(user_id, None)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config: &config,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx)?;

            let base = self
                .client_for(&ctx.credentials, ClientKind::Rest)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json")
                .header("Connection", "close");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                return Ok(McpCallResult {
                    response,
                    credential_id: ctx.id,
                    _credential_permit: ctx._credential_permit.take(),
                    _global_permit: ctx._global_permit.take(),
                });
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                if Self::is_input_too_long(&body) {
                    tracing::error!(
                        status = %status,
                        response_body_bytes = body.len(),
                        request_url = %url,
                        request_body_bytes = request_body.len(),
                        "MCP 400 Bad Request - 输入上下文过长"
                    );
                }
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
            {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                // 检测 MODEL_TEMPORARILY_UNAVAILABLE 并触发全局熔断
                if Self::is_model_temporarily_unavailable(&body)
                    && self.token_manager.report_model_unavailable()
                {
                    anyhow::bail!(
                        "MCP 请求失败（模型暂时不可用，已触发熔断）: {} {}",
                        status,
                        body
                    );
                }
                if status == StatusCode::TOO_MANY_REQUESTS {
                    self.token_manager.report_rate_limited(ctx.id);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    /// - 瞬态错误时按配置尝试备选 endpoint（对齐 Kiro-Go 多端点 fallback）
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        user_id: Option<&str>,
    ) -> anyhow::Result<ApiCallResult> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let mut excluded_credentials: HashSet<u64> = HashSet::new();
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 尝试从请求体中提取模型信息
        let model = Self::extract_model_from_request(request_body);

        for attempt in 0..max_retries {
            // 获取调用上下文（绑定 index、credentials、token）
            let mut ctx = match self
                .token_manager
                .acquire_context_for_session_excluding(
                    user_id,
                    model.as_deref(),
                    &excluded_credentials,
                )
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    excluded_credentials.insert(ctx.id);
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            // 构建备选 endpoint 列表（对齐 Kiro-Go 多端点 fallback）
            let current_ep_name = self.endpoint_name_for(&ctx.credentials);
            let alt_endpoints = self.alternative_endpoints(&current_ep_name);

            // 尝试主 endpoint
            let result = self
                .try_single_endpoint(
                    &endpoint,
                    &mut ctx,
                    request_body,
                    &config,
                    &machine_id,
                    api_type,
                    if is_stream {
                        ClientKind::Stream
                    } else {
                        ClientKind::Rest
                    },
                )
                .await;

            match result {
                Ok(api_result) => {
                    self.token_manager.report_success(ctx.id);
                    self.spawn_balance_refresh(ctx.id);
                    return Ok(api_result);
                }
                Err(EndpointError::Fatal(e)) => {
                    // 致命错误（400/402/凭据问题）：不尝试备选 endpoint
                    let action = Self::credential_failure_action(&e);
                    last_error = Some(e);
                    match action {
                        CredentialFailureAction::AccountSuspended => {
                            excluded_credentials.insert(ctx.id);
                            self.token_manager.mark_account_suspended(ctx.id);
                            continue;
                        }
                        CredentialFailureAction::SoftCooldown => {
                            excluded_credentials.insert(ctx.id);
                            self.token_manager.report_rate_limited(ctx.id);
                            continue;
                        }
                        CredentialFailureAction::AuthenticationFailed
                        | CredentialFailureAction::Generic => {}
                    }
                    // 401/403 尝试 force-refresh
                    if action == CredentialFailureAction::AuthenticationFailed
                        && !force_refreshed.contains(&ctx.id)
                    {
                        force_refreshed.insert(ctx.id);
                        if self
                            .token_manager
                            .force_refresh_token_for(ctx.id)
                            .await
                            .is_ok()
                        {
                            continue;
                        }
                    }
                    excluded_credentials.insert(ctx.id);
                    if action == CredentialFailureAction::AuthenticationFailed {
                        self.token_manager.mark_authentication_failed(ctx.id);
                    } else {
                        self.token_manager.report_failure(ctx.id);
                    }
                    continue;
                }
                Err(EndpointError::Transient(e)) => {
                    // 瞬态错误：尝试备选 endpoint（对齐 Kiro-Go fallback 逻辑）
                    last_error = Some(e);
                    let mut tried_alt = false;
                    let fallback_enabled = *self.endpoint_fallback.read();
                    for alt_ep in alt_endpoints.iter().filter(|_| fallback_enabled) {
                        tracing::info!(
                            "主 endpoint {} 瞬态失败，尝试备选 endpoint {}",
                            current_ep_name,
                            alt_ep.name()
                        );
                        let alt_result = self
                            .try_single_endpoint(
                                alt_ep,
                                &mut ctx,
                                request_body,
                                &config,
                                &machine_id,
                                api_type,
                                if is_stream {
                                    ClientKind::Stream
                                } else {
                                    ClientKind::Rest
                                },
                            )
                            .await;
                        match alt_result {
                            Ok(api_result) => {
                                self.token_manager.report_success(ctx.id);
                                self.spawn_balance_refresh(ctx.id);
                                return Ok(api_result);
                            }
                            Err(EndpointError::Transient(e)) => {
                                // 备选也失败，继续尝试下一个
                                last_error = Some(e);
                                tried_alt = true;
                                continue;
                            }
                            Err(EndpointError::Fatal(_)) => {
                                // 致命错误，停止尝试备选
                                break;
                            }
                        }
                    }
                    if tried_alt {
                        tracing::warn!(
                            "所有备选 endpoint 也失败，回退到凭据切换（尝试 {}/{}）",
                            attempt + 1,
                            max_retries
                        );
                    }
                    if let Some(error) = &last_error
                        && Self::is_rate_limited_error(error)
                    {
                        self.token_manager.report_rate_limited(ctx.id);
                    }
                    excluded_credentials.insert(ctx.id);
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 尝试单个 endpoint 的请求
    ///
    /// 返回 Ok(ApiCallResult) 表示成功，
    /// Err(EndpointError::Transient) 表示可重试的上游瞬态错误，
    /// Err(EndpointError::Fatal) 表示不可重试的致命错误（400/402/凭据问题）。
    async fn try_single_endpoint(
        &self,
        endpoint: &Arc<dyn KiroEndpoint>,
        ctx: &mut crate::kiro::token_manager::CallContext,
        request_body: &str,
        config: &crate::model::config::Config,
        machine_id: &str,
        api_type: &str,
        client_kind: ClientKind,
    ) -> Result<ApiCallResult, EndpointError> {
        let rctx = RequestContext {
            credentials: &ctx.credentials,
            token: &ctx.token,
            machine_id,
            config,
        };

        let url = endpoint.api_url(&rctx);
        let body = endpoint
            .transform_api_body(request_body, &rctx)
            .map_err(EndpointError::Fatal)?;

        let base = self
            .client_for(&ctx.credentials, client_kind)
            .map_err(EndpointError::Fatal)?
            .post(&url)
            .body(body)
            .header("content-type", "application/json")
            .header("Connection", "close");
        let request = endpoint.decorate_api(base, &rctx);

        let response = request.send().await.map_err(|e| {
            // 网络错误视为瞬态
            EndpointError::Transient(e.into())
        })?;

        let status = response.status();

        // 成功响应
        if status.is_success() {
            return Ok(ApiCallResult {
                response,
                credential_id: ctx.id,
                _credential_permit: ctx._credential_permit.take(),
                _global_permit: ctx._global_permit.take(),
            });
        }

        // 失败响应
        let body = response.text().await.unwrap_or_default();

        // 402 额度用尽：致命错误
        if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
            tracing::warn!("API 请求失败（额度已用尽）: {} {}", status, body);
            if !self.token_manager.report_quota_exhausted(ctx.id) {
                return Err(EndpointError::Fatal(anyhow::anyhow!(
                    "{} API 请求失败（所有凭据已用尽）: {} {}",
                    api_type,
                    status,
                    body
                )));
            }
            return Err(EndpointError::Fatal(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            )));
        }

        // 400 Bad Request：致命错误
        if status.as_u16() == 400 {
            return Err(EndpointError::Fatal(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            )));
        }

        // 401/403 凭据问题：致命错误
        // 对齐 Kiro-Go: 细粒度分类以便日志和监控
        if matches!(status.as_u16(), 401 | 403) {
            let error_kind = if Self::is_suspension_error(&body) {
                "账户暂停"
            } else if Self::is_profile_unavailable_error(&body) {
                "Profile 不可用"
            } else {
                "凭据错误"
            };
            tracing::warn!(
                "API 请求失败（{}，{} {}）: {}",
                error_kind,
                status,
                body.len(),
                &body[..body.len().min(200)]
            );
            return Err(EndpointError::Fatal(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            )));
        }

        // 402 非 MONTHLY_REQUEST_COUNT 的 overage 错误
        if status.as_u16() == 402 {
            tracing::warn!(
                "API 请求失败（402 overage）: {}",
                &body[..body.len().min(200)]
            );
            return Err(EndpointError::Fatal(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            )));
        }

        // 上游瞬态错误（可重试 + 可 fallback 到备选 endpoint）
        if status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
        {
            tracing::warn!("API 请求失败（上游瞬态错误）: {} {}", status, body);
            // 检测 MODEL_TEMPORARILY_UNAVAILABLE
            if Self::is_model_temporarily_unavailable(&body)
                && self.token_manager.report_model_unavailable()
            {
                return Err(EndpointError::Fatal(anyhow::anyhow!(
                    "{} API 请求失败（模型暂时不可用，已触发熔断）: {} {}",
                    api_type,
                    status,
                    body
                )));
            }
            return Err(EndpointError::Transient(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            )));
        }

        // 其他错误：当作瞬态
        Err(EndpointError::Transient(anyhow::anyhow!(
            "{} API 请求失败: {} {}",
            api_type,
            status,
            body
        )))
    }

    fn is_rate_limited_error(error: &anyhow::Error) -> bool {
        let msg = error.to_string().to_lowercase();
        msg.contains(" 429 ") || msg.contains("429 too many requests")
    }

    fn credential_failure_action(error: &anyhow::Error) -> CredentialFailureAction {
        let msg = error.to_string();
        if Self::is_suspension_error(&msg) {
            CredentialFailureAction::AccountSuspended
        } else if Self::is_profile_unavailable_error(&msg) {
            CredentialFailureAction::SoftCooldown
        } else if Self::is_auth_error(&msg) {
            CredentialFailureAction::AuthenticationFailed
        } else {
            CredentialFailureAction::Generic
        }
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// 检测响应体是否表示「模型暂时不可用」
    ///
    /// 对齐 BK：识别 `MODEL_TEMPORARILY_UNAVAILABLE` 字符串、顶层 `reason` 字段，
    /// 以及 `error.reason` 嵌套字段。命中后由调用方决定是否触发全局熔断。
    fn is_model_temporarily_unavailable(body: &str) -> bool {
        if body.contains("MODEL_TEMPORARILY_UNAVAILABLE") {
            return true;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return false;
        };

        if value
            .get("reason")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v == "MODEL_TEMPORARILY_UNAVAILABLE")
        {
            return true;
        }

        value
            .pointer("/error/reason")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v == "MODEL_TEMPORARILY_UNAVAILABLE")
    }

    /// 检测响应体是否表示「输入过长」
    ///
    /// 对齐 BK：典型返回
    /// `{"message":"Input is too long.","reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}`
    fn is_input_too_long(body: &str) -> bool {
        body.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") || body.contains("Input is too long")
    }

    /// 检测响应体是否表示「账户暂停」
    ///
    /// 对齐 Kiro-Go `isSuspensionErrorMessage()`: 大小写不敏感
    /// 匹配 "temporarily_suspended" / "temporarily is suspended" / "account suspended"
    fn is_suspension_error(body: &str) -> bool {
        let lower = body.to_lowercase();
        lower.contains("temporarily_suspended")
            || lower.contains("temporarily is suspended")
            || lower.contains("account suspended")
    }

    /// 检测响应体是否表示「Profile 不可用」
    ///
    /// 对齐 Kiro-Go `isProfileUnavailableErrorMessage()`: 大小写不敏感
    /// 匹配 "no available kiro profile"
    fn is_profile_unavailable_error(body: &str) -> bool {
        let lower = body.to_lowercase();
        lower.contains("no available kiro profile")
    }

    /// 检测响应体是否表示「认证错误」
    ///
    /// 对齐 Kiro-Go `isAuthErrorMessage()`: 大小写不敏感，10 种模式
    fn is_auth_error(body: &str) -> bool {
        let lower = body.to_lowercase();
        lower.contains("http 401")
            || lower.contains("http 403")
            || lower.contains("unauthorized")
            || lower.contains("forbidden")
            || lower.contains("authentication failed")
            || lower.contains("token invalid")
            || lower.contains("token expired")
            || lower.contains("invalid_grant")
            || lower.contains("access token expired")
            || lower.contains("refresh token expired")
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 后台异步刷新余额缓存（如果需要）
    ///
    /// 贴合 BK provider.rs:190-212：成功调用 API 后触发，仅在 TTL 到期时才发起
    /// `getUsageLimits` 请求，避免每次 API 调用都阻塞在余额查询上。
    ///
    /// 余额低于 1.0 时主动调用 `mark_insufficient_balance` 禁用凭据，确保
    /// admin UI 余额显示与故障转移逻辑同步。
    fn spawn_balance_refresh(&self, id: u64) {
        // 检查缓存是否需要刷新（TTL 5 分钟，xkiro 独有保留）
        if !self.token_manager.should_refresh_balance(id) {
            return;
        }
        let tm = Arc::clone(&self.token_manager);
        tokio::spawn(async move {
            match tm.get_usage_limits_for(id).await {
                Ok(resp) => {
                    let usage_limit = resp.usage_limit();
                    let current_usage = resp.current_usage();
                    let remaining = (usage_limit - current_usage).max(0.0);
                    // 真正不可用 = 正式额度耗尽 AND（超额未开启 OR 超额额度耗尽）
                    let overage_enabled = resp.overage_status() == Some("ENABLED");
                    let overage_used = (current_usage - usage_limit).max(0.0);
                    let overage_remaining = if overage_enabled {
                        (resp.overage_cap() - overage_used).max(0.0)
                    } else {
                        0.0
                    };
                    tm.update_balance_cache_full(id, remaining, overage_remaining);
                    let exhausted = remaining < 1.0 && overage_remaining < 1.0;
                    tracing::debug!(
                        "凭据 #{} 余额缓存已刷新: 正式 {:.2}, 超额 enabled={} remaining={:.2}",
                        id,
                        remaining,
                        overage_enabled,
                        overage_remaining
                    );
                    if exhausted {
                        if tm.mark_insufficient_balance(id) {
                            tracing::warn!(
                                "凭据 #{} 额度耗尽（正式 {:.2}, 超额 enabled={} remaining={:.2}），已主动禁用",
                                id,
                                remaining,
                                overage_enabled,
                                overage_remaining
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("凭据 #{} 余额刷新失败: {}", id, e);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientKind, KiroProvider, api_timeout_secs};
    use crate::kiro::endpoint::{
        KiroEndpoint, PreferenceRequestParts, RequestContext, UsageRequestParts,
    };
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::kiro::token_manager::MultiTokenManager;
    use crate::model::config::Config;
    use chrono::{Duration as ChronoDuration, Utc};
    use reqwest::RequestBuilder;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::thread;

    struct TestEndpoint {
        url: String,
    }

    impl KiroEndpoint for TestEndpoint {
        fn name(&self) -> &'static str {
            "test"
        }

        fn api_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }

        fn mcp_url(&self, _ctx: &RequestContext<'_>) -> String {
            self.url.clone()
        }

        fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
            req.header("authorization", format!("Bearer {}", ctx.token))
        }

        fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
            self.decorate_api(req, ctx)
        }

        fn transform_api_body(
            &self,
            body: &str,
            _ctx: &RequestContext<'_>,
        ) -> anyhow::Result<String> {
            Ok(body.to_string())
        }

        fn usage_request_parts(
            &self,
            _ctx: &RequestContext<'_>,
            _need_email: bool,
        ) -> anyhow::Result<UsageRequestParts> {
            Ok(UsageRequestParts {
                url: self.url.clone(),
                headers: Vec::new(),
            })
        }

        fn set_preference_request_parts(
            &self,
            _ctx: &RequestContext<'_>,
            _overage_status: &str,
        ) -> anyhow::Result<PreferenceRequestParts> {
            Ok(PreferenceRequestParts {
                url: self.url.clone(),
                headers: Vec::new(),
                body: "{}".to_string(),
            })
        }
    }

    fn valid_credential(token: &str, priority: u32) -> KiroCredentials {
        let mut credential = KiroCredentials::default();
        credential.access_token = Some(token.to_string());
        credential.refresh_token = Some(format!("refresh-{}-{}", token, "x".repeat(120)));
        credential.expires_at = Some((Utc::now() + ChronoDuration::hours(1)).to_rfc3339());
        credential.priority = priority;
        credential
    }

    #[test]
    fn api_timeouts_match_kiro_go_stream_and_rest_clients() {
        assert_eq!(api_timeout_secs(ClientKind::Stream), 5 * 60);
        assert_eq!(api_timeout_secs(ClientKind::Rest), 30);
    }

    #[test]
    fn account_failure_classifiers_match_kiro_go_non_rate_limit_cases() {
        assert!(KiroProvider::is_suspension_error(
            "Your User ID temporarily is suspended"
        ));
        assert!(KiroProvider::is_profile_unavailable_error(
            "no available Kiro profile"
        ));
        assert!(KiroProvider::is_auth_error(
            "Authentication failed - token invalid or expired"
        ));
        assert_eq!(
            KiroProvider::credential_failure_action(&anyhow::anyhow!(
                "HTTP 403: temporarily_suspended"
            )),
            super::CredentialFailureAction::AccountSuspended
        );
        assert_eq!(
            KiroProvider::credential_failure_action(&anyhow::anyhow!(
                "HTTP 403: no available Kiro profile"
            )),
            super::CredentialFailureAction::SoftCooldown
        );
        assert_eq!(
            KiroProvider::credential_failure_action(&anyhow::anyhow!("HTTP 401: unauthorized")),
            super::CredentialFailureAction::AuthenticationFailed
        );
    }

    #[tokio::test]
    async fn non_stream_retries_next_credential_after_pre_response_failure_like_kiro_go() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 2048];
                let n = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..n]).to_string();
                let auth = request
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("authorization: ")
                            .or_else(|| line.strip_prefix("Authorization: "))
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                tx.send(auth).unwrap();

                if attempt == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 9\r\nConnection: close\r\n\r\ntransient",
                        )
                        .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
            }
        });

        let mut config = Config::default();
        config.default_endpoint = "test".to_string();
        config.endpoint_fallback = false;
        let token_manager = Arc::new(
            MultiTokenManager::new(
                config,
                vec![
                    valid_credential("token-first", 0),
                    valid_credential("token-second", 1),
                ],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        token_manager.update_balance_cache_full(1, 10.0, 0.0);
        token_manager.update_balance_cache_full(2, 10.0, 0.0);

        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert("test".to_string(), Arc::new(TestEndpoint { url }));
        let provider =
            KiroProvider::with_proxy(Arc::clone(&token_manager), None, endpoints, "test".into());

        let result = provider.call_api("{}", None).await.unwrap();
        assert_eq!(result.response.text().await.unwrap(), "ok");

        server.join().unwrap();
        let tokens: Vec<String> = rx.try_iter().collect();
        assert_eq!(
            tokens,
            vec![
                "Bearer token-first".to_string(),
                "Bearer token-second".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn account_suspension_disables_credential_and_retries_next_like_kiro_go() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 2048];
                let n = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..n]).to_string();
                let auth = request
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("authorization: ")
                            .or_else(|| line.strip_prefix("Authorization: "))
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                tx.send(auth).unwrap();

                if attempt == 0 {
                    let body = "temporarily_suspended";
                    write!(
                        stream,
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
            }
        });

        let mut config = Config::default();
        config.default_endpoint = "test".to_string();
        config.endpoint_fallback = false;
        let token_manager = Arc::new(
            MultiTokenManager::new(
                config,
                vec![
                    valid_credential("token-first", 0),
                    valid_credential("token-second", 1),
                ],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        token_manager.update_balance_cache_full(1, 10.0, 0.0);
        token_manager.update_balance_cache_full(2, 10.0, 0.0);

        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert("test".to_string(), Arc::new(TestEndpoint { url }));
        let provider =
            KiroProvider::with_proxy(Arc::clone(&token_manager), None, endpoints, "test".into());

        let result = provider.call_api("{}", None).await.unwrap();
        assert_eq!(result.response.text().await.unwrap(), "ok");

        server.join().unwrap();
        let tokens: Vec<String> = rx.try_iter().collect();
        assert_eq!(
            tokens,
            vec![
                "Bearer token-first".to_string(),
                "Bearer token-second".to_string()
            ]
        );
        let snapshot = token_manager.snapshot();
        let first = snapshot.entries.iter().find(|entry| entry.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(first.disabled_reason.as_deref(), Some("AccountSuspended"));
    }

    #[tokio::test]
    async fn profile_unavailable_soft_cools_down_without_disabling_like_kiro_go() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 2048];
                let n = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..n]).to_string();
                let auth = request
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("authorization: ")
                            .or_else(|| line.strip_prefix("Authorization: "))
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                tx.send(auth).unwrap();

                if attempt == 0 {
                    let body = "no available Kiro profile";
                    write!(
                        stream,
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
            }
        });

        let mut config = Config::default();
        config.default_endpoint = "test".to_string();
        config.endpoint_fallback = false;
        let token_manager = Arc::new(
            MultiTokenManager::new(
                config,
                vec![
                    valid_credential("token-first", 0),
                    valid_credential("token-second", 1),
                ],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        token_manager.update_balance_cache_full(1, 10.0, 0.0);
        token_manager.update_balance_cache_full(2, 10.0, 0.0);

        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert("test".to_string(), Arc::new(TestEndpoint { url }));
        let provider =
            KiroProvider::with_proxy(Arc::clone(&token_manager), None, endpoints, "test".into());

        let result = provider.call_api("{}", None).await.unwrap();
        assert_eq!(result.response.text().await.unwrap(), "ok");

        server.join().unwrap();
        let tokens: Vec<String> = rx.try_iter().collect();
        assert_eq!(
            tokens,
            vec![
                "Bearer token-first".to_string(),
                "Bearer token-second".to_string()
            ]
        );
        let snapshot = token_manager.snapshot();
        let first = snapshot.entries.iter().find(|entry| entry.id == 1).unwrap();
        assert!(!first.disabled);
        assert_eq!(first.disabled_reason, None);
    }

    #[tokio::test]
    async fn endpoint_fallback_disabled_does_not_special_case_quota_transient() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_url = format!("http://{}", primary_listener.local_addr().unwrap());
        let alt_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        alt_listener.set_nonblocking(true).unwrap();
        let alt_url = format!("http://{}", alt_listener.local_addr().unwrap());
        let alt_called = Arc::new(AtomicBool::new(false));
        let alt_called_thread = Arc::clone(&alt_called);

        let primary = thread::spawn(move || {
            let (mut stream, _) = primary_listener.accept().unwrap();
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 14\r\nConnection: close\r\n\r\nquota exceeded",
                )
                .unwrap();
        });
        let alt = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
            while std::time::Instant::now() < deadline {
                match alt_listener.accept() {
                    Ok((mut stream, _)) => {
                        alt_called_thread.store(true, Ordering::SeqCst);
                        let mut buffer = [0_u8; 2048];
                        let _ = stream.read(&mut buffer);
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        return;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });

        let mut config = Config::default();
        config.default_endpoint = "primary".to_string();
        config.endpoint_fallback = false;
        let token_manager = Arc::new(
            MultiTokenManager::new(
                config,
                vec![valid_credential("token-primary", 0)],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        token_manager.update_balance_cache_full(1, 10.0, 0.0);

        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "primary".to_string(),
            Arc::new(TestEndpoint { url: primary_url }),
        );
        endpoints.insert("alt".to_string(), Arc::new(TestEndpoint { url: alt_url }));
        let provider = KiroProvider::with_proxy(
            Arc::clone(&token_manager),
            None,
            endpoints,
            "primary".into(),
        );

        let result = provider.call_api("{}", None).await;

        primary.join().unwrap();
        alt.join().unwrap();
        assert!(result.is_err());
        assert!(!alt_called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn endpoint_fallback_enabled_retries_alt_after_429_like_kiro_go() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_url = format!("http://{}", primary_listener.local_addr().unwrap());
        let alt_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let alt_url = format!("http://{}", alt_listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();

        let primary = thread::spawn(move || {
            let (mut stream, _) = primary_listener.accept().unwrap();
            let mut buffer = [0_u8; 2048];
            let n = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..n]).to_string();
            tx.send(format!("primary:{request}")).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 8\r\nConnection: close\r\n\r\nthrottle",
                )
                .unwrap();
        });
        let (alt_tx, alt_rx) = mpsc::channel();
        let alt = thread::spawn(move || {
            let (mut stream, _) = alt_listener.accept().unwrap();
            let mut buffer = [0_u8; 2048];
            let n = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..n]).to_string();
            alt_tx.send(format!("alt:{request}")).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });

        let mut config = Config::default();
        config.default_endpoint = "primary".to_string();
        config.endpoint_fallback = true;
        let token_manager = Arc::new(
            MultiTokenManager::new(
                config,
                vec![valid_credential("token-primary", 0)],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        token_manager.update_balance_cache_full(1, 10.0, 0.0);

        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "primary".to_string(),
            Arc::new(TestEndpoint { url: primary_url }),
        );
        endpoints.insert("alt".to_string(), Arc::new(TestEndpoint { url: alt_url }));
        let provider = KiroProvider::with_proxy(
            Arc::clone(&token_manager),
            None,
            endpoints,
            "primary".into(),
        );

        let result = provider.call_api("{}", None).await.unwrap();

        primary.join().unwrap();
        alt.join().unwrap();
        assert_eq!(result.response.text().await.unwrap(), "ok");
        assert!(rx.recv().unwrap().contains("Bearer token-primary"));
        assert!(alt_rx.recv().unwrap().contains("Bearer token-primary"));
    }

    #[tokio::test]
    async fn endpoint_fallback_does_not_retry_alt_on_auth_failure_like_kiro_go() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_url = format!("http://{}", primary_listener.local_addr().unwrap());
        let alt_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        alt_listener.set_nonblocking(true).unwrap();
        let alt_url = format!("http://{}", alt_listener.local_addr().unwrap());
        let alt_called = Arc::new(AtomicBool::new(false));
        let alt_called_thread = Arc::clone(&alt_called);

        let primary = thread::spawn(move || {
            let (mut stream, _) = primary_listener.accept().unwrap();
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\nConnection: close\r\n\r\nunauthorized",
                )
                .unwrap();
        });
        let alt = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
            while std::time::Instant::now() < deadline {
                match alt_listener.accept() {
                    Ok((mut stream, _)) => {
                        alt_called_thread.store(true, Ordering::SeqCst);
                        let mut buffer = [0_u8; 2048];
                        let _ = stream.read(&mut buffer);
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        return;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });

        let mut config = Config::default();
        config.default_endpoint = "primary".to_string();
        config.endpoint_fallback = true;
        let token_manager = Arc::new(
            MultiTokenManager::new(
                config,
                vec![valid_credential("token-primary", 0)],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        token_manager.update_balance_cache_full(1, 10.0, 0.0);

        let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
        endpoints.insert(
            "primary".to_string(),
            Arc::new(TestEndpoint { url: primary_url }),
        );
        endpoints.insert("alt".to_string(), Arc::new(TestEndpoint { url: alt_url }));
        let provider = KiroProvider::with_proxy(
            Arc::clone(&token_manager),
            None,
            endpoints,
            "primary".into(),
        );

        let result = provider.call_api("{}", None).await;

        primary.join().unwrap();
        alt.join().unwrap();
        assert!(result.is_err());
        assert!(!alt_called.load(Ordering::SeqCst));
    }

    #[test]
    fn detects_model_temporarily_unavailable_substring() {
        let body =
            r#"{"message":"Improperly formed request.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#;
        assert!(KiroProvider::is_model_temporarily_unavailable(body));
    }

    #[test]
    fn detects_model_temporarily_unavailable_top_level() {
        let body = r#"{"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#;
        assert!(KiroProvider::is_model_temporarily_unavailable(body));
    }

    #[test]
    fn detects_model_temporarily_unavailable_nested_error() {
        let body = r#"{"error":{"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}}"#;
        assert!(KiroProvider::is_model_temporarily_unavailable(body));
    }

    #[test]
    fn does_not_match_other_reasons() {
        let body = r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#;
        assert!(!KiroProvider::is_model_temporarily_unavailable(body));
    }

    #[test]
    fn detects_input_too_long_by_reason() {
        let body =
            r#"{"message":"Input is too long.","reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#;
        assert!(KiroProvider::is_input_too_long(body));
    }

    #[test]
    fn detects_input_too_long_by_message() {
        let body = r#"{"message":"Input is too long for requested model"}"#;
        assert!(KiroProvider::is_input_too_long(body));
    }

    #[test]
    fn input_too_long_does_not_match_unrelated() {
        let body = r#"{"message":"unauthorized"}"#;
        assert!(!KiroProvider::is_input_too_long(body));
    }
}
