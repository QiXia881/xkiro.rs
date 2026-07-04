//! Anthropic API Handler 函数

use std::convert::Infallible;

use crate::kiro::model::events::{Event, MeteringEvent};
use crate::kiro::model::requests::conversation::{HistoryUserMessage, Message as KiroMessage};
use crate::kiro::model::requests::kiro::{InferenceConfig, KiroRequest};
use crate::kiro::models::AvailableModel;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use anyhow::Error;
use axum::{
    Extension, Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde::Serialize;
use serde_json::json;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{
    ConversionError, convert_request_with_thinking_suffix, extract_session_id,
    generate_thinking_prefix, map_model_with_thinking_suffix,
};
use super::middleware::{AppState, MatchedApiKeyId};
use super::stream::{BufferedStreamContext, CacheUsageBreakdown, SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model,
    ModelCapabilities, ModelInfo, ModelInfoCapabilities, ModelInfoMeta, ModelModalities,
    ModelsResponse, OutputConfig, SystemMessage, Thinking,
};
use super::websearch;
use crate::model::claude::native_claude_model_id;
use crate::model::config::SystemPromptPosition;
use crate::model::runtime::SharedPromptConfig;

const PAYLOAD_TRUNCATION_MIN_RECENT_MESSAGES: usize = 4;
const PAYLOAD_TRUNCATION_PLACEHOLDER: &str = "[Earlier conversation history was truncated to fit the model's input limit. Older messages and tool activity have been omitted.]";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicStatsResponse {
    status: String,
    version: String,
    #[serde(rename = "accounts")]
    credentials_total_alias: usize,
    #[serde(rename = "available")]
    credentials_available_alias: usize,
    credentials_total: usize,
    credentials_available: usize,
    total_requests: i64,
    success_requests: i64,
    failed_requests: i64,
    total_tokens: i64,
    total_credits: f64,
    uptime: u64,
}

// ============================================================================
// Cache usage 工具集
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
struct StreamRequestContext<'a> {
    app_state: AppState,
    api_key_id: Option<String>,
    cache_tracker: Option<&'a std::sync::Arc<crate::anthropic::cache_tracker::CacheTracker>>,
    cache_profile: Option<&'a crate::anthropic::cache_tracker::CacheProfile>,
    request_body: &'a str,
    model: &'a str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    user_id: Option<&'a str>,
    claude_format: String,
}

/// 非流式请求上下文（同上）
struct NonStreamRequestContext<'a> {
    app_state: AppState,
    api_key_id: Option<String>,
    request_body: &'a str,
    model: &'a str,
    input_tokens: i32,
    thinking_enabled: bool,
    thinking_display_omitted: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    user_id: Option<&'a str>,
    cache_tracker: Option<&'a std::sync::Arc<crate::anthropic::cache_tracker::CacheTracker>>,
    cache_profile: Option<&'a crate::anthropic::cache_tracker::CacheProfile>,
    claude_format: String,
}

#[derive(Debug, Clone)]
struct NonStreamToolUseState {
    tool_use_id: String,
    name: String,
    input_buffer: String,
    generated_id: bool,
}

/// 从 payload + 总输入 token 构造 cache 画像
///
/// 内部薄封装，让上层调用 `cache_tracker.build_profile(...)` 时不必直接持有
/// cache_tracker 模块类型，便于后续替换实现。
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

fn handle_non_stream_tool_use_event(
    tool_use: crate::kiro::model::events::ToolUseEvent,
    current: &mut Option<NonStreamToolUseState>,
    output: &mut Vec<serde_json::Value>,
    tool_name_map: &std::collections::HashMap<String, String>,
) {
    let incoming_id = tool_use.tool_use_id.clone();
    let incoming_name = tool_use.name.clone();

    if !incoming_id.is_empty() && !incoming_name.is_empty() {
        match current {
            None => {
                *current = Some(NonStreamToolUseState {
                    tool_use_id: incoming_id,
                    name: incoming_name,
                    input_buffer: String::new(),
                    generated_id: false,
                });
            }
            Some(state) if state.tool_use_id != incoming_id => {
                if state.generated_id && state.name == incoming_name {
                    state.tool_use_id = incoming_id;
                    state.generated_id = false;
                } else {
                    finish_non_stream_tool_use(current, output, tool_name_map);
                    *current = Some(NonStreamToolUseState {
                        tool_use_id: incoming_id,
                        name: incoming_name,
                        input_buffer: String::new(),
                        generated_id: false,
                    });
                }
            }
            Some(_) => {}
        }
    } else if !incoming_name.is_empty() {
        match current {
            None => {
                *current = Some(NonStreamToolUseState {
                    tool_use_id: crate::kiro::model::events::ToolUseEvent::generate_fallback_id(),
                    name: incoming_name,
                    input_buffer: String::new(),
                    generated_id: true,
                });
            }
            Some(state) if state.name != incoming_name => {
                finish_non_stream_tool_use(current, output, tool_name_map);
                *current = Some(NonStreamToolUseState {
                    tool_use_id: crate::kiro::model::events::ToolUseEvent::generate_fallback_id(),
                    name: incoming_name,
                    input_buffer: String::new(),
                    generated_id: true,
                });
            }
            Some(_) => {}
        }
    }

    if let Some(state) = current.as_mut() {
        tool_use.apply_input_to_buffer(&mut state.input_buffer);
    }

    if tool_use.stop {
        finish_non_stream_tool_use(current, output, tool_name_map);
    }
}

fn finish_non_stream_tool_use(
    current: &mut Option<NonStreamToolUseState>,
    output: &mut Vec<serde_json::Value>,
    tool_name_map: &std::collections::HashMap<String, String>,
) {
    let Some(state) = current.take() else {
        return;
    };
    if state.name.is_empty() {
        return;
    }

    let input: serde_json::Value = if state.input_buffer.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&state.input_buffer).unwrap_or_else(|e| {
            tracing::warn!(
                "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                e,
                state.tool_use_id
            );
            serde_json::json!({})
        })
    };

    let original_name = tool_name_map
        .get(&state.name)
        .cloned()
        .unwrap_or_else(|| state.name.clone());

    output.push(json!({
        "type": "tool_use",
        "id": state.tool_use_id,
        "name": original_name,
        "input": input
    }));
}

fn append_non_stream_assistant_delta(
    text_content: &mut String,
    raw_content: &str,
    previous: &mut String,
) {
    let delta = crate::common::text::normalize_chunk(raw_content, previous);
    text_content.push_str(&delta);
}

fn append_non_stream_reasoning_delta(
    thinking_content: &mut String,
    raw_text: &str,
    previous: &mut String,
) {
    let delta = crate::common::text::normalize_chunk(raw_text, previous);
    if !delta.is_empty() {
        thinking_content.push_str(&delta);
    }
}

