//! Anthropic API Handler 函数

use std::convert::Infallible;

use crate::kiro::model::events::{Event, MeteringEvent};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request};
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, CacheUsageBreakdown, SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

/// 自适应压缩：最大迭代次数（避免极端输入导致过长 CPU 消耗）
const ADAPTIVE_COMPRESSION_MAX_ITERS: usize = 32;
/// tool_result 二次压缩的最低阈值（字符数）
const ADAPTIVE_MIN_TOOL_RESULT_MAX_CHARS: usize = 512;
/// tool_use input 二次压缩的最低阈值（字符数）
const ADAPTIVE_MIN_TOOL_USE_INPUT_MAX_CHARS: usize = 256;
/// 历史截断默认保留消息数（与 compressor.rs 的 preserve_count 保持一致）
const ADAPTIVE_HISTORY_PRESERVE_MESSAGES: usize = 2;
/// 消息内容二次压缩的最低阈值（字符数）
const ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS: usize = 8192;

// ============================================================================
// Cache usage 工具集（按 BK 严格对齐）
// ============================================================================
//
// 由 cache_tracker 在请求阶段算出 cache 命中分布，注入到 message_start /
// message_delta 的 usage 字段中。剔除 cooldown / rate_limiter，纯计数路线。

/// 单次请求的 cache usage 切片
///
/// 直接从 `cache_tracker::CacheComputeResult` 复制过来，作为 handlers 内部的窄
/// 接口，避免到处带着 cache_tracker 模块类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CacheUsageContext {
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
    cache_creation_5m_input_tokens: i32,
    cache_creation_1h_input_tokens: i32,
}

/// 流式请求上下文（聚合 cache_tracker 相关参数，避免函数签名爆炸）
#[allow(dead_code)] // 接入主链路前先就位
struct StreamRequestContext<'a> {
    cache_tracker: Option<&'a std::sync::Arc<crate::anthropic::cache_tracker::CacheTracker>>,
    cache_profile: Option<&'a crate::anthropic::cache_tracker::CacheProfile>,
    request_body: &'a str,
    model: &'a str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    user_id: Option<&'a str>,
}

/// 非流式请求上下文（同上）
#[allow(dead_code)]
struct NonStreamRequestContext<'a> {
    request_body: &'a str,
    model: &'a str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    user_id: Option<&'a str>,
    cache_tracker: Option<&'a std::sync::Arc<crate::anthropic::cache_tracker::CacheTracker>>,
    cache_profile: Option<&'a crate::anthropic::cache_tracker::CacheProfile>,
}

/// 从 payload + 总输入 token 构造 cache 画像
///
/// 内部薄封装，让上层调用 `cache_tracker.build_profile(...)` 时不必直接持有
/// cache_tracker 模块类型，便于后续替换实现。
#[allow(dead_code)]
fn build_cache_profile(
    cache_tracker: &crate::anthropic::cache_tracker::CacheTracker,
    payload: &MessagesRequest,
    total_input_tokens: i32,
) -> crate::anthropic::cache_tracker::CacheProfile {
    cache_tracker.build_profile(payload, total_input_tokens)
}

/// 复用底层 compute，把结果转成 handlers 内部的 `CacheUsageContext`
fn compute_cache_usage(
    cache_tracker: &crate::anthropic::cache_tracker::CacheTracker,
    credential_id: u64,
    profile: &crate::anthropic::cache_tracker::CacheProfile,
) -> CacheUsageContext {
    let result = cache_tracker.compute(credential_id, profile);
    CacheUsageContext {
        cache_creation_input_tokens: result.cache_creation_input_tokens,
        cache_read_input_tokens: result.cache_read_input_tokens,
        cache_creation_5m_input_tokens: result.cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens: result.cache_creation_1h_input_tokens,
    }
}

/// 选择凭据前的临时 cache 估算（credential_id = 0）
#[allow(dead_code)]
fn provisional_cache_usage(
    cache_tracker: &crate::anthropic::cache_tracker::CacheTracker,
    profile: &crate::anthropic::cache_tracker::CacheProfile,
) -> CacheUsageContext {
    compute_cache_usage(cache_tracker, 0, profile)
}

/// 凭据已选定后的精确 cache 计算
#[allow(dead_code)]
fn resolved_cache_usage(
    cache_tracker: &crate::anthropic::cache_tracker::CacheTracker,
    credential_id: u64,
    profile: &crate::anthropic::cache_tracker::CacheProfile,
) -> CacheUsageContext {
    compute_cache_usage(cache_tracker, credential_id, profile)
}

/// 将 cache usage 字段注入到 SSE/非流响应的 usage 对象中
fn inject_cache_usage_fields(usage: &mut serde_json::Value, cache_context: CacheUsageContext) {
    usage["cache_creation_input_tokens"] = json!(cache_context.cache_creation_input_tokens);
    usage["cache_read_input_tokens"] = json!(cache_context.cache_read_input_tokens);
    usage["cache_creation"] = json!({
        "ephemeral_5m_input_tokens": cache_context.cache_creation_5m_input_tokens,
        "ephemeral_1h_input_tokens": cache_context.cache_creation_1h_input_tokens
    });
}

