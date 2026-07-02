//! Anthropic API 中间件

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use parking_lot::{Mutex, RwLock};

use crate::common::auth;
use crate::kiro::provider::KiroProvider;
use crate::model::config::{CompressionConfig, PromptFilterConfig};
use crate::model::runtime::{PromptRuntimeConfig, SharedPromptConfig};

use super::cache_tracker::CacheTracker;
use super::types::ErrorResponse;

pub type SharedApiKeys = Arc<RwLock<Vec<crate::admin::types::ApiKeyEntry>>>;

#[derive(Clone, Debug)]
pub struct MatchedApiKeyId(pub Option<String>);

#[derive(Clone)]
pub(crate) struct PromptCacheSnapshot {
    pub accounting_enabled: bool,
    #[allow(dead_code)]
    pub ttl_seconds: u64,
    pub max_ratio: f64,
    pub tracker: Arc<CacheTracker>,
}

#[derive(Debug, Clone)]
pub struct ThinkingRuntimeConfig {
    pub suffix: String,
    pub openai_format: String,
    pub claude_format: String,
}

#[derive(Debug)]
pub struct GatewayStats {
    total_requests: AtomicI64,
    success_requests: AtomicI64,
    failed_requests: AtomicI64,
    total_tokens: AtomicI64,
    total_credits: Mutex<f64>,
    start_time: Instant,
}

#[derive(Debug, Clone, Copy)]
pub struct GatewayStatsSnapshot {
    pub total_requests: i64,
    pub success_requests: i64,
    pub failed_requests: i64,
    pub total_tokens: i64,
    pub total_credits: f64,
    pub uptime: u64,
}

impl GatewayStats {
    fn new() -> Self {
        Self {
            total_requests: AtomicI64::new(0),
            success_requests: AtomicI64::new(0),
            failed_requests: AtomicI64::new(0),
            total_tokens: AtomicI64::new(0),
            total_credits: Mutex::new(0.0),
            start_time: Instant::now(),
        }
    }

    fn record_success(&self, tokens: i64, credits: f64) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.success_requests.fetch_add(1, Ordering::Relaxed);
        if tokens > 0 {
            self.total_tokens.fetch_add(tokens, Ordering::Relaxed);
        }
        if credits > 0.0 {
            *self.total_credits.lock() += credits;
        }
    }

    fn record_failure(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.failed_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> GatewayStatsSnapshot {
        GatewayStatsSnapshot {
            total_requests: self.total_requests.load(Ordering::Relaxed),
            success_requests: self.success_requests.load(Ordering::Relaxed),
            failed_requests: self.failed_requests.load(Ordering::Relaxed),
            total_tokens: self.total_tokens.load(Ordering::Relaxed),
            total_credits: *self.total_credits.lock(),
            uptime: self.start_time.elapsed().as_secs(),
        }
    }
}

pub struct PromptCacheRuntime {
    accounting_enabled: bool,
    ttl_seconds: u64,
    max_ratio: f64,
    tracker: Arc<CacheTracker>,
    persistence_path: Option<PathBuf>,
    persistence_interval: Duration,
    persistence_loop: Option<PromptCachePersistenceLoop>,
}

impl PromptCacheRuntime {
    pub fn new(ttl_seconds: u64, accounting_enabled: bool, max_ratio: f64) -> Self {
        Self::build(
            ttl_seconds,
            accounting_enabled,
            max_ratio,
            None,
            Duration::from_secs(30),
        )
    }

    pub fn new_with_persistence(
        ttl_seconds: u64,
        accounting_enabled: bool,
        max_ratio: f64,
        persistence_path: PathBuf,
    ) -> Self {
        Self::build(
            ttl_seconds,
            accounting_enabled,
            max_ratio,
            Some(persistence_path),
            Duration::from_secs(30),
        )
    }

    fn build(
        ttl_seconds: u64,
        accounting_enabled: bool,
        max_ratio: f64,
        persistence_path: Option<PathBuf>,
        persistence_interval: Duration,
    ) -> Self {
        let tracker = Arc::new(CacheTracker::new_with_max_ratio(
            Duration::from_secs(ttl_seconds),
            max_ratio,
        ));
        let persistence_loop = persistence_path.as_ref().map(|path| {
            let loaded = tracker.load_from_path(path);
            if loaded > 0 {
                tracing::info!(
                    path = %path.display(),
                    entries = loaded,
                    "已加载 prompt cache 持久化条目"
                );
            }
            PromptCachePersistenceLoop::start(tracker.clone(), path.clone(), persistence_interval)
        });

        Self {
            accounting_enabled,
            ttl_seconds,
            max_ratio,
            tracker,
            persistence_path,
            persistence_interval,
            persistence_loop,
        }
    }