fn build_non_stream_content_blocks(
    text_content: &str,
    raw_thinking_content: &str,
    mut tool_uses: Vec<serde_json::Value>,
    thinking_enabled: bool,
    thinking_display_omitted: bool,
    claude_format: &str,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();

    if thinking_enabled {
        let (extracted_thinking, mut final_text) =
            super::stream::extract_thinking_from_complete_text(text_content);
        let mut response_thinking = raw_thinking_content.to_string();
        if response_thinking.is_empty()
            && let Some(extracted) = extracted_thinking
        {
            response_thinking = extracted;
        }

        if thinking_display_omitted && !response_thinking.is_empty() {
            content.push(json!({
                "type": "thinking",
                "thinking": "",
                "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER,
            }));
        } else if !response_thinking.is_empty() {
            match claude_format {
                "think" => {
                    final_text = format!("<think>{}</think>{}", response_thinking, final_text);
                }
                "reasoning_content" => {
                    final_text = format!("{}{}", response_thinking, final_text);
                }
                _ => {
                    content.push(json!({
                        "type": "thinking",
                        "thinking": response_thinking,
                        "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER,
                    }));
                }
            }
        }

        if !final_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": final_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.append(&mut tool_uses);
    content
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PayloadTruncationOutcome {
    pub(crate) initial_bytes: usize,
    pub(crate) final_bytes: usize,
    pub(crate) removed_history_messages: usize,
    pub(crate) inserted_placeholder: bool,
}

// ============================================================================
// 错误分类谓词
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

/// 判断是否是凭据队列等待超时错误（per-credential 并发限流命中后等待超时）
///
/// `wait_any_credential` 在所有候选凭据都达到并发上限、且全员排队仍超过
/// `acquireWaitTimeoutSecs` 时，会返回固定 sentinel 字符串 "credential queue wait timeout"。
/// 此类错误属于服务侧暂时过载，应返回 429 overloaded_error 让客户端做退避重试，
/// 而非 5xx 普通故障（避免 panic）也不应当作 quota_exhausted（含义不同）。
fn is_credential_queue_timeout_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("credential queue wait timeout")
}

/// 上游 provider 错误的语义分类。
///
/// Anthropic 与 OpenAI 两条协议路径共用同一分类逻辑（同一组 sentinel 谓词、
/// 同一判定顺序），各自渲染自己的 error body/code/日志与状态码——渲染侧的差异
/// （如 OpenAI 的 `context_length_exceeded` code、queue-timeout 的
/// `overloaded_error` vs `rate_limit_error`）是有意保留的。
///
/// 用 enum 承载分类：新增分类时 `match` 穷尽性会强制两侧渲染同步，消除
/// 此前 OpenAI 内联字符串匹配与 Anthropic 谓词各写一份带来的漂移风险（A3）。
pub(crate) enum ProviderErrorClass {
    /// 输入上下文过长（CONTENT_LENGTH_EXCEEDS_THRESHOLD / Input is too long）
    InputTooLong,
    /// 请求格式错误（Improperly formed request）
    ImproperlyFormed,
    /// 无可用凭据（没有可用的凭据）
    NoCredentials,
    /// 凭据队列等待超时（per-credential 并发满，credential queue wait timeout）
    QueueTimeout,
    /// 所有凭据配额耗尽（所有凭据已用尽）
    QuotaExhausted,
    /// 上游瞬态网络错误（连接类：error sending request / connection closed/reset）
    TransientNetwork,
    /// 上游瞬态错误（429/5xx 等，非网络）
    TransientUpstream,
    /// 其他未分类错误（→ 502 api_error）
    Unclassified,
}

/// 把 provider 的 `anyhow::Error` 归类为 [`ProviderErrorClass`]。
///
/// 判定顺序与既有渲染完全一致，谓词沿用现有 `is_*_error`，保证零行为变化。
pub(crate) fn classify_provider_error(err: &Error) -> ProviderErrorClass {
    if is_input_too_long_error(err) {
        return ProviderErrorClass::InputTooLong;
    }
    if is_improperly_formed_request_error(err) {
        return ProviderErrorClass::ImproperlyFormed;
    }
    if is_no_credentials_error(err) {
        return ProviderErrorClass::NoCredentials;
    }
    if is_credential_queue_timeout_error(err) {
        return ProviderErrorClass::QueueTimeout;
    }
    if is_quota_exhausted_error(err) {
        return ProviderErrorClass::QuotaExhausted;
    }
    if is_transient_upstream_error(err) {
        let s = err.to_string().to_lowercase();
        if is_network_error(&s) {
            return ProviderErrorClass::TransientNetwork;
        }
        return ProviderErrorClass::TransientUpstream;
    }
    ProviderErrorClass::Unclassified
}

fn anthropic_inference_config(req: &MessagesRequest) -> Option<InferenceConfig> {
    let max_tokens = (req.max_tokens > 0).then_some(req.max_tokens);
    let temperature = req.temperature.filter(|v| *v > 0.0);
    let top_p = req.top_p.filter(|v| *v > 0.0);

    if max_tokens.is_none() && temperature.is_none() && top_p.is_none() {
        return None;
    }

    Some(InferenceConfig {
        max_tokens,
        temperature,
        top_p,
    })
}

pub(crate) fn truncate_payload_to_body_limit(
    kiro_request: &mut KiroRequest,
    max_body: usize,
    request_body: &mut String,
    has_system_priming: bool,
) -> Result<Option<PayloadTruncationOutcome>, serde_json::Error> {
    if max_body == 0 || request_body.len() <= max_body {
        return Ok(None);
    }

    let initial_bytes = request_body.len();
    let current_model_id = kiro_request
        .conversation_state
        .current_message
        .user_input_message
        .model_id
        .clone();

    let history = std::mem::take(&mut kiro_request.conversation_state.history);
    let priming_count = if has_system_priming {
        system_priming_count(&history)
    } else {
        0
    };
    let (priming, conversation) = history.split_at(priming_count);
    let priming = priming.to_vec();
    let conversation = conversation.to_vec();

    let placeholder = KiroMessage::User(HistoryUserMessage::new(
        PAYLOAD_TRUNCATION_PLACEHOLDER,
        current_model_id,
    ));
    let entry_sizes: Vec<usize> = conversation
        .iter()
        .map(serialized_history_entry_size)
        .collect::<Result<_, _>>()?;

    kiro_request.conversation_state.history = priming.clone();
    let base_size =
        serde_json::to_string(kiro_request)?.len() + serialized_history_entry_size(&placeholder)?;

    let mut keep_from = conversation.len();
    let mut running = base_size;
    for i in (0..conversation.len()).rev() {
        running += entry_sizes[i];
        let kept = conversation.len() - i;
        if running > max_body && kept > PAYLOAD_TRUNCATION_MIN_RECENT_MESSAGES {
            break;
        }
        keep_from = i;
    }

    let mut tail = drop_leading_assistant(conversation[keep_from..].to_vec());
    let removed_history_messages = keep_from;
    let inserted_placeholder = removed_history_messages > 0;

    let mut rebuilt =
        Vec::with_capacity(priming.len() + usize::from(inserted_placeholder) + tail.len());
    rebuilt.extend(priming);
    if inserted_placeholder {
        rebuilt.push(placeholder);
    }
    rebuilt.append(&mut tail);
    kiro_request.conversation_state.history = rebuilt;

    *request_body = serde_json::to_string(kiro_request)?;

    if request_body.len() > max_body {
        truncate_current_message_to_fit(kiro_request, max_body)?;
        *request_body = serde_json::to_string(kiro_request)?;
    }

    Ok(Some(PayloadTruncationOutcome {
        initial_bytes,
        final_bytes: request_body.len(),
        removed_history_messages,
        inserted_placeholder,
    }))
}

/// [`serialize_and_fit_body`] 的失败分类。
///
/// 保持协议中立：caller 各自把它映射到自己的 error-response 类型
/// （Anthropic `ErrorResponse` / OpenAI `OpenAIErrorResponse`），
/// `TooLarge` 携带 body 回传以支持 anthropic 侧的 sensitive-logs 诊断。
pub(crate) enum BodyFitError {
    /// 序列化失败（初次或截断后重序列化）。
    Serialize(serde_json::Error),
    /// 安全截断后仍超过上游硬限制。
    TooLarge {
        body: String,
        bytes: usize,
        limit: usize,
    },
}

/// 序列化 Kiro 请求体并使其满足上游硬性大小限制。
///
/// serialize → (超限则)安全截断历史 → recheck 的公共管道，
/// Anthropic 与 OpenAI 两条 prepare 路径共用，避免大小限制逻辑漂移（ER-2）。
/// `log_label` 仅用于截断成功时的 operator 日志前缀（非协议可观测）。
pub(crate) fn serialize_and_fit_body(
    kiro_request: &mut KiroRequest,
    max_body: usize,
    has_system_priming: bool,
    log_label: &str,
) -> Result<String, BodyFitError> {
    let mut request_body = serde_json::to_string(kiro_request).map_err(BodyFitError::Serialize)?;

    // 请求体大小预检（上游存在硬性请求体大小限制；按实际序列化后的总字节数判断）
    if max_body > 0 && request_body.len() > max_body {
        match truncate_payload_to_body_limit(
            kiro_request,
            max_body,
            &mut request_body,
            has_system_priming,
        ) {
            Ok(Some(outcome)) => {
                tracing::warn!(
                    conversation_id = kiro_request.conversation_state.conversation_id.as_str(),
                    initial_bytes = outcome.initial_bytes,
                    final_bytes = outcome.final_bytes,
                    threshold = max_body,
                    removed_history_messages = outcome.removed_history_messages,
                    inserted_placeholder = outcome.inserted_placeholder,
                    "{}请求体超过阈值，已按安全截断策略截断历史",
                    log_label
                );
            }
            Ok(None) => {}
            Err(e) => return Err(BodyFitError::Serialize(e)),
        }
    }

    // 安全截断后仍超限，说明当前消息/工具/图片本身已超过上游限制。
    if max_body > 0 && request_body.len() > max_body {
        return Err(BodyFitError::TooLarge {
            bytes: request_body.len(),
            limit: max_body,
            body: request_body,
        });
    }

    Ok(request_body)
}

fn system_priming_count(history: &[KiroMessage]) -> usize {
    if history.len() < 2 {
        return 0;
    }
    match (&history[0], &history[1]) {
        (KiroMessage::User(_), KiroMessage::Assistant(a))
            if a.assistant_response_message
                .content
                .trim()
                .eq_ignore_ascii_case("I will follow these instructions.") =>
        {
            2
        }
        _ => 0,
    }
}

fn serialized_history_entry_size(entry: &KiroMessage) -> Result<usize, serde_json::Error> {
    Ok(serde_json::to_string(entry)?.len() + 1)
}

fn drop_leading_assistant(mut tail: Vec<KiroMessage>) -> Vec<KiroMessage> {
    let first_user = tail
        .iter()
        .position(|msg| matches!(msg, KiroMessage::User(_)))
        .unwrap_or(tail.len());
    if first_user > 0 {
        tail.drain(0..first_user);
    }
    tail
}

fn truncate_current_message_to_fit(
    kiro_request: &mut KiroRequest,
    max_body: usize,
) -> Result<(), serde_json::Error> {
    let current_len = kiro_request
        .conversation_state
        .current_message
        .user_input_message
        .content
        .len();
    let body_len = serde_json::to_string(kiro_request)?.len();
    let overhead = body_len.saturating_sub(current_len);
    let budget = max_body.saturating_sub(overhead);
    let content = &mut kiro_request
        .conversation_state
        .current_message
        .user_input_message
        .content;
    truncate_string_to_byte_budget(content, budget);
    Ok(())
}

fn truncate_string_to_byte_budget(content: &mut String, budget: usize) {
    if content.len() <= budget {
        return;
    }
    if budget == 0 {
        content.clear();
        content.push('.');
        return;
    }
    let cut = content
        .char_indices()
        .map(|(idx, _)| idx)
        .take_while(|idx| *idx <= budget)
        .last()
        .unwrap_or(0);
    content.truncate(cut);
    if content.is_empty() {
        content.push('.');
    }
}

/// 将 KiroProvider 错误映射为 HTTP 响应
///
/// 分类走共享的 [`classify_provider_error`]，本函数只负责 Anthropic 风格的渲染
/// （body/日志/状态码）——与 OpenAI 侧的渲染差异是有意的（见 `ProviderErrorClass`）。
fn map_kiro_provider_error_to_response(request_body: &str, err: Error) -> Response {
    match classify_provider_error(&err) {
        ProviderErrorClass::InputTooLong => {
            tracing::warn!(
                kiro_request_body_bytes = request_body.len(),
                error = %err,
                "上游拒绝请求：输入上下文过长（不应重试）"
            );
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "Input is too long (CONTENT_LENGTH_EXCEEDS_THRESHOLD). Reduce conversation history/system/tools; retrying the same request will not help.",
                )),
            )
                .into_response()
        }
        ProviderErrorClass::ImproperlyFormed => {
            tracing::warn!(
                error = %err,
                kiro_request_body_bytes = request_body.len(),
                "上游拒绝请求：请求格式错误（可能由超大请求体、消息/工具序列异常或空内容块导致）"
            );
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "Improperly formed request. This is often caused by oversized payloads, malformed message/tool sequences, or empty content blocks.",
                )),
            )
                .into_response()
        }
        ProviderErrorClass::NoCredentials => {
            tracing::error!(error = %err, "没有可用的凭据");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "No credentials available. Please add or enable credentials via Admin API or credentials.json.",
                )),
            )
                .into_response()
        }
        ProviderErrorClass::QueueTimeout => {
            tracing::warn!(error = %err, "凭据队列等待超时（per-credential 并发已满）");
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse::new(
                    "overloaded_error",
                    "All credentials are busy. Please retry shortly.",
                )),
            )
                .into_response()
        }
        ProviderErrorClass::QuotaExhausted => {
            tracing::warn!(error = %err, "所有凭据配额已耗尽");
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse::new(
                    "rate_limit_error",
                    "All credentials quota exhausted. Please wait for quota reset or add new credentials.",
                )),
            )
                .into_response()
        }
        ProviderErrorClass::TransientNetwork => {
            tracing::warn!(error = %err, "上游网络错误，不输出请求体");
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("上游网络错误: {}", err),
                )),
            )
                .into_response()
        }
        ProviderErrorClass::TransientUpstream => {
            tracing::warn!(error = %err, "上游瞬态错误（429/5xx），不输出请求体");
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse::new("rate_limit_error", err.to_string())),
            )
                .into_response()
        }
        ProviderErrorClass::Unclassified => {
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
    }
}

/// 对日志/审计中的 user_id 做脱敏
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
/// - 空 text block 不携带任何语义，直接移除是最小且安全的清理策略。
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
pub async fn get_models(State(state): State<AppState>) -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let thinking_suffix = state.thinking_config.read().suffix.clone();
    let mut cached = state.models_cache.read().clone();
    if cached.is_empty() {
        refresh_models_cache(&state).await;
        cached = state.models_cache.read().clone();
    }

    let mut models = build_anthropic_models_response(&cached, &thinking_suffix);
    if models.is_empty() {
        models = fallback_anthropic_models(&thinking_suffix);
    }
    models.extend(alias_models());

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// GET /v1/stats
///
/// 返回 API-key 保护的公开网关统计。
pub async fn get_public_stats(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.gateway_stats_snapshot();
    let (credentials_total, credentials_available) = state
        .kiro_provider
        .as_ref()
        .map(|provider| {
            let snapshot = provider.token_manager().snapshot();
            (snapshot.total, snapshot.available)
        })
        .unwrap_or((0, 0));

    Json(PublicStatsResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        credentials_total_alias: credentials_total,
        credentials_available_alias: credentials_available,
        credentials_total,
        credentials_available,
        total_requests: stats.total_requests,
        success_requests: stats.success_requests,
        failed_requests: stats.failed_requests,
        total_tokens: stats.total_tokens,
        total_credits: stats.total_credits,
        uptime: stats.uptime,
    })
}

