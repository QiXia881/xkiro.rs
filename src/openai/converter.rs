//! OpenAI → Anthropic MessagesRequest 转换
//!
//! 将 OpenAI Chat Completions / Responses 请求归一化为 [`MessagesRequest`]，
//! 之后由上层走 `anthropic::converter::convert_request` → Kiro。
//!
//! 字段贴合 OpenAI 官方协议字段名，并在边界归一化常见客户端形状。

use base64::{Engine, engine::general_purpose};
use serde_json::{Value, json};

use crate::anthropic::types::{Message, MessagesRequest, Metadata, SystemMessage, Tool};

use super::types::{
    ChatCompletionsRequest, ChatMessage, ChatTool, ChatToolCall, ChatToolCallFunction,
    ResponsesRequest,
};

/// OpenAI 默认上限 tokens（max_tokens 缺省时用）。
const DEFAULT_MAX_TOKENS: i32 = 8192;

// ============================================================================
// Chat Completions
// ============================================================================

pub fn chat_completions_to_messages_request(req: &ChatCompletionsRequest) -> MessagesRequest {
    let mut system_blocks: Vec<SystemMessage> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    // tool_call_id → 累积的 tool_result 块（在下一条 user/assistant 之前 flush）
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for msg in &req.messages {
        match msg.role.as_str() {
            "system" | "developer" => {
                let text = extract_message_text(msg.content.as_ref());
                if !text.is_empty() {
                    system_blocks.push(SystemMessage {
                        text,
                        block_type: None,
                        cache_control: None,
                    });
                }
            }
            "tool" => {
                // OpenAI 的 tool 消息映射到 Anthropic 的 tool_result content block，
                // 在下一条 user/assistant 之前作为独立 user turn 发出。
                let tool_call_id = msg.tool_call_id.clone().unwrap_or_default();
                let result_content =
                    tool_message_content_to_tool_result_content(msg.content.as_ref());
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": result_content,
                }));
            }
            "user" => {
                if !pending_tool_results.is_empty() {
                    let blocks = std::mem::take(&mut pending_tool_results);
                    messages.push(Message {
                        role: "user".to_string(),
                        content: blocks_to_value(blocks),
                    });
                }

                let blocks = content_to_anthropic_blocks(msg.content.as_ref());
                messages.push(Message {
                    role: "user".to_string(),
                    content: blocks_to_value(blocks),
                });
            }
            "assistant" => {
                // 先 flush 挂起的 tool_results 作为独立 user 消息（保证顺序）
                if !pending_tool_results.is_empty() {
                    let blocks = std::mem::take(&mut pending_tool_results);
                    messages.push(Message {
                        role: "user".to_string(),
                        content: blocks_to_value(blocks),
                    });
                }

                let mut blocks = assistant_content_to_anthropic_blocks(msg.content.as_ref());
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        blocks.push(tool_call_to_block(tc));
                    }
                }
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: blocks_to_value(blocks),
                });
            }
            _ => {}
        }
    }

    if !pending_tool_results.is_empty() {
        let blocks = std::mem::take(&mut pending_tool_results);
        messages.push(Message {
            role: "user".to_string(),
            content: blocks_to_value(blocks),
        });
    }

    let tools = req.tools.as_ref().map(|tools| {
        tools
            .iter()
            .filter_map(chat_tool_to_anthropic_tool)
            .collect::<Vec<_>>()
    });

    MessagesRequest {
        model: req.model.clone(),
        max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        temperature: req.temperature,
        top_p: req.top_p,
        messages,
        stream: req.stream,
        system: if system_blocks.is_empty() {
            None
        } else {
            Some(system_blocks)
        },
        tools,
        tool_choice: req.tool_choice.clone(),
        thinking: None,
        output_config: None,
        metadata: Some(Metadata {
            user_id: None,
            preserve_tool_names: true,
        }),
    }
}

// ============================================================================
// Responses
// ============================================================================

pub fn responses_to_messages_request(req: &ResponsesRequest) -> Result<MessagesRequest, String> {
    let mut messages = Vec::new();
    if let Some(instructions) = &req.instructions {
        if !instructions.is_empty() {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(Value::String(instructions.clone())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
    }
    messages.extend(parse_responses_input_messages(&req.input)?);
    responses_openai_messages_to_messages_request(req, messages)
}

pub fn responses_openai_messages_to_messages_request(
    req: &ResponsesRequest,
    messages: Vec<ChatMessage>,
) -> Result<MessagesRequest, String> {
    if messages.is_empty() {
        return Err("input must contain at least one message".to_string());
    }

    let has_user = messages.iter().any(|message| message.role == "user");
    if !has_user {
        return Err("input must contain at least one user message".to_string());
    }

    let chat_req = ChatCompletionsRequest {
        model: req.model.clone(),
        messages,
        stream: req.stream,
        max_tokens: req.max_output_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stop: None,
        tools: None,
        tool_choice: req.tool_choice.clone(),
        reasoning_effort: None,
    };
    let mut converted = chat_completions_to_messages_request(&chat_req);
    converted.tools = req
        .tools
        .as_ref()
        .map(|tools| tools.iter().flat_map(responses_tool_to_anthropic).collect());
    Ok(converted)
}

pub fn parse_responses_input_messages(input: &Value) -> Result<Vec<ChatMessage>, String> {
    match input {
        Value::String(text) => {
            if text.trim().is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![ChatMessage {
                    role: "user".to_string(),
                    content: Some(Value::String(text.clone())),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                }])
            }
        }
        Value::Array(items) => Ok(convert_responses_input_items_to_chat_messages(items)),
        Value::Object(_) => Ok(convert_responses_input_items_to_chat_messages(
            std::slice::from_ref(input),
        )),
        Value::Null => Ok(Vec::new()),
        other => Err(format!(
            "unsupported input shape: {}",
            other_type_name(other)
        )),
    }
}

