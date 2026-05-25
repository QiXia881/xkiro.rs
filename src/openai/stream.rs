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
use crate::kiro::model::events::{Event, MeteringEvent};
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
    let mut s = String::with_capacity(64);
    s.push_str("data: ");
    s.push_str(&value.to_string());
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

pub struct OpenAIChatStream {
    completion_id: String,
    created: i64,
    model: String,
    /// 客户端可能配置的 tool 短名映射（converter 层做了截断），回写时还原
    tool_name_map: HashMap<String, String>,
    /// tool_use_id → 累积器
    tool_acc: HashMap<String, ChatToolAccumulator>,
    next_tool_index: i32,
    role_emitted: bool,
    saw_tool_calls: bool,
    text_aggregated: String,
    /// 估算 input_tokens 的兜底值（在收到 contextUsageEvent 之前用）
    fallback_input_tokens: i32,
    context_input_tokens: Option<i32>,
    metering: Option<MeteringEvent>,
    finished_emitted: bool,
}

impl OpenAIChatStream {
    pub fn new(model: impl Into<String>, fallback_input_tokens: i32, tool_name_map: HashMap<String, String>) -> Self {
        Self {
            completion_id: format!("chatcmpl-{}", short_uuid()),
            created: now_unix(),
            model: model.into(),
            tool_name_map,
            tool_acc: HashMap::new(),
            next_tool_index: 0,
            role_emitted: false,
            saw_tool_calls: false,
            text_aggregated: String::new(),
            fallback_input_tokens,
            context_input_tokens: None,
            metering: None,
            finished_emitted: false,
        }
    }

    pub fn completion_id(&self) -> &str {
        &self.completion_id
    }

