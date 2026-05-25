//! OpenAI → Anthropic MessagesRequest 转换
//!
//! 将 OpenAI Chat Completions / Responses 请求归一化为 [`MessagesRequest`]，
//! 之后由上层走 `anthropic::converter::convert_request` → Kiro。
//!
//! 字段对齐参考 KAM `gateway/converter.rs`，并贴合 OpenAI 官方协议字段名。

use serde_json::{Value, json};

use crate::anthropic::types::{Message, MessagesRequest, SystemMessage, Tool};

use super::types::{
    ChatCompletionsRequest, ChatMessage, ChatTool, ChatToolCall, ResponsesRequest,
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
                // 由下一条 user 消息合并发出（KAM 同款策略）。
                let tool_call_id = msg.tool_call_id.clone().unwrap_or_default();
                let result_text = extract_message_text(msg.content.as_ref());
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": result_text,
                }));
            }
            "user" => {
                // 把任何挂起的 tool_results 合并到本条 user
                let mut blocks = std::mem::take(&mut pending_tool_results);
                blocks.extend(content_to_anthropic_blocks(msg.content.as_ref()));
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

                let mut blocks = content_to_anthropic_blocks(msg.content.as_ref());
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
        metadata: None,
    }
}

// ============================================================================
// Responses
// ============================================================================

pub fn responses_to_messages_request(req: &ResponsesRequest) -> Result<MessagesRequest, String> {
    let mut system_blocks: Vec<SystemMessage> = Vec::new();
    if let Some(instructions) = &req.instructions {
        if !instructions.is_empty() {
            system_blocks.push(SystemMessage {
                text: instructions.clone(),
                block_type: None,
                cache_control: None,
            });
        }
    }

    let mut messages: Vec<Message> = Vec::new();

    match &req.input {
        Value::String(text) => {
            messages.push(Message {
                role: "user".to_string(),
                content: Value::String(text.clone()),
            });
        }
        Value::Array(items) => {
            convert_responses_input_items(items, &mut messages);
        }
        Value::Null => {}
        other => {
            return Err(format!(
                "Responses 请求 input 字段格式不支持: {}",
                other_type_name(other)
            ));
        }
    }

    if messages.is_empty() {
        return Err("Responses 请求缺少可转换的 input".to_string());
    }

    let tools = req
        .tools
        .as_ref()
        .map(|tools| tools.iter().flat_map(responses_tool_to_anthropic).collect());

    Ok(MessagesRequest {
        model: req.model.clone(),
        max_tokens: req.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
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
        metadata: None,
    })
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

fn convert_responses_input_items(items: &[Value], messages: &mut Vec<Message>) {
    let mut pending_tool_results: Vec<Value> = Vec::new();

    let flush_tool_results = |pending: &mut Vec<Value>, messages: &mut Vec<Message>| {
        if pending.is_empty() {
            return;
        }
        let blocks = std::mem::take(pending);
        messages.push(Message {
            role: "user".to_string(),
            content: Value::Array(blocks),
        });
    };

    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");

        // 形如 {role, content}（与 OpenAI Responses message item 风格一致）
        if let Some(role) = item.get("role").and_then(Value::as_str) {
            flush_tool_results(&mut pending_tool_results, messages);
            let blocks = responses_message_blocks(item.get("content"));
            messages.push(Message {
                role: role.to_string(),
                content: Value::Array(blocks),
            });
            continue;
        }

        match item_type {
            "message" => {
                flush_tool_results(&mut pending_tool_results, messages);
                let role = item
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_string();
                let blocks = responses_message_blocks(item.get("content"));
                messages.push(Message {
                    role,
                    content: Value::Array(blocks),
                });
            }
            "function_call" => {
                flush_tool_results(&mut pending_tool_results, messages);
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = match item.get("arguments") {
                    Some(Value::String(s)) => parse_json_loose(s),
                    Some(other) => other.clone(),
                    None => json!({}),
                };
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: Value::Array(vec![json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": arguments,
                    })]),
                });
            }
            "function_call_output" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let output_text = match item.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": output_text,
                }));
            }
            // input_text / input_image 等顶层裸节点 → 累积成一条 user 消息
            _ => {
                let blocks = responses_message_blocks(Some(item));
                if !blocks.is_empty() {
                    flush_tool_results(&mut pending_tool_results, messages);
                    messages.push(Message {
                        role: "user".to_string(),
                        content: Value::Array(blocks),
                    });
                }
            }
        }
    }

    flush_tool_results(&mut pending_tool_results, messages);
}

