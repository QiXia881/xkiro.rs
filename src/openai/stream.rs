//! KiroEvent → OpenAI 流事件转换
//!
//! 提供两个上下文：
//! - [`OpenAIChatStream`]：转换为 OpenAI Chat Completions chunk 流（`chat.completion.chunk` + `[DONE]`）
//! - [`OpenAIResponsesStream`]：转换为 OpenAI Responses 协议事件流
//!   (`response.created` / `response.output_item.added` / `response.output_text.delta` /
//!    `response.function_call_arguments.delta` / `response.completed`)
//!
//! 上游事件来源是 Kiro 的 EventStream（assistantResponseEvent / toolUseEvent /
//! contextUsageEvent / meteringEvent），与 Anthropic 路径完全一致。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::anthropic::converter::get_context_window_size;
use crate::common::text::normalize_chunk;
use crate::common::thinking_source::ThinkingSourceArbiter;
use crate::kiro::model::events::{Event, MeteringEvent, ToolUseEvent};
use crate::token;

use super::types::{
    ChatChunkChoice, ChatChunkDelta, ChatChunkDeltaFunction, ChatChunkDeltaToolCall,
    ChatCompletionsChunk, ChatUsage,
};

// ============================================================================
// 通用工具
// ============================================================================

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn short_uuid() -> String {
    Uuid::new_v4().simple().to_string()
}

fn sse_data(value: &Value) -> Bytes {
    let body = value.to_string();
    let event_name = value
        .get("type")
        .and_then(Value::as_str)
        .filter(|event| event.starts_with("response."));
    let mut s = String::with_capacity(body.len() + event_name.map_or(8, |event| event.len() + 16));
    if let Some(event) = event_name {
        s.push_str("event: ");
        s.push_str(event);
        s.push('\n');
    }
    s.push_str("data: ");
    s.push_str(&body);
    s.push_str("\n\n");
    Bytes::from(s)
}

fn sse_done() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

fn sse_chunk(chunk: &ChatCompletionsChunk) -> Bytes {
    let body = serde_json::to_string(chunk).unwrap_or_else(|_| "{}".to_string());
    let mut s = String::with_capacity(body.len() + 8);
    s.push_str("data: ");
    s.push_str(&body);
    s.push_str("\n\n");
    Bytes::from(s)
}

/// SSE 输出：接受任意 JSON 值（用于非标准结构如 reasoning_content）
fn sse_chunk_json(chunk: serde_json::Value) -> Bytes {
    let body = serde_json::to_string(&chunk).unwrap_or_else(|_| "{}".to_string());
    let mut s = String::with_capacity(body.len() + 8);
    s.push_str("data: ");
    s.push_str(&body);
    s.push_str("\n\n");
    Bytes::from(s)
}

fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn tag_prefix_suffix_len(s: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|len| s.ends_with(&tag[..*len]))
        .unwrap_or(0)
}

fn estimate_openai_output_tokens<'a>(
    content: &str,
    reasoning: &str,
    tool_calls: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> i32 {
    let mut total = token::count_tokens(content).saturating_add(token::count_tokens(reasoning));
    for (name, arguments) in tool_calls {
        total = total.saturating_add(token::count_tokens(name));
        total = total.saturating_add(token::count_tokens(arguments));
    }
    total.min(i32::MAX as u64) as i32
}

fn responses_message_content(text: &str) -> Vec<Value> {
    if text.is_empty() {
        return vec![json!({
            "type": "output_text",
            "text": "",
        })];
    }

    vec![json!({
        "type": "output_text",
        "text": text,
        "annotations": [],
    })]
}

fn resolve_stream_tool_use_id(
    generated_tool_ids: &mut HashMap<String, String>,
    tool_use: &ToolUseEvent,
) -> String {
    if !tool_use.needs_generated_id() {
        return tool_use.tool_use_id.clone();
    }

    generated_tool_ids
        .entry(tool_use.name.clone())
        .or_insert_with(ToolUseEvent::generate_fallback_id)
        .clone()
}

fn resolve_stateful_tool_use_id(
    generated_tool_ids: &mut HashMap<String, String>,
    preferred_tool_ids: &mut HashMap<String, String>,
    existing_ids: &HashMap<String, impl Sized>,
    tool_use: &ToolUseEvent,
) -> String {
    if !tool_use.needs_generated_id()
        && let Some(generated) = generated_tool_ids.get(&tool_use.name).cloned()
        && existing_ids.contains_key(&generated)
    {
        preferred_tool_ids.insert(generated.clone(), tool_use.tool_use_id.clone());
        return generated;
    }

    resolve_stream_tool_use_id(generated_tool_ids, tool_use)
}

// ============================================================================
// OpenAI Chat Completions 流上下文
// ============================================================================

/// 累积工具调用：tool_use_id → (name, arguments_string, openai_tool_index, started_emitted)
struct ChatToolAccumulator {
    name: String,
    arguments: String,
    index: i32,
    started: bool,
}

#[derive(Clone, Copy)]
enum OpenAIThinkingPhase {
    Start,
    Continue,
    End,
}

pub struct OpenAIChatStream {
    completion_id: String,
    created: i64,
    model: String,
    /// 客户端可能配置的 tool 短名映射（converter 层做了截断），回写时还原
    tool_name_map: HashMap<String, String>,
    /// tool_use_id → 累积器
    tool_acc: HashMap<String, ChatToolAccumulator>,
    generated_tool_ids: HashMap<String, String>,
    preferred_tool_ids: HashMap<String, String>,
    next_tool_index: i32,
    role_emitted: bool,
    saw_tool_calls: bool,
    text_aggregated: String,
    reasoning_aggregated: String,
    /// 估算 input_tokens 的兜底值（在收到 contextUsageEvent 之前用）
    fallback_input_tokens: i32,
    actual_input_tokens: Option<i32>,
    actual_output_tokens: Option<i32>,
    context_input_tokens: Option<i32>,
    metering: Option<MeteringEvent>,
    finished_emitted: bool,
    /// 上一个 assistantResponseEvent 的完整内容（chunk 归一化）
    last_assistant_content: String,
    /// 上一个 reasoningContentEvent 的完整内容（chunk 归一化）
    last_reasoning_content: String,
    /// 是否已发出 reasoning_content 字段（OpenAI 格式）
    reasoning_emitted: bool,
    thinking_enabled: bool,
    thinking_format: String,
    text_buffer: String,
    in_thinking_block: bool,
    drop_tag_thinking: bool,
    thinking_source: ThinkingSourceArbiter,
    reasoning_open: bool,
}

impl OpenAIChatStream {
    pub fn new(
        model: impl Into<String>,
        fallback_input_tokens: i32,
        tool_name_map: HashMap<String, String>,
        thinking_format: impl Into<String>,
    ) -> Self {
        Self::new_with_thinking(
            model,
            fallback_input_tokens,
            tool_name_map,
            true,
            thinking_format,
        )
    }

