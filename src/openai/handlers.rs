//! OpenAI 协议 Handler（Chat Completions + Responses）
//!
//! 设计要点：
//! - OpenAI 请求 → MessagesRequest → 复用 anthropic::converter::convert_request
//! - 流式：使用 super::stream 的 OpenAIChatStream / OpenAIResponsesStream 翻译 Kiro EventStream
//! - 非流式：把上游响应解码完，再合成 OpenAI 协议响应体
//! - 错误：用 OpenAIErrorResponse 映射上游错误

use std::convert::Infallible;
use std::time::Duration;

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
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::interval;

use crate::anthropic::converter::{
    ConversionError, convert_request_with_thinking_suffix, extract_session_id,
};
use crate::anthropic::middleware::{AppState, MatchedApiKeyId};
use crate::anthropic::websearch;
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::{InferenceConfig, KiroRequest};
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::parser::frame::Frame;
use crate::kiro::provider::KiroProvider;
use crate::token;

use super::converter::{
    chat_completions_to_messages_request, parse_responses_input_messages,
    responses_openai_messages_to_messages_request,
};
use super::responses_store::{StoredResponseDoc, expand_previous_response_history, save_response};
use super::stream::{OpenAIChatStream, OpenAIResponsesStream};
use super::types::{
    ChatChoice, ChatChoiceMessage, ChatCompletionsRequest, ChatCompletionsResponse, ChatToolCall,
    ChatToolCallFunction, ChatUsage, OpenAIErrorResponse, ResponsesRequest,
};

const PING_INTERVAL_SECS: u64 = 25;

#[derive(Clone)]
struct ResponsesStoreContext {
    store_dir: Option<std::sync::Arc<std::path::PathBuf>>,
    store: bool,
    stored_input: serde_json::Value,
    instructions: Option<String>,
    previous_response_id: Option<String>,
    metadata: Option<serde_json::Value>,
}

fn create_ping_sse() -> Bytes {
    Bytes::from_static(b": keepalive\n\n")
}

fn apply_frame_usage_to_chat(ctx: &mut OpenAIChatStream, frame: &Frame) {
    if let Some(usage) = crate::kiro::model::events::extract_token_usage_from_frame_with_current(
        frame,
        ctx.current_usage_input_tokens().map(i64::from),
        ctx.current_usage_output_tokens().map(i64::from),
    ) {
        if let Some(input) = usage.input_tokens {
            ctx.set_actual_input_tokens(input as i32);
        }
        if let Some(output) = usage.output_tokens {
            ctx.set_actual_output_tokens(output as i32);
        }
    }
}

fn apply_frame_usage_to_responses(ctx: &mut OpenAIResponsesStream, frame: &Frame) {
    if let Some(usage) = crate::kiro::model::events::extract_token_usage_from_frame_with_current(
        frame,
        ctx.current_usage_input_tokens().map(i64::from),
        ctx.current_usage_output_tokens().map(i64::from),
    ) {
        if let Some(input) = usage.input_tokens {
            ctx.set_actual_input_tokens(input as i32);
        }
        if let Some(output) = usage.output_tokens {
            ctx.set_actual_output_tokens(output as i32);
        }
    }
}

// ============================================================================
// 错误映射（OpenAI 风）
// ============================================================================

fn map_provider_error(err: Error) -> Response {
    let s = err.to_string();
    let s_lower = s.to_lowercase();

    if s.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") || s.contains("Input is too long") {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                OpenAIErrorResponse::new(
                    "invalid_request_error",
                    "Input is too long. Reduce conversation history/system/tools.",
                )
                .with_code("context_length_exceeded"),
            ),
        )
            .into_response();
    }
    if s.contains("Improperly formed request") {
        return (
            StatusCode::BAD_REQUEST,
            Json(OpenAIErrorResponse::new(
                "invalid_request_error",
                "Improperly formed request.",
            )),
        )
            .into_response();
    }
    if s.contains("没有可用的凭据") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(OpenAIErrorResponse::new(
                "service_unavailable",
                "No credentials available.",
            )),
        )
            .into_response();
    }
    if s.contains("credential queue wait timeout") {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(OpenAIErrorResponse::new(
                "rate_limit_error",
                "All credentials are busy. Please retry shortly.",
            )),
        )
            .into_response();
    }
    if s.contains("所有凭据已用尽") {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(OpenAIErrorResponse::new(
                "rate_limit_error",
                "All credentials quota exhausted.",
            )),
        )
            .into_response();
    }

    let transient = s_lower.contains("429 too many requests")
        || s_lower.contains("insufficient_model_capacity")
        || s_lower.contains("high traffic")
        || s_lower.contains("408 request timeout")
        || s_lower.contains("502 bad gateway")
        || s_lower.contains("503 service unavailable")
        || s_lower.contains("504 gateway timeout")
        || s_lower.contains("error sending request")
        || s_lower.contains("connection closed")
        || s_lower.contains("connection reset");
    if transient {
        let is_network = s_lower.contains("error sending request")
            || s_lower.contains("connection closed")
            || s_lower.contains("connection reset");
        if is_network {
            return (
                StatusCode::BAD_GATEWAY,
                Json(OpenAIErrorResponse::new(
                    "api_error",
                    format!("上游网络错误: {}", err),
                )),
            )
                .into_response();
        }
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(OpenAIErrorResponse::new(
                "rate_limit_error",
                err.to_string(),
            )),
        )
            .into_response();
    }

    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(OpenAIErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

// ============================================================================
// 共享：MessagesRequest → KiroRequest body + tool_name_map + input_tokens
// ============================================================================

struct PreparedRequest {
    request_body: String,
    tool_name_map: std::collections::HashMap<String, String>,
    input_tokens: i32,
    user_id: Option<String>,
    model: String,
    openai_thinking_format: String,
}

fn prepare_kiro_request(
    state: &AppState,
    payload: crate::anthropic::types::MessagesRequest,
    fallback_input_tokens: Option<i32>,
    inference_config: Option<InferenceConfig>,
) -> Result<PreparedRequest, Response> {
    let model = payload.model.clone();
    let openai_thinking_format = state.thinking_config.read().openai_format.clone();

    let compression = state.compression_config.read().clone();
    let prompt_filter = state.prompt_filter_config.read().clone();
    let thinking_suffix = state.thinking_config.read().suffix.clone();
    let conversion_result = match convert_request_with_thinking_suffix(
        &payload,
        &compression,
        &prompt_filter,
        false,
        &thinking_suffix,
    ) {
        Ok(r) => r,
        Err(e) => {
            let (code, msg) = match &e {
                ConversionError::UnsupportedModel(m) => {
                    ("invalid_request_error", format!("模型不支持: {}", m))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::EmptyMessageContent => {
                    ("invalid_request_error", "消息内容为空".to_string())
                }
            };
            return Err((
                StatusCode::BAD_REQUEST,
                Json(OpenAIErrorResponse::new(code, msg)),
            )
                .into_response());
        }
    };

    let has_system_priming = conversion_result.has_system_priming;
    let mut kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        inference_config,
        profile_arn: None,
    };

    let mut request_body = match serde_json::to_string(&kiro_request) {
        Ok(b) => b,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OpenAIErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response());
        }
    };

    let max_body = compression.max_request_body_bytes;
    if max_body > 0 && request_body.len() > max_body {
        match crate::anthropic::truncate_payload_to_body_limit(
            &mut kiro_request,
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
                    "OpenAI 请求体超过阈值，已按兼容策略截断历史"
                );
            }
            Ok(None) => {}
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(OpenAIErrorResponse::new(
                        "internal_error",
                        format!("序列化请求失败: {}", e),
                    )),
                )
                    .into_response());
            }
        }
    }

    let input_tokens = fallback_input_tokens.unwrap_or_else(|| {
        token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32
    });

    let raw_user_id = payload.metadata.as_ref().and_then(|m| m.user_id.as_deref());
    let user_id = raw_user_id.and_then(extract_session_id);

    Ok(PreparedRequest {
        request_body,
        tool_name_map: conversion_result.tool_name_map,
        input_tokens,
        user_id,
        model,
        openai_thinking_format,
    })
}