/// 将总输入 token 转为 Anthropic usage 的 input_tokens 口径（剔除 cache 读写）
///
/// 与 `stream::billed_input_tokens` 同算法（饱和减 + max 0）。stream.rs 那份用于
/// SSE 路径，handlers 这份用于非流路径——保持两份独立避免跨模块 pub。
fn billed_input_tokens(
    input_tokens: i32,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> i32 {
    input_tokens
        .saturating_sub(cache_creation_input_tokens)
        .saturating_sub(cache_read_input_tokens)
        .max(0)
}

/// 将 metering 信息注入 usage（credit_usage / credit_unit / credit_unit_plural）
fn inject_credit_usage_fields(usage: &mut serde_json::Value, metering: &MeteringEvent) {
    usage["credit_usage"] = json!(metering.usage);
    usage["credit_unit"] = json!(metering.unit);
    usage["credit_unit_plural"] = json!(metering.unit_plural);
}

/// 自适应二次压缩结果，用于触发后回填日志/调试信息
#[derive(Debug, Default, Clone, Copy)]
struct AdaptiveCompressionOutcome {
    initial_bytes: usize,
    final_bytes: usize,
    iters: usize,
    additional_history_turns_removed: usize,
    final_tool_result_max_chars: usize,
    final_tool_use_input_max_chars: usize,
    final_message_content_max_chars: usize,
}

// ============================================================================
// 错误分类谓词（按 BK 移植）
// ============================================================================
//
// provider.rs 在不同失败路径会在错误字符串中保留稳定关键字，
// 这里通过字符串匹配把 anyhow::Error 分类成 6 种语义错误，
// 各自映射到合理的 HTTP 状态码（避免一律 502 诱发客户端无效重试）。

/// 网络错误关键字（is_transient_upstream_error 和 is_network_error 共用）
const NETWORK_ERROR_PATTERNS: &[&str] = &[
    "error sending request",
    "connection closed",
    "connection reset",
];

fn is_input_too_long_error(err: &Error) -> bool {
    // provider.rs 在遇到上游返回的 input-too-long 场景时，会在错误中保留以下关键字：
    // - CONTENT_LENGTH_EXCEEDS_THRESHOLD
    // - Input is too long
    //
    // 这类错误是确定性的请求问题（缩短输入才可恢复），不应返回 5xx（会诱发客户端重试）。
    // 注意：不包含 "Improperly formed request"，该错误可能由空消息内容等格式问题引起
    let s = err.to_string();
    s.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") || s.contains("Input is too long")
}

fn is_quota_exhausted_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("所有凭据已用尽")
}

fn is_no_credentials_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("没有可用的凭据")
}

fn is_network_error(s: &str) -> bool {
    NETWORK_ERROR_PATTERNS.iter().any(|p| s.contains(p))
}

fn is_transient_upstream_error(err: &Error) -> bool {
    let s = err.to_string().to_lowercase();
    s.contains("429 too many requests")
        || s.contains("insufficient_model_capacity")
        || s.contains("high traffic")
        || s.contains("408 request timeout")
        || s.contains("502 bad gateway")
        || s.contains("503 service unavailable")
        || s.contains("504 gateway timeout")
        || is_network_error(&s)
}

fn is_improperly_formed_request_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("Improperly formed request")
}


/// 计算 KiroRequest 中所有图片 base64 数据的总字节数。
///
/// 该统计用于归因请求体大小（图片 base64 往往占用大量 bytes）。
/// 注意：上游存在请求体大小硬限制（约 5MiB），因此图片也必须控制体积；
/// `max_request_body_bytes` 的校验以实际序列化后的总字节数为准。
fn total_image_bytes(kiro_request: &KiroRequest) -> usize {
    let state = &kiro_request.conversation_state;
    let mut total = 0usize;

    // currentMessage 中的图片
    for img in &state.current_message.user_input_message.images {
        total += img.source.bytes.len();
    }

    // 历史消息中的图片
    for msg in &state.history {
        if let crate::kiro::model::requests::conversation::Message::User(user_msg) = msg {
            for img in &user_msg.user_input_message.images {
                total += img.source.bytes.len();
            }
        }
    }

    total
}