    pub fn new_with_thinking(
        model: impl Into<String>,
        fallback_input_tokens: i32,
        tool_name_map: HashMap<String, String>,
        thinking_enabled: bool,
        thinking_format: impl Into<String>,
    ) -> Self {
        Self {
            completion_id: format!("chatcmpl-{}", short_uuid()),
            created: now_unix(),
            model: model.into(),
            tool_name_map,
            tool_acc: HashMap::new(),
            generated_tool_ids: HashMap::new(),
            preferred_tool_ids: HashMap::new(),
            next_tool_index: 0,
            role_emitted: false,
            saw_tool_calls: false,
            text_aggregated: String::new(),
            reasoning_aggregated: String::new(),
            fallback_input_tokens,
            actual_input_tokens: None,
            actual_output_tokens: None,
            context_input_tokens: None,
            metering: None,
            finished_emitted: false,
            last_assistant_content: String::new(),
            last_reasoning_content: String::new(),
            reasoning_emitted: false,
            thinking_enabled,
            thinking_format: thinking_format.into(),
            text_buffer: String::new(),
            in_thinking_block: false,
            drop_tag_thinking: false,
            thinking_source: ThinkingSourceArbiter::Unknown,
            reasoning_open: false,
        }
    }

    pub fn completion_id(&self) -> &str {
        &self.completion_id
    }

    pub fn metering(&self) -> Option<&MeteringEvent> {
        self.metering.as_ref()
    }

    pub fn set_actual_input_tokens(&mut self, input_tokens: i32) {
        self.actual_input_tokens = Some(input_tokens);
    }

    pub fn set_actual_output_tokens(&mut self, output_tokens: i32) {
        self.actual_output_tokens = Some(output_tokens);
    }

    pub fn current_usage_input_tokens(&self) -> Option<i32> {
        self.context_input_tokens.or(self.actual_input_tokens)
    }

    pub fn current_usage_output_tokens(&self) -> Option<i32> {
        self.actual_output_tokens
    }

    /// 初始 chunk：发出 `delta.role = "assistant"` 与空 content。
    pub fn initial_chunk(&mut self) -> Bytes {
        self.role_emitted = true;
        let chunk = ChatCompletionsChunk {
            id: self.completion_id.clone(),
            object: "chat.completion.chunk",
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatChunkDelta {
                    role: Some("assistant"),
                    content: Some(String::new()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        sse_chunk(&chunk)
    }

    fn content_chunk(&mut self, content: &str) -> Option<Bytes> {
        if content.is_empty() {
            return None;
        }
        self.text_aggregated.push_str(content);
        let chunk = ChatCompletionsChunk {
            id: self.completion_id.clone(),
            object: "chat.completion.chunk",
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatChunkDelta {
                    role: None,
                    content: Some(content.to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        Some(sse_chunk(&chunk))
    }

    fn reasoning_chunk(&mut self, content: &str, phase: OpenAIThinkingPhase) -> Option<Bytes> {
        if !self.thinking_enabled {
            return None;
        }
        if content.is_empty() && matches!(phase, OpenAIThinkingPhase::Continue) {
            return None;
        }

        match self.thinking_format.as_str() {
            "thinking" | "think" => {
                let (open_tag, close_tag) = if self.thinking_format == "think" {
                    ("<think>", "</think>")
                } else {
                    ("<thinking>", "</thinking>")
                };
                let text = match phase {
                    OpenAIThinkingPhase::Start => format!("{open_tag}{content}"),
                    OpenAIThinkingPhase::Continue => content.to_string(),
                    OpenAIThinkingPhase::End => format!("{content}{close_tag}"),
                };
                if text.is_empty() {
                    return None;
                }
                self.text_aggregated.push_str(&text);
                let chunk = serde_json::json!({
                    "id": self.completion_id,
                    "object": "chat.completion.chunk",
                    "created": self.created,
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "content": text },
                        "finish_reason": null
                    }]
                });
                Some(sse_chunk_json(chunk))
            }
            _ => {
                if content.is_empty() {
                    return None;
                }
                self.reasoning_emitted = true;
                self.reasoning_aggregated.push_str(content);
                let chunk = serde_json::json!({
                    "id": self.completion_id,
                    "object": "chat.completion.chunk",
                    "created": self.created,
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "reasoning_content": content },
                        "finish_reason": null
                    }]
                });
                Some(sse_chunk_json(chunk))
            }
        }
    }

    fn reasoning_delta(&mut self, content: &str) -> Vec<Bytes> {
        if !self.thinking_enabled {
            return Vec::new();
        }
        let phase = if self.reasoning_open {
            OpenAIThinkingPhase::Continue
        } else {
            self.reasoning_open = true;
            OpenAIThinkingPhase::Start
        };
        self.reasoning_chunk(content, phase).into_iter().collect()
    }

    fn close_reasoning(&mut self) -> Vec<Bytes> {
        if !self.thinking_enabled {
            self.reasoning_open = false;
            return Vec::new();
        }
        if !self.reasoning_open {
            return Vec::new();
        }
        self.reasoning_open = false;
        self.reasoning_chunk("", OpenAIThinkingPhase::End)
            .into_iter()
            .collect()
    }

    fn close_reasoning_with_content(&mut self, content: &str) -> Vec<Bytes> {
        if !self.thinking_enabled {
            self.reasoning_open = false;
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.reasoning_open {
            if let Some(chunk) = self.reasoning_chunk(content, OpenAIThinkingPhase::End) {
                out.push(chunk);
            }
        } else {
            if let Some(chunk) = self.reasoning_chunk(content, OpenAIThinkingPhase::Start) {
                out.push(chunk);
            }
            if let Some(chunk) = self.reasoning_chunk("", OpenAIThinkingPhase::End) {
                out.push(chunk);
            }
        }
        self.reasoning_open = false;
        out
    }

    fn process_text_delta(&mut self, text: &str, force_flush: bool) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !text.is_empty() {
            self.text_buffer.push_str(text);
        }

        loop {
            if !self.in_thinking_block {
                if let Some(start) = self.text_buffer.find("<thinking>") {
                    if start > 0 {
                        let before = self.text_buffer[..start].to_string();
                        out.extend(self.close_reasoning());
                        if let Some(chunk) = self.content_chunk(&before) {
                            out.push(chunk);
                        }
                    }
                    self.text_buffer = self.text_buffer[start + "<thinking>".len()..].to_string();
                    self.in_thinking_block = true;
                    self.drop_tag_thinking = !self.thinking_source.allow_tag();
                } else if force_flush || self.text_buffer.chars().count() > 50 {
                    let safe_len = if force_flush {
                        self.text_buffer.len()
                    } else {
                        let target = self.text_buffer.len().saturating_sub(15);
                        find_char_boundary(&self.text_buffer, target)
                    };
                    if safe_len > 0 {
                        let content = self.text_buffer[..safe_len].to_string();
                        self.text_buffer = self.text_buffer[safe_len..].to_string();
                        out.extend(self.close_reasoning());
                        if let Some(chunk) = self.content_chunk(&content) {
                            out.push(chunk);
                        }
                    }
                    break;
                } else {
                    break;
                }
            } else if let Some(end) = self.text_buffer.find("</thinking>") {
                let content = self.text_buffer[..end].to_string();
                if !self.drop_tag_thinking {
                    out.extend(self.close_reasoning_with_content(&content));
                }
                self.text_buffer = self.text_buffer[end + "</thinking>".len()..].to_string();
                self.in_thinking_block = false;
                self.drop_tag_thinking = false;
            } else if force_flush {
                if !self.text_buffer.is_empty() && !self.drop_tag_thinking {
                    let content = std::mem::take(&mut self.text_buffer);
                    out.extend(self.close_reasoning_with_content(&content));
                } else {
                    self.text_buffer.clear();
                    out.extend(self.close_reasoning());
                }
                self.in_thinking_block = false;
                self.drop_tag_thinking = false;
                break;
            } else {
                let safe_len = if self.text_buffer.chars().count() > 20 {
                    let target = self.text_buffer.len().saturating_sub(15);
                    find_char_boundary(&self.text_buffer, target)
                } else {
                    0
                };
                if safe_len > 0 {
                    let content = self.text_buffer[..safe_len].to_string();
                    self.text_buffer = self.text_buffer[safe_len..].to_string();
                    if !self.drop_tag_thinking {
                        out.extend(self.reasoning_delta(&content));
                    }
                }
                break;
            }
        }

        out
    }