fn openai_inference_config(
    max_tokens: Option<i32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
) -> Option<InferenceConfig> {
    let max_tokens = max_tokens.filter(|v| *v > 0);
    let temperature = temperature.filter(|v| *v > 0.0);
    let top_p = top_p.filter(|v| *v > 0.0);

    if max_tokens.is_none() && temperature.is_none() && top_p.is_none() {
        return None;
    }

    Some(InferenceConfig {
        max_tokens,
        temperature,
        top_p,
    })
}

// ============================================================================
// POST /v1/chat/completions
// ============================================================================

pub async fn post_chat_completions(
    State(state): State<AppState>,
    Extension(matched_api_key): Extension<MatchedApiKeyId>,
    JsonExtractor(req): JsonExtractor<ChatCompletionsRequest>,
) -> Response {
    tracing::info!(
        model = %req.model,
        stream = %req.stream,
        message_count = %req.messages.len(),
        "Received POST /v1/chat/completions"
    );

    if let Some(message) = validate_openai_chat_request_shape(&req) {
        return (
            StatusCode::BAD_REQUEST,
            Json(OpenAIErrorResponse::new("invalid_request_error", message)),
        )
            .into_response();
    }

    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(OpenAIErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    let stream_flag = req.stream;
    let mut messages_request = chat_completions_to_messages_request(&req);

    let thinking_suffix = state.thinking_config.read().suffix.clone();
    crate::anthropic::override_thinking_from_model_name(&mut messages_request, &thinking_suffix);

    if websearch::should_handle_websearch_request(&messages_request) {
        return handle_chat_websearch(provider, messages_request, stream_flag).await;
    }
    if websearch::has_web_search_tool(&messages_request) {
        websearch::strip_web_search_tools(&mut messages_request);
    }

    let fallback_input_tokens = estimate_openai_chat_request_input_tokens(&req);
    let inference_config = openai_inference_config(req.max_tokens, req.temperature, req.top_p);
    let prepared = match prepare_kiro_request(
        &state,
        messages_request,
        Some(fallback_input_tokens),
        inference_config,
    ) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if stream_flag {
        handle_chat_stream(provider, prepared, state.clone(), matched_api_key.0).await
    } else {
        handle_chat_non_stream(provider, prepared, state.clone(), matched_api_key.0).await
    }
}

fn validate_openai_chat_request_shape(req: &ChatCompletionsRequest) -> Option<&'static str> {
    if req.messages.is_empty() {
        return Some("messages must not be empty");
    }

    let mut has_non_system = false;
    let mut has_user_context = false;
    let mut last_role = "";

    for msg in &req.messages {
        let role = msg.role.trim();
        if role.is_empty() {
            continue;
        }
        if role != "system" {
            has_non_system = true;
            last_role = role;
        }
        if role == "user" && openai_chat_user_has_context(msg.content.as_ref()) {
            has_user_context = true;
        }
    }

    if !has_non_system {
        return Some("at least one non-system message is required");
    }
    if last_role == "assistant" {
        return Some(
            "assistant-prefill final message is not supported; last message must be user or tool",
        );
    }
    if !has_user_context {
        return Some("at least one non-empty user message is required");
    }

    None
}

fn openai_chat_user_has_context(content: Option<&serde_json::Value>) -> bool {
    match content {
        Some(serde_json::Value::String(text)) => !text.trim().is_empty(),
        Some(serde_json::Value::Array(parts)) => parts.iter().any(openai_chat_part_has_context),
        Some(serde_json::Value::Object(_)) => content.is_some_and(openai_chat_part_has_context),
        _ => false,
    }
}

fn openai_chat_part_has_context(part: &serde_json::Value) -> bool {
    let Some(obj) = part.as_object() else {
        return false;
    };
    match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "text" | "input_text" | "output_text" => obj
            .get("text")
            .and_then(|v| v.as_str())
            .is_some_and(|text| !text.trim().is_empty()),
        "image" | "image_url" | "input_image" | "file" | "input_file" => {
            openai_chat_part_has_inline_image(part)
        }
        _ => false,
    }
}

fn openai_chat_part_has_inline_image(part: &serde_json::Value) -> bool {
    let Some(obj) = part.as_object() else {
        return false;
    };
    if let Some(file) = obj.get("file").filter(|v| v.is_object())
        && openai_chat_part_has_inline_image(file)
    {
        return true;
    }
    if let Some(source) = obj.get("source").filter(|v| v.is_object())
        && openai_chat_part_has_inline_image(source)
    {
        return true;
    }
    for key in ["url", "data", "b64_json", "image_base64"] {
        if obj.get(key).and_then(|v| v.as_str()).is_some_and(|raw| {
            let raw = raw.trim();
            !raw.is_empty()
                && !raw.contains("[Image")
                && !raw.starts_with("http://")
                && !raw.starts_with("https://")
        }) {
            return true;
        }
    }
    match obj.get("image_url") {
        Some(serde_json::Value::String(raw)) => {
            let raw = raw.trim();
            !raw.is_empty()
                && !raw.contains("[Image")
                && !raw.starts_with("http://")
                && !raw.starts_with("https://")
        }
        Some(serde_json::Value::Object(map)) => {
            map.get("url").and_then(|v| v.as_str()).is_some_and(|raw| {
                let raw = raw.trim();
                !raw.is_empty()
                    && !raw.contains("[Image")
                    && !raw.starts_with("http://")
                    && !raw.starts_with("https://")
            })
        }
        _ => false,
    }
}