    pub fn metering(&self) -> Option<&MeteringEvent> {
        self.metering.as_ref()
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

    /// 处理一个 Kiro 事件，返回若干 SSE chunk。
    pub fn process_event(&mut self, event: &Event) -> Vec<Bytes> {
        match event {
            Event::AssistantResponse(resp) => {
                if resp.content.is_empty() {
                    return Vec::new();
                }
                self.text_aggregated.push_str(&resp.content);
                let chunk = ChatCompletionsChunk {
                    id: self.completion_id.clone(),
                    object: "chat.completion.chunk",
                    created: self.created,
                    model: self.model.clone(),
                    choices: vec![ChatChunkChoice {
                        index: 0,
                        delta: ChatChunkDelta {
                            role: None,
                            content: Some(resp.content.clone()),
                            tool_calls: None,
                        },
                        finish_reason: None,
                    }],
                    usage: None,
                };
                vec![sse_chunk(&chunk)]
            }
            Event::ToolUse(tool_use) => {
                let id = tool_use.tool_use_id.clone();
                let resolved_name = self
                    .tool_name_map
                    .get(&tool_use.name)
                    .cloned()
                    .unwrap_or_else(|| tool_use.name.clone());

                let mut out = Vec::new();
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
                    acc.arguments.push_str(&tool_use.input);
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
                let actual =
                    (usage.context_usage_percentage * (window as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual);
                Vec::new()
            }
            Event::Metering(metering) => {
                self.metering = Some(metering.clone());
                Vec::new()
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

        let finish_reason = if self.saw_tool_calls {
            Some("tool_calls".to_string())
        } else {
            Some("stop".to_string())
        };

        let prompt_tokens = self
            .context_input_tokens
            .unwrap_or(self.fallback_input_tokens);
        let completion_tokens = token::count_tokens(&self.text_aggregated) as i32;
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

        vec![sse_chunk(&final_chunk), sse_done()]
    }

    pub fn aggregated_text(&self) -> &str {
        &self.text_aggregated
    }

    pub fn aggregated_tool_calls(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<_> = self
            .tool_acc
            .iter()
            .map(|(id, acc)| (id.clone(), acc.name.clone(), acc.arguments.clone()))
            .collect();
        // 按起始顺序排序，保证响应输出稳定
        v.sort_by_key(|(id, _, _)| self.tool_acc.get(id).map(|a| a.index).unwrap_or(0));
        v
    }

    pub fn final_input_tokens(&self) -> i32 {
        self.context_input_tokens
            .unwrap_or(self.fallback_input_tokens)
    }

    pub fn final_output_tokens(&self) -> i32 {
        token::count_tokens(&self.text_aggregated) as i32
    }
}

// ============================================================================
// OpenAI Responses 流上下文
// ============================================================================

struct ResponsesToolAccumulator {
    name: String,
    arguments: String,
    output_index: usize,
    started: bool,
}

pub struct OpenAIResponsesStream {
    response_id: String,
    message_id: String,
    created_at: i64,
    model: String,
    previous_response_id: Option<String>,
    tool_name_map: HashMap<String, String>,
    next_output_index: usize,
    tool_acc: HashMap<String, ResponsesToolAccumulator>,
    text_aggregated: String,
    fallback_input_tokens: i32,
    context_input_tokens: Option<i32>,
    metering: Option<MeteringEvent>,
    /// `response.output_item.added` 是否已为 message 输出（output_index=0）发出
    message_item_started: bool,
    finished_emitted: bool,
}

impl OpenAIResponsesStream {
    pub fn new(
        model: impl Into<String>,
        fallback_input_tokens: i32,
        tool_name_map: HashMap<String, String>,
        previous_response_id: Option<String>,
    ) -> Self {
        Self {
            response_id: format!("resp_{}", short_uuid()),
            message_id: format!("msg_{}", short_uuid()),
            created_at: now_unix(),
            model: model.into(),
            previous_response_id,
            tool_name_map,
            next_output_index: 1, // 0 留给 assistant message
            tool_acc: HashMap::new(),
            text_aggregated: String::new(),
            fallback_input_tokens,
            context_input_tokens: None,
            metering: None,
            message_item_started: false,
            finished_emitted: false,
        }
    }

    pub fn metering(&self) -> Option<&MeteringEvent> {
        self.metering.as_ref()
    }

    pub fn response_id(&self) -> &str {
        &self.response_id
    }

    /// 初始事件：`response.created` + `response.output_item.added`（assistant message）
    pub fn initial_events(&mut self) -> Vec<Bytes> {
        let created = json!({
            "type": "response.created",
            "response": {
                "id": self.response_id,
                "object": "response",
                "created_at": self.created_at,
                "status": "in_progress",
                "model": self.model,
                "previous_response_id": self.previous_response_id,
                "output": []
            }
        });

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
        self.message_item_started = true;

        vec![sse_data(&created), sse_data(&item_added)]
    }

    pub fn process_event(&mut self, event: &Event) -> Vec<Bytes> {
        match event {
            Event::AssistantResponse(resp) => {
                if resp.content.is_empty() {
                    return Vec::new();
                }
                self.text_aggregated.push_str(&resp.content);
                let delta = json!({
                    "type": "response.output_text.delta",
                    "response_id": self.response_id,
                    "item_id": self.message_id,
                    "output_index": 0,
                    "content_index": 0,
                    "delta": resp.content,
                });
                vec![sse_data(&delta)]
            }
            Event::ToolUse(tool_use) => {
                let id = tool_use.tool_use_id.clone();
                let resolved_name = self
                    .tool_name_map
                    .get(&tool_use.name)
                    .cloned()
                    .unwrap_or_else(|| tool_use.name.clone());

                let mut out = Vec::new();
                let next_idx = self.next_output_index;
                let acc = self.tool_acc.entry(id.clone()).or_insert_with(|| {
                    ResponsesToolAccumulator {
                        name: resolved_name.clone(),
                        arguments: String::new(),
                        output_index: next_idx,
                        started: false,
                    }
                });
                if !acc.started {
                    acc.started = true;
                    self.next_output_index += 1;
                    let added = json!({
                        "type": "response.output_item.added",
                        "response_id": self.response_id,
                        "output_index": acc.output_index,
                        "item": {
                            "id": id,
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
                    acc.arguments.push_str(&tool_use.input);
                    let delta = json!({
                        "type": "response.function_call_arguments.delta",
                        "response_id": self.response_id,
                        "call_id": id,
                        "delta": tool_use.input,
                    });
                    out.push(sse_data(&delta));
                }

                if tool_use.stop {
                    let arguments_final = acc.arguments.clone();
                    let output_index = acc.output_index;
                    let name = acc.name.clone();
                    let done = json!({
                        "type": "response.function_call_arguments.done",
                        "response_id": self.response_id,
                        "call_id": id,
                        "arguments": arguments_final,
                    });
                    out.push(sse_data(&done));
                    let item_done = json!({
                        "type": "response.output_item.done",
                        "response_id": self.response_id,
                        "output_index": output_index,
                        "item": {
                            "id": id,
                            "type": "function_call",
                            "status": "completed",
                            "call_id": id,
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
                let actual =
                    (usage.context_usage_percentage * (window as f64) / 100.0) as i32;
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

        let mut out = Vec::new();

        // 1) text 段终结：output_text.done + content_part.done + output_item.done(message)
        if self.message_item_started {
            if !self.text_aggregated.is_empty() {
                let text_done = json!({
                    "type": "response.output_text.done",
                    "response_id": self.response_id,
                    "item_id": self.message_id,
                    "output_index": 0,
                    "content_index": 0,
                    "text": self.text_aggregated,
                });
                out.push(sse_data(&text_done));
            }
            let content: Vec<Value> = if self.text_aggregated.is_empty() {
                Vec::new()
            } else {
                vec![json!({
                    "type": "output_text",
                    "text": self.text_aggregated,
                    "annotations": [],
                })]
            };
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
        }

        // 2) response.completed
        let prompt_tokens = self
            .context_input_tokens
            .unwrap_or(self.fallback_input_tokens);
        let completion_tokens = token::count_tokens(&self.text_aggregated) as i32;
        let total = prompt_tokens.saturating_add(completion_tokens);

        let mut output_items: Vec<Value> = Vec::new();

        // assistant message
        let mut message_content: Vec<Value> = Vec::new();
        if !self.text_aggregated.is_empty() {
            message_content.push(json!({
                "type": "output_text",
                "text": self.text_aggregated,
                "annotations": [],
            }));
        }
        output_items.push(json!({
            "id": self.message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": message_content,
        }));

        // function calls，按 output_index 顺序
        let mut tool_items: Vec<(usize, Value)> = self
            .tool_acc
            .iter()
            .map(|(id, acc)| {
                (
                    acc.output_index,
                    json!({
                        "id": id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": id,
                        "name": acc.name,
                        "arguments": acc.arguments,
                    }),
                )
            })
            .collect();
        tool_items.sort_by_key(|(idx, _)| *idx);
        for (_, item) in tool_items {
            output_items.push(item);
        }

        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": self.response_id,
                "object": "response",
                "created_at": self.created_at,
                "status": "completed",
                "model": self.model,
                "previous_response_id": self.previous_response_id,
                "output": output_items,
                "usage": {
                    "input_tokens": prompt_tokens,
                    "output_tokens": completion_tokens,
                    "total_tokens": total,
                }
            }
        });
        out.push(sse_data(&completed));

        out
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
                    id.clone(),
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
            .unwrap_or(self.fallback_input_tokens)
    }

    pub fn final_output_tokens(&self) -> i32 {
        token::count_tokens(&self.text_aggregated) as i32
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