fn default_models_response(thinking_suffix: &str) -> Vec<Model> {
    let mut models = fallback_anthropic_models(thinking_suffix);
    models.extend(alias_models());
    models
}

async fn refresh_models_cache(state: &AppState) {
    let Some(provider) = &state.kiro_provider else {
        return;
    };

    let token_manager = provider.token_manager();
    let snapshot = token_manager.snapshot();
    let mut aggregated = Vec::new();

    for entry in snapshot.entries {
        if entry.disabled || entry.auth_method.as_deref() == Some("api_key") {
            continue;
        }

        match token_manager
            .list_available_models_for(entry.id, None)
            .await
        {
            Ok(response) => {
                aggregated = merge_unique_models(aggregated, response.available_models);
            }
            Err(err) => {
                tracing::warn!(
                    credential_id = entry.id,
                    error = %err,
                    "[ModelsCache] Failed to refresh models"
                );
            }
        }
    }

    if !aggregated.is_empty() {
        tracing::info!("[ModelsCache] Cached {} models", aggregated.len());
        *state.models_cache.write() = aggregated;
    }
}

fn alias_models() -> [Model; 3] {
    [
        build_model_info("auto", "kiro-proxy", true),
        build_model_info("gpt-4o", "kiro-proxy", true),
        build_model_info("gpt-4", "kiro-proxy", true),
    ]
}

fn fallback_anthropic_models(thinking_suffix: &str) -> Vec<Model> {
    [
        "claude-sonnet-4-6",
        "claude-opus-4-6",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-sonnet-4-5",
        "claude-sonnet-4",
        "claude-haiku-4-5",
        "claude-opus-4-5",
    ]
    .into_iter()
    .flat_map(|id| {
        [
            build_model_info(id, "anthropic", true),
            build_model_info(format!("{id}{thinking_suffix}"), "anthropic", true),
        ]
    })
    .collect()
}

fn build_anthropic_models_response(cached: &[AvailableModel], thinking_suffix: &str) -> Vec<Model> {
    if cached.is_empty() {
        return Vec::new();
    }

    cached
        .iter()
        .flat_map(|model| {
            let supports_image = model_supports_image(&model.supported_input_types);
            let model_id = native_claude_model_id(&model.model_id);
            [
                build_model_info(&model_id, "anthropic", supports_image),
                build_model_info(
                    format!("{}{}", model_id, thinking_suffix),
                    "anthropic",
                    supports_image,
                ),
            ]
        })
        .collect()
}

fn merge_unique_models(
    existing: Vec<AvailableModel>,
    incoming: Vec<AvailableModel>,
) -> Vec<AvailableModel> {
    if incoming.is_empty() {
        return existing;
    }

    let mut merged = existing;
    let mut index_by_id = std::collections::HashMap::with_capacity(merged.len());
    for (index, model) in merged.iter().enumerate() {
        let key = model.model_id.trim().to_lowercase();
        if !key.is_empty() {
            index_by_id.insert(key, index);
        }
    }

    for model in incoming {
        let key = model.model_id.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }

        if let Some(index) = index_by_id.get(&key).copied() {
            merged[index] = merge_model_info(merged[index].clone(), model);
            continue;
        }

        index_by_id.insert(key, merged.len());
        merged.push(model);
    }

    merged
}

fn merge_model_info(mut base: AvailableModel, extra: AvailableModel) -> AvailableModel {
    if base.model_name.is_empty() {
        base.model_name = extra.model_name;
    }
    if base.description.is_empty() {
        base.description = extra.description;
    }
    if base.provider.is_none() {
        base.provider = extra.provider;
    }
    if base.context_window.is_none() {
        base.context_window = extra.context_window;
    }
    if base.is_default.is_none() {
        base.is_default = extra.is_default;
    }
    if base.rate_multiplier.is_none() {
        base.rate_multiplier = extra.rate_multiplier;
    }
    if base.rate_unit.is_none() {
        base.rate_unit = extra.rate_unit;
    }
    if base.prompt_caching.is_none() {
        base.prompt_caching = extra.prompt_caching;
    }
    if base.token_limits.is_none() {
        base.token_limits = extra.token_limits;
    }
    base.supported_input_types =
        merge_string_lists(base.supported_input_types, extra.supported_input_types);
    base.capabilities = merge_string_lists(base.capabilities, extra.capabilities);
    base
}

fn merge_string_lists(base: Vec<String>, extra: Vec<String>) -> Vec<String> {
    if extra.is_empty() {
        return base;
    }

    let mut seen = std::collections::HashSet::with_capacity(base.len() + extra.len());
    let mut merged = Vec::with_capacity(base.len() + extra.len());

    for item in base.into_iter().chain(extra) {
        let key = item.trim().to_lowercase();
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        merged.push(item);
    }

    merged
}

fn model_supports_image(input_types: &[String]) -> bool {
    input_types.iter().any(|input_type| {
        let lower = input_type.to_lowercase();
        lower.contains("image") || lower.contains("vision")
    })
}

fn build_model_info(
    id: impl Into<String>,
    owned_by: impl Into<String>,
    supports_image: bool,
) -> Model {
    let mut input_modalities = vec!["text".to_string()];
    if supports_image {
        input_modalities.push("image".to_string());
    }

    Model {
        id: id.into(),
        object: "model".to_string(),
        owned_by: owned_by.into(),
        supports_image,
        input_modalities: input_modalities.clone(),
        modalities: ModelModalities {
            input: input_modalities,
            output: vec!["text".to_string()],
        },
        capabilities: ModelCapabilities {
            vision: supports_image,
            image: supports_image,
            image_vision: supports_image,
        },
        info: ModelInfo {
            meta: ModelInfoMeta {
                capabilities: ModelInfoCapabilities {
                    vision: supports_image,
                    image_vision: supports_image,
                },
            },
        },
        created: None,
        display_name: None,
        model_type: None,
        max_tokens: None,
        context_length: None,
        max_completion_tokens: None,
        thinking: None,
    }
}

/// 请求预处理结果，由 [`prepare_request`] 返回
struct PreparedRequest {
    request_body: String,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    user_id: Option<String>,
    accounting_payload: MessagesRequest,
}

/// 公共请求预处理前段：转换→压缩→序列化→大小检查→元数据提取
///
/// 调用方需在此函数之前完成：thinking override、系统提示注入、WebSearch 分流。
fn prepare_request(
    state: &AppState,
    payload: &MessagesRequest,
) -> Result<PreparedRequest, Response> {
    let compression = state.compression_config.read().clone();
    let prompt_filter = state.prompt_filter_config.read().clone();
    let thinking_suffix = state.thinking_config.read().suffix.clone();
    let conversion_result = match convert_request_with_thinking_suffix(
        payload,
        &compression,
        &prompt_filter,
        false,
        &thinking_suffix,
    ) {
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
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response());
        }
    };

    let has_system_priming = conversion_result.has_system_priming;
    let conversation_state = conversion_result.conversation_state;
    let mut kiro_request = KiroRequest {
        conversation_state,
        inference_config: anthropic_inference_config(&payload),
        profile_arn: None,
    };

    let max_body = compression.max_request_body_bytes;
    let request_body = match serialize_and_fit_body(
        &mut kiro_request,
        max_body,
        has_system_priming,
        "",
    ) {
        Ok(body) => body,
        Err(BodyFitError::Serialize(e)) => {
            tracing::error!("序列化请求失败: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response());
        }
        Err(BodyFitError::TooLarge { body, bytes, limit }) => {
            tracing::warn!(
                conversation_id = kiro_request.conversation_state.conversation_id.as_str(),
                request_body_bytes = bytes,
                threshold = limit,
                "安全截断策略执行后请求体仍超过安全阈值，拒绝发送"
            );
            #[cfg(feature = "sensitive-logs")]
            tracing::error!(
                "安全截断策略执行后仍超限，完整请求体（用于诊断）: {}",
                truncate_base64_in_request_body(&body)
            );
            #[cfg(not(feature = "sensitive-logs"))]
            let _ = &body;
            return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "invalid_request_error",
                        format!(
                            "Request too large ({} bytes total; limit {}). Reduce current message/tool output or number/size of images.",
                            bytes, limit
                        ),
                    )),
                )
                    .into_response());
        }
    };

    tracing::debug!(
        kiro_request_body_bytes = request_body.len(),
        "已构建 Kiro 请求体"
    );
    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let accounting_payload = request_with_thinking_accounting(payload.clone(), &thinking_suffix);
    let input_tokens = token::count_all_tokens(
        accounting_payload.model.clone(),
        accounting_payload.system.clone(),
        accounting_payload.messages.clone(),
        accounting_payload.tools.clone(),
    ) as i32;

    // 检查是否启用了 thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    let raw_user_id = payload.metadata.as_ref().and_then(|m| m.user_id.as_deref());
    // 提取 session_id 作为亲和 key；裸 user_id 是机器哈希常量，不能作 key
    let user_id = raw_user_id.and_then(extract_session_id);

    Ok(PreparedRequest {
        request_body,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        user_id,
        accounting_payload,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(matched_api_key): Extension<MatchedApiKeyId>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );
    // 入口诊断：统计每条消息的 role + content block types，定位 trae/Claude Code 等客户端
    // 历史 tool_use/tool_result 配对问题（call_* 风格 ID 来自 OpenAI 协议翻译）
    if tracing::enabled!(tracing::Level::DEBUG) {
        for (idx, m) in payload.messages.iter().enumerate() {
            let mut tool_use_ids: Vec<&str> = Vec::new();
            let mut tool_result_ids: Vec<&str> = Vec::new();
            if let serde_json::Value::Array(arr) = &m.content {
                for b in arr {
                    let ty = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match ty {
                        "tool_use" => {
                            if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
                                tool_use_ids.push(id);
                            }
                        }
                        "tool_result" => {
                            if let Some(id) = b.get("tool_use_id").and_then(|v| v.as_str()) {
                                tool_result_ids.push(id);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !tool_use_ids.is_empty() || !tool_result_ids.is_empty() {
                tracing::debug!(
                    idx,
                    role = %m.role,
                    tool_use_ids = ?tool_use_ids,
                    tool_result_ids = ?tool_result_ids,
                    "msg tool entries"
                );
            }
        }
    }
    if let Some(message) = validate_messages_request_shape(&payload) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", message)),
        )
            .into_response();
    }
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

    let thinking_suffix = state.thinking_config.read().suffix.clone();
    override_thinking_from_model_name(&mut payload, &thinking_suffix);

    // 注入用户配置的系统提示（preset + 自定义）
    inject_system_prompt(&mut payload, &state.prompt_runtime);

    // 检查是否为 WebSearch 请求
    if websearch::should_handle_websearch_request(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let accounting_payload =
            request_with_thinking_accounting(payload.clone(), &thinking_suffix);
        let input_tokens = token::count_all_tokens(
            accounting_payload.model,
            accounting_payload.system,
            accounting_payload.messages,
            accounting_payload.tools,
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, None, None, input_tokens)
            .await;
    }

    // 混合工具场景：剔除 web_search 后转发上游
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到混合工具列表中的 web_search，剔除后转发上游");
        websearch::strip_web_search_tools(&mut payload);
    }

    let prep = match prepare_request(&state, &payload) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let user_id = prep.user_id.as_deref();

    // 读 prompt-cache 快照 + 按 accounting_enabled 构造 cache_profile。
    let prompt_cache = state.prompt_cache_snapshot();
    let claude_format = state.thinking_config.read().claude_format.clone();
    let cache_profile = prompt_cache.accounting_enabled.then(|| {
        build_cache_profile(
            prompt_cache.tracker.as_ref(),
            &prep.accounting_payload,
            prep.input_tokens,
        )
    });

    if payload.stream {
        // 流式响应
        let stream_request = StreamRequestContext {
            app_state: state.clone(),
            api_key_id: matched_api_key.0.clone(),
            cache_tracker: prompt_cache
                .accounting_enabled
                .then_some(&prompt_cache.tracker),
            cache_profile: cache_profile.as_ref(),
            request_body: &prep.request_body,
            model: &payload.model,
            input_tokens: prep.input_tokens,
            thinking_enabled: prep.thinking_enabled,
            tool_name_map: prep.tool_name_map.clone(),
            user_id,
            claude_format: claude_format.clone(),
        };
        handle_stream_request(provider, stream_request).await
    } else {
        // 非流式响应
        let non_stream_request = NonStreamRequestContext {
            app_state: state.clone(),
            api_key_id: matched_api_key.0,
            request_body: &prep.request_body,
            model: &payload.model,
            input_tokens: prep.input_tokens,
            thinking_enabled: prep.thinking_enabled,
            thinking_display_omitted: payload.thinking.as_ref().is_some_and(|t| {
                t.is_enabled() && t.effective_display().eq_ignore_ascii_case("omitted")
            }),
            tool_name_map: prep.tool_name_map,
            user_id,
            cache_tracker: prompt_cache
                .accounting_enabled
                .then_some(&prompt_cache.tracker),
            cache_profile: cache_profile.as_ref(),
            claude_format,
        };
        handle_non_stream_request(provider, non_stream_request).await
    }
}

fn validate_messages_request_shape(req: &MessagesRequest) -> Option<&'static str> {
    if req.messages.is_empty() {
        return Some("messages must not be empty");
    }
    if let Some(message) = validate_thinking_config(req.thinking.as_ref(), req.max_tokens) {
        return Some(message);
    }

    let mut has_user_context = false;
    let mut last_role = "";
    for msg in &req.messages {
        let role = msg.role.trim();
        if role.is_empty() {
            continue;
        }
        last_role = role;
        if role == "user" && anthropic_user_has_context(&msg.content) {
            has_user_context = true;
        }
    }

    if last_role == "assistant" {
        return Some("assistant-prefill final message is not supported; last message must be user");
    }
    if !has_user_context {
        return Some("at least one non-empty user message is required");
    }

    None
}