fn estimate_openai_chat_request_input_tokens(req: &ChatCompletionsRequest) -> i32 {
    let mut total: u64 = 0;

    for msg in &req.messages {
        total = total.saturating_add(estimate_openai_content_tokens(msg.content.as_ref()));
        if let Some(tool_call_id) = &msg.tool_call_id {
            total = total.saturating_add(token::count_tokens(tool_call_id));
        }
        if let Some(tool_calls) = &msg.tool_calls {
            for tool_call in tool_calls {
                total = total.saturating_add(token::count_tokens(&tool_call.function.name));
                total = total.saturating_add(token::count_tokens(&tool_call.function.arguments));
            }
        }
    }

    if let Some(tools) = &req.tools {
        for tool in tools {
            if let Some(function) = &tool.function {
                total = total.saturating_add(token::count_tokens(&function.name));
                if let Some(description) = &function.description {
                    total = total.saturating_add(token::count_tokens(description));
                }
                if let Some(parameters) = &function.parameters {
                    let json = serde_json::to_string(parameters).unwrap_or_default();
                    total = total.saturating_add(token::count_tokens(&json));
                }
            }
        }
    }

    total.min(i32::MAX as u64) as i32
}

fn estimate_openai_responses_request_input_tokens(req: &ResponsesRequest) -> i32 {
    let mut total: u64 = 0;

    if let Some(instructions) = &req.instructions {
        total = total.saturating_add(token::count_tokens(instructions));
    }
    total = total.saturating_add(estimate_openai_content_tokens(Some(&req.input)));

    if let Some(tools) = &req.tools {
        for tool in tools {
            total = total.saturating_add(estimate_responses_tool_tokens(tool));
        }
    }

    total.min(i32::MAX as u64) as i32
}

fn estimate_responses_tool_tokens(tool: &serde_json::Value) -> u64 {
    if tool.get("type").and_then(|v| v.as_str()) == Some("namespace") {
        return tool
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|tools| {
                tools
                    .iter()
                    .map(estimate_responses_tool_tokens)
                    .fold(0u64, u64::saturating_add)
            })
            .unwrap_or(0);
    }

    let mut total: u64 = 0;
    if let Some(name) = tool.get("name").and_then(|v| v.as_str()).or_else(|| {
        tool.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
    }) {
        total = total.saturating_add(token::count_tokens(name));
    }
    if let Some(description) = tool
        .get("description")
        .and_then(|v| v.as_str())
        .or_else(|| {
            tool.get("function")
                .and_then(|f| f.get("description"))
                .and_then(|v| v.as_str())
        })
    {
        total = total.saturating_add(token::count_tokens(description));
    }
    let parameters = tool
        .get("parameters")
        .or_else(|| tool.get("function").and_then(|f| f.get("parameters")));
    if let Some(parameters) = parameters {
        let json = serde_json::to_string(parameters).unwrap_or_default();
        total = total.saturating_add(token::count_tokens(&json));
    }
    total
}

fn estimate_openai_content_tokens(content: Option<&serde_json::Value>) -> u64 {
    match content {
        None | Some(serde_json::Value::Null) => 0,
        Some(serde_json::Value::String(text)) => token::count_tokens(text),
        Some(value) => {
            let text = extract_openai_message_text_for_estimate(value);
            if !text.is_empty() {
                token::count_tokens(&text)
            } else {
                let json = serde_json::to_string(value).unwrap_or_default();
                token::count_tokens(&json)
            }
        }
    }
}

fn extract_openai_message_text_for_estimate(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                let part_text = extract_openai_message_text_for_estimate(item);
                if !part_text.trim().is_empty() {
                    parts.push(part_text);
                }
            }
            if !parts.is_empty() {
                parts.join("")
            } else {
                serde_json::to_string(value).unwrap_or_default()
            }
        }
        serde_json::Value::Object(obj) => {
            if let Some(text) = extract_openai_user_content_text_for_estimate(obj)
                && !text.trim().is_empty()
            {
                return text;
            }
            if let Some(content) = obj.get("content") {
                let nested_text = extract_openai_message_text_for_estimate(content);
                if !nested_text.trim().is_empty() {
                    return nested_text;
                }
            }
            serde_json::to_string(value).unwrap_or_default()
        }
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn extract_openai_user_content_text_for_estimate(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let part_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match part_type {
        "text" | "input_text" | "output_text" => obj.get("text")?.as_str().map(str::to_string),
        _ => obj.get("text")?.as_str().map(str::to_string),
    }
}

async fn handle_chat_stream(
    provider: std::sync::Arc<KiroProvider>,
    prepared: PreparedRequest,
    app_state: AppState,
    api_key_id: Option<String>,
) -> Response {
    let mut api_result = match provider
        .call_api_stream(&prepared.request_body, prepared.user_id.as_deref())
        .await
    {
        Ok(r) => r,
        Err(e) => return map_provider_error(e),
    };

    let mut ctx = OpenAIChatStream::new(
        prepared.model,
        prepared.input_tokens,
        prepared.tool_name_map,
        prepared.openai_thinking_format,
    );
    let initial = ctx.initial_chunk();

    let cred_permit = api_result._credential_permit.take();
    let glb_permit = api_result._global_permit.take();
    let tm = provider.token_manager().clone();
    let credential_id = api_result.credential_id;

    let stream = create_chat_sse_stream(
        api_result.response,
        ctx,
        initial,
        cred_permit,
        glb_permit,
        tm,
        credential_id,
        app_state,
        api_key_id,
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn create_chat_sse_stream(
    response: reqwest::Response,
    ctx: OpenAIChatStream,
    initial: Bytes,
    cred_permit: Option<OwnedSemaphorePermit>,
    glb_permit: Option<OwnedSemaphorePermit>,
    tm: std::sync::Arc<crate::kiro::token_manager::MultiTokenManager>,
    credential_id: u64,
    app_state: AppState,
    api_key_id: Option<String>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let initial_stream = stream::iter(vec![Ok::<Bytes, Infallible>(initial)]);
    let body_stream = response.bytes_stream();

    let processing = stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            cred_permit,
            glb_permit,
            tm,
            credential_id,
            app_state,
            api_key_id,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping, cred_permit, glb_permit, tm, credential_id, app_state, api_key_id)| async move {
            if finished {
                return None;
            }
            tokio::select! {
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }
                            let mut bytes_out: Vec<Result<Bytes, Infallible>> = Vec::new();
                            for r in decoder.decode_iter() {
                                if let Ok(frame) = r {
                                    apply_frame_usage_to_chat(&mut ctx, &frame);
                                    if let Ok(event) = Event::from_frame(frame) {
                                        for b in ctx.process_event(&event) {
                                            bytes_out.push(Ok(b));
                                        }
                                    }
                                }
                            }
                            Some((stream::iter(bytes_out), (body_stream, ctx, decoder, false, ping, cred_permit, glb_permit, tm, credential_id, app_state, api_key_id)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            drop(cred_permit);
                            drop(glb_permit);
                            if let Some(m) = ctx.metering() {
                                tm.apply_credit_usage(credential_id, m.usage);
                            }
                            let final_bytes: Vec<Result<Bytes, Infallible>> = ctx
                                .finish_events()
                                .into_iter()
                                .map(Ok)
                                .collect();
                            Some((stream::iter(final_bytes), (body_stream, ctx, decoder, true, ping, None, None, tm, credential_id, app_state, api_key_id)))
                        }
                        None => {
                            drop(cred_permit);
                            drop(glb_permit);
                            if let Some(m) = ctx.metering() {
                                tm.apply_credit_usage(credential_id, m.usage);
                            }
                            let credits = ctx.metering().map(|m| m.usage).unwrap_or(0.0);
                            let tokens = i64::from(ctx.final_input_tokens())
                                + i64::from(ctx.final_output_tokens());
                            app_state.record_api_key_usage(api_key_id.as_deref(), tokens, credits);
                            let final_bytes: Vec<Result<Bytes, Infallible>> = ctx
                                .finish_events()
                                .into_iter()
                                .map(Ok)
                                .collect();
                            Some((stream::iter(final_bytes), (body_stream, ctx, decoder, true, ping, None, None, tm, credential_id, app_state, api_key_id)))
                        }
                    }
                }
                _ = ping.tick() => {
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping, cred_permit, glb_permit, tm, credential_id, app_state, api_key_id)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing)
}