/// 自适应二次压缩：按 5 层降级策略将 request_body 缩到 max_body 以内。
///
/// 本函数会在第一轮 `compressor::compress` 之后再做一次兜底压缩；
/// 每轮调整一项参数后重新跑压缩管道并重新序列化。
///
/// 触发条件：`max_body > 0 && request_body.len() > max_body && base_config.enabled`
///
/// 5 层降级（按顺序尝试）：
/// 1. tool_result_max_chars × 0.75
/// 2. tool_use_input_max_chars × 0.75
/// 3. compress_long_messages_pass × 0.75（截断超长 user 消息内容）
/// 4. remove_history_images（保留 current_message 图片）
/// 5. 成对移除最老的 user+assistant 历史消息（保留前 2 条）
fn adaptive_shrink_request_body(
    kiro_request: &mut KiroRequest,
    base_config: &crate::model::config::CompressionConfig,
    max_body: usize,
    request_body: &mut String,
) -> Result<Option<AdaptiveCompressionOutcome>, serde_json::Error> {
    if max_body == 0 || request_body.len() <= max_body || !base_config.enabled {
        return Ok(None);
    }

    let mut outcome = AdaptiveCompressionOutcome {
        initial_bytes: request_body.len(),
        final_bytes: request_body.len(),
        iters: 0,
        additional_history_turns_removed: 0,
        final_tool_result_max_chars: base_config.tool_result_max_chars,
        final_tool_use_input_max_chars: base_config.tool_use_input_max_chars,
        final_message_content_max_chars: 0,
    };

    let mut adaptive_config = base_config.clone();
    let mut history_images_removed = false;

    // 是否存在任何 tool_result / tools（否则降低阈值只会浪费迭代次数）
    let has_any_tool_results_or_tools = {
        let state = &kiro_request.conversation_state;
        if !state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results
            .is_empty()
            || !state
                .current_message
                .user_input_message
                .user_input_message_context
                .tools
                .is_empty()
        {
            true
        } else {
            state.history.iter().any(|msg| match msg {
                crate::kiro::model::requests::conversation::Message::User(u) => {
                    !u.user_input_message
                        .user_input_message_context
                        .tool_results
                        .is_empty()
                        || !u
                            .user_input_message
                            .user_input_message_context
                            .tools
                            .is_empty()
                }
                _ => false,
            })
        }
    };

    // 是否存在任何 tool_use（否则降低阈值只会浪费迭代次数）
    let has_any_tool_uses = kiro_request
        .conversation_state
        .history
        .iter()
        .any(|msg| match msg {
            crate::kiro::model::requests::conversation::Message::Assistant(a) => a
                .assistant_response_message
                .tool_uses
                .as_ref()
                .is_some_and(|t| !t.is_empty()),
            _ => false,
        });

    // 是否存在历史图片（否则无需尝试图片降级）
    let has_history_images = kiro_request
        .conversation_state
        .history
        .iter()
        .any(|msg| match msg {
            crate::kiro::model::requests::conversation::Message::User(u) => {
                !u.user_input_message.images.is_empty()
            }
            _ => false,
        });

    // 扫描所有用户消息，找到最大 content 字符数作为初始 message_content_max_chars
    let max_content_chars = {
        let mut max_chars = kiro_request
            .conversation_state
            .current_message
            .user_input_message
            .content
            .chars()
            .count();
        for msg in &kiro_request.conversation_state.history {
            if let crate::kiro::model::requests::conversation::Message::User(u) = msg {
                max_chars = max_chars.max(u.user_input_message.content.chars().count());
            }
        }
        max_chars
    };
    let mut message_content_max_chars =
        (max_content_chars * 3 / 4).max(ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS);

    for _ in 0..ADAPTIVE_COMPRESSION_MAX_ITERS {
        if request_body.len() <= max_body {
            break;
        }

        let mut changed = false;

        if has_any_tool_results_or_tools
            && adaptive_config.tool_result_max_chars > ADAPTIVE_MIN_TOOL_RESULT_MAX_CHARS
        {
            let next = (adaptive_config.tool_result_max_chars * 3 / 4)
                .max(ADAPTIVE_MIN_TOOL_RESULT_MAX_CHARS);
            if next < adaptive_config.tool_result_max_chars {
                adaptive_config.tool_result_max_chars = next;
                changed = true;
            }
        } else if has_any_tool_uses
            && adaptive_config.tool_use_input_max_chars > ADAPTIVE_MIN_TOOL_USE_INPUT_MAX_CHARS
        {
            let next = (adaptive_config.tool_use_input_max_chars * 3 / 4)
                .max(ADAPTIVE_MIN_TOOL_USE_INPUT_MAX_CHARS);
            if next < adaptive_config.tool_use_input_max_chars {
                adaptive_config.tool_use_input_max_chars = next;
                changed = true;
            }
        } else {
            // 单条 user content 超过 max_body 时，移除历史也救不了，必须截断超长内容
            let max_single_user_content_bytes = {
                let state = &kiro_request.conversation_state;
                let mut max_bytes = state.current_message.user_input_message.content.len();
                for msg in &state.history {
                    if let crate::kiro::model::requests::conversation::Message::User(u) = msg {
                        max_bytes = max_bytes.max(u.user_input_message.content.len());
                    }
                }
                max_bytes
            };

            let history = &mut kiro_request.conversation_state.history;
            if (max_single_user_content_bytes > max_body
                || history.len() <= ADAPTIVE_HISTORY_PRESERVE_MESSAGES + 2)
                && message_content_max_chars >= ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS
            {
                // 第三层：截断超长消息内容
                let saved = super::compressor::compress_long_messages_pass(
                    &mut kiro_request.conversation_state,
                    message_content_max_chars,
                );
                if saved > 0 {
                    changed = true;
                }
                outcome.final_message_content_max_chars = message_content_max_chars;
                message_content_max_chars =
                    (message_content_max_chars * 3 / 4).max(ADAPTIVE_MIN_MESSAGE_CONTENT_MAX_CHARS);
            } else if !history_images_removed && has_history_images {
                // 第四层：仅清除历史图片，保留 current_message 图片
                let removed = kiro_request.conversation_state.remove_history_images();
                if removed > 0 {
                    history_images_removed = true;
                    changed = true;
                }
            } else if history.len() > ADAPTIVE_HISTORY_PRESERVE_MESSAGES + 2 {
                // 第五层：成对移除最老 user+assistant 消息
                let preserve = ADAPTIVE_HISTORY_PRESERVE_MESSAGES;
                let min_len = preserve + 2;
                let removable = history.len().saturating_sub(min_len);
                let mut remove_msgs = removable.min(16);
                remove_msgs -= remove_msgs % 2;
                if remove_msgs > 0 {
                    history.drain(preserve..preserve + remove_msgs);
                    outcome.additional_history_turns_removed += remove_msgs / 2;
                    changed = true;
                }
            }
        }

        if !changed {
            break;
        }

        super::compressor::compress(&mut kiro_request.conversation_state, &adaptive_config);
        *request_body = serde_json::to_string(kiro_request)?;
        outcome.iters += 1;
        outcome.final_bytes = request_body.len();
    }

    outcome.final_tool_result_max_chars = adaptive_config.tool_result_max_chars;
    outcome.final_tool_use_input_max_chars = adaptive_config.tool_use_input_max_chars;

    Ok(Some(outcome))
}