    pub fn snapshot(&self) -> PromptCacheSnapshot {
        PromptCacheSnapshot {
            accounting_enabled: self.accounting_enabled,
            ttl_seconds: self.ttl_seconds,
            max_ratio: self.max_ratio,
            tracker: self.tracker.clone(),
        }
    }

    pub fn update(
        &mut self,
        ttl_seconds: Option<u64>,
        accounting_enabled: Option<bool>,
        max_ratio: Option<f64>,
    ) {
        if let Some(value) = accounting_enabled {
            self.accounting_enabled = value;
        }

        if let Some(value) = max_ratio {
            self.max_ratio = value;
            self.tracker.set_max_cache_read_ratio(value);
        }

        if let Some(value) = ttl_seconds
            && self.ttl_seconds != value
        {
            self.ttl_seconds = value;
            self.persistence_loop = None;
            self.tracker = Arc::new(CacheTracker::new_with_max_ratio(
                Duration::from_secs(value),
                self.max_ratio,
            ));
            if let Some(path) = &self.persistence_path {
                let loaded = self.tracker.load_from_path(path);
                if loaded > 0 {
                    tracing::info!(
                        path = %path.display(),
                        entries = loaded,
                        "已重新加载 prompt cache 持久化条目"
                    );
                }
                self.persistence_loop = Some(PromptCachePersistenceLoop::start(
                    self.tracker.clone(),
                    path.clone(),
                    self.persistence_interval,
                ));
            }
        }
    }
}

struct PromptCachePersistenceLoop {
    stop_tx: mpsc::Sender<()>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl PromptCachePersistenceLoop {
    fn start(tracker: Arc<CacheTracker>, path: PathBuf, interval: Duration) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = thread::spawn(move || {
            loop {
                match stop_rx.recv_timeout(interval) {
                    Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        if let Err(err) = tracker.flush_to_path(&path) {
                            tracing::warn!(
                                path = %path.display(),
                                error = %err,
                                "prompt cache 最终持久化失败"
                            );
                        }
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Err(err) = tracker.flush_to_path(&path) {
                            tracing::warn!(
                                path = %path.display(),
                                error = %err,
                                "prompt cache 持久化失败"
                            );
                        }
                    }
                }
            }
        });
        Self {
            stop_tx,
            join_handle: Some(join_handle),
        }
    }
}

impl Drop for PromptCachePersistenceLoop {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// API 密钥（单 key 配置）
    pub api_key: Arc<RwLock<String>>,
    /// 是否要求客户端 API 密钥
    pub require_api_key: Arc<AtomicBool>,
    /// 多 API 密钥列表（可选，启用后支持多个密钥）
    pub api_keys: Option<SharedApiKeys>,
    /// 多 API 密钥持久化路径
    pub api_keys_path: Option<Arc<PathBuf>>,
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// Profile ARN（可选，用于请求注入）
    pub profile_arn: Option<String>,
    /// 共享压缩配置（运行时可修改）
    pub compression_config: Arc<RwLock<CompressionConfig>>,
    /// 共享系统提示清洗配置（运行时可修改）
    pub prompt_filter_config: Arc<RwLock<PromptFilterConfig>>,
    /// 共享系统提示注入运行时配置（运行时可修改）
    pub prompt_runtime: SharedPromptConfig,
    /// Prompt Cache 运行时配置（共享引用，支持热更新）
    pub prompt_cache_runtime: Arc<RwLock<PromptCacheRuntime>>,
    /// thinking 设置（运行时可改）
    pub thinking_config: Arc<RwLock<ThinkingRuntimeConfig>>,
    /// OpenAI Responses 历史存储目录
    pub responses_store_dir: Option<Arc<PathBuf>>,
    /// Kiro 可用模型缓存
    pub models_cache: Arc<RwLock<Vec<crate::kiro::models::AvailableModel>>>,
    /// 公开网关统计，用于 `/v1/stats`
    pub gateway_stats: Arc<GatewayStats>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(
        api_key: impl Into<String>,
        require_api_key: bool,
        extract_thinking: bool,
        prompt_cache_runtime: Arc<RwLock<PromptCacheRuntime>>,
        thinking_config: ThinkingRuntimeConfig,
    ) -> Self {
        Self {
            api_key: Arc::new(RwLock::new(api_key.into())),
            require_api_key: Arc::new(AtomicBool::new(require_api_key)),
            api_keys: None,
            api_keys_path: None,
            kiro_provider: None,
            extract_thinking,
            profile_arn: None,
            compression_config: Arc::new(RwLock::new(CompressionConfig::default())),
            prompt_filter_config: Arc::new(RwLock::new(PromptFilterConfig::default())),
            prompt_runtime: Arc::new(RwLock::new(PromptRuntimeConfig {
                enabled: false,
                enabled_presets: Vec::new(),
                user_presets: Vec::new(),
                custom_content: None,
                position: crate::model::config::SystemPromptPosition::default(),
            })),
            prompt_cache_runtime,
            thinking_config: Arc::new(RwLock::new(thinking_config)),
            responses_store_dir: None,
            models_cache: Arc::new(RwLock::new(Vec::new())),
            gateway_stats: Arc::new(GatewayStats::new()),
        }
    }