    pub fn flush_pending(&mut self) -> Vec<Bytes> {
        let mut out = self.process_text_delta("", true);
        out.extend(self.close_reasoning());
        out
    }

    /// 处理一个 Kiro 事件，返回若干 SSE chunk。
    pub fn process_event(&mut self, event: &Event) -> Vec<Bytes> {
        match event {
            Event::AssistantResponse(resp) => {
                // Kiro 上游发送累积文本，需要归一化为增量
                let delta = normalize_chunk(&resp.content, &mut self.last_assistant_content);
                if delta.is_empty() {
                    return Vec::new();
                }
                self.process_text_delta(&delta, false)
            }
            Event::ToolUse(tool_use) => {
                let mut out = self.flush_pending();
                let id = resolve_stateful_tool_use_id(
                    &mut self.generated_tool_ids,
                    &mut self.preferred_tool_ids,
                    &self.tool_acc,
                    tool_use,
                );
                let resolved_name = self
                    .tool_name_map
                    .get(&tool_use.name)
                    .cloned()
                    .unwrap_or_else(|| tool_use.name.clone());

                let next_index = self.next_tool_index;
                let acc = self.tool_acc.entry(id.clone()).or_insert_with(|| {
                    let idx = next_index;
                    ChatToolAccumulator {
                        name: resolved_name.clone(),
                        arguments: String::new(),
                        index: idx,
                        started: false,
                    }
                });
                if !acc.started {
                    acc.started = true;
                    self.next_tool_index += 1;
                    self.saw_tool_calls = true;

                    // 起始 chunk：包含 id / type / function.name，arguments 为空字符串
                    let start_chunk = ChatCompletionsChunk {
                        id: self.completion_id.clone(),
                        object: "chat.completion.chunk",
                        created: self.created,
                        model: self.model.clone(),
                        choices: vec![ChatChunkChoice {
                            index: 0,
                            delta: ChatChunkDelta {
                                role: None,
                                content: None,
                                tool_calls: Some(vec![ChatChunkDeltaToolCall {
                                    index: acc.index,
                                    id: Some(id.clone()),
                                    call_type: Some("function"),
                                    function: ChatChunkDeltaFunction {
                                        name: Some(resolved_name.clone()),
                                        arguments: Some(String::new()),
                                    },
                                }]),
                            },
                            finish_reason: None,
                        }],
                        usage: None,
                    };
                    out.push(sse_chunk(&start_chunk));
                }

                if !tool_use.input.is_empty() {
                    tool_use.apply_input_to_buffer(&mut acc.arguments);
                    let delta_chunk = ChatCompletionsChunk {
                        id: self.completion_id.clone(),
                        object: "chat.completion.chunk",
                        created: self.created,
                        model: self.model.clone(),
                        choices: vec![ChatChunkChoice {
                            index: 0,
                            delta: ChatChunkDelta {
                                role: None,
                                content: None,
                                tool_calls: Some(vec![ChatChunkDeltaToolCall {
                                    index: acc.index,
                                    id: None,
                                    call_type: None,
                                    function: ChatChunkDeltaFunction {
                                        name: None,
                                        arguments: Some(tool_use.input.clone()),
                                    },
                                }]),
                            },
                            finish_reason: None,
                        }],
                        usage: None,
                    };
                    out.push(sse_chunk(&delta_chunk));
                }

                out
            }
            Event::ContextUsage(usage) => {
                let window = get_context_window_size(&self.model);
                let actual = (usage.context_usage_percentage * (window as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual);
                Vec::new()
            }
            Event::Metering(metering) => {
                self.metering = Some(metering.clone());
                Vec::new()
            }
            Event::ReasoningContent(resp) => {
                if !self.thinking_enabled {
                    return Vec::new();
                }
                // reasoningContentEvent: 推理内容作为 reasoning_content 字段
                let delta = normalize_chunk(&resp.text, &mut self.last_reasoning_content);
                if delta.is_empty() {
                    return Vec::new();
                }
                if !self.thinking_source.allow_reasoning() {
                    return Vec::new();
                }
                self.reasoning_delta(&delta)
            }
            Event::Exception { exception_type, .. } => {
                if exception_type == "ContentLengthExceededException" {
                    self.saw_tool_calls = false;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// 终结事件：finish_reason chunk + usage + [DONE]。
    pub fn finish_events(&mut self) -> Vec<Bytes> {
        if self.finished_emitted {
            return Vec::new();
        }
        self.finished_emitted = true;

        let mut out = self.flush_pending();

        let finish_reason = if self.saw_tool_calls {
            Some("tool_calls".to_string())
        } else {
            Some("stop".to_string())
        };

        let prompt_tokens = self
            .context_input_tokens
            .or(self.actual_input_tokens)
            .unwrap_or(self.fallback_input_tokens);
        let completion_tokens = self.final_output_tokens();
        let total = prompt_tokens.saturating_add(completion_tokens);

        let final_chunk = ChatCompletionsChunk {
            id: self.completion_id.clone(),
            object: "chat.completion.chunk",
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: None,
                },
                finish_reason,
            }],
            usage: Some(ChatUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: total,
            }),
        };

        out.push(sse_chunk(&final_chunk));
        out.push(sse_done());
        out
    }

    pub fn abort_events(&mut self) -> Vec<Bytes> {
        if self.finished_emitted {
            return Vec::new();
        }
        self.finished_emitted = true;
        Vec::new()
    }

    pub fn aggregated_text(&self) -> &str {
        &self.text_aggregated
    }

    pub fn aggregated_reasoning(&self) -> &str {
        &self.reasoning_aggregated
    }

    pub fn aggregated_tool_calls(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<_> = self
            .tool_acc
            .iter()
            .map(|(id, acc)| {
                (
                    acc.index,
                    self.preferred_tool_ids
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| id.clone()),
                    acc.name.clone(),
                    acc.arguments.clone(),
                )
            })
            .collect();
        // 按起始顺序排序，保证响应输出稳定
        v.sort_by_key(|(idx, _, _, _)| *idx);
        v.into_iter()
            .map(|(_, id, name, args)| (id, name, args))
            .collect()
    }

    pub fn final_input_tokens(&self) -> i32 {
        self.context_input_tokens
            .or(self.actual_input_tokens)
            .unwrap_or(self.fallback_input_tokens)
    }

    pub fn final_output_tokens(&self) -> i32 {
        if let Some(output_tokens) = self.actual_output_tokens {
            return output_tokens;
        }

        estimate_openai_output_tokens(
            &self.text_aggregated,
            &self.reasoning_aggregated,
            self.tool_acc
                .values()
                .map(|acc| (acc.name.as_str(), acc.arguments.as_str())),
        )
    }
}

// ============================================================================
// OpenAI Responses 流上下文
// ============================================================================

struct ResponsesToolAccumulator {
    item_id: String,
    name: String,
    arguments: String,
    output_index: usize,
    started: bool,
    done: bool,
}