/// 将 KiroProvider 错误映射为 HTTP 响应（按 BK 完整分类）
fn map_kiro_provider_error_to_response(request_body: &str, err: Error) -> Response {
    if is_input_too_long_error(&err) {
        tracing::warn!(
            kiro_request_body_bytes = request_body.len(),
            error = %err,
            "上游拒绝请求：输入上下文过长（不应重试）"
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long (CONTENT_LENGTH_EXCEEDS_THRESHOLD). Reduce conversation history/system/tools; retrying the same request will not help.",
            )),
        )
            .into_response();
    }

    if is_improperly_formed_request_error(&err) {
        tracing::warn!(
            error = %err,
            kiro_request_body_bytes = request_body.len(),
            "上游拒绝请求：请求格式错误（可能由超大请求体、消息/工具序列异常或空内容块导致）"
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Improperly formed request. This is often caused by oversized payloads, malformed message/tool sequences, or empty content blocks.",
            )),
        )
            .into_response();
    }

    if is_no_credentials_error(&err) {
        tracing::error!(error = %err, "没有可用的凭据");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "No credentials available. Please add or enable credentials via Admin API or credentials.json.",
            )),
        )
            .into_response();
    }

    if is_quota_exhausted_error(&err) {
        tracing::warn!(error = %err, "所有凭据配额已耗尽");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new(
                "rate_limit_error",
                "All credentials quota exhausted. Please wait for quota reset or add new credentials.",
            )),
        )
            .into_response();
    }

    if is_transient_upstream_error(&err) {
        let err_str = err.to_string().to_lowercase();
        if is_network_error(&err_str) {
            tracing::warn!(error = %err, "上游网络错误，不输出请求体");
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("上游网络错误: {}", err),
                )),
            )
                .into_response();
        }
        tracing::warn!(error = %err, "上游瞬态错误（429/5xx），不输出请求体");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new("rate_limit_error", err.to_string())),
        )
            .into_response();
    }

    tracing::error!("Kiro API 调用失败: {}", err);
    #[cfg(feature = "sensitive-logs")]
    tracing::error!(
        request_body_bytes = request_body.len(),
        "上游报错，请求体大小: {} bytes",
        request_body.len()
    );
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

/// 对日志/审计中的 user_id 做脱敏（按 BK 严格对齐）
///
/// 截断规则：
/// - len > 25：保留前 13 + 后 8，中间 `***`
/// - len > 12：保留前 4 + 后 4，中间 `***`
/// - 其他：直接 `***`
/// - None：返回 `"None"`
///
/// 用 `chars().collect()` 而非字节索引，避免 UTF-8 多字节边界 panic。
#[allow(dead_code)] // 留给主链路接入时使用
fn mask_user_id(user_id: Option<&str>) -> String {
    match user_id {
        Some(id) => {
            let chars: Vec<char> = id.chars().collect();
            let len = chars.len();
            if len > 25 {
                format!(
                    "{}***{}",
                    chars[..13].iter().collect::<String>(),
                    chars[len - 8..].iter().collect::<String>()
                )
            } else if len > 12 {
                format!(
                    "{}***{}",
                    chars[..4].iter().collect::<String>(),
                    chars[len - 4..].iter().collect::<String>()
                )
            } else {
                "***".to_string()
            }
        }
        None => "None".to_string(),
    }
}