async fn handle_chat_non_stream(
    provider: std::sync::Arc<KiroProvider>,
    prepared: PreparedRequest,
    app_state: AppState,
    api_key_id: Option<String>,
) -> Response {
    let api_result = match provider
        .call_api(&prepared.request_body, prepared.user_id.as_deref())
        .await
    {
        Ok(r) => r,
        Err(e) => return map_provider_error(e),
    };

    let body_bytes = match api_result.response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(OpenAIErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    let mut ctx = OpenAIChatStream::new(
        prepared.model.clone(),
        prepared.input_tokens,
        prepared.tool_name_map,
        prepared.openai_thinking_format.clone(),
    );
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }
    for r in decoder.decode_iter() {
        if let Ok(frame) = r {
            apply_frame_usage_to_chat(&mut ctx, &frame);
            if let Ok(event) = Event::from_frame(frame) {
                let _ = ctx.process_event(&event);
            }
        }
    }

    if let Some(m) = ctx.metering() {
        provider
            .token_manager()
            .apply_credit_usage(api_result.credential_id, m.usage);
    }

    let text = ctx.aggregated_text().to_string();
    let reasoning_content = ctx.aggregated_reasoning().to_string();
    let tool_calls_raw = ctx.aggregated_tool_calls();

    let tool_calls: Option<Vec<ChatToolCall>> = if tool_calls_raw.is_empty() {
        None
    } else {
        Some(
            tool_calls_raw
                .into_iter()
                .map(|(id, name, args)| ChatToolCall {
                    id,
                    call_type: "function".to_string(),
                    function: ChatToolCallFunction {
                        name,
                        arguments: args,
                    },
                })
                .collect(),
        )
    };

    let has_tool_calls = tool_calls.is_some();
    let finish_reason = if has_tool_calls {
        Some("tool_calls".to_string())
    } else {
        Some("stop".to_string())
    };

    let prompt_tokens = ctx.final_input_tokens();
    let completion_tokens = ctx.final_output_tokens();
    let total = prompt_tokens.saturating_add(completion_tokens);
    app_state.record_api_key_usage(
        api_key_id.as_deref(),
        i64::from(total),
        ctx.metering().map(|m| m.usage).unwrap_or(0.0),
    );

    let resp = ChatCompletionsResponse {
        id: ctx.completion_id().to_string(),
        object: "chat.completion",
        created: chrono::Utc::now().timestamp(),
        model: prepared.model,
        choices: vec![ChatChoice {
            index: 0,
            message: build_chat_non_stream_message(text, reasoning_content, tool_calls),
            finish_reason,
        }],
        usage: ChatUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: total,
        },
    };

    (StatusCode::OK, Json(resp)).into_response()
}

fn build_chat_non_stream_message(
    text: String,
    reasoning_content: String,
    tool_calls: Option<Vec<ChatToolCall>>,
) -> ChatChoiceMessage {
    if tool_calls.is_some() {
        return ChatChoiceMessage {
            role: "assistant",
            content: None,
            reasoning_content: None,
            tool_calls,
        };
    }

    ChatChoiceMessage {
        role: "assistant",
        content: Some(text),
        reasoning_content: if reasoning_content.is_empty() {
            None
        } else {
            Some(reasoning_content)
        },
        tool_calls,
    }
}

// ============================================================================
// POST /v1/responses
// ============================================================================

pub async fn post_responses(
    State(state): State<AppState>,
    Extension(matched_api_key): Extension<MatchedApiKeyId>,
    JsonExtractor(req): JsonExtractor<ResponsesRequest>,
) -> Response {
    tracing::info!(
        model = %req.model,
        stream = %req.stream,
        "Received POST /v1/responses"
    );

    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(OpenAIErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    let stream_flag = req.stream;
    let previous_response_id = req.previous_response_id.clone();
    let responses_fallback_input_tokens = previous_response_id
        .is_none()
        .then(|| estimate_openai_responses_request_input_tokens(&req));
    let store_ctx = ResponsesStoreContext {
        store_dir: state.responses_store_dir.clone(),
        store: req.store.unwrap_or(true),
        stored_input: req.input.clone(),
        instructions: req.instructions.clone(),
        previous_response_id: previous_response_id.clone(),
        metadata: req.metadata.clone(),
    };
    let mut final_messages = Vec::new();
    if let Some(prev_id) = previous_response_id.as_deref() {
        let Some(store_dir) = state.responses_store_dir.as_deref() else {
            return (
                StatusCode::BAD_REQUEST,
                Json(OpenAIErrorResponse::new(
                    "invalid_request_error",
                    "Responses store is not configured.",
                )),
            )
                .into_response();
        };
        let expanded = match expand_previous_response_history(store_dir, prev_id) {
            Ok(history) => history,
            Err(e) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(OpenAIErrorResponse::new(
                        "invalid_request_error",
                        format!("previous_response_id not found: {}", e),
                    )),
                )
                    .into_response();
            }
        };
        final_messages.extend(expanded.messages);
    }

    if let Some(instructions) = &req.instructions
        && !instructions.trim().is_empty()
    {
        final_messages.push(super::types::ChatMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(instructions.clone())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    let input_messages = match parse_responses_input_messages(&req.input) {
        Ok(messages) => messages,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(OpenAIErrorResponse::new("invalid_request_error", e)),
            )
                .into_response();
        }
    };
    final_messages.extend(input_messages);

    let mut messages_request =
        match responses_openai_messages_to_messages_request(&req, final_messages) {
            Ok(m) => m,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(OpenAIErrorResponse::new("invalid_request_error", e)),
                )
                    .into_response();
            }
        };
    let thinking_suffix = state.thinking_config.read().suffix.clone();
    crate::anthropic::override_thinking_from_model_name(&mut messages_request, &thinking_suffix);

    if websearch::should_handle_websearch_request(&messages_request) {
        return handle_responses_websearch(provider, messages_request, store_ctx, stream_flag)
            .await;
    }
    if websearch::has_web_search_tool(&messages_request) {
        websearch::strip_web_search_tools(&mut messages_request);
    }

    let inference_config =
        openai_inference_config(req.max_output_tokens, req.temperature, req.top_p);
    let prepared = match prepare_kiro_request(
        &state,
        messages_request,
        responses_fallback_input_tokens,
        inference_config,
    ) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if stream_flag {
        handle_responses_stream(
            provider,
            prepared,
            store_ctx,
            state.clone(),
            matched_api_key.0,
        )
        .await
    } else {
        handle_responses_non_stream(
            provider,
            prepared,
            store_ctx,
            state.clone(),
            matched_api_key.0,
        )
        .await
    }
}