    /// 设置多 API 密钥列表
    pub fn with_api_keys(mut self, keys: Vec<crate::admin::types::ApiKeyEntry>) -> Self {
        self.api_keys = Some(Arc::new(RwLock::new(keys)));
        self
    }

    pub fn with_api_keys_runtime(mut self, keys: SharedApiKeys) -> Self {
        self.api_keys = Some(keys);
        self
    }

    pub fn with_api_keys_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.api_keys_path = Some(Arc::new(path.into()));
        self
    }

    pub fn with_auth_runtime(
        mut self,
        api_key: Arc<RwLock<String>>,
        require_api_key: Arc<AtomicBool>,
    ) -> Self {
        self.api_key = api_key;
        self.require_api_key = require_api_key;
        self
    }

    pub fn with_thinking_config(
        mut self,
        thinking_config: Arc<RwLock<ThinkingRuntimeConfig>>,
    ) -> Self {
        self.thinking_config = thinking_config;
        self
    }

    pub fn with_responses_store_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.responses_store_dir = Some(Arc::new(dir.into()));
        self
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: Arc<KiroProvider>) -> Self {
        self.kiro_provider = Some(provider);
        self
    }

    /// 设置 Profile ARN
    #[allow(dead_code)]
    pub fn with_profile_arn(mut self, arn: impl Into<String>) -> Self {
        self.profile_arn = Some(arn.into());
        self
    }

    /// 设置压缩配置（接受共享引用）
    pub fn with_compression_config(mut self, config: Arc<RwLock<CompressionConfig>>) -> Self {
        self.compression_config = config;
        self
    }

    /// 设置系统提示清洗配置（接受共享引用）
    pub fn with_prompt_filter_config(mut self, config: Arc<RwLock<PromptFilterConfig>>) -> Self {
        self.prompt_filter_config = config;
        self
    }

    /// 设置系统提示注入运行时配置（接受共享引用）
    pub fn with_prompt_runtime(mut self, runtime: SharedPromptConfig) -> Self {
        self.prompt_runtime = runtime;
        self
    }

    pub fn prompt_cache_snapshot(&self) -> PromptCacheSnapshot {
        self.prompt_cache_runtime.read().snapshot()
    }

    pub fn record_gateway_failure(&self) {
        self.gateway_stats.record_failure();
    }

    pub fn gateway_stats_snapshot(&self) -> GatewayStatsSnapshot {
        self.gateway_stats.snapshot()
    }

    pub fn record_api_key_usage(&self, key_id: Option<&str>, tokens: i64, credits: f64) {
        self.gateway_stats.record_success(tokens, credits);

        let Some(key_id) = key_id.filter(|id| !id.is_empty()) else {
            return;
        };
        let Some(api_keys) = &self.api_keys else {
            return;
        };

        let mut keys = api_keys.write();
        let Some(entry) = keys.iter_mut().find(|k| k.id == key_id) else {
            tracing::warn!(api_key_id = key_id, "API 密钥使用量记录失败：未找到密钥");
            return;
        };

        if tokens > 0 {
            entry.tokens_used += tokens;
        }
        if credits > 0.0 {
            entry.credits_used += credits;
        }
        entry.requests_count += 1;
        entry.last_used_at = Some(chrono::Utc::now().timestamp());

        if let Some(path) = &self.api_keys_path {
            match serde_json::to_string_pretty(&*keys) {
                Ok(data) => {
                    if let Err(e) = std::fs::write(path.as_ref(), data) {
                        tracing::warn!(path = %path.display(), error = %e, "保存 API 密钥文件失败");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "序列化 API 密钥失败");
                }
            }
        }
    }
}