/// 剔除 messages 中的空 text content block（`{"type":"text","text":""}` 或纯空白）。
///
/// 说明：
/// - Claude Code/claude-cli 在某些 tool_use-only 场景下可能会把空 text block 写回 history；
/// - 上游会拒绝空 text block（400: "text content blocks must be non-empty"）。
/// - 空 text block 不携带任何语义，直接移除是最小且安全的兼容策略。
#[allow(dead_code)]
fn strip_empty_text_content_blocks(messages: &mut [super::types::Message]) -> usize {
    let mut removed = 0usize;

    for msg in messages {
        let serde_json::Value::Array(arr) = &mut msg.content else {
            continue;
        };

        let before = arr.len();
        arr.retain(|item| {
            let Some(obj) = item.as_object() else {
                return true;
            };

            if obj.get("type").and_then(|v| v.as_str()) != Some("text") {
                return true;
            }

            match obj.get("text") {
                Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
                Some(serde_json::Value::Null) | None => false,
                // text 字段类型异常：保守起见不删，交由后续转换/上游校验处理
                _ => true,
            }
        });
        removed += before - arr.len();
    }

    removed
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let models = vec![
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1772992800, // Mar 7, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1772992800, // Mar 7, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            context_length: None,
            max_completion_tokens: None,
            thinking: None,
        },
    ];

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::should_handle_websearch_request(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, None, None, input_tokens)
            .await;
    }

    // 转换请求
    let compression = state.compression_config.read().clone();
    let conversion_result = match convert_request(&payload, &compression) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::EmptyMessageContent => {
                    ("invalid_request_error", "消息内容为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let mut conversation_state = conversion_result.conversation_state;

    // 多层压缩
    if compression.enabled {
        let stats = super::compressor::compress(&mut conversation_state, &compression);
        tracing::debug!("压缩统计: {:?}", stats);
    }

    let mut kiro_request = KiroRequest {
        conversation_state,
        profile_arn: None,
    };

    let mut request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 请求体大小预检（上游存在硬性请求体大小限制；按实际序列化后的总字节数判断）
    let max_body = compression.max_request_body_bytes;
    if max_body > 0 && request_body.len() > max_body && compression.enabled {
        // 自适应二次压缩：按 request_body_bytes 迭代截断，尽量把请求缩到阈值内
        match adaptive_shrink_request_body(
            &mut kiro_request,
            &compression,
            max_body,
            &mut request_body,
        ) {
            Ok(Some(outcome)) => {
                tracing::warn!(
                    conversation_id = kiro_request.conversation_state.conversation_id.as_str(),
                    initial_bytes = outcome.initial_bytes,
                    final_bytes = outcome.final_bytes,
                    threshold = max_body,
                    iters = outcome.iters,
                    additional_history_turns_removed = outcome.additional_history_turns_removed,
                    final_tool_result_max_chars = outcome.final_tool_result_max_chars,
                    final_tool_use_input_max_chars = outcome.final_tool_use_input_max_chars,
                    final_message_content_max_chars = outcome.final_message_content_max_chars,
                    "请求体超过阈值，已执行自适应二次压缩"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("自适应二次压缩序列化失败: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new(
                        "internal_error",
                        format!("序列化请求失败: {}", e),
                    )),
                )
                    .into_response();
            }
        }
    }

    // 压缩后再次检查（输出 image_bytes/non-image bytes 便于排查）
    let final_img_bytes = total_image_bytes(&kiro_request);
    let final_effective_len = request_body.len().saturating_sub(final_img_bytes);
    if max_body > 0 && request_body.len() > max_body {
        tracing::warn!(
            conversation_id = kiro_request.conversation_state.conversation_id.as_str(),
            request_body_bytes = request_body.len(),
            image_bytes = final_img_bytes,
            effective_bytes = final_effective_len,
            threshold = max_body,
            "请求体超过安全阈值，拒绝发送"
        );
        #[cfg(feature = "sensitive-logs")]
        tracing::error!(
            "自适应压缩仍超限，完整请求体（用于诊断）: {}",
            truncate_base64_in_request_body(&request_body)
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "Request too large ({} bytes total; images {} bytes; non-image {} bytes; limit {}). Reduce conversation history/tool output or number/size of images.",
                    request_body.len(),
                    final_img_bytes,
                    final_effective_len,
                    max_body
                ),
            )),
        )
            .into_response();
    }

    tracing::debug!(
        kiro_request_body_bytes = request_body.len(),
        "已构建 Kiro 请求体"
    );
    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens（贴 BK：clone 让 payload 后续仍可用于 build_cache_profile）
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    let user_id = payload
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref());

    // 读 prompt-cache 快照 + 按 accounting_enabled 构造 cache_profile（BK 模式）
    let prompt_cache = state.prompt_cache_snapshot();
    let cache_profile = prompt_cache.accounting_enabled.then(|| {
        build_cache_profile(prompt_cache.tracker.as_ref(), &payload, input_tokens)
    });

    if payload.stream {
        // 流式响应
        let stream_request = StreamRequestContext {
            cache_tracker: prompt_cache
                .accounting_enabled
                .then_some(&prompt_cache.tracker),
            cache_profile: cache_profile.as_ref(),
            request_body: &request_body,
            model: &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map: tool_name_map.clone(),
            user_id,
        };
        handle_stream_request(provider, stream_request).await
    } else {
        // 非流式响应
        let non_stream_request = NonStreamRequestContext {
            request_body: &request_body,
            model: &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            user_id,
            cache_tracker: prompt_cache
                .accounting_enabled
                .then_some(&prompt_cache.tracker),
            cache_profile: cache_profile.as_ref(),
        };
        handle_non_stream_request(provider, non_stream_request).await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    context: StreamRequestContext<'_>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_result = match provider
        .call_api_stream(context.request_body, context.user_id)
        .await
    {
        Ok(resp) => resp,
        Err(e) => return map_kiro_provider_error_to_response(context.request_body, e),
    };

    // 凭据已选定 → 用 resolved_cache_usage 重算并提交 cache_tracker（BK 模式）
    let final_cache_context = match (context.cache_tracker, context.cache_profile) {
        (Some(tracker), Some(profile)) => {
            let resolved = resolved_cache_usage(tracker, api_result.credential_id, profile);
            tracing::info!(
                credential_id = api_result.credential_id,
                final_cache_creation_input_tokens = resolved.cache_creation_input_tokens,
                final_cache_read_input_tokens = resolved.cache_read_input_tokens,
                "Resolved cache usage for stream request"
            );
            tracker.update(api_result.credential_id, profile);
            Some(resolved)
        }
        _ => None,
    };
    let final_cache_usage = final_cache_context.map(|ctx| CacheUsageBreakdown {
        cache_creation_input_tokens: ctx.cache_creation_input_tokens,
        cache_read_input_tokens: ctx.cache_read_input_tokens,
        cache_creation_5m_input_tokens: ctx.cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens: ctx.cache_creation_1h_input_tokens,
    });

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(
        context.model,
        context.input_tokens,
        final_cache_usage,
        context.thinking_enabled,
        context.tool_name_map,
    );

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(api_result.response, ctx, initial_events);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS))),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 发送最终事件并结束
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                        None => {
                            // 流结束，发送最终事件
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    context: NonStreamRequestContext<'_>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_result = match provider
        .call_api(context.request_body, context.user_id)
        .await
    {
        Ok(resp) => resp,
        Err(e) => return map_kiro_provider_error_to_response(context.request_body, e),
    };

    // 凭据已选定 → 用 resolved_cache_usage 重算并提交 cache_tracker（BK 模式）
    let final_cache_context = match (context.cache_tracker, context.cache_profile) {
        (Some(tracker), Some(profile)) => {
            let resolved = resolved_cache_usage(tracker, api_result.credential_id, profile);
            tracing::info!(
                credential_id = api_result.credential_id,
                final_cache_creation_input_tokens = resolved.cache_creation_input_tokens,
                final_cache_read_input_tokens = resolved.cache_read_input_tokens,
                "Resolved cache usage for non-stream request"
            );
            tracker.update(api_result.credential_id, profile);
            Some(resolved)
        }
        _ => None,
    };

    // 读取响应体
    let body_bytes = match api_result.response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // 从 meteringEvent 透传的 credit usage，仅用于最终 usage 字段
    let mut metering: Option<MeteringEvent> = None;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

                            // 累积工具的 JSON 输入
                            let buffer = tool_json_buffers
                                .entry(tool_use.tool_use_id.clone())
                                .or_insert_with(String::new);
                            buffer.push_str(&tool_use.input);

                            // 如果是完整的工具调用，添加到列表
                            if tool_use.stop {
                                let input: serde_json::Value = if buffer.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    serde_json::from_str(buffer).unwrap_or_else(|e| {
                                        // 检测是否为截断导致的解析失败
                                        if let Some(truncation_info) =
                                            super::truncation::detect_truncation(
                                                &tool_use.name,
                                                &tool_use.tool_use_id,
                                                buffer,
                                            )
                                        {
                                            let soft_msg =
                                                super::truncation::build_soft_failure_result(
                                                    &truncation_info,
                                                );
                                            tracing::warn!(
                                                tool_use_id = %tool_use.tool_use_id,
                                                truncation_type = %truncation_info.truncation_type,
                                                "检测到工具调用截断: {}", soft_msg
                                            );
                                        }
                                        tracing::warn!(
                                            "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                                            e,
                                            tool_use.tool_use_id
                                        );
                                        serde_json::json!({})
                                    })
                                };

                                let original_name = context
                                    .tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use.tool_use_id,
                                    "name": original_name,
                                    "input": input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(context.model);
                            let actual_input_tokens =
                                (context_usage.context_usage_percentage * (window_size as f64)
                                    / 100.0) as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(event_metering) => {
                            tracing::debug!(
                                usage = event_metering.usage,
                                unit = %event_metering.unit,
                                unit_plural = %event_metering.unit_plural,
                                "收到 meteringEvent"
                            );
                            metering = Some(event_metering);
                        }
                        Event::Exception { exception_type, .. } => {
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if context.thinking_enabled {
        // 从完整文本中提取 thinking 块
        let (thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(thinking_text) = thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // xkiro 独有：优先使用 contextUsageEvent 的上游值，无则回落估算（保留 BK 没有的能力）
    let final_input_tokens = context_input_tokens.unwrap_or(context.input_tokens);
    // BK 模式：billed = final - cache_creation - cache_read（用 saturating_sub 防负）
    let billed_input_tokens = final_cache_context
        .map(|ctx| {
            billed_input_tokens(
                final_input_tokens,
                ctx.cache_creation_input_tokens,
                ctx.cache_read_input_tokens,
            )
        })
        .unwrap_or(final_input_tokens);

    tracing::info!(
        estimated_input_tokens = context.input_tokens,
        context_input_tokens = ?context_input_tokens,
        final_input_tokens,
        billed_input_tokens,
        output_tokens,
        "Non-stream usage: final={} context={:?} billed={} output={}",
        final_input_tokens,
        context_input_tokens,
        billed_input_tokens,
        output_tokens
    );

    // 构建 Anthropic 响应（usage 字段按 BK 模式注入 credit + cache）
    let response_body = {
        let mut usage = json!({
            "input_tokens": billed_input_tokens,
            "output_tokens": output_tokens
        });
        if let Some(ref metering) = metering {
            inject_credit_usage_fields(&mut usage, metering);
        }
        if let Some(cache_context) = final_cache_context {
            inject_cache_usage_fields(&mut usage, cache_context);
        }

        json!({
            "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            "type": "message",
            "role": "assistant",
            "content": content,
            "model": context.model,
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "usage": usage
        })
    };

    (StatusCode::OK, Json(response_body)).into_response()
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6 {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::should_handle_websearch_request(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, None, None, input_tokens)
            .await;
    }

    // 转换请求
    let compression = state.compression_config.read().clone();
    let conversion_result = match convert_request(&payload, &compression) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::EmptyMessageContent => {
                    ("invalid_request_error", "消息内容为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let mut conversation_state = conversion_result.conversation_state;

    // 多层压缩
    if compression.enabled {
        let stats = super::compressor::compress(&mut conversation_state, &compression);
        tracing::debug!("压缩统计: {:?}", stats);
    }

    let mut kiro_request = KiroRequest {
        conversation_state,
        profile_arn: None,
    };

    let mut request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 请求体大小预检（上游存在硬性请求体大小限制；按实际序列化后的总字节数判断）
    let max_body = compression.max_request_body_bytes;
    if max_body > 0 && request_body.len() > max_body && compression.enabled {
        // 自适应二次压缩：按 request_body_bytes 迭代截断，尽量把请求缩到阈值内
        match adaptive_shrink_request_body(
            &mut kiro_request,
            &compression,
            max_body,
            &mut request_body,
        ) {
            Ok(Some(outcome)) => {
                tracing::warn!(
                    conversation_id = kiro_request.conversation_state.conversation_id.as_str(),
                    initial_bytes = outcome.initial_bytes,
                    final_bytes = outcome.final_bytes,
                    threshold = max_body,
                    iters = outcome.iters,
                    additional_history_turns_removed = outcome.additional_history_turns_removed,
                    final_tool_result_max_chars = outcome.final_tool_result_max_chars,
                    final_tool_use_input_max_chars = outcome.final_tool_use_input_max_chars,
                    final_message_content_max_chars = outcome.final_message_content_max_chars,
                    "请求体超过阈值，已执行自适应二次压缩"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("自适应二次压缩序列化失败: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new(
                        "internal_error",
                        format!("序列化请求失败: {}", e),
                    )),
                )
                    .into_response();
            }
        }
    }

    // 压缩后再次检查（输出 image_bytes/non-image bytes 便于排查）
    let final_img_bytes = total_image_bytes(&kiro_request);
    let final_effective_len = request_body.len().saturating_sub(final_img_bytes);
    if max_body > 0 && request_body.len() > max_body {
        tracing::warn!(
            conversation_id = kiro_request.conversation_state.conversation_id.as_str(),
            request_body_bytes = request_body.len(),
            image_bytes = final_img_bytes,
            effective_bytes = final_effective_len,
            threshold = max_body,
            "请求体超过安全阈值，拒绝发送"
        );
        #[cfg(feature = "sensitive-logs")]
        tracing::error!(
            "自适应压缩仍超限，完整请求体（用于诊断）: {}",
            truncate_base64_in_request_body(&request_body)
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "Request too large ({} bytes total; images {} bytes; non-image {} bytes; limit {}). Reduce conversation history/tool output or number/size of images.",
                    request_body.len(),
                    final_img_bytes,
                    final_effective_len,
                    max_body
                ),
            )),
        )
            .into_response();
    }

    tracing::debug!(
        kiro_request_body_bytes = request_body.len(),
        "已构建 Kiro 请求体"
    );
    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens（贴 BK：clone 让 payload 后续仍可用于 build_cache_profile）
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    let user_id = payload
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref());

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            user_id,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        // 注：post_messages_cc 路径目前不接 cache_tracker（xkiro 独有 Claude Code 端点），
        //     待后续按需打开缓存计费时再传入 prompt_cache_snapshot
        let non_stream_request = NonStreamRequestContext {
            request_body: &request_body,
            model: &payload.model,
            input_tokens,
            thinking_enabled: extract_thinking,
            tool_name_map,
            user_id,
            cache_tracker: None,
            cache_profile: None,
        };
        handle_non_stream_request(provider, non_stream_request).await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    user_id: Option<&str>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_result = match provider.call_api_stream(request_body, user_id).await {
        Ok(resp) => resp,
        Err(e) => return map_kiro_provider_error_to_response(request_body, e),
    };
    let response = api_result.response;
    let _credential_id = api_result.credential_id;

    // 创建缓冲流处理上下文
    let ctx = BufferedStreamContext::new(
        model,
        estimated_input_tokens,
        thinking_enabled,
        tool_name_map,
    );

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(response, ctx);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 发生错误，完成处理并返回所有事件
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

/// 字符级中段截断 + 占位符（用于敏感日志的缩略输出）。
///
/// 按字符计数避免 UTF-8 字节切到边界报错；当字符数 ≤ keep*2+30 时返回原串。
#[cfg(feature = "sensitive-logs")]
fn truncate_middle(s: &str, keep: usize) -> std::borrow::Cow<'_, str> {
    // 按字符数计算，避免截断后反而更长
    let char_count = s.chars().count();
    let min_omit = 30; // 省略号 + 数字的最小开销，确保截断有意义
    if char_count <= keep * 2 + min_omit {
        return std::borrow::Cow::Borrowed(s);
    }

    // 找到第 keep 个字符的字节边界
    let head_end = s
        .char_indices()
        .nth(keep)
        .map(|(i, _)| i)
        .unwrap_or(s.len());

    // 找到倒数第 keep 个字符的字节边界
    let tail_start = s
        .char_indices()
        .nth_back(keep - 1)
        .map(|(i, _)| i)
        .unwrap_or(0);

    let omitted = s.len() - head_end - (s.len() - tail_start);
    std::borrow::Cow::Owned(format!(
        "{}...({} bytes omitted)...{}",
        &s[..head_end],
        omitted,
        &s[tail_start..]
    ))
}

