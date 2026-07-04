//! Anthropic API 路由配置

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};
use parking_lot::RwLock;

use crate::kiro::provider::KiroProvider;
use crate::model::config::{CompressionConfig, PromptFilterConfig};
use crate::model::runtime::{SharedModelMappingConfig, SharedPromptConfig};

use super::{
    handlers::{count_tokens, get_models, get_public_stats, post_messages, post_messages_cc},
    middleware::{AppState, ThinkingRuntimeConfig, auth_middleware, cors_layer},
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
/// - `POST /v1/chat/completions` - OpenAI Chat Completions 端点
/// - `POST /v1/responses` - OpenAI Responses 端点
/// - `GET /v1/stats` - 公开网关统计
///
/// ## 根路径别名
/// - `GET /models`
/// - `POST /messages`
/// - `POST /messages/count_tokens`
/// - `POST /chat/completions`
/// - `POST /responses`
/// - `POST /anthropic/v1/messages`
///
/// ## Claude Code 端点 (/cc/v1)
/// - `POST /cc/v1/messages` - 创建消息（流式响应会等待 contextUsageEvent 后再发送 message_start）
/// - `POST /cc/v1/messages/count_tokens` - 计算 token 数量
///
/// # 认证
/// 除公开的 `GET /v1/models` / `GET /models` 外，其它 API 路径需要 API 密钥认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer ***` header
pub fn create_router_with_provider(
    api_key: impl Into<String>,
    require_api_key: bool,
    api_key_runtime: Arc<RwLock<String>>,
    require_api_key_runtime: Arc<AtomicBool>,
    kiro_provider: Option<Arc<KiroProvider>>,
    profile_arn: Option<String>,
    extract_thinking: bool,
    compression: Arc<RwLock<CompressionConfig>>,
    prompt_filter: Arc<RwLock<PromptFilterConfig>>,
    model_mapping: SharedModelMappingConfig,
    prompt_runtime: SharedPromptConfig,
    prompt_cache_runtime: Arc<RwLock<super::middleware::PromptCacheRuntime>>,
    thinking_config: Arc<RwLock<ThinkingRuntimeConfig>>,
    api_keys_runtime: super::middleware::SharedApiKeys,
    api_keys_store_path: Option<std::path::PathBuf>,
    responses_store_dir: Option<std::path::PathBuf>,
) -> Router {
    let thinking_snapshot = thinking_config.read().clone();
    let mut state = AppState::new(
        api_key,
        require_api_key,
        extract_thinking,
        prompt_cache_runtime,
        thinking_snapshot,
    )
    .with_auth_runtime(api_key_runtime, require_api_key_runtime)
    .with_api_keys_runtime(api_keys_runtime)
    .with_thinking_config(thinking_config)
    .with_compression_config(compression)
    .with_prompt_filter_config(prompt_filter)
    .with_model_mapping_config(model_mapping)
    .with_prompt_runtime(prompt_runtime);
    if let Some(path) = api_keys_store_path {
        state = state.with_api_keys_path(path);
    }
    if let Some(dir) = responses_store_dir {
        state = state.with_responses_store_dir(dir);
    }
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }
    if let Some(arn) = profile_arn {
        state = state.with_profile_arn(arn);
    }

    // 模型列表端点公开；其它 /v1 路由仍需要认证。
    let public_v1_routes = Router::new().route("/models", get(get_models));

    let v1_routes = Router::new()
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .route("/chat/completions", post(post_chat_completions))
        .route("/responses", post(post_responses))
        .route("/stats", get(get_public_stats))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 根路径别名；/models 公开，其它别名复用认证层。
    let public_root_routes = Router::new().route("/models", get(get_models));

    let root_alias_routes = Router::new()
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .route("/chat/completions", post(post_chat_completions))
        .route("/responses", post(post_responses))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let anthropic_v1_alias_routes = Router::new().route("/messages", post(post_messages)).layer(
        middleware::from_fn_with_state(state.clone(), auth_middleware),
    );

    // 需要认证的 /cc/v1 路由（Claude Code 端点）
    // 与 /v1 的区别：流式响应会等待 contextUsageEvent 后再发送 message_start
    let cc_v1_routes = Router::new()
        .route("/messages", post(post_messages_cc))
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .merge(public_root_routes)
        .merge(root_alias_routes)
        .nest("/anthropic/v1", anthropic_v1_alias_routes)
        .nest("/v1", public_v1_routes.merge(v1_routes))
        .nest("/cc/v1", cc_v1_routes)
        .layer(cors_layer())
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use axum::body::{Body, to_bytes};
    use http::{Method, Request, StatusCode};
    use parking_lot::RwLock;
    use tower::ServiceExt;

    use super::*;
    use crate::anthropic::middleware::PromptCacheRuntime;
    use crate::model::config::{CompressionConfig, Config, PromptFilterConfig};

    fn test_router() -> Router {
        let config = Config::default();
        create_router_with_provider(
            "test-key",
            true,
            Arc::new(RwLock::new("test-key".to_string())),
            Arc::new(AtomicBool::new(true)),
            None,
            None,
            false,
            Arc::new(RwLock::new(CompressionConfig::default())),
            Arc::new(RwLock::new(PromptFilterConfig::default())),
            crate::model::runtime::model_mapping_from_config(&config),
            crate::model::runtime::shared_from_config(&config),
            Arc::new(RwLock::new(PromptCacheRuntime::new(
                config.prompt_cache_ttl_seconds,
                config.prompt_cache_accounting_enabled,
                config.prompt_cache_max_ratio,
            ))),
            Arc::new(RwLock::new(ThinkingRuntimeConfig {
                suffix: config.thinking_suffix.clone(),
                openai_format: config.openai_thinking_format.clone(),
                claude_format: config.claude_thinking_format.clone(),
            })),
            Arc::new(RwLock::new(Vec::new())),
            None,
            None,
        )
    }

    async fn assert_route_requires_auth(app: Router, method: Method, path: &str) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }

    #[tokio::test]
    async fn model_routes_are_public() {
        let app = test_router();
        for path in ["/models", "/v1/models"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["object"], "list");
            assert!(
                json["data"]
                    .as_array()
                    .is_some_and(|models| !models.is_empty()),
                "{path} should return fallback models without auth"
            );
        }
    }

    #[tokio::test]
    async fn public_aliases_keep_existing_auth_layer_except_models() {
        let app = test_router();
        for (method, path) in [
            (Method::GET, "/v1/stats"),
            (Method::POST, "/messages"),
            (Method::POST, "/messages/count_tokens"),
            (Method::POST, "/chat/completions"),
            (Method::POST, "/responses"),
            (Method::POST, "/anthropic/v1/messages"),
        ] {
            assert_route_requires_auth(app.clone(), method, path).await;
        }
    }

    #[tokio::test]
    async fn cors_exposes_request_and_rate_limit_headers() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/models")
                    .header("origin", "https://example.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );
        let exposed = response
            .headers()
            .get("access-control-expose-headers")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        for header in [
            "x-request-id",
            "x-ratelimit-limit-requests",
            "x-ratelimit-limit-tokens",
            "x-ratelimit-remaining-requests",
            "x-ratelimit-remaining-tokens",
            "x-ratelimit-reset-requests",
            "x-ratelimit-reset-tokens",
        ] {
            assert!(exposed.contains(header), "missing exposed header {header}");
        }
    }

    #[tokio::test]
    async fn public_stats_returns_gateway_shape_with_api_key() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/stats")
                    .header("authorization", "Bearer test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        for key in [
            "status",
            "version",
            "accounts",
            "available",
            "credentialsTotal",
            "credentialsAvailable",
            "totalRequests",
            "successRequests",
            "failedRequests",
            "totalTokens",
            "totalCredits",
            "uptime",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        assert_eq!(json["status"], "ok");
        assert_eq!(json["credentialsTotal"], json["accounts"]);
        assert_eq!(json["credentialsAvailable"], json["available"]);
    }
}