fn validate_thinking_config(
    thinking: Option<&crate::anthropic::types::Thinking>,
    max_tokens: i32,
) -> Option<&'static str> {
    let thinking = thinking?;

    match thinking.thinking_type.trim().to_lowercase().as_str() {
        "enabled" => {
            if max_tokens == 0 {
                return Some("thinking.type enabled cannot be used with max_tokens=0");
            }
            let Some(budget_tokens) = thinking.budget_tokens else {
                return Some("thinking.budget_tokens is required when thinking.type is enabled");
            };
            if budget_tokens <= 0 {
                return Some("thinking.budget_tokens is required when thinking.type is enabled");
            }
            if budget_tokens < 1024 {
                return Some("thinking.budget_tokens must be at least 1024");
            }
            if max_tokens > 0 && budget_tokens >= max_tokens {
                return Some("thinking.budget_tokens must be less than max_tokens");
            }
        }
        "adaptive" => {
            if thinking.budget_tokens.unwrap_or_default() != 0 {
                return Some(
                    "thinking.budget_tokens is not supported when thinking.type is adaptive",
                );
            }
        }
        "disabled" => {
            if thinking.budget_tokens.unwrap_or_default() != 0 {
                return Some(
                    "thinking.budget_tokens is not supported when thinking.type is disabled",
                );
            }
        }
        _ => return Some("thinking.type must be one of: enabled, adaptive, disabled"),
    }

    if let Some(display) = thinking.display.as_deref() {
        let display = display.trim().to_lowercase();
        if display.is_empty() {
            return None;
        }
        if display != "summarized" && display != "omitted" {
            return Some("thinking.display must be one of: summarized, omitted");
        }
        if thinking
            .thinking_type
            .trim()
            .eq_ignore_ascii_case("disabled")
        {
            return Some("thinking.display is not supported when thinking.type is disabled");
        }
    }

    None
}

fn anthropic_user_has_context(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::String(text) => !text.trim().is_empty(),
        serde_json::Value::Array(blocks) => blocks.iter().any(anthropic_content_block_has_context),
        serde_json::Value::Object(_) => anthropic_content_block_has_context(content),
        _ => false,
    }
}

fn anthropic_content_block_has_context(block: &serde_json::Value) -> bool {
    let Some(obj) = block.as_object() else {
        return false;
    };
    match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "text" | "input_text" => obj
            .get("text")
            .and_then(|v| v.as_str())
            .is_some_and(|text| !text.trim().is_empty()),
        "image" | "image_url" | "input_image" | "file" | "input_file" => {
            anthropic_block_has_inline_image(block)
        }
        "tool_result" => true,
        _ => false,
    }
}

fn anthropic_block_has_inline_image(block: &serde_json::Value) -> bool {
    let Some(obj) = block.as_object() else {
        return false;
    };
    if let Some(source) = obj.get("source").filter(|v| v.is_object()) {
        return anthropic_block_has_inline_image(source)
            || source
                .get("data")
                .and_then(|v| v.as_str())
                .is_some_and(is_inline_image_payload)
            || source
                .get("url")
                .and_then(|v| v.as_str())
                .is_some_and(is_inline_image_payload);
    }
    for key in ["data", "url", "b64_json", "image_base64"] {
        if obj
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(is_inline_image_payload)
        {
            return true;
        }
    }
    match obj.get("image_url") {
        Some(serde_json::Value::String(raw)) => is_inline_image_payload(raw),
        Some(serde_json::Value::Object(map)) => map
            .get("url")
            .and_then(|v| v.as_str())
            .is_some_and(is_inline_image_payload),
        _ => false,
    }
}

fn is_inline_image_payload(raw: &str) -> bool {
    let raw = raw.trim();
    !raw.is_empty()
        && !raw.contains("[Image")
        && !raw.starts_with("http://")
        && !raw.starts_with("https://")
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    context: StreamRequestContext<'_>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let mut api_result = match provider
        .call_api_stream_with_client_affinity(
            context.request_body,
            context.user_id,
            context.api_key_id.as_deref(),
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            context.app_state.record_gateway_failure();
            return map_kiro_provider_error_to_response(context.request_body, e);
        }
    };

    // 凭据已选定 → 用 resolved_cache_usage 重算并提交 cache_tracker。
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
    let mut ctx = StreamContext::new_with_thinking_format(
        context.model,
        context.input_tokens,
        final_cache_usage,
        context.thinking_enabled,
        context.tool_name_map,
        context.claude_format,
    );

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流（permit 随 stream 一起持有，body 消费完成后再释放）
    let cred_permit = api_result._credential_permit.take();
    let glb_permit = api_result._global_permit.take();
    let proxy_permit = api_result._proxy_permit.take();
    let tm = provider.token_manager().clone();
    let credential_id = api_result.credential_id;
    let stream = create_sse_stream(
        api_result.response,
        ctx,
        initial_events,
        cred_permit,
        glb_permit,
        proxy_permit,
        tm,
        credential_id,
        context.app_state,
        context.api_key_id,
    );

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