pub struct OpenAIResponsesStream {
    response_id: String,
    message_id: String,
    created_at: i64,
    model: String,
    previous_response_id: Option<String>,
    metadata: Option<Value>,
    instructions: Option<String>,
    tool_name_map: HashMap<String, String>,
    next_output_index: usize,
    tool_acc: HashMap<String, ResponsesToolAccumulator>,
    generated_tool_ids: HashMap<String, String>,
    preferred_tool_ids: HashMap<String, String>,
    text_aggregated: String,
    reasoning_aggregated: String,
    fallback_input_tokens: i32,
    actual_input_tokens: Option<i32>,
    actual_output_tokens: Option<i32>,
    context_input_tokens: Option<i32>,
    metering: Option<MeteringEvent>,
    /// `response.output_item.added` 是否已为 message 输出（output_index=0）发出
    message_item_started: bool,
    message_item_done: bool,
    finished_emitted: bool,
    /// 上一个 assistantResponseEvent 的完整内容（chunk 归一化）
    last_assistant_content: String,
    /// 上一个 reasoningContentEvent 的完整内容（chunk 归一化）
    last_reasoning_content: String,
    text_buffer: String,
    in_thinking_block: bool,
    thinking_enabled: bool,
    thinking_format: String,
}

impl OpenAIResponsesStream {
    pub fn new(
        model: impl Into<String>,
        fallback_input_tokens: i32,
        tool_name_map: HashMap<String, String>,
        previous_response_id: Option<String>,
        metadata: Option<Value>,
        thinking_format: impl Into<String>,
    ) -> Self {
        Self::new_with_thinking(
            model,
            fallback_input_tokens,
            tool_name_map,
            previous_response_id,
            metadata,
            true,
            thinking_format,
        )
    }

    pub fn new_with_thinking(
        model: impl Into<String>,
        fallback_input_tokens: i32,
        tool_name_map: HashMap<String, String>,
        previous_response_id: Option<String>,
        metadata: Option<Value>,
        thinking_enabled: bool,
        thinking_format: impl Into<String>,
    ) -> Self {
        Self {
            response_id: format!("resp_{}", short_uuid()),
            message_id: format!("msg_{}", short_uuid()),
            created_at: now_unix(),
            model: model.into(),
            previous_response_id,
            metadata,
            instructions: None,
            tool_name_map,
            next_output_index: 0,
            tool_acc: HashMap::new(),
            generated_tool_ids: HashMap::new(),
            preferred_tool_ids: HashMap::new(),
            text_aggregated: String::new(),
            reasoning_aggregated: String::new(),
            fallback_input_tokens,
            actual_input_tokens: None,
            actual_output_tokens: None,
            context_input_tokens: None,
            metering: None,
            message_item_started: false,
            message_item_done: false,
            finished_emitted: false,
            last_assistant_content: String::new(),
            last_reasoning_content: String::new(),
            text_buffer: String::new(),
            in_thinking_block: false,
            thinking_enabled,
            thinking_format: thinking_format.into(),
        }
    }

    pub fn metering(&self) -> Option<&MeteringEvent> {
        self.metering.as_ref()
    }

    pub fn set_actual_input_tokens(&mut self, input_tokens: i32) {
        self.actual_input_tokens = Some(input_tokens);
    }

    pub fn set_actual_output_tokens(&mut self, output_tokens: i32) {
        self.actual_output_tokens = Some(output_tokens);
    }

    pub fn current_usage_input_tokens(&self) -> Option<i32> {
        self.context_input_tokens.or(self.actual_input_tokens)
    }

    pub fn current_usage_output_tokens(&self) -> Option<i32> {
        self.actual_output_tokens
    }

    pub fn set_instructions(&mut self, instructions: Option<String>) {
        self.instructions = instructions.filter(|s| !s.trim().is_empty());
    }

    pub fn response_id(&self) -> &str {
        &self.response_id
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn completed_usage(&self) -> Value {
        let prompt_tokens = self
            .context_input_tokens
            .or(self.actual_input_tokens)
            .unwrap_or(self.fallback_input_tokens);
        let completion_tokens = self.final_output_tokens();
        let total = prompt_tokens.saturating_add(completion_tokens);
        json!({
            "input_tokens": prompt_tokens,
            "output_tokens": completion_tokens,
            "total_tokens": total,
        })
    }

    pub fn completed_output_items(&self) -> Vec<Value> {
        let mut output_items: Vec<Value> = Vec::new();

        if !self.text_aggregated.is_empty() || self.tool_acc.is_empty() {
            output_items.push(json!({
                "id": self.message_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": responses_message_content(&self.text_aggregated),
            }));
        }

        let mut tool_items: Vec<(usize, Value)> = self
            .tool_acc
            .iter()
            .map(|(id, acc)| {
                let output_id = self
                    .preferred_tool_ids
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.clone());
                (
                    acc.output_index,
                    json!({
                        "id": acc.item_id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": output_id,
                        "name": acc.name,
                        "arguments": acc.arguments,
                    }),
                )
            })
            .collect();
        tool_items.sort_by_key(|(idx, _)| *idx);
        output_items.extend(tool_items.into_iter().map(|(_, item)| item));
        output_items
    }

