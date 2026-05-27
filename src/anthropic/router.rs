//! Anthropic API 路由配置

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};
use parking_lot::RwLock;

use crate::kiro::provider::KiroProvider;
use crate::model::config::{CompressionConfig, PromptFilterConfig};
use crate::model::runtime::SharedPromptConfig;

use super::{
    handlers::{count_tokens, get_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware, cors_layer},
};
use crate::openai::handlers::{post_chat_completions, post_responses};

/// 请求体最大大小限制 (50MB)
const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

/// 创建带有 KiroProvider 的 Anthropic API 路由
///
/// # 端点
/// ## 标准端点 (/v1)
/// - `GET /v1/models` - 获取可用模型列表
/// - `POST /v1/messages` - 创建消息（对话）
/// - `POST /v1/messages/count_tokens` - 计算 token 数量
///
/// ## Claude Code 兼容端点 (/cc/v1)
/// - `POST /cc/v1/messages` - 创建消息（流式响应会等待 contextUsageEvent 后再发送 message_start）
/// - `POST /cc/v1/messages/count_tokens` - 计算 token 数量
///
/// # 认证
/// 所有路径需要 API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer ***` header
pub fn create_router_with_provider(
    api_key: impl Into<String>,
    kiro_provider: Option<Arc<KiroProvider>>,
    profile_arn: Option<String>,
    extract_thinking: bool,
    compression: Arc<RwLock<CompressionConfig>>,
    prompt_filter: Arc<RwLock<PromptFilterConfig>>,
    prompt_runtime: SharedPromptConfig,
    prompt_cache_runtime: Arc<RwLock<super::middleware::PromptCacheRuntime>>,
    truncation_recovery_notice: Arc<std::sync::atomic::AtomicBool>,
) -> Router {
    let mut state = AppState::new(
        api_key,
        extract_thinking,
        prompt_cache_runtime,
        truncation_recovery_notice,
    )
        .with_compression_config(compression)
        .with_prompt_filter_config(prompt_filter)
        .with_prompt_runtime(prompt_runtime);
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }
    if let Some(arn) = profile_arn {
        state = state.with_profile_arn(arn);
    }

    // 需要认证的 /v1 路由
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .route("/chat/completions", post(post_chat_completions))
        .route("/responses", post(post_responses))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 需要认证的 /cc/v1 路由（Claude Code 兼容端点）
    // 与 /v1 的区别：流式响应会等待 contextUsageEvent 后再发送 message_start
    let cc_v1_routes = Router::new()
        .route("/messages", post(post_messages_cc))
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(cors_layer())
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}