/// sensitive-logs 模式下输出完整请求体，但截断 base64 图片数据。
///
/// 图片 base64 数据对诊断 400 错误没有价值，但可能占几十 KB。
/// 扫描 `"bytes":"<base64...>"` 模式，将长 base64 替换为占位符。
#[cfg(feature = "sensitive-logs")]
fn truncate_base64_in_request_body(s: &str) -> std::borrow::Cow<'_, str> {
    const MARKER: &str = r#""bytes":""#;
    const MIN_BASE64_LEN: usize = 200;

    // 快速路径：没有 "bytes":" 就直接返回
    if !s.contains(MARKER) {
        return std::borrow::Cow::Borrowed(s);
    }

    let mut result = String::with_capacity(s.len());
    let mut pos = 0;
    let bytes = s.as_bytes();

    while pos < bytes.len() {
        if let Some(offset) = s[pos..].find(MARKER) {
            let marker_start = pos + offset;
            let value_start = marker_start + MARKER.len();

            // 找到闭合引号（处理转义）
            let mut end = value_start;
            let mut escaped = false;
            while end < bytes.len() {
                if escaped {
                    escaped = false;
                    end += 1;
                    continue;
                }
                match bytes[end] {
                    b'\\' => {
                        escaped = true;
                        end += 1;
                    }
                    b'"' => break,
                    _ => end += 1,
                }
            }

            let value_len = end - value_start;
            if value_len >= MIN_BASE64_LEN && is_likely_base64(&s[value_start..end]) {
                result.push_str(&s[pos..value_start]);
                result.push_str(&format!("<BASE64_TRUNCATED:{}>", value_len));
                pos = end; // 跳到闭合引号，下一轮会输出它
            } else {
                // 不是 base64 或太短，原样保留
                result.push_str(&s[pos..value_start]);
                pos = value_start;
            }
        } else {
            result.push_str(&s[pos..]);
            break;
        }
    }

    std::borrow::Cow::Owned(result)
}