fn other_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn convert_responses_input_items_to_chat_messages(items: &[Value]) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut pending_user_parts = Vec::new();

    let flush_pending_user = |pending: &mut Vec<Value>, messages: &mut Vec<ChatMessage>| {
        if pending.is_empty() {
            return;
        }
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: Some(Value::Array(std::mem::take(pending))),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    };

    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let role = obj.get("role").and_then(Value::as_str);

        match item_type {
            "message" => {
                flush_pending_user(&mut pending_user_parts, &mut messages);
                if let Some(message) = responses_chat_message_from_input_item(item, role) {
                    messages.push(message);
                }
            }
            "function_call_output" | "tool_result" => {
                flush_pending_user(&mut pending_user_parts, &mut messages);
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("tool_call_id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                let output = stringify_responses_output(item);
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(Value::String(output)),
                    tool_calls: None,
                    tool_call_id: Some(call_id),
                    name: None,
                });
            }
            "function_call" => {
                flush_pending_user(&mut pending_user_parts, &mut messages);
                let tool_call = ChatToolCall {
                    id: response_string_field(item, &["call_id", "id"]),
                    call_type: "function".to_string(),
                    function: ChatToolCallFunction {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        arguments: stringify_responses_arguments(item.get("arguments")),
                    },
                };
                if let Some(last) = messages.last_mut()
                    && last.role == "assistant"
                    && last
                        .tool_calls
                        .as_ref()
                        .is_some_and(|tool_calls| !tool_calls.is_empty())
                    && extract_message_text(last.content.as_ref())
                        .trim()
                        .is_empty()
                {
                    last.tool_calls.get_or_insert_with(Vec::new).push(tool_call);
                } else {
                    messages.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: Some(Value::String(String::new())),
                        tool_calls: Some(vec![tool_call]),
                        tool_call_id: None,
                        name: None,
                    });
                }
            }
            "input_text" | "text" => {
                if item
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
                {
                    pending_user_parts.push(item.clone());
                }
            }
            "input_image" | "image" | "image_url" | "file" | "input_file" => {
                pending_user_parts.push(item.clone());
            }
            "output_text" => {
                flush_pending_user(&mut pending_user_parts, &mut messages);
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    messages.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: Some(Value::String(text.to_string())),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                }
            }
            _ if role.is_some() => {
                flush_pending_user(&mut pending_user_parts, &mut messages);
                if let Some(message) = responses_chat_message_from_input_item(item, role) {
                    messages.push(message);
                }
            }
            _ => {}
        }
    }

    flush_pending_user(&mut pending_user_parts, &mut messages);
    messages
}