/// 流式上游结束时的 permit 释放 + credit 结算不变式。
///
/// 占用语义 = 一次上游来回，与客户端消费速度解耦：上游 body 一旦结束（正常或异常），
/// 三个 permit（credential/global/proxy）必须一起释放，且若本次有 metering 用量则提交给
/// token manager。三条流式循环（Anthropic / OpenAI chat / OpenAI responses）的 6 处收尾
/// 共用此不变式（ER-1）。调用方先各自读出 `metering_usage`（field 或 method 访问差异留在外层），
/// record_api_key_usage / 最终事件生成的时序仍由各 caller 保留，helper 不介入。
pub(crate) fn settle_stream_permits(
    tm: &crate::kiro::token_manager::MultiTokenManager,
    credential_id: u64,
    cred_permit: Option<OwnedSemaphorePermit>,
    glb_permit: Option<OwnedSemaphorePermit>,
    proxy_permit: Option<OwnedSemaphorePermit>,
    metering_usage: Option<f64>,
) {
    drop(cred_permit);
    drop(glb_permit);
    drop(proxy_permit);
    if let Some(usage) = metering_usage {
        tm.apply_credit_usage(credential_id, usage);
    }
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    cred_permit: Option<OwnedSemaphorePermit>,
    glb_permit: Option<OwnedSemaphorePermit>,
    proxy_permit: Option<OwnedSemaphorePermit>,
    tm: std::sync::Arc<crate::kiro::token_manager::MultiTokenManager>,
    credential_id: u64,
    app_state: AppState,
    api_key_id: Option<String>,
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
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), cred_permit, glb_permit, proxy_permit, tm, credential_id, app_state, api_key_id),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, cred_permit, glb_permit, proxy_permit, tm, credential_id, app_state, api_key_id)| async move {
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
                                        // 从帧中提取 token 使用量（比估算更准确）
                                        if let Some(usage) = crate::kiro::model::events::extract_token_usage_from_frame_with_current(
                                            &frame,
                                            None,
                                            Some(i64::from(ctx.output_tokens)),
                                        ) {
                                            if let Some(input) = usage.input_tokens {
                                                ctx.context_input_tokens = Some(input as i32);
                                            }
                                            if let Some(output) = usage.output_tokens {
                                                ctx.set_actual_output_tokens(output as i32);
                                            }
                                        }
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

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, cred_permit, glb_permit, proxy_permit, tm, credential_id, app_state, api_key_id)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 上游异常结束 → 立即释放 permit（占用语义 = 一次上游来回，不绑客户端消费速度）
                            settle_stream_permits(&tm, credential_id, cred_permit, glb_permit, proxy_permit, ctx.metering.as_ref().map(|m| m.usage));
                            app_state.record_gateway_failure();
                            let final_events = ctx.generate_error_events(e.to_string());
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, None, None, None, tm, credential_id, app_state, api_key_id)))
                        }
                        None => {
                            // 上游正常结束 → 立即释放 permit
                            settle_stream_permits(&tm, credential_id, cred_permit, glb_permit, proxy_permit, ctx.metering.as_ref().map(|m| m.usage));
                            let final_events = ctx.generate_final_events();
                            let credits = ctx.metering.as_ref().map(|m| m.usage).unwrap_or(0.0);
                            let tokens = i64::from(ctx.final_input_tokens())
                                + i64::from(ctx.final_output_tokens());
                            app_state.record_api_key_usage(api_key_id.as_deref(), tokens, credits);
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, None, None, None, tm, credential_id, app_state, api_key_id)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, cred_permit, glb_permit, proxy_permit, tm, credential_id, app_state, api_key_id)))
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
        .call_api_with_client_affinity(
            context.request_body,
            context.user_id,
            context.api_key_id.as_deref(),
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            context.app_state.record_gateway_failure();
            return map_kiro_provider_error_to_response(context.request_body, e);
        }
    };

    // 凭据已选定 → 用 resolved_cache_usage 重算并提交 cache_tracker。
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
    let credential_id = api_result.credential_id;
    let _cred_permit = api_result._credential_permit;
    let _glb_permit = api_result._global_permit;
    let _proxy_permit = api_result._proxy_permit;
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
    let mut raw_thinking_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // 从事件 payload 中提取的输出 tokens（比估算更准确）
    let mut _actual_output_tokens: Option<i32> = None;
    // 从 meteringEvent 透传的 credit usage，仅用于最终 usage 字段
    let mut metering: Option<MeteringEvent> = None;

    let mut current_tool_use: Option<NonStreamToolUseState> = None;
    let mut last_assistant_content = String::new();
    let mut last_reasoning_content = String::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                // 从帧 payload 中提取 token 使用量。
                if let Some(usage) =
                    crate::kiro::model::events::extract_token_usage_from_frame_with_current(
                        &frame,
                        None,
                        _actual_output_tokens.map(i64::from),
                    )
                {
                    if let Some(input) = usage.input_tokens {
                        context_input_tokens = Some(input as i32);
                    }
                    if let Some(output) = usage.output_tokens {
                        // 累积输出 token（比估算更准确）
                        _actual_output_tokens = Some(output as i32);
                    }
                }
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            append_non_stream_assistant_delta(
                                &mut text_content,
                                &resp.content,
                                &mut last_assistant_content,
                            );
                        }
                        Event::ReasoningContent(resp) => {
                            append_non_stream_reasoning_delta(
                                &mut raw_thinking_content,
                                &resp.text,
                                &mut last_reasoning_content,
                            );
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;
                            handle_non_stream_tool_use_event(
                                tool_use,
                                &mut current_tool_use,
                                &mut tool_uses,
                                &context.tool_name_map,
                            );
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
    finish_non_stream_tool_use(
        &mut current_tool_use,
        &mut tool_uses,
        &context.tool_name_map,
    );

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // Bracket-style 工具调用回退：仅在结构化 toolUseEvent 未出现工具调用、
    // 且文本里包含 `[Called name with args: {...}]` 模式时才扫描，避免误伤普通文本。
    if !has_tool_use {
        let bracket_calls = super::bracket_tool_parser::parse_bracket_tool_calls(&text_content);
        if !bracket_calls.is_empty() {
            tracing::info!(
                count = bracket_calls.len(),
                "检出 bracket 风格工具调用，回退转为标准 tool_use"
            );
            text_content =
                super::bracket_tool_parser::strip_bracket_spans(&text_content, &bracket_calls);
            for call in bracket_calls {
                let original_name = context
                    .tool_name_map
                    .get(&call.name)
                    .cloned()
                    .unwrap_or(call.name);
                let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                tool_uses.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": original_name,
                    "input": call.input,
                }));
            }
            if stop_reason == "end_turn" {
                stop_reason = "tool_use".to_string();
            }
        }
    }

    let content = build_non_stream_content_blocks(
        &text_content,
        &raw_thinking_content,
        tool_uses,
        context.thinking_enabled,
        context.thinking_display_omitted,
        &context.claude_format,
    );

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 优先使用上游 real input tokens，无则回落请求侧估算。
    let final_input_tokens = context_input_tokens.unwrap_or(context.input_tokens);
    // billed = final - cache_creation - cache_read（用 saturating_sub 防负）。
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

    // 构建 Anthropic 响应（usage 字段注入 credit + cache）。
    let response_body = {
        let mut usage = json!({
            "input_tokens": billed_input_tokens,
            "output_tokens": output_tokens
        });
        if let Some(ref metering) = metering {
            inject_credit_usage_fields(&mut usage, metering);
            // 同步扣减运行时余额缓存（drains primary→overage），观察者回调 admin disk cache
            provider
                .token_manager()
                .apply_credit_usage(credential_id, metering.usage);
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

    context.app_state.record_api_key_usage(
        context.api_key_id.as_deref(),
        i64::from(final_input_tokens) + i64::from(output_tokens),
        metering.as_ref().map(|m| m.usage).unwrap_or(0.0),
    );

    (StatusCode::OK, Json(response_body)).into_response()
}

/// 注入运行时配置的系统提示
///
/// 拼接 `enabled_presets` 内启用的内置 + 用户预设和 `custom_content`，
/// 按 `position` 插入或追加到 `payload.system`。`enabled=false` 或拼接结果空时直接 no-op。
fn inject_system_prompt(payload: &mut MessagesRequest, shared: &SharedPromptConfig) {
    let (injection, position) = {
        let cfg = shared.read();
        (cfg.build_injection_text(), cfg.position)
    };

    let Some(text) = injection else {
        return;
    };
    let injected = SystemMessage {
        text,
        block_type: None,
        cache_control: None,
    };

    match &mut payload.system {
        Some(existing) => match position {
            SystemPromptPosition::Prepend => existing.insert(0, injected),
            SystemPromptPosition::Append => existing.push(injected),
        },
        None => {
            payload.system = Some(vec![injected]);
        }
    }
}

/// 模型名/请求兜底，确保 thinking 配置满足上游要求
///
/// 1. **Opus 4.7 不支持 `type: "enabled"`**：自动降级为 `adaptive`，
///    补 `display=summarized` + `output_config.effort=high`，
///    不论是否带 thinking 后缀。
/// 2. **`*-thinking` 后缀**：强制开启 thinking
///    - Opus 4.6/4.7 → `adaptive`（带 `effort: high`、`display: summarized`）
///    - 其他模型 → `enabled`，budget_tokens=20000
pub(crate) fn override_thinking_from_model_name(
    payload: &mut MessagesRequest,
    thinking_suffix: &str,
) {
    let model_lower = payload.model.to_lowercase();
    let suffix_lower = thinking_suffix.to_lowercase();
    let is_opus = model_lower.contains("opus");
    let is_opus_4_7 = is_opus && (model_lower.contains("4-7") || model_lower.contains("4.7"));
    let is_opus_4_6 = is_opus && (model_lower.contains("4-6") || model_lower.contains("4.6"));
    let is_opus_4_6_or_newer = is_opus_4_6 || is_opus_4_7;
    let has_thinking_suffix = !suffix_lower.is_empty() && model_lower.ends_with(&suffix_lower);

    // Case 1: Opus 4.7 不支持 enabled，自动降级 adaptive；不论有无后缀
    if is_opus_4_7 {
        if let Some(ref mut t) = payload.thinking {
            if t.thinking_type.trim().eq_ignore_ascii_case("enabled") {
                tracing::info!(
                    model = %payload.model,
                    "Opus 4.7 不支持 thinking.type=\"enabled\"，自动降级为 \"adaptive\""
                );
                t.thinking_type = "adaptive".to_string();
            }
            if t.display.is_none() {
                t.display = Some("summarized".to_string());
            }
            if payload.output_config.is_none() {
                payload.output_config = Some(OutputConfig {
                    effort: "high".to_string(),
                });
            }
        }
    }

    // Case 2: 模型名带 *-thinking 后缀 → 强制开启
    if !has_thinking_suffix {
        return;
    }

    let thinking_type = if is_opus_4_6_or_newer {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: if thinking_type == "enabled" {
            Some(20000)
        } else {
            None
        },
        display: if thinking_type == "adaptive" {
            Some("summarized".to_string())
        } else {
            None
        },
    });

    if is_opus_4_6_or_newer {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    State(state): State<AppState>,
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    if let Some(message) = validate_thinking_config(payload.thinking.as_ref(), payload.max_tokens) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", message)),
        )
            .into_response();
    }

    let thinking_suffix = state.thinking_config.read().suffix.clone();
    let effective = effective_count_tokens_request(payload, &thinking_suffix);

    let total_tokens = token::count_all_tokens(
        effective.model,
        effective.system,
        effective.messages,
        effective.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
    .into_response()
}

fn effective_count_tokens_request(
    payload: CountTokensRequest,
    thinking_suffix: &str,
) -> CountTokensRequest {
    let effective = MessagesRequest {
        model: payload.model,
        max_tokens: payload.max_tokens,
        temperature: None,
        top_p: None,
        messages: payload.messages,
        stream: false,
        system: payload.system,
        tools: payload.tools,
        tool_choice: None,
        thinking: payload.thinking,
        output_config: payload.output_config,
        metadata: None,
    };
    let effective = request_with_thinking_accounting(effective, thinking_suffix);

    CountTokensRequest {
        model: effective.model,
        max_tokens: effective.max_tokens,
        messages: effective.messages,
        system: effective.system,
        tools: effective.tools,
        thinking: effective.thinking,
        output_config: effective.output_config,
    }
}

fn request_with_thinking_accounting(
    mut payload: MessagesRequest,
    thinking_suffix: &str,
) -> MessagesRequest {
    override_thinking_from_model_name(&mut payload, thinking_suffix);
    payload.model = map_model_with_thinking_suffix(&payload.model, thinking_suffix);
    apply_thinking_prefix_for_accounting(&mut payload);
    payload
}

fn apply_thinking_prefix_for_accounting(payload: &mut MessagesRequest) {
    let Some(prefix) = generate_thinking_prefix(payload) else {
        return;
    };

    payload.system = Some(match payload.system.take() {
        Some(mut system) if !system.is_empty() => {
            if !system
                .iter()
                .any(|block| block.text.contains("<thinking_mode>"))
            {
                system.insert(
                    0,
                    SystemMessage {
                        text: prefix,
                        block_type: Some("text".to_string()),
                        cache_control: None,
                    },
                );
            }
            system
        }
        _ => vec![SystemMessage {
            text: prefix,
            block_type: Some("text".to_string()),
            cache_control: None,
        }],
    });
}