async fn handle_responses_stream(
    provider: std::sync::Arc<KiroProvider>,
    prepared: PreparedRequest,
    store_ctx: ResponsesStoreContext,
    app_state: AppState,
    api_key_id: Option<String>,
) -> Response {
    let mut api_result = match provider
        .call_api_stream(&prepared.request_body, prepared.user_id.as_deref())
        .await
    {
        Ok(r) => r,
        Err(e) => return map_provider_error(e),
    };

    let mut ctx = OpenAIResponsesStream::new(
        prepared.model,
        prepared.input_tokens,
        prepared.tool_name_map,
        store_ctx.previous_response_id.clone(),
        store_ctx.metadata.clone(),
        prepared.openai_thinking_format,
    );
    ctx.set_instructions(store_ctx.instructions.clone());
    let initial = ctx.initial_events();

    let cred_permit = api_result._credential_permit.take();
    let glb_permit = api_result._global_permit.take();
    let tm = provider.token_manager().clone();
    let credential_id = api_result.credential_id;

    let stream = create_responses_sse_stream(
        api_result.response,
        ctx,
        initial,
        cred_permit,
        glb_permit,
        tm,
        credential_id,
        store_ctx,
        app_state,
        api_key_id,
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn create_responses_sse_stream(
    response: reqwest::Response,
    ctx: OpenAIResponsesStream,
    initial: Vec<Bytes>,
    cred_permit: Option<OwnedSemaphorePermit>,
    glb_permit: Option<OwnedSemaphorePermit>,
    tm: std::sync::Arc<crate::kiro::token_manager::MultiTokenManager>,
    credential_id: u64,
    store_ctx: ResponsesStoreContext,
    app_state: AppState,
    api_key_id: Option<String>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let initial_stream = stream::iter(
        initial
            .into_iter()
            .map(|b| Ok::<Bytes, Infallible>(b))
            .collect::<Vec<_>>(),
    );
    let body_stream = response.bytes_stream();

    let processing = stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            cred_permit,
            glb_permit,
            tm,
            credential_id,
            store_ctx,
            app_state,
            api_key_id,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping, cred_permit, glb_permit, tm, credential_id, store_ctx, app_state, api_key_id)| async move {
            if finished {
                return None;
            }
            tokio::select! {
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }
                            let mut bytes_out: Vec<Result<Bytes, Infallible>> = Vec::new();
                            for r in decoder.decode_iter() {
                                if let Ok(frame) = r {
                                    apply_frame_usage_to_responses(&mut ctx, &frame);
                                    if let Ok(event) = Event::from_frame(frame) {
                                        for b in ctx.process_event(&event) {
                                            bytes_out.push(Ok(b));
                                        }
                                    }
                                }
                            }
                            Some((stream::iter(bytes_out), (body_stream, ctx, decoder, false, ping, cred_permit, glb_permit, tm, credential_id, store_ctx, app_state, api_key_id)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            drop(cred_permit);
                            drop(glb_permit);
                            if let Some(m) = ctx.metering() {
                                tm.apply_credit_usage(credential_id, m.usage);
                            }
                            let final_bytes: Vec<Result<Bytes, Infallible>> = ctx
                                .finish_events()
                                .into_iter()
                                .map(Ok)
                                .collect();
                            Some((stream::iter(final_bytes), (body_stream, ctx, decoder, true, ping, None, None, tm, credential_id, store_ctx, app_state, api_key_id)))
                        }
                        None => {
                            drop(cred_permit);
                            drop(glb_permit);
                            if let Some(m) = ctx.metering() {
                                tm.apply_credit_usage(credential_id, m.usage);
                            }
                            let credits = ctx.metering().map(|m| m.usage).unwrap_or(0.0);
                            let tokens = i64::from(ctx.final_input_tokens())
                                + i64::from(ctx.final_output_tokens());
                            app_state.record_api_key_usage(api_key_id.as_deref(), tokens, credits);
                            persist_response_if_needed(
                                &store_ctx,
                                ctx.response_id(),
                                ctx.created_at(),
                                "completed",
                                ctx.model(),
                                ctx.completed_output_items(),
                                ctx.completed_usage(),
                            );
                            let final_bytes: Vec<Result<Bytes, Infallible>> = ctx
                                .finish_events()
                                .into_iter()
                                .map(Ok)
                                .collect();
                            Some((stream::iter(final_bytes), (body_stream, ctx, decoder, true, ping, None, None, tm, credential_id, store_ctx, app_state, api_key_id)))
                        }
                    }
                }
                _ = ping.tick() => {
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping, cred_permit, glb_permit, tm, credential_id, store_ctx, app_state, api_key_id)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing)
}

async fn handle_responses_non_stream(
    provider: std::sync::Arc<KiroProvider>,
    prepared: PreparedRequest,
    store_ctx: ResponsesStoreContext,
    app_state: AppState,
    api_key_id: Option<String>,
) -> Response {
    let api_result = match provider
        .call_api(&prepared.request_body, prepared.user_id.as_deref())
        .await
    {
        Ok(r) => r,
        Err(e) => return map_provider_error(e),
    };

    let body_bytes = match api_result.response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(OpenAIErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    let mut ctx = OpenAIResponsesStream::new(
        prepared.model.clone(),
        prepared.input_tokens,
        prepared.tool_name_map,
        store_ctx.previous_response_id.clone(),
        store_ctx.metadata.clone(),
        prepared.openai_thinking_format,
    );
    ctx.set_instructions(store_ctx.instructions.clone());
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }
    for r in decoder.decode_iter() {
        if let Ok(frame) = r {
            apply_frame_usage_to_responses(&mut ctx, &frame);
            if let Ok(event) = Event::from_frame(frame) {
                let _ = ctx.process_event(&event);
            }
        }
    }

    if let Some(m) = ctx.metering() {
        provider
            .token_manager()
            .apply_credit_usage(api_result.credential_id, m.usage);
    }

    let response_model = prepared.model.clone();
    let output_items = ctx.completed_output_items();
    let usage = ctx.completed_usage();
    let tokens = i64::from(ctx.final_input_tokens()) + i64::from(ctx.final_output_tokens());
    app_state.record_api_key_usage(
        api_key_id.as_deref(),
        tokens,
        ctx.metering().map(|m| m.usage).unwrap_or(0.0),
    );
    let mut body = serde_json::json!({
        "id": ctx.response_id(),
        "object": "response",
        "created_at": ctx.created_at(),
        "status": "completed",
        "model": response_model,
        "previous_response_id": ctx.previous_response_id(),
        "output": output_items.clone(),
        "usage": usage,
    });
    if let Some(metadata) = &store_ctx.metadata {
        body["metadata"] = metadata.clone();
    }
    if let Some(instructions) = store_ctx.instructions.as_deref()
        && !instructions.trim().is_empty()
    {
        body["instructions"] = serde_json::json!(instructions);
    }

    persist_response_if_needed(
        &store_ctx,
        ctx.response_id(),
        ctx.created_at(),
        "completed",
        &response_model,
        output_items,
        usage,
    );

    (StatusCode::OK, Json(body)).into_response()
}