    pub fn initial_events(&mut self) -> Vec<Bytes> {
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": "in_progress",
            "model": self.model,
            "previous_response_id": self.previous_response_id,
            "output": []
        });
        if let Some(metadata) = &self.metadata {
            response["metadata"] = metadata.clone();
        }
        let created = json!({
            "type": "response.created",
            "response": response.clone(),
        });
        let in_progress = json!({
            "type": "response.in_progress",
            "response": response,
        });

        vec![sse_data(&created), sse_data(&in_progress)]
    }

    fn ensure_message_started(&mut self) -> Vec<Bytes> {
        if self.message_item_started {
            return Vec::new();
        }
        self.message_item_started = true;
        if self.next_output_index == 0 {
            self.next_output_index = 1;
        }

        let item_added = json!({
            "type": "response.output_item.added",
            "response_id": self.response_id,
            "output_index": 0,
            "item": {
                "id": self.message_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        });
        let content_part_added = json!({
            "type": "response.content_part.added",
            "response_id": self.response_id,
            "item_id": self.message_id,
            "output_index": 0,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": "",
            },
        });

        vec![sse_data(&item_added), sse_data(&content_part_added)]
    }

    fn finish_message_item(&mut self) -> Vec<Bytes> {
        if !self.message_item_started || self.message_item_done {
            return Vec::new();
        }
        self.message_item_done = true;

        let content = responses_message_content(&self.text_aggregated);
        let content_done = json!({
            "type": "response.content_part.done",
            "response_id": self.response_id,
            "item_id": self.message_id,
            "output_index": 0,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": self.text_aggregated,
            },
        });
        let item_done = json!({
            "type": "response.output_item.done",
            "response_id": self.response_id,
            "output_index": 0,
            "item": {
                "id": self.message_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": content,
            }
        });

        vec![sse_data(&content_done), sse_data(&item_done)]
    }

    fn emit_output_text_delta(&mut self, delta_text: String) -> Vec<Bytes> {
        if delta_text.is_empty() {
            return Vec::new();
        }
        self.text_aggregated.push_str(&delta_text);
        let mut out = self.ensure_message_started();
        let delta = json!({
            "type": "response.output_text.delta",
            "response_id": self.response_id,
            "item_id": self.message_id,
            "output_index": 0,
            "content_index": 0,
            "delta": delta_text,
        });
        out.push(sse_data(&delta));
        out
    }

    fn process_text_delta(&mut self, text: &str, force_flush: bool) -> Vec<Bytes> {
        const THINKING_OPEN: &str = "<thinking>";
        const THINKING_CLOSE: &str = "</thinking>";

        let mut out = Vec::new();
        if !text.is_empty() {
            self.text_buffer.push_str(text);
        }

        loop {
            if !self.in_thinking_block {
                if let Some(start) = self.text_buffer.find(THINKING_OPEN) {
                    if start > 0 {
                        let visible = self.text_buffer[..start].to_string();
                        out.extend(self.emit_output_text_delta(visible));
                    }
                    self.text_buffer = self.text_buffer[start + THINKING_OPEN.len()..].to_string();
                    self.in_thinking_block = true;
                    continue;
                }

                let keep = if force_flush {
                    0
                } else {
                    tag_prefix_suffix_len(&self.text_buffer, THINKING_OPEN)
                };
                let emit_len = self.text_buffer.len().saturating_sub(keep);
                if emit_len > 0 {
                    let visible = self.text_buffer[..emit_len].to_string();
                    self.text_buffer = self.text_buffer[emit_len..].to_string();
                    out.extend(self.emit_output_text_delta(visible));
                }
                break;
            }

            if let Some(end) = self.text_buffer.find(THINKING_CLOSE) {
                self.text_buffer = self.text_buffer[end + THINKING_CLOSE.len()..].to_string();
                self.in_thinking_block = false;
                continue;
            }

            if force_flush {
                self.text_buffer.clear();
                self.in_thinking_block = false;
                break;
            }

            let keep = tag_prefix_suffix_len(&self.text_buffer, THINKING_CLOSE);
            if self.text_buffer.len() > keep {
                self.text_buffer = self.text_buffer[self.text_buffer.len() - keep..].to_string();
            }
            break;
        }

        out
    }

    pub fn flush_pending(&mut self) -> Vec<Bytes> {
        self.process_text_delta("", true)
    }

    pub fn process_event(&mut self, event: &Event) -> Vec<Bytes> {
        match event {
            Event::AssistantResponse(resp) => {
                // Kiro 上游发送累积文本，需要归一化为增量
                let delta_text = normalize_chunk(&resp.content, &mut self.last_assistant_content);
                if delta_text.is_empty() {
                    return Vec::new();
                }
                self.process_text_delta(&delta_text, false)
            }
            Event::ReasoningContent(resp) => {
                if !self.thinking_enabled {
                    return Vec::new();
                }
                let delta_text = normalize_chunk(&resp.text, &mut self.last_reasoning_content);
                if delta_text.is_empty() {
                    return Vec::new();
                }
                self.reasoning_aggregated.push_str(&delta_text);
                Vec::new()
            }
            Event::ToolUse(tool_use) => {
                let id = resolve_stateful_tool_use_id(
                    &mut self.generated_tool_ids,
                    &mut self.preferred_tool_ids,
                    &self.tool_acc,
                    tool_use,
                );
                let resolved_name = self
                    .tool_name_map
                    .get(&tool_use.name)
                    .cloned()
                    .unwrap_or_else(|| tool_use.name.clone());

                let mut out = self.flush_pending();
                out.extend(self.finish_message_item());
                let next_idx = self.next_output_index;
                let acc =
                    self.tool_acc
                        .entry(id.clone())
                        .or_insert_with(|| ResponsesToolAccumulator {
                            item_id: format!("fc_{}", short_uuid()),
                            name: resolved_name.clone(),
                            arguments: String::new(),
                            output_index: next_idx,
                            started: false,
                            done: false,
                        });
                if !acc.started {
                    acc.started = true;
                    self.next_output_index += 1;
                    let added = json!({
                        "type": "response.output_item.added",
                        "response_id": self.response_id,
                        "output_index": acc.output_index,
                        "item": {
                            "id": acc.item_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": id,
                            "name": resolved_name,
                            "arguments": ""
                        }
                    });
                    out.push(sse_data(&added));
                }

                if !tool_use.input.is_empty() {
                    tool_use.apply_input_to_buffer(&mut acc.arguments);
                    let delta = json!({
                        "type": "response.function_call_arguments.delta",
                        "response_id": self.response_id,
                        "item_id": acc.item_id,
                        "output_index": acc.output_index,
                        "delta": tool_use.input,
                    });
                    out.push(sse_data(&delta));
                }

                if tool_use.stop {
                    let arguments_final = acc.arguments.clone();
                    let item_id = acc.item_id.clone();
                    let output_index = acc.output_index;
                    let name = acc.name.clone();
                    let call_id = self
                        .preferred_tool_ids
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| id.clone());
                    acc.done = true;
                    let item_done = json!({
                        "type": "response.output_item.done",
                        "response_id": self.response_id,
                        "output_index": output_index,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "completed",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments_final,
                        }
                    });
                    out.push(sse_data(&item_done));
                }

                out
            }
            Event::ContextUsage(usage) => {
                let window = get_context_window_size(&self.model);
                let actual = (usage.context_usage_percentage * (window as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual);
                Vec::new()
            }
            Event::Metering(metering) => {
                self.metering = Some(metering.clone());
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub fn finish_events(&mut self) -> Vec<Bytes> {
        if self.finished_emitted {
            return Vec::new();
        }
        self.finished_emitted = true;

        let mut out = self.flush_pending();

        // 1) text 段终结：content_part.done + output_item.done(message)
        if self.message_item_started && !self.message_item_done {
            let content = responses_message_content(&self.text_aggregated);
            let content_done = json!({
                "type": "response.content_part.done",
                "response_id": self.response_id,
                "item_id": self.message_id,
                "output_index": 0,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": self.text_aggregated,
                },
            });
            out.push(sse_data(&content_done));
            let item_done = json!({
                "type": "response.output_item.done",
                "response_id": self.response_id,
                "output_index": 0,
                "item": {
                    "id": self.message_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": content,
                }
            });
            out.push(sse_data(&item_done));
            self.message_item_done = true;
        }

        let mut unfinished_tools: Vec<(usize, Value)> = self
            .tool_acc
            .iter_mut()
            .filter_map(|(call_id, acc)| {
                if !acc.started || acc.done {
                    return None;
                }
                acc.done = true;
                Some((
                    acc.output_index,
                    json!({
                        "type": "response.output_item.done",
                        "response_id": self.response_id,
                        "output_index": acc.output_index,
                        "item": {
                            "id": acc.item_id,
                            "type": "function_call",
                            "status": "completed",
                            "call_id": self.preferred_tool_ids
                                .get(call_id)
                                .cloned()
                                .unwrap_or_else(|| call_id.clone()),
                            "name": acc.name,
                            "arguments": acc.arguments,
                        }
                    }),
                ))
            })
            .collect();
        unfinished_tools.sort_by_key(|(idx, _)| *idx);
        out.extend(
            unfinished_tools
                .into_iter()
                .map(|(_, item)| sse_data(&item)),
        );

        // 2) response.completed
        let output_items = self.completed_output_items();

        let mut completed = json!({
            "type": "response.completed",
            "response": {
                "id": self.response_id,
                "object": "response",
                "created_at": self.created_at,
                "status": "completed",
                "model": self.model,
                "previous_response_id": self.previous_response_id,
                "output": output_items,
                "usage": self.completed_usage(),
            }
        });
        if let Some(metadata) = &self.metadata {
            completed["response"]["metadata"] = metadata.clone();
        }
        if let Some(instructions) = &self.instructions {
            completed["response"]["instructions"] = json!(instructions);
        }
        out.push(sse_data(&completed));
        out.push(sse_done());

        out
    }

    pub fn failed_events(&mut self, message: impl AsRef<str>) -> Vec<Bytes> {
        if self.finished_emitted {
            return Vec::new();
        }
        self.finished_emitted = true;

        let failed = json!({
            "type": "response.failed",
            "response": {
                "id": self.response_id,
                "status": "failed",
                "error": {
                    "type": "server_error",
                    "message": message.as_ref(),
                },
            }
        });
        vec![sse_data(&failed)]
    }

    pub fn aggregated_text(&self) -> &str {
        &self.text_aggregated
    }

    pub fn aggregated_tool_calls(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<(usize, String, String, String)> = self
            .tool_acc
            .iter()
            .map(|(id, acc)| {
                (
                    acc.output_index,
                    self.preferred_tool_ids
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| id.clone()),
                    acc.name.clone(),
                    acc.arguments.clone(),
                )
            })
            .collect();
        v.sort_by_key(|(idx, _, _, _)| *idx);
        v.into_iter()
            .map(|(_, id, name, args)| (id, name, args))
            .collect()
    }

    pub fn final_input_tokens(&self) -> i32 {
        self.context_input_tokens
            .or(self.actual_input_tokens)
            .unwrap_or(self.fallback_input_tokens)
    }

    pub fn final_output_tokens(&self) -> i32 {
        if let Some(output_tokens) = self.actual_output_tokens {
            return output_tokens;
        }

        estimate_openai_output_tokens(
            &self.text_aggregated,
            &self.reasoning_aggregated,
            self.tool_acc
                .values()
                .map(|acc| (acc.name.as_str(), acc.arguments.as_str())),
        )
    }

    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    pub fn previous_response_id(&self) -> Option<&str> {
        self.previous_response_id.as_deref()
    }

    pub fn created_at(&self) -> i64 {
        self.created_at
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::kiro::model::events::{
        AssistantResponseEvent, ContextUsageEvent, Event, ReasoningContentEvent, ToolUseEvent,
    };

    use super::{OpenAIChatStream, OpenAIResponsesStream, normalize_chunk};

    fn sse_text(bytes: &[bytes::Bytes]) -> String {
        bytes
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn openai_streams_use_actual_usage_with_context_usage_input_precedence() {
        let mut chat =
            OpenAIChatStream::new("claude-sonnet-4.5", 12, HashMap::new(), "reasoning_content");
        chat.set_actual_input_tokens(31);
        chat.set_actual_output_tokens(7);
        assert_eq!(chat.final_input_tokens(), 31);
        assert_eq!(chat.final_output_tokens(), 7);
        chat.process_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 50.0,
        }));
        assert_eq!(chat.final_input_tokens(), 100_000);

        let mut responses = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "reasoning_content",
        );
        responses.set_actual_input_tokens(31);
        responses.set_actual_output_tokens(7);
        assert_eq!(responses.completed_usage()["input_tokens"], 31);
        assert_eq!(responses.completed_usage()["output_tokens"], 7);
        responses.process_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 50.0,
        }));
        assert_eq!(responses.completed_usage()["input_tokens"], 100_000);
    }

    #[test]
    fn responses_completed_snapshot_contains_text_and_tool_calls() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            Some("resp_prev".to_string()),
            None,
            "reasoning_content",
        );

        let assistant_event: AssistantResponseEvent =
            serde_json::from_value(serde_json::json!({ "content": "hello" })).unwrap();
        stream.process_event(&Event::AssistantResponse(assistant_event));
        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_1".to_string(),
            input: "{\"cmd\":\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        }));

        let output = stream.completed_output_items();

        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "hello");
        assert_eq!(output[1]["type"], "function_call");
        assert!(output[1]["id"].as_str().unwrap().starts_with("fc_"));
        assert_ne!(output[1]["id"], output[1]["call_id"]);
        assert_eq!(output[1]["call_id"], "call_1");
        assert_eq!(output[1]["name"], "exec_command");
        assert_eq!(output[1]["arguments"], "{\"cmd\":\"pwd\"}");
        assert_eq!(stream.completed_usage()["input_tokens"], 12);
    }

    #[test]
    fn responses_stream_closes_message_before_tool_call() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "reasoning_content",
        );

        let assistant_event: AssistantResponseEvent =
            serde_json::from_value(serde_json::json!({ "content": "hello" })).unwrap();
        stream.process_event(&Event::AssistantResponse(assistant_event));
        let tool_events = sse_text(&stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_1".to_string(),
            input: "{\"cmd\":\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        })));
        let finish = sse_text(&stream.finish_events());

        let message_done_pos = tool_events
            .find("\"type\":\"response.output_item.done\"")
            .expect("message should be closed before tool");
        let tool_added_pos = tool_events
            .find("\"type\":\"response.output_item.added\"")
            .expect("tool should be added");
        assert!(message_done_pos < tool_added_pos);
        assert!(tool_events.contains("\"type\":\"response.function_call_arguments.delta\""));
        assert!(tool_events.contains("\"item_id\":\"fc_"));
        assert!(tool_events.contains("\"call_id\":\"call_1\""));
        assert!(tool_events.contains("\"output_index\":1"));
        assert!(!tool_events.contains("response.function_call_arguments.done"));
        assert_eq!(
            finish
                .matches("\"type\":\"response.output_item.done\"")
                .count(),
            0
        );
    }

    #[test]
    fn normalize_chunk_handles_multibyte_overlap_without_panic() {
        let mut previous = "你好世界".to_string();

        let delta = normalize_chunk("世界🙂继续", &mut previous);

        assert_eq!(delta, "🙂继续");
        assert_eq!(previous, "世界🙂继续");
    }

    #[test]
    fn responses_initial_event_does_not_precreate_empty_message() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        let initial = sse_text(&stream.initial_events());

        assert!(initial.contains("\"type\":\"response.created\""));
        assert!(initial.contains("\"type\":\"response.in_progress\""));
        assert!(!initial.contains("response.output_item.added"));
        assert!(stream.completed_output_items()[0]["type"] == "message");
    }

    #[test]
    fn responses_sse_frames_include_named_events() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        let initial = sse_text(&stream.initial_events());
        assert!(initial.contains("event: response.created\ndata: "));
        assert!(initial.contains("event: response.in_progress\ndata: "));

        let assistant_event: AssistantResponseEvent =
            serde_json::from_value(serde_json::json!({ "content": "hello" })).unwrap();
        let delta = sse_text(&stream.process_event(&Event::AssistantResponse(assistant_event)));
        assert!(delta.contains("event: response.output_item.added\ndata: "));
        assert!(delta.contains("event: response.content_part.added\ndata: "));
        assert!(delta.contains("event: response.output_text.delta\ndata: "));

        let completed = sse_text(&stream.finish_events());
        assert!(completed.contains("event: response.completed\ndata: "));
        assert!(completed.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn responses_failed_events_use_response_failed_not_completed() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        let failed = sse_text(&stream.failed_events("upstream closed"));

        assert!(failed.contains("event: response.failed\ndata: "));
        assert!(failed.contains("\"type\":\"response.failed\""));
        assert!(failed.contains("\"status\":\"failed\""));
        assert!(failed.contains("upstream closed"));
        assert!(!failed.contains("response.completed"));
        assert!(!failed.contains("[DONE]"));
        assert!(stream.finish_events().is_empty());
    }

    #[test]
    fn responses_empty_output_contains_empty_output_text() {
        let stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        let output = stream.completed_output_items();

        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["type"], "output_text");
        assert_eq!(output[0]["content"][0]["text"], "");
    }

    #[test]
    fn responses_text_lazily_starts_message_and_finishes_content_part() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        let assistant_event: AssistantResponseEvent =
            serde_json::from_value(serde_json::json!({ "content": "hello" })).unwrap();
        let delta = sse_text(&stream.process_event(&Event::AssistantResponse(assistant_event)));
        let finish = sse_text(&stream.finish_events());

        assert!(delta.contains("\"type\":\"response.output_item.added\""));
        assert!(delta.contains("\"type\":\"response.content_part.added\""));
        assert!(delta.contains("\"type\":\"response.output_text.delta\""));
        assert!(!finish.contains("\"type\":\"response.output_text.done\""));
        assert!(finish.contains("\"type\":\"response.content_part.done\""));
        assert!(finish.contains("\"type\":\"response.output_item.done\""));
    }

    #[test]
    fn responses_tool_only_output_does_not_persist_empty_message() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_1".to_string(),
            input: "{\"cmd\":\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        }));

        let output = stream.completed_output_items();

        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["call_id"], "call_1");
    }

    #[test]
    fn responses_tool_only_stream_uses_output_index_zero() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        let events = sse_text(&stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_1".to_string(),
            input: "{\"cmd\":\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        })));

        assert!(!events.contains("\"type\":\"message\""));
        assert!(events.contains("\"type\":\"function_call\""));
        assert!(events.contains("\"output_index\":0"));
    }

    #[test]
    fn responses_output_tokens_count_tool_calls() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_1".to_string(),
            input: "{\"cmd\":\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        }));

        assert!(stream.final_output_tokens() > 0);
        assert_eq!(
            stream.completed_usage()["output_tokens"],
            stream.final_output_tokens()
        );
    }

    #[test]
    fn responses_reasoning_is_metered_but_not_output() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "reasoning_content",
        );

        let reasoning_event: ReasoningContentEvent =
            serde_json::from_value(serde_json::json!({ "text": "hidden chain" })).unwrap();
        let out = stream.process_event(&Event::ReasoningContent(reasoning_event));

        assert!(out.is_empty());
        let output = stream.completed_output_items();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["type"], "output_text");
        assert_eq!(output[0]["content"][0]["text"], "");
        assert!(stream.final_output_tokens() > 0);
    }

    #[test]
    fn responses_reasoning_is_ignored_when_thinking_disabled() {
        let mut stream = OpenAIResponsesStream::new_with_thinking(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            false,
            "reasoning_content",
        );

        let reasoning_event: ReasoningContentEvent =
            serde_json::from_value(serde_json::json!({ "text": "hidden chain" })).unwrap();
        let out = stream.process_event(&Event::ReasoningContent(reasoning_event));

        assert!(out.is_empty());
        assert_eq!(stream.final_output_tokens(), 0);
        assert_eq!(stream.completed_usage()["output_tokens"], 0);
    }

    #[test]
    fn chat_stream_reuses_generated_tool_id_for_missing_id_chunks() {
        let mut stream =
            OpenAIChatStream::new("claude-sonnet-4.5", 12, HashMap::new(), "reasoning_content");

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: String::new(),
            input: "{\"cmd\":".to_string(),
            input_is_json_object: false,
            stop: false,
        }));
        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: String::new(),
            input: "\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        }));

        let calls = stream.aggregated_tool_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.starts_with("toolu_"));
        assert_eq!(calls[0].1, "exec_command");
        assert_eq!(calls[0].2, "{\"cmd\":\"pwd\"}");
    }

    #[test]
    fn chat_stream_adopts_late_real_tool_id_in_final_snapshot() {
        let mut stream =
            OpenAIChatStream::new("claude-sonnet-4.5", 12, HashMap::new(), "reasoning_content");

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: String::new(),
            input: "{\"cmd\":".to_string(),
            input_is_json_object: false,
            stop: false,
        }));
        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_real".to_string(),
            input: "\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        }));

        let calls = stream.aggregated_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "call_real");
        assert_eq!(calls[0].1, "exec_command");
        assert_eq!(calls[0].2, "{\"cmd\":\"pwd\"}");
    }

    #[test]
    fn chat_stream_object_input_replaces_buffer() {
        let mut stream =
            OpenAIChatStream::new("claude-sonnet-4.5", 12, HashMap::new(), "reasoning_content");

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: String::new(),
            input: "{\"cmd\":\"old".to_string(),
            input_is_json_object: false,
            stop: false,
        }));
        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_real".to_string(),
            input: serde_json::json!({"cmd": "pwd"}).to_string(),
            input_is_json_object: true,
            stop: true,
        }));

        let calls = stream.aggregated_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "call_real");
        assert_eq!(calls[0].2, "{\"cmd\":\"pwd\"}");
    }

    #[test]
    fn chat_stream_extracts_tagged_thinking_from_text() {
        let mut stream =
            OpenAIChatStream::new("claude-sonnet-4.5", 12, HashMap::new(), "reasoning_content");

        let assistant_event: AssistantResponseEvent = serde_json::from_value(
            serde_json::json!({ "content": "<thinking>hidden</thinking>visible" }),
        )
        .unwrap();
        let mut out = stream.process_event(&Event::AssistantResponse(assistant_event));
        out.extend(stream.finish_events());
        let text = sse_text(&out);

        assert!(text.contains("\"reasoning_content\":\"hidden\""));
        assert!(text.contains("\"content\":\"visible\""));
        assert_eq!(stream.aggregated_text(), "visible");
        assert_eq!(stream.aggregated_reasoning(), "hidden");
    }

    #[test]
    fn chat_stream_ignores_reasoning_when_thinking_disabled() {
        let mut stream = OpenAIChatStream::new_with_thinking(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            false,
            "reasoning_content",
        );

        let reasoning_event: ReasoningContentEvent =
            serde_json::from_value(serde_json::json!({ "text": "hidden event" })).unwrap();
        assert!(
            stream
                .process_event(&Event::ReasoningContent(reasoning_event))
                .is_empty()
        );

        let assistant_event: AssistantResponseEvent = serde_json::from_value(
            serde_json::json!({ "content": "<thinking>hidden tag</thinking>visible" }),
        )
        .unwrap();
        let mut out = stream.process_event(&Event::AssistantResponse(assistant_event));
        out.extend(stream.finish_events());
        let text = sse_text(&out);

        assert!(text.contains("\"content\":\"visible\""));
        assert!(!text.contains("reasoning_content"));
        assert!(!text.contains("hidden"));
        assert_eq!(stream.aggregated_text(), "visible");
        assert_eq!(stream.aggregated_reasoning(), "");
    }

    #[test]
    fn chat_stream_wraps_reasoning_events_once_for_thinking_format() {
        let mut stream = OpenAIChatStream::new("claude-sonnet-4.5", 12, HashMap::new(), "thinking");

        let first: ReasoningContentEvent =
            serde_json::from_value(serde_json::json!({ "text": "step" })).unwrap();
        let second: ReasoningContentEvent =
            serde_json::from_value(serde_json::json!({ "text": "step two" })).unwrap();
        let mut out = stream.process_event(&Event::ReasoningContent(first));
        out.extend(stream.process_event(&Event::ReasoningContent(second)));
        out.extend(stream.finish_events());
        let text = sse_text(&out);

        assert!(text.contains("<thinking>step"));
        assert!(text.contains(" two"));
        assert!(text.contains("</thinking>"));
        assert_eq!(text.matches("<thinking>").count(), 1);
        assert_eq!(text.matches("</thinking>").count(), 1);
        assert!(!text.contains("</thinking><thinking>"));
    }

    #[test]
    fn chat_abort_events_do_not_emit_normal_done() {
        let mut stream =
            OpenAIChatStream::new("claude-sonnet-4.5", 12, HashMap::new(), "reasoning_content");

        let aborted = sse_text(&stream.abort_events());

        assert!(aborted.is_empty());
        assert!(stream.finish_events().is_empty());
    }

    #[test]
    fn responses_stream_reuses_generated_tool_id_for_missing_id_chunks() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: String::new(),
            input: "{\"cmd\":".to_string(),
            input_is_json_object: false,
            stop: false,
        }));
        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: String::new(),
            input: "\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        }));

        let calls = stream.aggregated_tool_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.starts_with("toolu_"));
        assert_eq!(calls[0].1, "exec_command");
        assert_eq!(calls[0].2, "{\"cmd\":\"pwd\"}");
    }

    #[test]
    fn responses_stream_finishes_unstopped_tool_on_stream_end() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_1".to_string(),
            input: "{\"cmd\":\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: false,
        }));
        let finish = sse_text(&stream.finish_events());

        assert!(finish.contains("\"type\":\"response.output_item.done\""));
        assert!(finish.contains("\"type\":\"function_call\""));
        assert!(finish.contains("\"call_id\":\"call_1\""));
        assert!(finish.contains("\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\""));
        assert!(finish.contains("\"type\":\"response.completed\""));
        assert!(finish.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn responses_stream_adopts_late_real_tool_id_in_final_snapshot() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: String::new(),
            input: "{\"cmd\":".to_string(),
            input_is_json_object: false,
            stop: false,
        }));
        let done_events = sse_text(&stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_real".to_string(),
            input: "\"pwd\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        })));

        let calls = stream.aggregated_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "call_real");
        assert_eq!(calls[0].1, "exec_command");
        assert_eq!(calls[0].2, "{\"cmd\":\"pwd\"}");
        assert!(done_events.contains("\"type\":\"response.output_item.done\""));
        assert!(done_events.contains("\"call_id\":\"call_real\""));

        let output = stream.completed_output_items();
        assert_eq!(output[0]["call_id"], "call_real");
    }

    #[test]
    fn responses_stream_object_input_replaces_buffer() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );

        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: String::new(),
            input: "{\"cmd\":\"old".to_string(),
            input_is_json_object: false,
            stop: false,
        }));
        stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_real".to_string(),
            input: serde_json::json!({"cmd": "pwd"}).to_string(),
            input_is_json_object: true,
            stop: true,
        }));

        let calls = stream.aggregated_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "call_real");
        assert_eq!(calls[0].2, "{\"cmd\":\"pwd\"}");
    }

    #[test]
    fn responses_stream_strips_tagged_thinking_from_text() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "reasoning_content",
        );

        let assistant_event: AssistantResponseEvent = serde_json::from_value(
            serde_json::json!({ "content": "<thinking>hidden</thinking>visible" }),
        )
        .unwrap();
        let mut out = stream.process_event(&Event::AssistantResponse(assistant_event));
        out.extend(stream.finish_events());
        let text = sse_text(&out);
        let output = stream.completed_output_items();

        assert!(text.contains("\"delta\":\"visible\""));
        assert!(!text.contains("hidden"));
        assert_eq!(output[0]["content"][0]["text"], "visible");
    }

    #[test]
    fn responses_stream_strips_tagged_thinking_across_chunks() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "reasoning_content",
        );

        let first: AssistantResponseEvent =
            serde_json::from_value(serde_json::json!({ "content": "pre <think" })).unwrap();
        let second: AssistantResponseEvent = serde_json::from_value(
            serde_json::json!({ "content": "pre <thinking>hidden</thinking>visible" }),
        )
        .unwrap();
        let mut out = stream.process_event(&Event::AssistantResponse(first));
        out.extend(stream.process_event(&Event::AssistantResponse(second)));
        out.extend(stream.finish_events());
        let text = sse_text(&out);
        let output = stream.completed_output_items();

        assert!(text.contains("\"delta\":\"pre \""));
        assert!(text.contains("\"delta\":\"visible\""));
        assert!(!text.contains("hidden"));
        assert_eq!(output[0]["content"][0]["text"], "pre visible");
    }

    #[test]
    fn responses_stream_preserves_metadata() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            Some(serde_json::json!({ "trace_id": "abc" })),
            "thinking",
        );

        let initial = sse_text(&stream.initial_events());
        let completed = sse_text(&stream.finish_events());

        assert!(initial.contains("\"metadata\":{\"trace_id\":\"abc\"}"));
        assert!(completed.contains("\"metadata\":{\"trace_id\":\"abc\"}"));
    }

    #[test]
    fn responses_stream_preserves_instructions() {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );
        stream.set_instructions(Some("be terse".to_string()));

        let completed = sse_text(&stream.finish_events());

        assert!(completed.contains("\"instructions\":\"be terse\""));
    }

    // ===== SSE 输出快照（ER-1/ER-3 回归防护网）=====
    //
    // 固定事件序列喂入 chat / responses stream，序列化全部 SSE 输出后写 golden。
    // 掩码易变 ID/时间戳。重构流式管道后输出必须逐字不变。

    fn chat_transcript(thinking_format: &str) -> String {
        let mut stream =
            OpenAIChatStream::new("claude-sonnet-4.5", 12, HashMap::new(), thinking_format);
        let mut out: Vec<bytes::Bytes> = Vec::new();

        let assistant: AssistantResponseEvent =
            serde_json::from_value(serde_json::json!({ "content": "Hello world" })).unwrap();
        out.extend(stream.process_event(&Event::AssistantResponse(assistant)));

        let mut reasoning = ReasoningContentEvent::default();
        reasoning.text = "let me think".to_string();
        out.extend(stream.process_event(&Event::ReasoningContent(reasoning)));

        out.extend(stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_snap".to_string(),
            input: "{\"cmd\":\"ls\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        })));

        out.extend(
            stream.process_event(&Event::ContextUsage(ContextUsageEvent {
                context_usage_percentage: 25.0,
            })),
        );

        out.extend(stream.finish_events());
        sse_text(&out)
    }

    fn responses_transcript() -> String {
        let mut stream = OpenAIResponsesStream::new(
            "claude-sonnet-4.5",
            12,
            HashMap::new(),
            None,
            None,
            "thinking",
        );
        let mut out: Vec<bytes::Bytes> = Vec::new();

        let assistant: AssistantResponseEvent =
            serde_json::from_value(serde_json::json!({ "content": "Hello world" })).unwrap();
        out.extend(stream.process_event(&Event::AssistantResponse(assistant)));

        out.extend(stream.process_event(&Event::ToolUse(ToolUseEvent {
            name: "exec_command".to_string(),
            tool_use_id: "call_snap".to_string(),
            input: "{\"cmd\":\"ls\"}".to_string(),
            input_is_json_object: false,
            stop: true,
        })));

        out.extend(
            stream.process_event(&Event::ContextUsage(ContextUsageEvent {
                context_usage_percentage: 25.0,
            })),
        );

        out.extend(stream.finish_events());
        sse_text(&out)
    }

    #[test]
    fn snapshot_openai_chat_reasoning_content() {
        crate::common::snapshot::assert_golden(
            "openai_chat_reasoning_content",
            &chat_transcript("reasoning_content"),
        );
    }

    #[test]
    fn snapshot_openai_chat_think_tag() {
        crate::common::snapshot::assert_golden(
            "openai_chat_think_tag",
            &chat_transcript("<think>"),
        );
    }

    #[test]
    fn snapshot_openai_responses() {
        crate::common::snapshot::assert_golden("openai_responses", &responses_transcript());
    }
}