/// POST /cc/v1/messages
///
/// Claude Code 端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    Extension(matched_api_key): Extension<MatchedApiKeyId>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    if let Some(message) = validate_messages_request_shape(&payload) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", message)),
        )
            .into_response();
    }

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

    let thinking_suffix = state.thinking_config.read().suffix.clone();
    override_thinking_from_model_name(&mut payload, &thinking_suffix);

    // 注入用户配置的系统提示（preset + 自定义）
    inject_system_prompt(&mut payload, &state.prompt_runtime);

    // 检查是否为 WebSearch 请求
    if websearch::should_handle_websearch_request(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let accounting_payload =
            request_with_thinking_accounting(payload.clone(), &thinking_suffix);
        let input_tokens = token::count_all_tokens(
            accounting_payload.model,
            accounting_payload.system,
            accounting_payload.messages,
            accounting_payload.tools,
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, None, None, input_tokens)
            .await;
    }

    // 混合工具场景：剔除 web_search 后转发上游
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到混合工具列表中的 web_search，剔除后转发上游");
        websearch::strip_web_search_tools(&mut payload);
    }

    let prep = match prepare_request(&state, &payload) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let user_id = prep.user_id.as_deref();

    let prompt_cache = state.prompt_cache_snapshot();
    let claude_format = state.thinking_config.read().claude_format.clone();
    let cache_profile = prompt_cache.accounting_enabled.then(|| {
        build_cache_profile(
            prompt_cache.tracker.as_ref(),
            &prep.accounting_payload,
            prep.input_tokens,
        )
    });

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &prep.request_body,
            &payload.model,
            prep.input_tokens,
            prep.thinking_enabled,
            prep.tool_name_map,
            user_id,
            prompt_cache
                .accounting_enabled
                .then_some(&prompt_cache.tracker),
            cache_profile.as_ref(),
            claude_format.clone(),
            state.clone(),
            matched_api_key.0.clone(),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && prep.thinking_enabled;
        let non_stream_request = NonStreamRequestContext {
            app_state: state.clone(),
            api_key_id: matched_api_key.0,
            request_body: &prep.request_body,
            model: &payload.model,
            input_tokens: prep.input_tokens,
            thinking_enabled: extract_thinking,
            thinking_display_omitted: payload.thinking.as_ref().is_some_and(|t| {
                t.is_enabled() && t.effective_display().eq_ignore_ascii_case("omitted")
            }),
            tool_name_map: prep.tool_name_map,
            user_id,
            cache_tracker: prompt_cache
                .accounting_enabled
                .then_some(&prompt_cache.tracker),
            cache_profile: cache_profile.as_ref(),
            claude_format,
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
    cache_tracker: Option<&std::sync::Arc<crate::anthropic::cache_tracker::CacheTracker>>,
    cache_profile: Option<&crate::anthropic::cache_tracker::CacheProfile>,
    claude_format: String,
    app_state: AppState,
    api_key_id: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let mut api_result = match provider
        .call_api_stream_with_client_affinity(request_body, user_id, api_key_id.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            app_state.record_gateway_failure();
            return map_kiro_provider_error_to_response(request_body, e);
        }
    };

    // 凭据已选定 → 用 resolved_cache_usage 重算并提交 cache_tracker。
    let final_cache_usage = match (cache_tracker, cache_profile) {
        (Some(tracker), Some(profile)) => {
            let resolved = resolved_cache_usage(tracker, api_result.credential_id, profile);
            tracing::debug!(
                credential_id = api_result.credential_id,
                final_cache_creation_input_tokens = resolved.cache_creation_input_tokens,
                final_cache_read_input_tokens = resolved.cache_read_input_tokens,
                "Resolved cache usage for buffered stream request"
            );
            tracker.update(api_result.credential_id, profile);
            Some(CacheUsageBreakdown {
                cache_creation_input_tokens: resolved.cache_creation_input_tokens,
                cache_read_input_tokens: resolved.cache_read_input_tokens,
                cache_creation_5m_input_tokens: resolved.cache_creation_5m_input_tokens,
                cache_creation_1h_input_tokens: resolved.cache_creation_1h_input_tokens,
            })
        }
        _ => None,
    };

    let _cred_permit = api_result._credential_permit.take();
    let _glb_permit = api_result._global_permit.take();
    let _proxy_permit = api_result._proxy_permit.take();
    let response = api_result.response;
    let _credential_id = api_result.credential_id;

    // 创建缓冲流处理上下文
    let ctx = BufferedStreamContext::new_with_format(
        model,
        estimated_input_tokens,
        thinking_enabled,
        tool_name_map,
        final_cache_usage,
        claude_format,
    );

    // 创建缓冲 SSE 流
    let tm = provider.token_manager().clone();
    let stream = create_buffered_sse_stream(
        response,
        ctx,
        _cred_permit,
        _glb_permit,
        _proxy_permit,
        tm,
        _credential_id,
        app_state,
        api_key_id,
    );

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
    cred_permit: Option<OwnedSemaphorePermit>,
    glb_permit: Option<OwnedSemaphorePermit>,
    proxy_permit: Option<OwnedSemaphorePermit>,
    tm: std::sync::Arc<crate::kiro::token_manager::MultiTokenManager>,
    credential_id: u64,
    app_state: AppState,
    api_key_id: Option<String>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            cred_permit,
            glb_permit,
            proxy_permit,
            tm,
            credential_id,
            app_state,
            api_key_id,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, cred_permit, glb_permit, proxy_permit, tm, credential_id, app_state, api_key_id)| async move {
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
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, cred_permit, glb_permit, proxy_permit, tm, credential_id, app_state, api_key_id)));
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
                                            // 从帧中提取 token 使用量。
                                            if let Some(usage) = crate::kiro::model::events::extract_token_usage_from_frame_with_current(
                                                &frame,
                                                None,
                                                Some(i64::from(ctx.inner.output_tokens)),
                                            ) {
                                                if let Some(input) = usage.input_tokens {
                                                    ctx.inner.context_input_tokens = Some(input as i32);
                                                }
                                                if let Some(output) = usage.output_tokens {
                                                    ctx.inner.set_actual_output_tokens(output as i32);
                                                }
                                            }
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
                                // 上游异常结束 → 立即释放 permit
                                drop(cred_permit);
                                drop(glb_permit);
                                drop(proxy_permit);
                                if let Some(m) = ctx.metering() {
                                    tm.apply_credit_usage(credential_id, m.usage);
                                }
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, None, None, None, tm, credential_id, app_state, api_key_id)));
                            }
                            None => {
                                // 上游正常结束 → 立即释放 permit
                                drop(cred_permit);
                                drop(glb_permit);
                                drop(proxy_permit);
                                if let Some(m) = ctx.metering() {
                                    tm.apply_credit_usage(credential_id, m.usage);
                                }
                                let all_events = ctx.finish_and_get_all_events();
                                let credits = ctx.metering().map(|m| m.usage).unwrap_or(0.0);
                                let tokens = i64::from(ctx.final_input_tokens())
                                    + i64::from(ctx.final_output_tokens());
                                app_state.record_api_key_usage(api_key_id.as_deref(), tokens, credits);
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, None, None, None, tm, credential_id, app_state, api_key_id)));
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
        ConversationState, CurrentMessage, Message as KiroMessage, UserInputMessage,
    };
    use crate::kiro::models::AvailableModel;

    fn test_state_with_compression(
        compression: crate::model::config::CompressionConfig,
    ) -> AppState {
        AppState::new(
            "test-key",
            false,
            false,
            std::sync::Arc::new(parking_lot::RwLock::new(
                crate::anthropic::middleware::PromptCacheRuntime::new(300, false, 0.85),
            )),
            crate::anthropic::middleware::ThinkingRuntimeConfig {
                suffix: "-thinking".to_string(),
                openai_format: "reasoning_content".to_string(),
                claude_format: "thinking".to_string(),
            },
        )
        .with_compression_config(std::sync::Arc::new(parking_lot::RwLock::new(compression)))
    }

    fn available_model_fixture(id: &str, input_types: &[&str]) -> AvailableModel {
        AvailableModel {
            model_id: id.to_string(),
            model_name: String::new(),
            description: String::new(),
            provider: None,
            capabilities: Vec::new(),
            context_window: None,
            is_default: None,
            rate_multiplier: None,
            rate_unit: None,
            prompt_caching: None,
            supported_input_types: input_types.iter().map(|v| (*v).to_string()).collect(),
            token_limits: None,
        }
    }

    fn tool_use_event(
        name: &str,
        tool_use_id: &str,
        input: &str,
        stop: bool,
    ) -> crate::kiro::model::events::ToolUseEvent {
        crate::kiro::model::events::ToolUseEvent {
            name: name.to_string(),
            tool_use_id: tool_use_id.to_string(),
            input: input.to_string(),
            input_is_json_object: false,
            stop,
        }
    }

    fn tool_use_object_event(
        name: &str,
        tool_use_id: &str,
        input: serde_json::Value,
        stop: bool,
    ) -> crate::kiro::model::events::ToolUseEvent {
        crate::kiro::model::events::ToolUseEvent {
            name: name.to_string(),
            tool_use_id: tool_use_id.to_string(),
            input: input.to_string(),
            input_is_json_object: true,
            stop,
        }
    }

    #[test]
    fn non_stream_tool_use_flushes_without_stop() {
        let mut current = None;
        let mut output = Vec::new();
        let names = std::collections::HashMap::new();

        handle_non_stream_tool_use_event(
            tool_use_event("exec_command", "call_1", "{\"cmd\":\"pwd\"}", false),
            &mut current,
            &mut output,
            &names,
        );
        finish_non_stream_tool_use(&mut current, &mut output, &names);

        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["id"], "call_1");
        assert_eq!(output[0]["name"], "exec_command");
        assert_eq!(output[0]["input"], serde_json::json!({"cmd": "pwd"}));
    }

    #[test]
    fn non_stream_tool_use_adopts_late_real_id() {
        let mut current = None;
        let mut output = Vec::new();
        let names = std::collections::HashMap::new();

        handle_non_stream_tool_use_event(
            tool_use_event("exec_command", "", "{\"cmd\":", false),
            &mut current,
            &mut output,
            &names,
        );
        handle_non_stream_tool_use_event(
            tool_use_event("exec_command", "call_real", "\"pwd\"}", true),
            &mut current,
            &mut output,
            &names,
        );

        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["id"], "call_real");
        assert_eq!(output[0]["input"], serde_json::json!({"cmd": "pwd"}));
    }

    #[test]
    fn non_stream_tool_use_object_input_replaces_buffer() {
        let mut current = None;
        let mut output = Vec::new();
        let names = std::collections::HashMap::new();

        handle_non_stream_tool_use_event(
            tool_use_event("exec_command", "", "{\"cmd\":\"old", false),
            &mut current,
            &mut output,
            &names,
        );
        handle_non_stream_tool_use_event(
            tool_use_object_event(
                "exec_command",
                "call_real",
                serde_json::json!({"cmd": "pwd"}),
                true,
            ),
            &mut current,
            &mut output,
            &names,
        );

        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["id"], "call_real");
        assert_eq!(output[0]["input"], serde_json::json!({"cmd": "pwd"}));
    }

    #[test]
    fn non_stream_tool_use_name_change_flushes_previous() {
        let mut current = None;
        let mut output = Vec::new();
        let names = std::collections::HashMap::new();

        handle_non_stream_tool_use_event(
            tool_use_event("first_tool", "", "{\"a\":1}", false),
            &mut current,
            &mut output,
            &names,
        );
        handle_non_stream_tool_use_event(
            tool_use_event("second_tool", "", "{\"b\":2}", true),
            &mut current,
            &mut output,
            &names,
        );

        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["name"], "first_tool");
        assert_eq!(output[0]["input"], serde_json::json!({"a": 1}));
        assert_eq!(output[1]["name"], "second_tool");
        assert_eq!(output[1]["input"], serde_json::json!({"b": 2}));
    }

    #[test]
    fn non_stream_assistant_text_normalizes_cumulative_chunks() {
        let mut text = String::new();
        let mut previous = String::new();

        append_non_stream_assistant_delta(&mut text, "hello", &mut previous);
        append_non_stream_assistant_delta(&mut text, "hello world", &mut previous);
        append_non_stream_assistant_delta(&mut text, "hello world", &mut previous);

        assert_eq!(text, "hello world");
    }

    #[test]
    fn non_stream_reasoning_text_normalizes_cumulative_chunks() {
        let mut thinking = String::new();
        let mut previous = String::new();

        append_non_stream_reasoning_delta(&mut thinking, "think", &mut previous);
        append_non_stream_reasoning_delta(&mut thinking, "think more", &mut previous);

        assert_eq!(thinking, "think more");
    }

    #[test]
    fn non_stream_omitted_thinking_emits_empty_block() {
        let content = build_non_stream_content_blocks(
            "final answer",
            "private reasoning",
            Vec::new(),
            true,
            true,
            "thinking",
        );

        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "final answer");
    }

    #[test]
    fn non_stream_omitted_thinking_extracts_assistant_tag_without_leaking() {
        let content = build_non_stream_content_blocks(
            "<thinking>private reasoning</thinking>\n\nfinal answer",
            "",
            Vec::new(),
            true,
            true,
            "thinking",
        );

        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "");
        assert_eq!(content[1]["text"], "final answer");
    }

    #[test]
    fn non_stream_think_format_keeps_reasoning_in_text() {
        let content = build_non_stream_content_blocks(
            "final answer",
            "visible reasoning",
            Vec::new(),
            true,
            false,
            "think",
        );

        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[0]["text"],
            "<think>visible reasoning</think>final answer"
        );
    }

    #[test]
    fn test_build_model_info_includes_image_capability_shape() {
        let model = build_model_info("claude-sonnet-4.6", "anthropic", true);

        assert_eq!(model.id, "claude-sonnet-4.6");
        assert_eq!(model.object, "model");
        assert_eq!(model.owned_by, "anthropic");
        assert!(model.supports_image);
        assert_eq!(model.input_modalities, vec!["text", "image"]);
        assert_eq!(model.modalities.input, vec!["text", "image"]);
        assert_eq!(model.modalities.output, vec!["text"]);
        assert!(model.capabilities.vision);
        assert!(model.capabilities.image);
        assert!(model.capabilities.image_vision);
        assert!(model.info.meta.capabilities.vision);
        assert!(model.info.meta.capabilities.image_vision);
        assert!(model.created.is_none());
        assert!(model.display_name.is_none());

        let json = serde_json::to_value(&model).unwrap();
        assert!(json.get("created").is_none());
        assert!(json.get("display_name").is_none());
        assert!(json.get("max_tokens").is_none());
        assert_eq!(json["supports_image"], true);
        assert_eq!(
            json["modalities"]["input"],
            serde_json::json!(["text", "image"])
        );
    }

    #[test]
    fn test_default_models_response_generates_thinking_variants_and_aliases() {
        let models = default_models_response("-thinking");
        let ids: std::collections::HashSet<_> = models.iter().map(|m| m.id.as_str()).collect();

        assert!(ids.contains("claude-sonnet-4-6"));
        assert!(ids.contains("claude-sonnet-4-6-thinking"));
        assert!(ids.contains("claude-opus-4-8"));
        assert!(ids.contains("claude-opus-4-8-thinking"));
        assert!(ids.contains("claude-opus-4-7"));
        assert!(ids.contains("claude-opus-4-7-thinking"));
        assert!(ids.contains("auto"));
        assert!(ids.contains("gpt-4o"));
        assert!(ids.contains("gpt-4"));
    }

    #[test]
    fn test_merge_unique_models_preserves_union_across_accounts() {
        let base = vec![available_model_fixture("claude-sonnet-4.5", &["TEXT"])];
        let incoming = vec![
            available_model_fixture("claude-sonnet-4.5", &["image"]),
            available_model_fixture("claude-opus-4-7", &["text"]),
        ];

        let merged = merge_unique_models(base, incoming);

        assert_eq!(merged.len(), 2);
        assert!(model_supports_image(&merged[0].supported_input_types));
        assert_eq!(merged[1].model_id, "claude-opus-4-7");
    }

    #[test]
    fn test_build_anthropic_models_response_uses_supported_input_types() {
        let cached = vec![
            available_model_fixture("text-only", &["TEXT"]),
            available_model_fixture("vision-model", &["vision"]),
        ];

        let models = build_anthropic_models_response(&cached, "-thinking");
        let text_only = models.iter().find(|m| m.id == "text-only").unwrap();
        let vision = models.iter().find(|m| m.id == "vision-model").unwrap();
        let vision_thinking = models
            .iter()
            .find(|m| m.id == "vision-model-thinking")
            .unwrap();

        assert!(!text_only.supports_image);
        assert_eq!(text_only.input_modalities, vec!["text"]);
        assert!(vision.supports_image);
        assert_eq!(vision.input_modalities, vec!["text", "image"]);
        assert!(vision_thinking.supports_image);
    }

    fn messages_req(value: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(value).expect("messages request fixture should parse")
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_assistant_prefill() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "prefill"}
            ]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("assistant-prefill final message is not supported; last message must be user")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_empty_messages() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": []
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("messages must not be empty")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_without_user_context() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "   "}
            ]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("at least one non-empty user message is required")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_accepts_tool_result_user_context() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tool_1", "name": "read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tool_1", "content": "done"}
                ]}
            ]
        }));

        assert_eq!(validate_messages_request_shape(&req), None);
    }

    #[test]
    fn test_validate_messages_request_shape_accepts_inline_image_context() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}
                }]
            }]
        }));

        assert_eq!(validate_messages_request_shape(&req), None);
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_invalid_thinking_type() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "auto"},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("thinking.type must be one of: enabled, adaptive, disabled")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_enabled_thinking_budget_below_minimum() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "enabled", "budget_tokens": 512},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("thinking.budget_tokens must be at least 1024")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_enabled_thinking_budget_at_max_tokens() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("thinking.budget_tokens must be less than max_tokens")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_enabled_thinking_with_zero_max_tokens() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 0,
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("thinking.type enabled cannot be used with max_tokens=0")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_invalid_thinking_display() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "adaptive", "display": "verbose"},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(
            req.thinking.as_ref().unwrap().display.as_deref(),
            Some("verbose")
        );
        assert_eq!(
            validate_messages_request_shape(&req),
            Some("thinking.display must be one of: summarized, omitted")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_disabled_thinking_display() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "disabled", "display": "summarized"},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("thinking.display is not supported when thinking.type is disabled")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_allows_empty_thinking_display() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "adaptive", "display": ""},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(validate_messages_request_shape(&req), None);
    }

    #[test]
    fn test_validate_messages_request_shape_allows_large_enabled_budget_without_rust_clamp() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 300000,
            "thinking": {"type": "enabled", "budget_tokens": 200000},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(req.thinking.as_ref().unwrap().budget_tokens, Some(200000));
        assert_eq!(validate_messages_request_shape(&req), None);
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_adaptive_thinking_budget() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "adaptive", "budget_tokens": 20000},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("thinking.budget_tokens is not supported when thinking.type is adaptive")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_allows_adaptive_zero_budget() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "adaptive", "budget_tokens": 0},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(validate_messages_request_shape(&req), None);
    }

    #[test]
    fn test_validate_messages_request_shape_rejects_disabled_thinking_budget() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "disabled", "budget_tokens": 1},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(
            validate_messages_request_shape(&req),
            Some("thinking.budget_tokens is not supported when thinking.type is disabled")
        );
    }

    #[test]
    fn test_validate_messages_request_shape_allows_disabled_zero_budget() {
        let req = messages_req(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "disabled", "budget_tokens": 0},
            "messages": [{"role": "user", "content": "hello"}]
        }));

        assert_eq!(validate_messages_request_shape(&req), None);
    }

    #[test]
    fn test_count_tokens_effective_request_includes_thinking_prefix() {
        let payload: CountTokensRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-6",
            "max_tokens": 4096,
            "thinking": {"type": "adaptive", "display": "summarized"},
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        let base_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        );
        let effective = effective_count_tokens_request(payload, "-thinking");
        let effective_tokens = token::count_all_tokens(
            effective.model,
            effective.system.clone(),
            effective.messages,
            effective.tools,
        );

        let system = effective.system.expect("thinking should inject system");
        assert!(
            system[0]
                .text
                .contains("<thinking_mode>adaptive</thinking_mode>")
        );
        assert!(effective_tokens > base_tokens);
    }

    #[test]
    fn test_count_tokens_effective_request_honors_thinking_model_suffix() {
        let payload: CountTokensRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-5-thinking",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        let effective = effective_count_tokens_request(payload, "-thinking");

        assert_eq!(effective.model, "claude-sonnet-4.5");
        assert_eq!(
            effective.thinking.as_ref().and_then(|t| t.budget_tokens),
            Some(20000)
        );
        assert!(
            effective
                .system
                .as_ref()
                .unwrap()
                .first()
                .unwrap()
                .text
                .contains("<thinking_mode>enabled</thinking_mode>")
        );
    }

    #[test]
    fn test_messages_accounting_request_includes_thinking_prefix() {
        let payload: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-5-thinking",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        let base_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        );
        let effective = request_with_thinking_accounting(payload, "-thinking");
        let effective_tokens = token::count_all_tokens(
            effective.model.clone(),
            effective.system.clone(),
            effective.messages.clone(),
            effective.tools.clone(),
        );

        assert_eq!(effective.model, "claude-sonnet-4.5");
        assert!(
            effective
                .system
                .as_ref()
                .unwrap()
                .first()
                .unwrap()
                .text
                .contains("<thinking_mode>enabled</thinking_mode>")
        );
        assert!(effective_tokens > base_tokens);
    }

    fn sample_messages_request() -> MessagesRequest {
        // 生成一个超过 1024 tokens 的 system message 用于测试缓存
        let long_text = "This is a test system message. ".repeat(100); // 约 600 tokens
        let very_long_text = format!("{}{}", long_text, long_text); // 约 1200 tokens

        MessagesRequest {
            model: "claude-sonnet-4-thinking".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type": "text", "text": "hello raw"},
                    {"type": "text", "text": ""}
                ]),
            }],
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
    fn test_cache_context_uses_cache_profile_tokens() {
        let payload = sample_messages_request();

        let cache_tracker =
            crate::anthropic::cache_tracker::CacheTracker::new(std::time::Duration::from_secs(300));

        let system_text = &payload.system.as_ref().unwrap()[0].text;
        let raw_system_tokens = token::count_tokens(system_text) as i32;

        let cache_profile = build_cache_profile(&cache_tracker, &payload, raw_system_tokens);
        let cache_context = compute_cache_usage(&cache_tracker, 1, &cache_profile);

        assert!(cache_profile.total_input_tokens() >= raw_system_tokens);
        assert_eq!(
            cache_context.cache_creation_input_tokens,
            cache_profile.total_input_tokens()
        );
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
    fn test_non_stream_usage_prefers_real_input_tokens() {
        let estimated_input_tokens = 1493;
        let upstream_context_input_tokens = 3106;
        let cache_creation_input_tokens = 9;
        let cache_read_input_tokens = 1480;

        let final_input_tokens =
            Some(upstream_context_input_tokens).unwrap_or(estimated_input_tokens);
        let billed = billed_input_tokens(
            final_input_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        );

        assert_eq!(final_input_tokens, 3106);
        assert_eq!(billed, 1617);
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
    fn test_build_claude_usage_map_includes_cache_fields() {
        let cache_context = CacheUsageContext {
            cache_creation_input_tokens: 30,
            cache_read_input_tokens: 20,
            cache_creation_5m_input_tokens: 10,
            cache_creation_1h_input_tokens: 20,
        };
        let mut usage = serde_json::json!({
            "input_tokens": billed_input_tokens(100, cache_context.cache_creation_input_tokens, cache_context.cache_read_input_tokens),
            "output_tokens": 50
        });

        inject_cache_usage_fields(&mut usage, cache_context);

        assert_eq!(usage["input_tokens"], 50);
        assert_eq!(usage["output_tokens"], 50);
        assert_eq!(usage["cache_creation_input_tokens"], 30);
        assert_eq!(usage["cache_read_input_tokens"], 20);
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 10);
        assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 20);
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
    fn test_truncate_payload_to_body_limit_preserves_current_and_marks_history_gap() {
        let mut history = vec![
            KiroMessage::user("system prompt", "model"),
            KiroMessage::assistant("I will follow these instructions."),
        ];
        let big = "old context ".repeat(700);
        for i in 0..12 {
            history.push(KiroMessage::user(format!("old user {i}: {big}"), "model"));
            history.push(KiroMessage::assistant(format!("old assistant {i}: {big}")));
        }
        history.push(KiroMessage::user("recent user 1", "model"));
        history.push(KiroMessage::assistant("recent assistant 1"));
        history.push(KiroMessage::user("recent user 2", "model"));
        history.push(KiroMessage::assistant("recent assistant 2"));

        let mut kiro_request = KiroRequest {
            conversation_state: ConversationState::new("conv-truncate")
                .with_current_message(CurrentMessage::new(UserInputMessage::new(
                    "FINAL current message",
                    "model",
                )))
                .with_history(history),
            inference_config: None,
            profile_arn: None,
        };
        let mut body = serde_json::to_string(&kiro_request).unwrap();

        let outcome = truncate_payload_to_body_limit(&mut kiro_request, 4_000, &mut body, true)
            .unwrap()
            .expect("oversized payload should be truncated");

        assert!(body.len() <= 4_000);
        assert!(outcome.removed_history_messages > 0);
        assert!(outcome.inserted_placeholder);
        assert_eq!(
            kiro_request
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "FINAL current message"
        );

        let history = &kiro_request.conversation_state.history;
        assert!(matches!(history[0], KiroMessage::User(_)));
        assert!(matches!(history[1], KiroMessage::Assistant(_)));
        assert!(history.iter().any(|msg| {
            match msg {
                KiroMessage::User(user) => user
                    .user_input_message
                    .content
                    .contains("Earlier conversation history was truncated"),
                _ => false,
            }
        }));
        assert!(history.iter().any(|msg| match msg {
            KiroMessage::User(user) => user.user_input_message.content == "recent user 2",
            _ => false,
        }));
    }

    #[test]
    fn test_truncate_payload_does_not_infer_system_priming_from_ordinary_turn() {
        let big = "ordinary context ".repeat(700);
        let mut history = vec![
            KiroMessage::user(format!("ordinary user: {big}"), "model"),
            KiroMessage::assistant("I will follow these instructions."),
        ];
        for i in 0..10 {
            history.push(KiroMessage::user(format!("old user {i}: {big}"), "model"));
            history.push(KiroMessage::assistant(format!("old assistant {i}: {big}")));
        }
        history.push(KiroMessage::user("recent user", "model"));
        history.push(KiroMessage::assistant("recent assistant"));

        let mut kiro_request = KiroRequest {
            conversation_state: ConversationState::new("conv-truncate")
                .with_current_message(CurrentMessage::new(UserInputMessage::new(
                    "FINAL current message",
                    "model",
                )))
                .with_history(history),
            inference_config: None,
            profile_arn: None,
        };
        let mut body = serde_json::to_string(&kiro_request).unwrap();

        let outcome = truncate_payload_to_body_limit(&mut kiro_request, 4_000, &mut body, false)
            .unwrap()
            .expect("oversized payload should be truncated");

        assert!(outcome.removed_history_messages > 0);
        let history = &kiro_request.conversation_state.history;
        assert!(
            matches!(history.first(), Some(KiroMessage::User(user)) if user.user_input_message.content.contains("Earlier conversation history was truncated"))
        );
        assert!(!history.iter().any(|msg| {
            match msg {
                KiroMessage::User(user) => user
                    .user_input_message
                    .content
                    .starts_with("ordinary user:"),
                _ => false,
            }
        }));
    }

    #[test]
    fn test_prepare_request_truncates_payload_when_compression_disabled() {
        let mut compression = crate::model::config::CompressionConfig::default();
        compression.max_request_body_bytes = 50_000;
        let state = test_state_with_compression(compression);

        let big = "old context ".repeat(700);
        let mut messages = Vec::new();
        for i in 0..12 {
            messages.push(Message {
                role: "user".to_string(),
                content: serde_json::Value::String(format!("old user {i}: {big}")),
            });
            messages.push(Message {
                role: "assistant".to_string(),
                content: serde_json::Value::String(format!("old assistant {i}: {big}")),
            });
        }
        messages.push(Message {
            role: "user".to_string(),
            content: serde_json::Value::String("FINAL current message".to_string()),
        });

        let payload = MessagesRequest {
            model: "claude-sonnet-4.6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let prepared = prepare_request(&state, &payload).expect("prepare");

        assert!(prepared.request_body.len() <= 50_000);
        assert!(
            prepared
                .request_body
                .contains(PAYLOAD_TRUNCATION_PLACEHOLDER)
        );
        assert!(prepared.request_body.contains("FINAL current message"));
    }

    #[test]
    fn test_prepare_request_serializes_anthropic_inference_config() {
        let state = test_state_with_compression(crate::model::config::CompressionConfig::default());
        let payload = MessagesRequest {
            model: "claude-sonnet-4.6".to_string(),
            max_tokens: 123,
            temperature: Some(0.7),
            top_p: Some(0.9),
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String("hello".to_string()),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let prepared = prepare_request(&state, &payload).expect("prepare");
        let body: serde_json::Value =
            serde_json::from_str(&prepared.request_body).expect("request body json");

        assert_eq!(body["inferenceConfig"]["maxTokens"], 123);
        assert_eq!(body["inferenceConfig"]["temperature"], 0.7);
        assert_eq!(body["inferenceConfig"]["topP"], 0.9);
    }

    #[test]
    fn test_improperly_formed_request_message_mentions_common_causes() {
        let response = map_kiro_provider_error_to_response(
            "{}",
            anyhow::anyhow!("400 Improperly formed request"),
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// 凭据队列等待超时 → 429 overloaded_error
    ///
    /// 验证 `wait_any_credential` 抛出的 sentinel 字符串
    /// `"credential queue wait timeout"` 能被 `is_credential_queue_timeout_error`
    /// 正确识别并映射为 429 + overloaded_error，让客户端做指数退避重试。
    #[test]
    fn test_credential_queue_timeout_maps_to_429_overloaded() {
        let response = map_kiro_provider_error_to_response(
            "{}",
            anyhow::anyhow!("credential queue wait timeout"),
        );
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    fn make_req(
        model: &str,
        thinking: Option<Thinking>,
        output_config: Option<OutputConfig>,
    ) -> MessagesRequest {
        MessagesRequest {
            model: model.to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking,
            output_config,
            metadata: None,
        }
    }

    #[test]
    fn test_override_thinking_opus_4_7_enabled_downgrades_to_adaptive() {
        let mut req = make_req(
            "claude-opus-4-7",
            Some(Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: Some(20000),
                display: None,
            }),
            None,
        );
        override_thinking_from_model_name(&mut req, "-thinking");
        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(t.display.as_deref(), Some("summarized"));
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn test_override_thinking_opus_4_7_enabled_trims_and_folds_case() {
        let mut req = make_req(
            "claude-opus-4-7",
            Some(Thinking {
                thinking_type: " Enabled ".to_string(),
                budget_tokens: Some(20000),
                display: None,
            }),
            None,
        );
        override_thinking_from_model_name(&mut req, "-thinking");
        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(t.display.as_deref(), Some("summarized"));
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn test_override_thinking_opus_4_7_no_thinking_does_nothing() {
        let mut req = make_req("claude-opus-4-7", None, None);
        override_thinking_from_model_name(&mut req, "-thinking");
        assert!(req.thinking.is_none());
        assert!(req.output_config.is_none());
    }

    #[test]
    fn test_override_thinking_opus_4_7_thinking_suffix_forces_adaptive() {
        let mut req = make_req("claude-opus-4-7-thinking", None, None);
        override_thinking_from_model_name(&mut req, "-thinking");
        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(t.budget_tokens, None);
        assert_eq!(t.display.as_deref(), Some("summarized"));
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn test_override_thinking_opus_4_6_thinking_suffix_keeps_adaptive() {
        let mut req = make_req("claude-opus-4-6-thinking", None, None);
        override_thinking_from_model_name(&mut req, "-thinking");
        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(t.display.as_deref(), Some("summarized"));
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn test_override_thinking_sonnet_thinking_suffix_uses_enabled() {
        let mut req = make_req("claude-sonnet-4-5-thinking", None, None);
        override_thinking_from_model_name(&mut req, "-thinking");
        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "enabled");
        assert_eq!(t.display, None);
        assert!(req.output_config.is_none());
    }

    #[test]
    fn test_override_thinking_opus_4_7_existing_display_preserved() {
        let mut req = make_req(
            "claude-opus-4-7",
            Some(Thinking {
                thinking_type: "adaptive".to_string(),
                budget_tokens: None,
                display: Some("omitted".to_string()),
            }),
            None,
        );
        override_thinking_from_model_name(&mut req, "-thinking");
        assert_eq!(req.thinking.unwrap().display.as_deref(), Some("omitted"));
    }
}