fn persist_response_if_needed(
    ctx: &ResponsesStoreContext,
    id: &str,
    created_at: i64,
    status: &str,
    model: &str,
    output: Vec<serde_json::Value>,
    usage: serde_json::Value,
) {
    if !ctx.store {
        return;
    }
    let Some(store_dir) = ctx.store_dir.as_deref() else {
        return;
    };
    let doc = StoredResponseDoc {
        id: id.to_string(),
        object: "response".to_string(),
        created_at,
        status: status.to_string(),
        model: model.to_string(),
        output,
        usage,
        previous_response_id: ctx.previous_response_id.clone(),
        metadata: ctx.metadata.clone(),
        instructions: ctx.instructions.clone(),
        stored_input: ctx.stored_input.clone(),
        stored_at: 0,
    };
    if let Err(e) = save_response(store_dir, doc) {
        tracing::warn!(response_id = id, error = %e, "Responses 持久化失败");
    }
}

// ============================================================================
// WebSearch 处理（hosted web_search）
// ============================================================================

struct WebSearchExecution {
    query: String,
    tool_call_id: String,
    summary: String,
    sources: Vec<serde_json::Value>,
    input_tokens: i32,
    output_tokens: i32,
}

async fn execute_websearch(
    provider: std::sync::Arc<KiroProvider>,
    payload: &crate::anthropic::types::MessagesRequest,
) -> Result<WebSearchExecution, Response> {
    let query = match websearch::extract_search_query(payload) {
        Some(q) => q,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(OpenAIErrorResponse::new(
                    "invalid_request_error",
                    "无法从消息中提取搜索查询",
                )),
            )
                .into_response());
        }
    };

    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    let (tool_call_id, mcp_request) = websearch::create_mcp_request(&query);

    let results = match websearch::call_mcp_api(&provider, &mcp_request).await {
        Ok(api_result) => websearch::parse_search_results(&api_result.response),
        Err(e) => {
            tracing::warn!("WebSearch MCP 调用失败: {}", e);
            None
        }
    };

    let summary = websearch::generate_search_summary(&query, &results);
    let sources: Vec<serde_json::Value> = match &results {
        Some(r) => r
            .results
            .iter()
            .map(|item| {
                serde_json::json!({
                    "type": "url_citation",
                    "url": item.url,
                    "title": item.title,
                    "snippet": item.snippet.clone().unwrap_or_default(),
                })
            })
            .collect(),
        None => Vec::new(),
    };

    let output_tokens = (summary.len() as i32 + 3) / 4;

    Ok(WebSearchExecution {
        query,
        tool_call_id,
        summary,
        sources,
        input_tokens,
        output_tokens,
    })
}