fn responses_chat_message_from_input_item(item: &Value, role: Option<&str>) -> Option<ChatMessage> {
    let role = role.unwrap_or("user").to_string();

    if let Some(content) = item.get("content") {
        match content {
            Value::String(text) => {
                return Some(ChatMessage {
                    role,
                    content: Some(Value::String(text.clone())),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            Value::Array(parts) => {
                let content = responses_chat_content_value(parts);
                return Some(ChatMessage {
                    role,
                    content: Some(content),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            Value::Object(_) => {
                return responses_chat_message_from_input_item(content, Some(&role));
            }
            _ => {}
        }
    }

    if let Some(text) = item.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        return Some(ChatMessage {
            role,
            content: Some(Value::String(text.to_string())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    None
}

fn responses_chat_content_value(parts: &[Value]) -> Value {
    let mut text_only = String::new();
    let mut normalized_parts = Vec::new();
    let mut has_non_text = false;

    for part in parts {
        let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
        match part_type {
            "input_text" | "text" => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    text_only.push_str(text);
                    normalized_parts.push(json!({ "type": "input_text", "text": text }));
                }
            }
            "output_text" => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    text_only.push_str(text);
                    normalized_parts.push(json!({ "type": "input_text", "text": text }));
                }
            }
            "input_image" | "image" | "image_url" | "file" | "input_file" => {
                has_non_text = true;
                normalized_parts.push(part.clone());
            }
            _ => {
                if let Some(text) = part.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    text_only.push_str(text);
                    normalized_parts.push(json!({ "type": "input_text", "text": text }));
                }
            }
        }
    }

    if has_non_text {
        Value::Array(normalized_parts)
    } else {
        Value::String(text_only)
    }
}

fn stringify_responses_arguments(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn stringify_responses_output(item: &Value) -> String {
    if let Some(output) = item.get("output") {
        return match output {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(content) = item.get("content") {
        return match content {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    String::new()
}

fn response_string_field(item: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = item.get(*key).and_then(Value::as_str)
            && !value.is_empty()
        {
            return value.to_string();
        }
    }
    String::new()
}

struct DataUrl {
    media_type: String,
    data: String,
}

fn parse_data_url(url: &str) -> Option<DataUrl> {
    let cleaned = url.trim().replace(['\n', '\r'], "");
    if cleaned.contains("[Image") {
        return None;
    }
    let stripped = cleaned.strip_prefix("data:image/")?;
    let (meta, data) = stripped.split_once(',')?;
    let meta_lower = meta.to_lowercase();
    if !meta_lower.contains(";base64") || !is_valid_base64_image_data(data) {
        return None;
    }
    let format = meta
        .split(';')
        .next()
        .unwrap_or("png")
        .trim()
        .to_lowercase();
    let format = if format == "jpg" { "jpeg" } else { &format };
    Some(DataUrl {
        media_type: format!("image/{format}"),
        data: data.to_string(),
    })
}

fn is_valid_base64_image_data(data: &str) -> bool {
    if data.contains("[Image") {
        return false;
    }
    general_purpose::STANDARD.decode(data).is_ok()
        || general_purpose::STANDARD_NO_PAD.decode(data).is_ok()
        || general_purpose::URL_SAFE.decode(data).is_ok()
        || general_purpose::URL_SAFE_NO_PAD.decode(data).is_ok()
}

fn base64_image_block(data: &str, media_type: Option<&str>) -> Option<Value> {
    let media_type = normalize_image_media_type(media_type)?;
    let cleaned = data.trim().replace(['\n', '\r'], "");
    if cleaned.is_empty() || cleaned.contains("[Image") {
        return None;
    }
    if let Some(parsed) = parse_data_url(&cleaned) {
        return Some(image_block(parsed.media_type, parsed.data));
    }
    if cleaned.starts_with("http://") || cleaned.starts_with("https://") {
        return None;
    }
    if !is_valid_base64_image_data(&cleaned) {
        return None;
    }
    Some(image_block(media_type, cleaned))
}

fn image_block(media_type: impl Into<String>, data: impl Into<String>) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type.into(),
            "data": data.into(),
        }
    })
}

fn normalize_image_media_type(raw: Option<&str>) -> Option<String> {
    let media_type = raw.unwrap_or("image/png").trim();
    if media_type.to_lowercase().starts_with("image/") {
        Some(media_type.to_string())
    } else {
        None
    }
}

pub(super) fn openai_image_part_to_block(item: &Value) -> Option<Value> {
    if let Some(file) = item.get("file").filter(|v| v.is_object())
        && let Some(block) = openai_image_part_to_block(file)
    {
        return Some(block);
    }
    if let Some(source) = item.get("source").filter(|v| v.is_object())
        && let Some(block) = openai_image_part_to_block(source)
    {
        return Some(block);
    }

    let media_type = item
        .get("media_type")
        .and_then(Value::as_str)
        .or_else(|| item.get("mime_type").and_then(Value::as_str))
        .or_else(|| item.get("mime").and_then(Value::as_str));

    for key in ["url", "image_url", "data", "b64_json", "image_base64"] {
        match item.get(key) {
            Some(Value::String(raw)) => {
                if let Some(block) = base64_image_block(raw, media_type) {
                    return Some(block);
                }
            }
            Some(Value::Object(obj)) => {
                if let Some(raw) = obj.get("url").and_then(Value::as_str)
                    && let Some(block) = base64_image_block(raw, media_type)
                {
                    return Some(block);
                }
            }
            _ => {}
        }
    }

    None
}

// ============================================================================
// 通用辅助
// ============================================================================

fn extract_message_text(content: Option<&Value>) -> String {
    let Some(value) = content else {
        return String::new();
    };

    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                let text = extract_message_text(Some(item));
                if !text.trim().is_empty() {
                    parts.push(text);
                }
            }
            if parts.is_empty() {
                serde_json::to_string(value).unwrap_or_default()
            } else {
                parts.join("")
            }
        }
        Value::Object(obj) => {
            if let Some(text) = obj.get("text").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                return text.to_string();
            }
            if let Some(nested) = obj.get("content") {
                let text = extract_message_text(Some(nested));
                if !text.trim().is_empty() {
                    return text;
                }
            }
            serde_json::to_string(value).unwrap_or_default()
        }
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn content_to_anthropic_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![json!({ "type": "text", "text": s })]
            }
        }
        Some(Value::Array(items)) => {
            let mut text = String::new();
            let mut blocks = Vec::new();
            for item in items {
                if let Some(part_text) = openai_text_part(item) {
                    text.push_str(part_text);
                    continue;
                }
                if let Some(block) = chat_content_part_to_block(item) {
                    blocks.push(block);
                }
            }
            if !text.is_empty() {
                blocks.insert(0, json!({ "type": "text", "text": text }));
            }
            blocks
        }
        Some(Value::Null) | None => Vec::new(),
        Some(other @ Value::Object(_)) => {
            if let Some(block) = chat_content_part_to_block(other) {
                vec![block]
            } else {
                let text = extract_message_text(Some(other));
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({ "type": "text", "text": text })]
                }
            }
        }
        Some(other) => vec![json!({ "type": "text", "text": other.to_string() })],
    }
}

fn assistant_content_to_anthropic_blocks(content: Option<&Value>) -> Vec<Value> {
    let text = extract_message_text(content);
    if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": text })]
    }
}

fn tool_message_content_to_tool_result_content(content: Option<&Value>) -> Value {
    let blocks = content_to_anthropic_blocks(content);
    if blocks.is_empty() {
        return Value::String(extract_message_text(content));
    }
    Value::Array(blocks)
}

fn chat_content_part_to_block(item: &Value) -> Option<Value> {
    let part_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match part_type {
        "text" | "input_text" | "output_text" => {
            let text = openai_text_part(item).unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(json!({ "type": "text", "text": text }))
            }
        }
        "image_url" | "image" | "input_image" | "file" | "input_file" => {
            openai_image_part_to_block(item)
        }
        "" => item
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| json!({ "type": "text", "text": text }))
            .or_else(|| openai_image_part_to_block(item)),
        _ => item
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| json!({ "type": "text", "text": text })),
    }
}

fn openai_text_part(item: &Value) -> Option<&str> {
    item.get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn blocks_to_value(blocks: Vec<Value>) -> Value {
    if blocks.is_empty() {
        Value::String(String::new())
    } else {
        Value::Array(blocks)
    }
}

fn tool_call_to_block(tc: &ChatToolCall) -> Value {
    let input = parse_json_loose(&tc.function.arguments);
    json!({
        "type": "tool_use",
        "id": tc.id,
        "name": tc.function.name,
        "input": input,
    })
}

fn parse_json_loose(s: &str) -> Value {
    if s.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str(s) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => json!({}),
    }
}

fn web_search_hosted_tool() -> Tool {
    Tool {
        tool_type: Some("web_search".to_string()),
        name: "web_search".to_string(),
        description: String::new(),
        input_schema: std::collections::HashMap::new(),
        max_uses: None,
        cache_control: None,
    }
}