/// API 密钥认证中间件
///
/// 支持两种模式：
/// 1. 单 key 模式：检查提取的 key 是否与配置的 api_key 匹配
/// 2. 多 key 模式：检查提取的 key 是否在 api_keys 列表中（且 enabled）
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    if !state.require_api_key.load(Ordering::Relaxed) {
        request.extensions_mut().insert(MatchedApiKeyId(None));
        return next.run(request).await;
    }

    match auth::extract_api_key(&request) {
        Some(key) => match authenticate_client_api_key(&state, &key) {
            Ok(api_key_id) => {
                request.extensions_mut().insert(MatchedApiKeyId(api_key_id));
                next.run(request).await
            }
            Err(ClientApiKeyAuthError::TokenLimitExceeded) => {
                let error = ErrorResponse::rate_limit_error("Token limit exceeded");
                (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response()
            }
            Err(ClientApiKeyAuthError::CreditLimitExceeded) => {
                let error = ErrorResponse::rate_limit_error("Credit limit exceeded");
                (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response()
            }
            Err(ClientApiKeyAuthError::Invalid) => {
                let error = ErrorResponse::authentication_error();
                (StatusCode::UNAUTHORIZED, Json(error)).into_response()
            }
        },
        _ => {
            let error = ErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientApiKeyAuthError {
    Invalid,
    TokenLimitExceeded,
    CreditLimitExceeded,
}

fn authenticate_client_api_key(
    state: &AppState,
    key: &str,
) -> Result<Option<String>, ClientApiKeyAuthError> {
    if let Some(api_keys) = &state.api_keys {
        let keys = api_keys.read();
        if !keys.is_empty() {
            let Some(entry) = keys.iter().find(|k| k.key == key) else {
                return Err(ClientApiKeyAuthError::Invalid);
            };
            if !entry.enabled {
                return Err(ClientApiKeyAuthError::Invalid);
            }
            if entry.token_limit > 0 && entry.tokens_used >= entry.token_limit {
                return Err(ClientApiKeyAuthError::TokenLimitExceeded);
            }
            if entry.credit_limit > 0.0 && entry.credits_used >= entry.credit_limit {
                return Err(ClientApiKeyAuthError::CreditLimitExceeded);
            }
            return Ok(Some(entry.id.clone()));
        }
    }

    let config_key = state.api_key.read().clone();
    if !config_key.trim().is_empty() && auth::constant_time_eq(key, &config_key) {
        return Ok(None);
    }

    Err(ClientApiKeyAuthError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(api_key: &str) -> AppState {
        AppState::new(
            api_key,
            true,
            false,
            Arc::new(RwLock::new(PromptCacheRuntime::new(300, false, 0.85))),
            ThinkingRuntimeConfig {
                suffix: "-thinking".to_string(),
                openai_format: "reasoning_content".to_string(),
                claude_format: "thinking".to_string(),
            },
        )
    }

    fn api_key_entry(key: &str, enabled: bool) -> crate::admin::types::ApiKeyEntry {
        crate::admin::types::ApiKeyEntry {
            id: format!("id-{key}"),
            name: Some(key.to_string()),
            key: key.to_string(),
            enabled,
            migrated: false,
            created_at: 1,
            last_used_at: None,
            token_limit: 0,
            credit_limit: 0.0,
            tokens_used: 0,
            credits_used: 0.0,
            requests_count: 0,
        }
    }

    #[test]
    fn shared_api_keys_update_auth_immediately() {
        let keys = Arc::new(RwLock::new(Vec::new()));
        let state = test_state("").with_api_keys_runtime(keys.clone());

        assert_eq!(
            authenticate_client_api_key(&state, "sk-live"),
            Err(ClientApiKeyAuthError::Invalid)
        );

        keys.write().push(api_key_entry("sk-live", true));

        assert_eq!(
            authenticate_client_api_key(&state, "sk-live"),
            Ok(Some("id-sk-live".to_string()))
        );
    }

    #[test]
    fn shared_api_keys_disable_immediately() {
        let keys = Arc::new(RwLock::new(vec![api_key_entry("sk-live", true)]));
        let state = test_state("").with_api_keys_runtime(keys.clone());

        assert_eq!(
            authenticate_client_api_key(&state, "sk-live"),
            Ok(Some("id-sk-live".to_string()))
        );

        keys.write()[0].enabled = false;

        assert_eq!(
            authenticate_client_api_key(&state, "sk-live"),
            Err(ClientApiKeyAuthError::Invalid)
        );
    }

    #[test]
    fn empty_config_key_fails_closed_when_auth_is_required() {
        let state = test_state("");

        assert_eq!(
            authenticate_client_api_key(&state, ""),
            Err(ClientApiKeyAuthError::Invalid)
        );
    }

    #[test]
    fn configured_api_keys_take_priority_over_config_key() {
        let keys = Arc::new(RwLock::new(vec![api_key_entry("sk-live", true)]));
        let state = test_state("config-key").with_api_keys_runtime(keys);

        assert_eq!(
            authenticate_client_api_key(&state, "config-key"),
            Err(ClientApiKeyAuthError::Invalid)
        );
    }

    #[test]
    fn record_api_key_usage_persists_counters() {
        let dir = std::env::temp_dir().join(format!(
            "xkiro-api-key-usage-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kiro_api_keys.json");
        let keys = Arc::new(RwLock::new(vec![api_key_entry("sk-live", true)]));
        let state = test_state("")
            .with_api_keys_runtime(keys)
            .with_api_keys_path(path.clone());

        state.record_api_key_usage(Some("id-sk-live"), 7, 0.5);
        state.record_api_key_usage(Some("id-sk-live"), 0, 0.0);

        let persisted: Vec<crate::admin::types::ApiKeyEntry> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entry = &persisted[0];
        assert_eq!(entry.tokens_used, 7);
        assert_eq!(entry.credits_used, 0.5);
        assert_eq!(entry.requests_count, 2);
        assert!(entry.last_used_at.is_some());
        let stats = state.gateway_stats_snapshot();
        assert_eq!(stats.total_requests, 2);
        assert_eq!(stats.success_requests, 2);
        assert_eq!(stats.total_tokens, 7);
        assert_eq!(stats.total_credits, 0.5);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn record_api_key_usage_empty_id_skips_per_key_counters() {
        let keys = Arc::new(RwLock::new(vec![api_key_entry("sk-live", true)]));
        let state = test_state("").with_api_keys_runtime(keys.clone());

        state.record_api_key_usage(Some(""), 100, 1.0);
        state.record_api_key_usage(None, 100, 1.0);

        let entry = &keys.read()[0];
        assert_eq!(entry.tokens_used, 0);
        assert_eq!(entry.credits_used, 0.0);
        assert_eq!(entry.requests_count, 0);
        assert!(entry.last_used_at.is_none());
        let stats = state.gateway_stats_snapshot();
        assert_eq!(stats.total_requests, 2);
        assert_eq!(stats.success_requests, 2);
        assert_eq!(stats.total_tokens, 200);
        assert_eq!(stats.total_credits, 2.0);
    }

    #[test]
    fn gateway_failure_stats_increment_without_per_key_usage() {
        let state = test_state("config-key");

        state.record_gateway_failure();

        let stats = state.gateway_stats_snapshot();
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.success_requests, 0);
        assert_eq!(stats.failed_requests, 1);
    }
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
/// - `expose_headers(...)`: 暴露 Anthropic 风格的 request/rate-limit 响应头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use http::HeaderName;
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([
            HeaderName::from_static("x-request-id"),
            HeaderName::from_static("x-ratelimit-limit-requests"),
            HeaderName::from_static("x-ratelimit-limit-tokens"),
            HeaderName::from_static("x-ratelimit-remaining-requests"),
            HeaderName::from_static("x-ratelimit-remaining-tokens"),
            HeaderName::from_static("x-ratelimit-reset-requests"),
            HeaderName::from_static("x-ratelimit-reset-tokens"),
        ])
}