async fn handle_chat_websearch(
    provider: std::sync::Arc<KiroProvider>,
    payload: crate::anthropic::types::MessagesRequest,
    stream_flag: bool,
) -> Response {
    let model = payload.model.clone();
    let exec = match execute_websearch(provider, &payload).await {
        Ok(e) => e,
        Err(resp) => return resp,
    };

    let completion_id = format!(
        "chatcmpl-{}",
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]
    );
    let created = chrono::Utc::now().timestamp();
    let usage = ChatUsage {
        prompt_tokens: exec.input_tokens,
        completion_tokens: exec.output_tokens,
        total_tokens: exec.input_tokens.saturating_add(exec.output_tokens),
    };

    if !stream_flag {
        let resp = ChatCompletionsResponse {
            id: completion_id,
            object: "chat.completion",
            created,
            model,
            choices: vec![ChatChoice {
                index: 0,
                message: ChatChoiceMessage {
                    role: "assistant",
                    content: Some(exec.summary),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage,
        };
        let _ = exec.tool_call_id;
        let _ = exec.sources;
        let _ = exec.query;
        return (StatusCode::OK, Json(resp)).into_response();
    }

    // 流式：role / content / finish + [DONE]
    let role_chunk = serde_json::json!({
        "id": &completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": &model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": "" },
            "finish_reason": null
        }]
    });
    let content_chunk = serde_json::json!({
        "id": &completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": &model,
        "choices": [{
            "index": 0,
            "delta": { "content": exec.summary },
            "finish_reason": null
        }]
    });
    let finish_chunk = serde_json::json!({
        "id": &completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": &model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens
        }
    });

    let frames: Vec<Result<Bytes, Infallible>> = vec![
        Ok(Bytes::from(format!("data: {}\n\n", role_chunk))),
        Ok(Bytes::from(format!("data: {}\n\n", content_chunk))),
        Ok(Bytes::from(format!("data: {}\n\n", finish_chunk))),
        Ok(Bytes::from_static(b"data: [DONE]\n\n")),
    ];
    let body_stream = stream::iter(frames);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state_with_compression(
        compression: crate::model::config::CompressionConfig,
    ) -> AppState {
        AppState::new(
            "test-key",
            false,
            false,
            std::sync::Arc::new(parking_lot::RwLock::new(
                crate::anthropic::middleware::PromptCacheRuntime::new(300, false),
            )),
            crate::anthropic::middleware::ThinkingRuntimeConfig {
                suffix: "-thinking".to_string(),
                openai_format: "reasoning_content".to_string(),
                claude_format: "thinking".to_string(),
            },
        )
        .with_compression_config(std::sync::Arc::new(parking_lot::RwLock::new(compression)))
    }

    fn chat_req(value: serde_json::Value) -> ChatCompletionsRequest {
        serde_json::from_value(value).expect("chat request fixture should parse")
    }

    fn responses_req(value: serde_json::Value) -> ResponsesRequest {
        serde_json::from_value(value).expect("responses request fixture should parse")
    }

    #[test]
    fn test_validate_openai_chat_request_shape_rejects_assistant_prefill() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "prefill"}
            ]
        }));

        assert_eq!(
            validate_openai_chat_request_shape(&req),
            Some(
                "assistant-prefill final message is not supported; last message must be user or tool"
            )
        );
    }

    #[test]
    fn test_validate_openai_chat_request_shape_allows_tool_result_final_turn() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [
                {"role": "user", "content": "find weather"},
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{}"}
                    }]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ]
        }));

        assert_eq!(validate_openai_chat_request_shape(&req), None);
    }

    #[test]
    fn test_validate_openai_chat_request_shape_rejects_empty_messages() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": []
        }));

        assert_eq!(
            validate_openai_chat_request_shape(&req),
            Some("messages must not be empty")
        );
    }

    #[test]
    fn test_validate_openai_chat_request_shape_rejects_system_only() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [
                {"role": "system", "content": "rules"}
            ]
        }));

        assert_eq!(
            validate_openai_chat_request_shape(&req),
            Some("at least one non-system message is required")
        );
    }

    #[test]
    fn test_validate_openai_chat_request_shape_rejects_without_user_context() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [
                {"role": "user", "content": "   "},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ]
        }));

        assert_eq!(
            validate_openai_chat_request_shape(&req),
            Some("at least one non-empty user message is required")
        );
    }

    #[test]
    fn test_validate_openai_chat_request_shape_accepts_inline_image_user_context() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {
                        "url": "data:image/png;base64,iVBORw0KGgo="
                    }
                }]
            }]
        }));

        assert_eq!(validate_openai_chat_request_shape(&req), None);
    }

    #[test]
    fn test_validate_openai_chat_request_shape_rejects_image_placeholder_like_kiro_go() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {
                        "url": "[Image 1]"
                    }
                }]
            }]
        }));

        assert_eq!(
            validate_openai_chat_request_shape(&req),
            Some("at least one non-empty user message is required")
        );
    }

    #[test]
    fn chat_non_stream_message_drops_text_and_reasoning_when_tool_calls_like_kiro_go() {
        let message = build_chat_non_stream_message(
            "partial text".to_string(),
            "hidden reasoning".to_string(),
            Some(vec![ChatToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: ChatToolCallFunction {
                    name: "lookup".to_string(),
                    arguments: "{\"q\":\"x\"}".to_string(),
                },
            }]),
        );

        assert_eq!(message.content, None);
        assert_eq!(message.reasoning_content, None);
        let tool_calls = message
            .tool_calls
            .as_ref()
            .expect("tool calls should be preserved");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].function.name, "lookup");

        let value = serde_json::to_value(&message).expect("message serializes");
        assert_eq!(value["content"], serde_json::Value::Null);
        assert!(value.get("reasoning_content").is_none());
    }

    #[test]
    fn chat_non_stream_message_keeps_text_and_reasoning_without_tool_calls() {
        let message = build_chat_non_stream_message(
            "final text".to_string(),
            "hidden reasoning".to_string(),
            None,
        );

        assert_eq!(message.content.as_deref(), Some("final text"));
        assert_eq!(
            message.reasoning_content.as_deref(),
            Some("hidden reasoning")
        );
        assert!(message.tool_calls.is_none());
    }

    #[test]
    fn chat_non_stream_message_serializes_empty_content_without_tool_calls_like_kiro_go() {
        let message = build_chat_non_stream_message(String::new(), String::new(), None);

        let value = serde_json::to_value(&message).expect("message serializes");
        assert_eq!(value["content"], "");
        assert!(value.get("reasoning_content").is_none());
        assert!(value.get("tool_calls").is_none());
    }

    #[test]
    fn test_estimate_openai_chat_request_input_tokens_counts_tool_calls_and_tools() {
        let without_tools = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        }));
        let with_tools = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [
                {"role": "user", "content": "hello"},
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "lookup_weather",
                            "arguments": "{\"city\":\"Paris\"}"
                        }
                    }]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup_weather",
                    "description": "Get weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}}
                    }
                }
            }]
        }));

        assert!(
            estimate_openai_chat_request_input_tokens(&with_tools)
                > estimate_openai_chat_request_input_tokens(&without_tools)
        );
    }

    #[test]
    fn test_estimate_openai_chat_request_input_tokens_uses_array_text() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "input_text", "text": "world"}
                ]
            }]
        }));

        let expected = token::count_tokens("helloworld") as i32;
        assert_eq!(estimate_openai_chat_request_input_tokens(&req), expected);
    }

    #[test]
    fn test_estimate_openai_chat_request_input_tokens_recurses_nested_content_like_kiro_go() {
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [{
                "role": "user",
                "content": {
                    "type": "message",
                    "content": [
                        {"type": "input_text", "text": "alpha"},
                        {"type": "input_text", "text": "beta"}
                    ]
                }
            }]
        }));

        let expected = token::count_tokens("alphabeta") as i32;
        assert_eq!(estimate_openai_chat_request_input_tokens(&req), expected);
    }

    #[test]
    fn test_estimate_openai_responses_request_input_tokens_counts_tools_and_instructions() {
        let base = responses_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "input": "hello"
        }));
        let with_tools = responses_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "instructions": "be concise",
            "input": [
                {"type": "message", "role": "user", "content": "hello"}
            ],
            "tools": [{
                "type": "function",
                "name": "lookup_weather",
                "description": "Get weather",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}}
                }
            }]
        }));

        assert!(
            estimate_openai_responses_request_input_tokens(&with_tools)
                > estimate_openai_responses_request_input_tokens(&base)
        );
    }

    #[test]
    fn test_estimate_openai_responses_request_input_tokens_counts_namespace_tools() {
        let req = responses_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "input": "hello",
            "tools": [{
                "type": "namespace",
                "tools": [{
                    "type": "function",
                    "name": "lookup_weather",
                    "description": "Get weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}}
                    }
                }]
            }]
        }));
        let expected_tool_tokens = estimate_responses_tool_tokens(&req.tools.as_ref().unwrap()[0]);

        assert!(expected_tool_tokens > 0);
        assert!(
            estimate_openai_responses_request_input_tokens(&req)
                >= token::count_tokens("hello") as i32 + expected_tool_tokens as i32
        );
    }

    #[test]
    fn test_prepare_kiro_request_truncates_openai_payload_like_kiro_go() {
        let mut compression = crate::model::config::CompressionConfig::default();
        compression.max_request_body_bytes = 50_000;
        let state = test_state_with_compression(compression);

        let big = "old context ".repeat(700);
        let mut messages = Vec::new();
        for i in 0..12 {
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!("old user {i}: {big}")
            }));
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("old assistant {i}: {big}")
            }));
        }
        messages.push(serde_json::json!({
            "role": "user",
            "content": "FINAL current message"
        }));

        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": messages
        }));
        let messages_request = chat_completions_to_messages_request(&req);

        let prepared = prepare_kiro_request(&state, messages_request, None, None).expect("prepare");

        assert!(prepared.request_body.len() <= 50_000);
        assert!(prepared.request_body.contains(
            "[Earlier conversation history was truncated to fit the model's input limit."
        ));
        assert!(prepared.request_body.contains("FINAL current message"));
    }

    #[test]
    fn test_prepare_kiro_request_serializes_openai_inference_config_like_kiro_go() {
        let state = test_state_with_compression(crate::model::config::CompressionConfig::default());
        let req = chat_req(serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 123,
            "temperature": 0.7,
            "top_p": 0.9
        }));
        let messages_request = chat_completions_to_messages_request(&req);
        let inference_config = openai_inference_config(req.max_tokens, req.temperature, req.top_p);

        let prepared = prepare_kiro_request(&state, messages_request, None, inference_config)
            .expect("prepare");
        let body: serde_json::Value =
            serde_json::from_str(&prepared.request_body).expect("request body json");

        assert_eq!(body["inferenceConfig"]["maxTokens"], 123);
        assert_eq!(body["inferenceConfig"]["temperature"], 0.7);
        assert_eq!(body["inferenceConfig"]["topP"], 0.9);
    }

    #[test]
    fn test_responses_continuation_keeps_new_instructions_like_kiro_go() {
        let dir = std::env::temp_dir().join(format!(
            "xkiro-responses-continuation-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        save_response(
            &dir,
            StoredResponseDoc {
                id: "resp_prev".to_string(),
                object: "response".to_string(),
                created_at: 1,
                status: "completed".to_string(),
                model: "claude-sonnet-4.5".to_string(),
                output: vec![serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "first reply" }]
                })],
                usage: serde_json::Value::Null,
                previous_response_id: None,
                metadata: None,
                instructions: Some("old instruction".to_string()),
                stored_input: serde_json::json!("first user message"),
                stored_at: 0,
            },
        )
        .expect("save previous response");

        let req = responses_req(serde_json::json!({
            "model": "claude-sonnet-4.5",
            "input": "second user turn",
            "previous_response_id": "resp_prev",
            "instructions": "speak only French"
        }));
        let mut final_messages = expand_previous_response_history(&dir, "resp_prev")
            .expect("expand")
            .messages;
        if let Some(instructions) = &req.instructions
            && !instructions.trim().is_empty()
        {
            final_messages.push(crate::openai::types::ChatMessage {
                role: "system".to_string(),
                content: Some(serde_json::Value::String(instructions.clone())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        final_messages.extend(parse_responses_input_messages(&req.input).expect("parse input"));

        let converted =
            responses_openai_messages_to_messages_request(&req, final_messages).expect("convert");
        let system_text = serde_json::to_string(&converted.system).expect("system json");
        let messages_text = serde_json::to_string(&converted.messages).expect("messages json");

        assert!(system_text.contains("old instruction"));
        assert!(system_text.contains("speak only French"));
        assert!(messages_text.contains("first user message"));
        assert!(messages_text.contains("second user turn"));

        let _ = std::fs::remove_dir_all(dir);
    }
}

async fn handle_responses_websearch(
    provider: std::sync::Arc<KiroProvider>,
    payload: crate::anthropic::types::MessagesRequest,
    store_ctx: ResponsesStoreContext,
    stream_flag: bool,
) -> Response {
    let model = payload.model.clone();
    let exec = match execute_websearch(provider, &payload).await {
        Ok(e) => e,
        Err(resp) => return resp,
    };

    let response_id = format!(
        "resp_{}",
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]
    );
    let message_id = format!(
        "msg_{}",
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]
    );
    let websearch_item_id = format!(
        "ws_{}",
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..22]
    );
    let created = chrono::Utc::now().timestamp();
    let prev_id_value = store_ctx
        .previous_response_id
        .clone()
        .map(serde_json::Value::String)
        .unwrap_or(serde_json::Value::Null);

    let websearch_item = serde_json::json!({
        "id": websearch_item_id,
        "type": "web_search_call",
        "status": "completed",
        "action": {
            "type": "search",
            "query": exec.query,
            "sources": exec.sources,
        }
    });
    let message_item = serde_json::json!({
        "id": message_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": exec.summary,
            "annotations": []
        }]
    });

    let usage = serde_json::json!({
        "input_tokens": exec.input_tokens,
        "output_tokens": exec.output_tokens,
        "total_tokens": exec.input_tokens.saturating_add(exec.output_tokens),
    });

    if !stream_flag {
        let output = vec![websearch_item.clone(), message_item.clone()];
        let body = serde_json::json!({
            "id": response_id,
            "object": "response",
            "created_at": created,
            "status": "completed",
            "model": model,
            "previous_response_id": prev_id_value,
            "output": output.clone(),
            "usage": usage,
        });
        persist_response_if_needed(
            &store_ctx,
            body["id"].as_str().unwrap_or_default(),
            created,
            "completed",
            body["model"].as_str().unwrap_or_default(),
            output,
            body["usage"].clone(),
        );
        let _ = exec.tool_call_id;
        return (StatusCode::OK, Json(body)).into_response();
    }

    let created_event = serde_json::json!({
        "type": "response.created",
        "response": {
            "id": &response_id,
            "object": "response",
            "created_at": created,
            "status": "in_progress",
            "model": &model,
            "previous_response_id": &prev_id_value,
            "output": [],
        }
    });
    let ws_added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": &websearch_item,
    });
    let ws_done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": &websearch_item,
    });
    let msg_added_item = serde_json::json!({
        "id": &message_id,
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let msg_added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 1,
        "item": msg_added_item,
    });
    let delta_event = serde_json::json!({
        "type": "response.output_text.delta",
        "item_id": &message_id,
        "output_index": 1,
        "content_index": 0,
        "delta": &exec.summary,
    });
    let msg_done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 1,
        "item": &message_item,
    });
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": &response_id,
            "object": "response",
            "created_at": created,
            "status": "completed",
            "model": &model,
            "previous_response_id": &prev_id_value,
            "output": [&websearch_item, &message_item],
            "usage": &usage,
        }
    });

    let frames: Vec<Result<Bytes, Infallible>> = vec![
        Ok(Bytes::from(format!(
            "event: response.created\ndata: {}\n\n",
            created_event
        ))),
        Ok(Bytes::from(format!(
            "event: response.output_item.added\ndata: {}\n\n",
            ws_added
        ))),
        Ok(Bytes::from(format!(
            "event: response.output_item.done\ndata: {}\n\n",
            ws_done
        ))),
        Ok(Bytes::from(format!(
            "event: response.output_item.added\ndata: {}\n\n",
            msg_added
        ))),
        Ok(Bytes::from(format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            delta_event
        ))),
        Ok(Bytes::from(format!(
            "event: response.output_item.done\ndata: {}\n\n",
            msg_done
        ))),
        Ok(Bytes::from(format!(
            "event: response.completed\ndata: {}\n\n",
            completed
        ))),
    ];
    let body_stream = stream::iter(frames);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}