fn is_web_search_alias(tool_type: &str) -> bool {
    matches!(
        tool_type,
        "web_search"
            | "web_search_preview"
            | "web_search_2025_03_11"
            | "web_search_preview_2025_03_11"
            | "browser"
    ) || tool_type.starts_with("web_search_")
}

fn chat_tool_to_anthropic_tool(t: &ChatTool) -> Option<Tool> {
    if is_web_search_alias(&t.tool_type) {
        return Some(web_search_hosted_tool());
    }

    if t.tool_type != "function" {
        tracing::warn!(
            tool_type = %t.tool_type,
            "OpenAI Chat Completions 请求包含未支持的 hosted tool，已忽略"
        );
        return None;
    }

    let func = t.function.as_ref()?;
    if func.name.trim().is_empty() {
        return None;
    }
    let input_schema = match func.parameters.clone() {
        Some(Value::Object(map)) => map.into_iter().collect(),
        _ => std::collections::HashMap::new(),
    };
    Some(Tool {
        tool_type: None,
        name: func.name.clone(),
        description: func.description.clone().unwrap_or_default(),
        input_schema,
        max_uses: None,
        cache_control: None,
    })
}

/// Responses tool 形如 { type:"function", name, description, parameters }
/// hosted web_search → Anthropic 形 web_search（type:"web_search"），由 OpenAI handlers 检测后走本地 MCP
/// type=namespace 时递归展平内部 tools（OpenAI Agents SDK / Codex / Cursor 等使用）
/// 其他 hosted 类型（file_search / code_interpreter / image_generation / computer_use* / local_shell / mcp）
/// Kiro 后端无对应能力，记录 warn 后丢弃
fn responses_tool_to_anthropic(value: &Value) -> Vec<Tool> {
    let t = value.get("type").and_then(Value::as_str).unwrap_or("");

    if is_web_search_alias(t) {
        return vec![web_search_hosted_tool()];
    }

    if t == "namespace" {
        let inner = value.get("tools").and_then(Value::as_array);
        let Some(inner) = inner else {
            tracing::warn!("OpenAI Responses namespace tool 缺少 tools 字段，已忽略");
            return Vec::new();
        };
        return inner.iter().flat_map(responses_tool_to_anthropic).collect();
    }

    if t != "function" && !t.is_empty() {
        tracing::warn!(
            tool_type = %t,
            "OpenAI Responses 请求包含未支持的 hosted tool，已忽略"
        );
        return Vec::new();
    }

    let function = value.get("function");
    let name = function
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| value.get("name").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    if name.trim().is_empty() {
        return Vec::new();
    }
    let description = function
        .and_then(|f| f.get("description"))
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty())
        .or_else(|| value.get("description").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let parameters = function
        .and_then(|f| f.get("parameters"))
        .filter(|parameters| !parameters.is_null())
        .cloned()
        .or_else(|| {
            value
                .get("parameters")
                .filter(|parameters| !parameters.is_null())
                .cloned()
        });
    let input_schema = match parameters {
        Some(Value::Object(map)) => map.into_iter().collect(),
        _ => std::collections::HashMap::new(),
    };
    vec![Tool {
        tool_type: None,
        name,
        description,
        input_schema,
        max_uses: None,
        cache_control: None,
    }]
}