fn responses_message_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => vec![json!({ "type": "text", "text": text })],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(responses_content_part_to_block)
            .collect(),
        Some(Value::Object(_)) => content
            .and_then(responses_content_part_to_block)
            .map(|b| vec![b])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn responses_content_part_to_block(item: &Value) -> Option<Value> {
    let part_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match part_type {
        "input_text" | "output_text" | "text" => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(json!({ "type": "text", "text": text }))
            }
        }
        "input_image" => {
            // OpenAI Responses image 形如 { type:"input_image", image_url:"data:image/...;base64,..." | "https://..." }
            let url = item.get("image_url").and_then(Value::as_str)?;
            if let Some(data_url) = parse_data_url(url) {
                Some(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": data_url.media_type,
                        "data": data_url.data,
                    }
                }))
            } else {
                Some(json!({
                    "type": "image",
                    "source": { "type": "url", "url": url }
                }))
            }
        }
        _ => None,
    }
}

struct DataUrl {
    media_type: String,
    data: String,
}

fn parse_data_url(url: &str) -> Option<DataUrl> {
    let stripped = url.strip_prefix("data:")?;
    let (meta, data) = stripped.split_once(',')?;
    let mut media_type = "image/png";
    let mut is_base64 = false;
    for part in meta.split(';') {
        if part == "base64" {
            is_base64 = true;
        } else if !part.is_empty() {
            media_type = part;
        }
    }
    if !is_base64 {
        return None;
    }
    Some(DataUrl {
        media_type: media_type.to_string(),
        data: data.to_string(),
    })
}

// ============================================================================
// 通用辅助
// ============================================================================

fn extract_message_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut buf = String::new();
            for item in items {
                let t = item.get("type").and_then(Value::as_str).unwrap_or("");
                if t == "text" || t == "input_text" || t == "output_text" {
                    if let Some(s) = item.get("text").and_then(Value::as_str) {
                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(s);
                    }
                }
            }
            buf
        }
        _ => String::new(),
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
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(chat_content_part_to_block)
            .collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![json!({ "type": "text", "text": other.to_string() })],
    }
}

fn chat_content_part_to_block(item: &Value) -> Option<Value> {
    let part_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match part_type {
        "text" | "input_text" | "output_text" => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(json!({ "type": "text", "text": text }))
            }
        }
        "image_url" => {
            // OpenAI Chat 多模态: { type:"image_url", image_url: { url: "data:..." | "https://..." } }
            let url = item
                .get("image_url")
                .and_then(|v| v.get("url"))
                .and_then(Value::as_str)
                .or_else(|| item.get("image_url").and_then(Value::as_str))?;
            if let Some(data_url) = parse_data_url(url) {
                Some(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": data_url.media_type,
                        "data": data_url.data,
                    }
                }))
            } else {
                Some(json!({
                    "type": "image",
                    "source": { "type": "url", "url": url }
                }))
            }
        }
        _ => None,
    }
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
    serde_json::from_str(s).unwrap_or_else(|_| json!({}))
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

    if t.tool_type != "function" && !t.tool_type.is_empty() {
        tracing::warn!(
            tool_type = %t.tool_type,
            "OpenAI Chat Completions 请求包含未支持的 hosted tool，已忽略"
        );
        return None;
    }

    let func = t.function.as_ref()?;
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

    // 字段可能位于顶层（Responses）或 function 子对象（Chat 风格）
    let Some(name) = value
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| value.get("function").and_then(|f| f.get("name")).and_then(Value::as_str))
    else {
        return Vec::new();
    };
    let name = name.to_string();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("function")
                .and_then(|f| f.get("description"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_string();
    let parameters = value
        .get("parameters")
        .cloned()
        .or_else(|| value.get("function").and_then(|f| f.get("parameters")).cloned());
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

    use super::{chat_tool_to_anthropic_tool, responses_tool_to_anthropic};
    use crate::openai::types::ChatTool;

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
    fn chat_browser_tool_maps_to_hosted_web_search() {
        let tool = chat_tool_to_anthropic_tool(&ChatTool {
            tool_type: "browser".to_string(),
            function: None,
        })
        .expect("browser alias should be preserved");

        assert_eq!(tool.name, "web_search");
        assert_eq!(tool.tool_type.as_deref(), Some("web_search"));
    }
}
