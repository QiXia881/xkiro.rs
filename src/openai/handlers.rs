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
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::interval;

use crate::anthropic::converter::{ConversionError, convert_request, extract_session_id};
use crate::anthropic::middleware::AppState;
use crate::anthropic::websearch;
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::token;

use super::converter::{
    chat_completions_to_messages_request, responses_to_messages_request,
};
use super::stream::{OpenAIChatStream, OpenAIResponsesStream};
use super::types::{
    ChatChoice, ChatChoiceMessage, ChatCompletionsRequest, ChatCompletionsResponse, ChatToolCall,
    ChatToolCallFunction, ChatUsage, OpenAIErrorResponse, ResponsesRequest,
};

const PING_INTERVAL_SECS: u64 = 25;

fn create_ping_sse() -> Bytes {
    Bytes::from_static(b": keepalive\n\n")
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
            Json(OpenAIErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce conversation history/system/tools.",
            )
            .with_code("context_length_exceeded")),
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
            Json(OpenAIErrorResponse::new("rate_limit_error", err.to_string())),
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
}

fn prepare_kiro_request(
    state: &AppState,
    payload: crate::anthropic::types::MessagesRequest,
) -> Result<PreparedRequest, Response> {
    let model = payload.model.clone();

    let compression = state.compression_config.read().clone();
    let prompt_filter = state.prompt_filter_config.read().clone();
    let conversion_result = match convert_request(
        &payload,
        &compression,
        &prompt_filter,
        state.truncation_recovery_notice_enabled(),
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

    let mut conversation_state = conversion_result.conversation_state;
    if compression.enabled {
        let _ = crate::anthropic::compressor::compress(&mut conversation_state, &compression);
    }

    let kiro_request = KiroRequest {
        conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
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

    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    let raw_user_id = payload
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref());
    let user_id = raw_user_id.and_then(extract_session_id);

    Ok(PreparedRequest {
        request_body,
        tool_name_map: conversion_result.tool_name_map,
        input_tokens,
        user_id,
        model,
    })
}

// ============================================================================
// POST /v1/chat/completions
// ============================================================================

pub async fn post_chat_completions(
    State(state): State<AppState>,
    JsonExtractor(req): JsonExtractor<ChatCompletionsRequest>,
) -> Response {
    tracing::info!(
        model = %req.model,
        stream = %req.stream,
        message_count = %req.messages.len(),
        "Received POST /v1/chat/completions"
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
    let mut messages_request = chat_completions_to_messages_request(&req);

    if websearch::should_handle_websearch_request(&messages_request) {
        return handle_chat_websearch(provider, messages_request, stream_flag).await;
    }
    if websearch::has_web_search_tool(&messages_request) {
        websearch::strip_web_search_tools(&mut messages_request);
    }

    let prepared = match prepare_kiro_request(&state, messages_request) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if stream_flag {
        handle_chat_stream(provider, prepared).await
    } else {
        handle_chat_non_stream(provider, prepared).await
    }
}

async fn handle_chat_stream(
    provider: std::sync::Arc<KiroProvider>,
    prepared: PreparedRequest,
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
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping, cred_permit, glb_permit, tm, credential_id)| async move {
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
                                    if let Ok(event) = Event::from_frame(frame) {
                                        for b in ctx.process_event(&event) {
                                            bytes_out.push(Ok(b));
                                        }
                                    }
                                }
                            }
                            Some((stream::iter(bytes_out), (body_stream, ctx, decoder, false, ping, cred_permit, glb_permit, tm, credential_id)))
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
                            Some((stream::iter(final_bytes), (body_stream, ctx, decoder, true, ping, None, None, tm, credential_id)))
                        }
                        None => {
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
                            Some((stream::iter(final_bytes), (body_stream, ctx, decoder, true, ping, None, None, tm, credential_id)))
                        }
                    }
                }
                _ = ping.tick() => {
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping, cred_permit, glb_permit, tm, credential_id)))
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
    );
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }
    for r in decoder.decode_iter() {
        if let Ok(frame) = r {
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
                    function: ChatToolCallFunction { name, arguments: args },
                })
                .collect(),
        )
    };

    let finish_reason = if tool_calls.is_some() {
        Some("tool_calls".to_string())
    } else {
        Some("stop".to_string())
    };

    let prompt_tokens = ctx.final_input_tokens();
    let completion_tokens = ctx.final_output_tokens();
    let total = prompt_tokens.saturating_add(completion_tokens);

    let resp = ChatCompletionsResponse {
        id: ctx.completion_id().to_string(),
        object: "chat.completion",
        created: chrono::Utc::now().timestamp(),
        model: prepared.model,
        choices: vec![ChatChoice {
            index: 0,
            message: ChatChoiceMessage {
                role: "assistant",
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls,
            },
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

// ============================================================================
// POST /v1/responses
// ============================================================================

pub async fn post_responses(
    State(state): State<AppState>,
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
    let messages_request = match responses_to_messages_request(&req) {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(OpenAIErrorResponse::new("invalid_request_error", e)),
            )
                .into_response();
        }
    };
    let mut messages_request = messages_request;

    if websearch::should_handle_websearch_request(&messages_request) {
        return handle_responses_websearch(
            provider,
            messages_request,
            previous_response_id,
            stream_flag,
        )
        .await;
    }
    if websearch::has_web_search_tool(&messages_request) {
        websearch::strip_web_search_tools(&mut messages_request);
    }

    let prepared = match prepare_kiro_request(&state, messages_request) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if stream_flag {
        handle_responses_stream(provider, prepared, previous_response_id).await
    } else {
        handle_responses_non_stream(provider, prepared, previous_response_id).await
    }
}

async fn handle_responses_stream(
    provider: std::sync::Arc<KiroProvider>,
    prepared: PreparedRequest,
    previous_response_id: Option<String>,
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
        previous_response_id,
    );
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
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping, cred_permit, glb_permit, tm, credential_id)| async move {
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
                                    if let Ok(event) = Event::from_frame(frame) {
                                        for b in ctx.process_event(&event) {
                                            bytes_out.push(Ok(b));
                                        }
                                    }
                                }
                            }
                            Some((stream::iter(bytes_out), (body_stream, ctx, decoder, false, ping, cred_permit, glb_permit, tm, credential_id)))
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
                            Some((stream::iter(final_bytes), (body_stream, ctx, decoder, true, ping, None, None, tm, credential_id)))
                        }
                        None => {
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
                            Some((stream::iter(final_bytes), (body_stream, ctx, decoder, true, ping, None, None, tm, credential_id)))
                        }
                    }
                }
                _ = ping.tick() => {
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping, cred_permit, glb_permit, tm, credential_id)))
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
    previous_response_id: Option<String>,
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
        previous_response_id,
    );
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }
    for r in decoder.decode_iter() {
        if let Ok(frame) = r {
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

    let prompt_tokens = ctx.final_input_tokens();
    let completion_tokens = ctx.final_output_tokens();
    let total = prompt_tokens.saturating_add(completion_tokens);

    let text = ctx.aggregated_text().to_string();
    let tool_calls_raw = ctx.aggregated_tool_calls();

    let mut output_items: Vec<serde_json::Value> = Vec::new();
    let mut message_content: Vec<serde_json::Value> = Vec::new();
    if !text.is_empty() {
        message_content.push(serde_json::json!({
            "type": "output_text",
            "text": text,
            "annotations": [],
        }));
    }
    output_items.push(serde_json::json!({
        "id": ctx.message_id(),
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": message_content,
    }));
    for (id, name, args) in tool_calls_raw {
        output_items.push(serde_json::json!({
            "id": id,
            "type": "function_call",
            "status": "completed",
            "call_id": id,
            "name": name,
            "arguments": args,
        }));
    }

    let body = serde_json::json!({
        "id": ctx.response_id(),
        "object": "response",
        "created_at": ctx.created_at(),
        "status": "completed",
        "model": prepared.model,
        "previous_response_id": ctx.previous_response_id(),
        "output": output_items,
        "usage": {
            "input_tokens": prompt_tokens,
            "output_tokens": completion_tokens,
            "total_tokens": total,
        }
    });

    (StatusCode::OK, Json(body)).into_response()
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

async fn handle_responses_websearch(
    provider: std::sync::Arc<KiroProvider>,
    payload: crate::anthropic::types::MessagesRequest,
    previous_response_id: Option<String>,
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
    let prev_id_value = previous_response_id
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
        let body = serde_json::json!({
            "id": response_id,
            "object": "response",
            "created_at": created,
            "status": "completed",
            "model": model,
            "previous_response_id": prev_id_value,
            "output": [websearch_item, message_item],
            "usage": usage,
        });
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