#[allow(dead_code)]
pub(super) fn _unused_chat_message(_m: &ChatMessage) {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        chat_completions_to_messages_request, chat_content_part_to_block,
        chat_tool_to_anthropic_tool, parse_json_loose, parse_responses_input_messages,
        responses_to_messages_request, responses_tool_to_anthropic,
    };
    use crate::{
        anthropic::converter::convert_request,
        kiro::model::requests::conversation::Message as KiroMessage,
        model::config::{CompressionConfig, PromptFilterConfig},
        openai::types::{ChatCompletionsRequest, ChatTool, ResponsesRequest},
    };

    const VALID_IMAGE_B64: &str = "iVBORw0KGgo=";

    #[test]
    fn responses_browser_tool_maps_to_hosted_web_search() {
        let tools = responses_tool_to_anthropic(&json!({
            "type": "browser"
        }));

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "web_search");
        assert_eq!(tools[0].tool_type.as_deref(), Some("web_search"));
    }

    #[test]
    fn responses_namespace_browser_tool_maps_to_hosted_web_search() {
        let tools = responses_tool_to_anthropic(&json!({
            "type": "namespace",
            "tools": [
                { "type": "browser" }
            ]
        }));

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "web_search");
        assert_eq!(tools[0].tool_type.as_deref(), Some("web_search"));
    }

    #[test]
    fn responses_tool_uses_nested_fields_first() {
        let tools = responses_tool_to_anthropic(&json!({
            "type": "function",
            "name": "flat_name",
            "description": "flat description",
            "parameters": { "type": "object", "properties": { "flat": { "type": "string" } } },
            "function": {
                "name": "nested_name",
                "description": "nested description",
                "parameters": { "type": "object", "properties": { "nested": { "type": "string" } } }
            }
        }));

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "nested_name");
        assert_eq!(tools[0].description, "nested description");
        assert!(tools[0].input_schema.contains_key("properties"));
        assert!(tools[0].input_schema["properties"].get("nested").is_some());
    }

    #[test]
    fn responses_tool_falls_back_to_flat_when_nested_incomplete() {
        let tools = responses_tool_to_anthropic(&json!({
            "type": "function",
            "name": "flat_name",
            "description": "flat description",
            "parameters": { "type": "object", "properties": { "flat": { "type": "string" } } },
            "function": {
                "name": "",
                "description": "",
                "parameters": null
            }
        }));

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "flat_name");
        assert_eq!(tools[0].description, "flat description");
        assert!(tools[0].input_schema["properties"].get("flat").is_some());
    }

    #[test]
    fn responses_tool_skips_empty_names() {
        let tools = responses_tool_to_anthropic(&json!({
            "type": "function",
            "name": "  ",
            "parameters": { "type": "object" }
        }));

        assert!(tools.is_empty());
    }

    #[test]
    fn chat_browser_tool_maps_to_hosted_web_search() {
        let tool = chat_tool_to_anthropic_tool(&ChatTool {
            tool_type: "browser".to_string(),
            function: None,
        })
        .expect("browser alias should be preserved");

        assert_eq!(tool.name, "web_search");
        assert_eq!(tool.tool_type.as_deref(), Some("web_search"));
    }

    #[test]
    fn chat_tool_accepts_responses_flat_format() {
        let tool: ChatTool = serde_json::from_value(json!({
            "type": "function",
            "name": "exec_command",
            "description": "Run a shell command",
            "parameters": {
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                }
            }
        }))
        .expect("flat tool should parse");

        let converted = chat_tool_to_anthropic_tool(&tool).expect("flat tool should convert");
        assert_eq!(converted.name, "exec_command");
        assert_eq!(converted.description, "Run a shell command");
        assert!(converted.input_schema.contains_key("properties"));
    }

    #[test]
    fn chat_tool_accepts_nested_format() {
        let tool: ChatTool = serde_json::from_value(json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": { "type": "object" }
            }
        }))
        .expect("nested tool should parse");

        let converted = chat_tool_to_anthropic_tool(&tool).expect("nested tool should convert");
        assert_eq!(converted.name, "get_weather");
        assert_eq!(converted.description, "Get weather");
    }

    #[test]
    fn chat_tool_skips_empty_names() {
        let tool: ChatTool = serde_json::from_value(json!({
            "type": "function",
            "name": "",
            "parameters": { "type": "object" }
        }))
        .expect("empty-name tool should parse");

        assert!(chat_tool_to_anthropic_tool(&tool).is_none());
    }

    #[test]
    fn chat_tool_skips_missing_type() {
        let tool: ChatTool = serde_json::from_value(json!({
            "name": "exec_command",
            "description": "Run a shell command",
            "parameters": { "type": "object" }
        }))
        .expect("missing-type tool should parse with Go zero value");

        assert!(chat_tool_to_anthropic_tool(&tool).is_none());
    }

    #[test]
    fn tool_call_arguments_non_object_falls_back_to_empty_object() {
        assert_eq!(parse_json_loose("[]"), json!({}));
        assert_eq!(parse_json_loose("\"text\""), json!({}));
        assert_eq!(parse_json_loose("true"), json!({}));
        assert_eq!(parse_json_loose("{\"cmd\":\"pwd\"}"), json!({"cmd": "pwd"}));
    }

    #[test]
    fn chat_tools_preserve_openai_names_in_final_kiro_payload() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "run it" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "exec_command",
                            "arguments": "{\"cmd\":\"pwd\"}"
                        }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_1", "content": "/home/xia" }
            ],
            "tools": [{
                "type": "function",
                "name": "exec_command",
                "description": "Run a shell command",
                "parameters": { "type": "object" }
            }]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let tools = payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;
        assert!(!tools.is_empty(), "tool definitions should be attached");
        assert_eq!(tools[0].tool_specification.name, "exec_command");

        let tool_name = payload
            .conversation_state
            .history
            .iter()
            .find_map(|item| match item {
                KiroMessage::Assistant(assistant) => assistant
                    .assistant_response_message
                    .tool_uses
                    .as_ref()
                    .and_then(|tool_uses| tool_uses.first())
                    .map(|tool_use| tool_use.name.as_str()),
                KiroMessage::User(_) => None,
            })
            .expect("history assistant tool_use should exist");
        assert_eq!(tool_name, "exec_command");
    }

    #[test]
    fn chat_assistant_map_content_in_history() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "u1" },
                { "role": "assistant", "content": { "type": "text", "text": "assistant-map" } },
                { "role": "user", "content": "u2" }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let assistant = payload
            .conversation_state
            .history
            .iter()
            .find_map(|item| match item {
                KiroMessage::Assistant(assistant) => Some(assistant),
                KiroMessage::User(_) => None,
            })
            .expect("assistant history should exist");

        assert_eq!(
            assistant.assistant_response_message.content,
            "assistant-map"
        );
    }

    #[test]
    fn chat_assistant_unknown_array_content_falls_back_to_json() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "u1" },
                {
                    "role": "assistant",
                    "content": [
                        { "custom": "kept" }
                    ]
                },
                { "role": "user", "content": "u2" }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let assistant = payload
            .conversation_state
            .history
            .iter()
            .find_map(|item| match item {
                KiroMessage::Assistant(assistant) => Some(assistant),
                KiroMessage::User(_) => None,
            })
            .expect("assistant history should exist");

        assert!(
            assistant
                .assistant_response_message
                .content
                .contains("\"custom\":\"kept\"")
        );
    }

    #[test]
    fn chat_assistant_tool_calls_do_not_inject_placeholder() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "find weather" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{}"
                        }
                    }]
                },
                { "role": "user", "content": "continue" }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        for item in &payload.conversation_state.history {
            let KiroMessage::Assistant(assistant) = item else {
                continue;
            };
            let arm = &assistant.assistant_response_message;
            assert!(
                arm.tool_uses.as_ref().is_none_or(Vec::is_empty),
                "non-active history assistant must not retain tool_uses"
            );
            assert!(!arm.content.contains("get_weather"));
            assert!(!arm.content.contains("[Called tool"));
            assert_ne!(arm.content.trim(), ".");
            assert!(!arm.content.trim().is_empty());
        }
    }

    #[test]
    fn chat_structured_assistant_and_orphan_tool_result() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                {
                    "role": "system",
                    "content": [
                        { "type": "text", "text": "system-a" },
                        { "type": "text", "text": "system-b" }
                    ]
                },
                { "role": "user", "content": "first-question" },
                {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "assistant-structured" }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": [{ "type": "text", "text": "tool-result-structured" }]
                }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let history_text = payload
            .conversation_state
            .history
            .iter()
            .map(|item| match item {
                KiroMessage::User(user) => user.user_input_message.content.as_str(),
                KiroMessage::Assistant(assistant) => {
                    assistant.assistant_response_message.content.as_str()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(history_text.contains("system-a"));
        assert!(history_text.contains("system-b"));
        assert!(history_text.contains("first-question"));
        assert!(history_text.contains("assistant-structured"));

        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains("tool-result-structured"));
        assert!(current.user_input_message_context.tool_results.is_empty());
    }

    #[test]
    fn chat_history_tool_cycles_do_not_pollute_assistant() {
        let mut messages = vec![json!({
            "role": "user",
            "content": "start a multi-step task"
        })];
        for i in 0..4 {
            messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{i}"),
                    "type": "function",
                    "function": {
                        "name": "exec_command",
                        "arguments": format!("{{\"cmd\":\"step {i}\"}}")
                    }
                }]
            }));
            messages.push(json!({
                "role": "tool",
                "tool_call_id": format!("call_{i}"),
                "content": format!("OUTPUT_{i}")
            }));
            messages.push(json!({
                "role": "user",
                "content": format!("continue {i}")
            }));
        }
        messages.push(json!({
            "role": "user",
            "content": "summarize"
        }));

        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": messages
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        for item in &payload.conversation_state.history {
            if let KiroMessage::Assistant(assistant) = item {
                let content = &assistant.assistant_response_message.content;
                assert_ne!(
                    content.trim(),
                    "OK",
                    "converter must not synthesize trailing OK assistant turns"
                );
                assert!(!content.contains("[Called tool"));
                assert!(!content.contains("with input {"));
                assert!(
                    assistant
                        .assistant_response_message
                        .tool_uses
                        .as_ref()
                        .is_none_or(Vec::is_empty),
                    "non-active history assistant must not retain structured tool_uses"
                );
            }
        }

        let combined_user_text = payload
            .conversation_state
            .history
            .iter()
            .filter_map(|item| match item {
                KiroMessage::User(user) => Some(user.user_input_message.content.as_str()),
                KiroMessage::Assistant(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for i in 0..4 {
            assert!(combined_user_text.contains(&format!("OUTPUT_{i}")));
        }
        assert!(combined_user_text.contains("[exec_command]"));
    }

    #[test]
    fn chat_image_url_data_url_maps_to_base64_image() {
        let block = chat_content_part_to_block(&json!({
            "type": "image_url",
            "image_url": { "url": format!("data:image/jpeg;base64,{VALID_IMAGE_B64}") }
        }))
        .expect("data URL image should map");

        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["media_type"], "image/jpeg");
        assert_eq!(block["source"]["data"], VALID_IMAGE_B64);
    }

    #[test]
    fn chat_untyped_source_image_maps() {
        let block = chat_content_part_to_block(&json!({
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": VALID_IMAGE_B64
            }
        }))
        .expect("untyped source image should map");

        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(block["source"]["data"], VALID_IMAGE_B64);
    }

    #[test]
    fn responses_b64_json_maps_to_base64_image() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "input": {
                "type": "input_image",
                "b64_json": VALID_IMAGE_B64,
                "media_type": "image/png"
            }
        }))
        .expect("responses request should parse");
        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");
        let block = &messages_req.messages[0].content.as_array().unwrap()[0];

        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(block["source"]["data"], VALID_IMAGE_B64);
    }

    #[test]
    fn responses_nested_file_image_maps_to_base64_image() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "input": {
                "type": "input_file",
                "file": {
                    "mime_type": "image/webp",
                    "data": VALID_IMAGE_B64
                }
            }
        }))
        .expect("responses request should parse");
        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");
        let block = &messages_req.messages[0].content.as_array().unwrap()[0];

        assert_eq!(block["source"]["media_type"], "image/webp");
        assert_eq!(block["source"]["data"], VALID_IMAGE_B64);
    }

    #[test]
    fn remote_image_url_is_not_emitted_as_invalid_anthropic_image() {
        let block = chat_content_part_to_block(&json!({
            "type": "image_url",
            "image_url": { "url": "https://example.com/a.png" }
        }));

        assert!(block.is_none());
    }

    #[test]
    fn invalid_base64_image_is_skipped() {
        let block = chat_content_part_to_block(&json!({
            "type": "input_image",
            "b64_json": "not-base64!!",
            "media_type": "image/png"
        }));

        assert!(block.is_none());
    }

    #[test]
    fn image_placeholder_is_skipped() {
        let block = chat_content_part_to_block(&json!({
            "type": "image_url",
            "image_url": { "url": "[Image #1]" }
        }));

        assert!(block.is_none());
    }

    #[test]
    fn chat_unknown_text_part_is_preserved() {
        let block = chat_content_part_to_block(&json!({
            "type": "custom_text",
            "text": "keep this text"
        }))
        .expect("unknown text part should be preserved");

        assert_eq!(block, json!({"type": "text", "text": "keep this text"}));
    }

    #[test]
    fn responses_unknown_text_part_is_preserved() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "custom_text",
                    "text": "keep this text"
                }]
            }]
        }))
        .expect("responses request should parse");
        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");
        let block = &messages_req.messages[0].content.as_array().unwrap()[0];

        assert_eq!(*block, json!({"type": "text", "text": "keep this text"}));
    }

    #[test]
    fn chat_system_nested_content_text_is_preserved() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                {
                    "role": "system",
                    "content": {
                        "type": "message",
                        "content": [
                            {"type": "input_text", "text": "alpha"},
                            {"type": "custom_text", "text": "beta"}
                        ]
                    }
                },
                { "role": "user", "content": "hello" }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);

        assert_eq!(
            messages_req.system.unwrap()[0].text,
            "alphabeta".to_string()
        );
    }

    #[test]
    fn chat_user_text_parts_concatenate_without_newline() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "alpha" },
                        { "type": "input_text", "text": "beta" }
                    ]
                }
            ]
        }))
        .expect("chat request should parse");

        let payload = convert_request(
            &chat_completions_to_messages_request(&req),
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        assert_eq!(
            payload
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "alphabeta"
        );
    }

    #[test]
    fn chat_assistant_image_content_falls_back_to_json_text() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "u1" },
                {
                    "role": "assistant",
                    "content": [{
                        "type": "image_url",
                        "image_url": { "url": format!("data:image/png;base64,{VALID_IMAGE_B64}") }
                    }]
                },
                { "role": "user", "content": "u2" }
            ]
        }))
        .expect("chat request should parse");

        let payload = convert_request(
            &chat_completions_to_messages_request(&req),
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let assistant = payload
            .conversation_state
            .history
            .iter()
            .find_map(|item| match item {
                KiroMessage::Assistant(assistant) => Some(assistant),
                KiroMessage::User(_) => None,
            })
            .expect("assistant history should exist");

        assert!(
            assistant
                .assistant_response_message
                .content
                .contains("\"image_url\"")
        );
    }

    #[test]
    fn chat_tool_result_unknown_json_falls_back_to_text() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "run" },
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "inspect", "arguments": "{}" }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": [{ "payload": { "answer": 42 } }]
                }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let tool_result_turn = messages_req
            .messages
            .iter()
            .find(|msg| msg.role == "user" && msg.content.to_string().contains("tool_result"))
            .expect("tool result should be flushed");
        let tool_result = &tool_result_turn.content.as_array().unwrap()[0];

        assert_eq!(
            tool_result["content"],
            json!("{\"payload\":{\"answer\":42}}")
        );
    }

    #[test]
    fn chat_tool_result_followed_by_user_is_flushed() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "run it" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_1", "content": "UNIQUE_OUTPUT_MARKER_12345" },
                { "role": "user", "content": "now summarize" }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert_eq!(current.content, "now summarize");
        assert!(current.user_input_message_context.tool_results.is_empty());

        let mut marker_count = 0;
        for item in &payload.conversation_state.history {
            match item {
                KiroMessage::User(user) => {
                    marker_count += user
                        .user_input_message
                        .content
                        .matches("UNIQUE_OUTPUT_MARKER_12345")
                        .count();
                    assert!(
                        user.user_input_message
                            .user_input_message_context
                            .tool_results
                            .is_empty()
                    );
                }
                KiroMessage::Assistant(assistant) => {
                    marker_count += assistant
                        .assistant_response_message
                        .content
                        .matches("UNIQUE_OUTPUT_MARKER_12345")
                        .count();
                    assert!(
                        assistant
                            .assistant_response_message
                            .tool_uses
                            .as_ref()
                            .is_none_or(Vec::is_empty)
                    );
                }
            }
        }

        assert_eq!(marker_count, 1);
    }

    #[test]
    fn chat_consecutive_identical_tool_results_collapse() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "start" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_0",
                        "type": "function",
                        "function": { "name": "exec_command", "arguments": "{\"cmd\":\"x\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_0", "content": "SAME_ERROR_OUTPUT" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "exec_command", "arguments": "{\"cmd\":\"x\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_1", "content": "SAME_ERROR_OUTPUT" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_2",
                        "type": "function",
                        "function": { "name": "exec_command", "arguments": "{\"cmd\":\"x\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_2", "content": "SAME_ERROR_OUTPUT" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_3",
                        "type": "function",
                        "function": { "name": "exec_command", "arguments": "{\"cmd\":\"x\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_3", "content": "SAME_ERROR_OUTPUT" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_4",
                        "type": "function",
                        "function": { "name": "exec_command", "arguments": "{\"cmd\":\"x\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_4", "content": "SAME_ERROR_OUTPUT" },
                { "role": "user", "content": "final" }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let count = payload
            .conversation_state
            .history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    KiroMessage::User(user)
                        if user.user_input_message.content.contains("SAME_ERROR_OUTPUT")
                )
            })
            .count();

        assert_eq!(count, 1);
        assert_eq!(
            payload
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "final"
        );
    }

    #[test]
    fn chat_tool_result_image_followed_by_user_is_carried() {
        const DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "look at the file" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_img",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"a.png\"}" }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_img",
                    "content": [{
                        "type": "image_url",
                        "image_url": { "url": DATA_URL }
                    }]
                },
                { "role": "user", "content": "what do you see?" }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let history_image_count: usize = payload
            .conversation_state
            .history
            .iter()
            .filter_map(|item| match item {
                KiroMessage::User(user) => Some(user.user_input_message.images.len()),
                KiroMessage::Assistant(_) => None,
            })
            .sum();
        assert_eq!(history_image_count, 1);
        assert!(
            payload
                .conversation_state
                .history
                .iter()
                .any(|item| matches!(
                    item,
                    KiroMessage::User(user)
                        if user.user_input_message.content.contains("[Tool returned an image")
                ))
        );
        assert!(
            payload
                .conversation_state
                .current_message
                .user_input_message
                .images
                .is_empty()
        );
    }

    #[test]
    fn chat_tool_result_image_attaches_to_current_message() {
        const DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "look at the file" },
                {
                    "role": "tool",
                    "tool_call_id": "call_img",
                    "content": [{
                        "type": "image_url",
                        "image_url": { "url": DATA_URL }
                    }]
                }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert_eq!(current.images.len(), 1);
        assert_eq!(current.images[0].format, "png");
        assert!(current.user_input_message_context.tool_results.is_empty());
        assert_eq!(current.content, "Please analyze the attached image.");
    }

    #[test]
    fn chat_tool_results_continuation_includes_prefix() {
        let req: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "user", "content": "find data" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "fetch", "arguments": "{}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_1", "content": "result-1" }
            ]
        }))
        .expect("chat request should parse");

        let messages_req = chat_completions_to_messages_request(&req);
        let payload = convert_request(
            &messages_req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .expect("conversion should succeed");

        let content = &payload
            .conversation_state
            .current_message
            .user_input_message
            .content;
        assert!(content.contains("Tool results:"));
        assert!(content.contains("result-1"));
    }

    #[test]
    fn chat_conversation_id_stable_from_anchor() {
        let req_a: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "system", "content": "You are helpful" },
                { "role": "user", "content": "Build calculator" },
                { "role": "assistant", "content": "Sure" },
                { "role": "user", "content": "Continue" }
            ]
        }))
        .expect("chat request should parse");
        let req_b: ChatCompletionsRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                { "role": "system", "content": "You are helpful" },
                { "role": "user", "content": "Build calculator" },
                { "role": "assistant", "content": "Sure" },
                { "role": "user", "content": "Continue" },
                { "role": "assistant", "content": "Next step" }
            ]
        }))
        .expect("chat request should parse");

        let cfg = CompressionConfig::default();
        let pf = PromptFilterConfig::default();
        let id_a = convert_request(
            &chat_completions_to_messages_request(&req_a),
            &cfg,
            &pf,
            false,
        )
        .expect("conversion should succeed")
        .conversation_state
        .conversation_id;
        let id_b = convert_request(
            &chat_completions_to_messages_request(&req_b),
            &cfg,
            &pf,
            false,
        )
        .expect("conversion should succeed")
        .conversation_state
        .conversation_id;

        assert_eq!(id_a, id_b);
    }

    #[test]
    fn responses_parallel_function_calls_merge() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "run two commands" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_a",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"ls\"}"
                },
                {
                    "type": "function_call",
                    "call_id": "call_b",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"pwd\"}"
                },
                { "type": "function_call_output", "call_id": "call_a", "output": "file1" },
                { "type": "function_call_output", "call_id": "call_b", "output": "/home" }
            ]
        }))
        .expect("responses request should parse");

        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");
        let assistant_tool_turns: Vec<_> = messages_req
            .messages
            .iter()
            .filter(|msg| msg.role == "assistant")
            .collect();

        assert_eq!(assistant_tool_turns.len(), 1);
        let blocks = assistant_tool_turns[0]
            .content
            .as_array()
            .expect("assistant tool turn should be an array");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["id"], "call_a");
        assert_eq!(blocks[1]["id"], "call_b");
    }

    #[test]
    fn responses_input_parts_accumulate_and_output_text_is_assistant() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "input": [
                { "type": "input_text", "text": "alpha" },
                { "type": "input_text", "text": "beta" },
                { "type": "output_text", "text": "assistant done" }
            ]
        }))
        .expect("responses request should parse");

        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");

        assert_eq!(messages_req.messages.len(), 2);
        assert_eq!(messages_req.messages[0].role, "user");
        assert_eq!(messages_req.messages[1].role, "assistant");

        let user_blocks = messages_req.messages[0]
            .content
            .as_array()
            .expect("user input should be an array");
        assert_eq!(user_blocks.len(), 1);
        assert_eq!(user_blocks[0]["text"], "alphabeta");
        assert_eq!(
            messages_req.messages[1].content,
            json!([{ "type": "text", "text": "assistant done" }])
        );
    }

    #[test]
    fn responses_object_input_is_supported() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "input": {
                "type": "input_text",
                "text": "single object input"
            }
        }))
        .expect("responses request should parse");

        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");

        assert_eq!(messages_req.messages.len(), 1);
        assert_eq!(messages_req.messages[0].role, "user");
        let blocks = messages_req.messages[0]
            .content
            .as_array()
            .expect("loose input part should remain an array");
        assert_eq!(blocks[0]["text"], "single object input");
    }

    #[test]
    fn responses_message_text_only_array_collapses() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "first" },
                    { "type": "output_text", "text": "second" }
                ]
            }]
        }))
        .expect("responses request should parse");

        let parsed =
            parse_responses_input_messages(&req.input).expect("responses input should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].content, Some(json!("firstsecond")));

        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");

        assert_eq!(messages_req.messages.len(), 1);
        assert_eq!(messages_req.messages[0].role, "user");
        assert_eq!(
            messages_req.messages[0].content,
            json!([{ "type": "text", "text": "firstsecond" }])
        );
    }

    #[test]
    fn responses_function_call_uses_id_fallback() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "input": [
                { "type": "input_text", "text": "run command" },
                {
                    "type": "function_call",
                    "id": "fallback_call_id",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"pwd\"}"
                }
            ]
        }))
        .expect("responses request should parse");

        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");

        assert_eq!(messages_req.messages.len(), 2);
        assert_eq!(messages_req.messages[1].role, "assistant");
        let blocks = messages_req.messages[1]
            .content
            .as_array()
            .expect("function call should become tool_use block");
        assert_eq!(blocks[0]["id"], "fallback_call_id");
    }

    #[test]
    fn responses_missing_model_defaults() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "input": "hello"
        }))
        .expect("responses request should parse without model");

        assert_eq!(req.model, "claude-sonnet-4.5");
        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");
        assert_eq!(messages_req.model, "claude-sonnet-4.5");
    }

    #[test]
    fn responses_blank_model_defaults() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "   ",
            "input": "hello"
        }))
        .expect("responses request should parse with blank model");

        assert_eq!(req.model, "claude-sonnet-4.5");
        let messages_req = responses_to_messages_request(&req).expect("conversion should succeed");
        assert_eq!(messages_req.model, "claude-sonnet-4.5");
    }
}