/// 判断字符串前 100 字节是否像 base64（仅 ASCII 字母数字 + `+/=`）。
#[cfg(feature = "sensitive-logs")]
fn is_likely_base64(s: &str) -> bool {
    s.bytes()
        .take(100)
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::{Message, SystemMessage};
    use crate::kiro::model::requests::conversation::{
        ConversationState, CurrentMessage, KiroImage, Message as KiroMessage, UserInputMessage,
    };

    fn sample_messages_request() -> MessagesRequest {
        // 生成一个超过 1024 tokens 的 system message 用于测试缓存
        let long_text = "This is a test system message. ".repeat(100); // 约 600 tokens
        let very_long_text = format!("{}{}", long_text, long_text); // 约 1200 tokens

        MessagesRequest {
            model: "claude-sonnet-4-thinking".to_string(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "hello raw"},
                        {"type": "text", "text": ""}
                    ]),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("prefill that convert will drop"),
                },
            ],
            stream: false,
            system: Some(vec![SystemMessage {
                text: very_long_text,
                block_type: Some("text".to_string()),
                cache_control: Some(crate::anthropic::types::CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            }]),
            tools: Some(vec![crate::anthropic::types::Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: "search web".to_string(),
                input_schema: std::collections::HashMap::new(),
                max_uses: Some(1),
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    #[test]
    fn test_cache_context_uses_raw_system_tokens() {
        let payload = sample_messages_request();

        let cache_tracker =
            crate::anthropic::cache_tracker::CacheTracker::new(std::time::Duration::from_secs(300));

        // 计算实际的 system message tokens
        let system_text = &payload.system.as_ref().unwrap()[0].text;
        let expected = token::count_tokens(system_text) as i32;

        let cache_profile = build_cache_profile(&cache_tracker, &payload, expected);
        let cache_context = compute_cache_usage(&cache_tracker, 0, &cache_profile);

        // 验证 cache_creation_input_tokens 等于 system message 的 token 数
        assert_eq!(cache_context.cache_creation_input_tokens, expected);
        assert_eq!(cache_context.cache_read_input_tokens, 0);
    }

    #[test]
    fn test_resolved_cache_usage_uses_real_credential_id() {
        let payload = sample_messages_request();
        let estimated = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;
        let cache_tracker =
            crate::anthropic::cache_tracker::CacheTracker::new(std::time::Duration::from_secs(300));
        let cache_profile = build_cache_profile(&cache_tracker, &payload, estimated);

        let provisional = provisional_cache_usage(&cache_tracker, &cache_profile);
        assert_eq!(provisional.cache_read_input_tokens, 0);

        cache_tracker.update(42, &cache_profile);
        let resolved = resolved_cache_usage(&cache_tracker, 42, &cache_profile);

        assert!(resolved.cache_read_input_tokens > 0);
        assert!(resolved.cache_creation_input_tokens <= provisional.cache_creation_input_tokens);
    }

    #[test]
    fn test_billed_input_tokens_subtracts_cache_tokens() {
        assert_eq!(billed_input_tokens(3829, 0, 1788), 2041);
        assert_eq!(billed_input_tokens(4131, 544, 2544), 1043);
        assert_eq!(billed_input_tokens(10, 3, 20), 0);
    }

    #[test]
    fn test_non_stream_usage_uses_estimated_input_tokens_as_base() {
        let estimated_input_tokens = 1493;
        let upstream_context_input_tokens = 3106;
        let cache_creation_input_tokens = 9;
        let cache_read_input_tokens = 1480;

        let final_input_tokens = estimated_input_tokens;
        let billed = billed_input_tokens(
            final_input_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        );

        assert_eq!(final_input_tokens, 1493);
        assert_eq!(upstream_context_input_tokens, 3106);
        assert_eq!(billed, 4);
        assert_ne!(final_input_tokens, upstream_context_input_tokens);
    }

    #[test]
    fn test_inject_cache_usage_fields_only_for_cc_usage() {
        let mut usage = serde_json::json!({
            "input_tokens": 123,
            "output_tokens": 45
        });

        inject_cache_usage_fields(
            &mut usage,
            CacheUsageContext {
                cache_creation_input_tokens: 7,
                cache_read_input_tokens: 8,
                cache_creation_5m_input_tokens: 3,
                cache_creation_1h_input_tokens: 4,
            },
        );

        assert_eq!(usage["cache_creation_input_tokens"], 7);
        assert_eq!(usage["cache_read_input_tokens"], 8);
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 3);
        assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 4);
    }

    #[test]
    fn test_inject_credit_usage_fields_appends_metering_usage() {
        let mut usage = serde_json::json!({
            "input_tokens": 123,
            "output_tokens": 45,
            "cache_creation_input_tokens": 7,
            "cache_read_input_tokens": 8
        });

        inject_credit_usage_fields(
            &mut usage,
            &MeteringEvent {
                unit: "credit".to_string(),
                unit_plural: "credits".to_string(),
                usage: 0.5,
            },
        );

        assert_eq!(usage["input_tokens"], 123);
        assert_eq!(usage["cache_creation_input_tokens"], 7);
        assert_eq!(usage["cache_read_input_tokens"], 8);
        assert_eq!(usage["credit_usage"], json!(0.5));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
    }

    #[test]
    fn test_is_no_credentials_error() {
        let err = anyhow::anyhow!("没有可用的凭据");
        assert!(is_no_credentials_error(&err));

        let err = anyhow::anyhow!("所有凭据已用尽");
        assert!(!is_no_credentials_error(&err));
    }

    #[test]
    fn test_is_quota_exhausted_error() {
        let err = anyhow::anyhow!("流式 API 请求失败（所有凭据已用尽）: 429 Quota exceeded");
        assert!(is_quota_exhausted_error(&err));

        let err = anyhow::anyhow!("没有可用的凭据（可用: 0/0），请添加或启用凭据后重试");
        assert!(!is_quota_exhausted_error(&err));
    }

    #[test]
    fn test_adaptive_shrink_removes_only_history_images() {
        let big = "A".repeat(20_000);
        let mut kiro_request = KiroRequest {
            conversation_state: ConversationState::new("conv-1")
                .with_current_message(CurrentMessage::new(
                    UserInputMessage::new("current", "model")
                        .with_images(vec![KiroImage::from_base64("png", big.clone())]),
                ))
                .with_history(vec![KiroMessage::user("history", "model")]),
            profile_arn: None,
        };
        if let KiroMessage::User(user) = &mut kiro_request.conversation_state.history[0] {
            user.user_input_message.images = vec![KiroImage::from_base64("png", big.clone())];
        }

        let removed = kiro_request.conversation_state.remove_history_images();

        assert_eq!(removed, 1);
        assert_eq!(
            kiro_request
                .conversation_state
                .current_message
                .user_input_message
                .images
                .len(),
            1
        );
        assert!(match &kiro_request.conversation_state.history[0] {
            KiroMessage::User(user) => user.user_input_message.images.is_empty(),
            _ => false,
        });
    }

    #[test]
    fn test_improperly_formed_request_message_mentions_common_causes() {
        let response = map_kiro_provider_error_to_response(
            "{}",
            anyhow::anyhow!("400 Improperly formed request"),
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
