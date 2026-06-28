//! OpenAI 兼容协议类型
//!
//! 覆盖 Chat Completions（/v1/chat/completions）与 Responses（/v1/responses）所需的请求/响应结构。

use serde::{Deserialize, Serialize};

// ============================================================================
// 通用错误（OpenAI 风）
// ============================================================================

#[derive(Debug, Serialize)]
pub struct OpenAIErrorResponse {
    pub error: OpenAIErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct OpenAIErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl OpenAIErrorResponse {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: OpenAIErrorDetail {
                message: message.into(),
                error_type: error_type.into(),
                code: None,
            },
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.error.code = Some(code.into());
        self
    }
}

// ============================================================================
// Chat Completions 请求
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    pub max_tokens: Option<i32>,
    #[allow(dead_code)]
    pub temperature: Option<f32>,
    #[allow(dead_code)]
    pub top_p: Option<f32>,
    #[allow(dead_code)]
    pub stop: Option<serde_json::Value>,
    pub tools: Option<Vec<ChatTool>>,
    pub tool_choice: Option<serde_json::Value>,
    /// 仅做兼容字段（reasoning_effort 等），暂不影响行为
    #[serde(default)]
    #[allow(dead_code)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    /// 可为字符串、数组（多模态）或 null（仅 tool_calls 的 assistant）
    #[serde(default)]
    pub content: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ChatToolCallFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatToolCallFunction {
    pub name: String,
    /// OpenAI 规范是字符串（已序列化的 JSON）
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ChatTool {
    pub tool_type: String,
    pub function: Option<ChatToolDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
}

impl<'de> Deserialize<'de> for ChatTool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawTool {
            #[serde(rename = "type", default)]
            tool_type: String,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            parameters: Option<serde_json::Value>,
            #[serde(default)]
            function: Option<RawToolFunction>,
        }

        #[derive(Deserialize)]
        struct RawToolFunction {
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            parameters: Option<serde_json::Value>,
        }

        let raw = RawTool::deserialize(deserializer)?;
        let function = if raw.function.is_some()
            || raw.name.is_some()
            || raw.description.is_some()
            || raw.parameters.is_some()
        {
            let nested = raw.function;
            let nested_name = nested.as_ref().and_then(|f| f.name.clone());
            let nested_description = nested.as_ref().and_then(|f| f.description.clone());
            let nested_parameters = nested.and_then(|f| f.parameters);

            let name = match nested_name {
                Some(name) if !name.is_empty() => name,
                _ => raw.name.unwrap_or_default(),
            };
            let description = match nested_description {
                Some(description) if !description.is_empty() => Some(description),
                _ => raw.description,
            };
            let parameters = nested_parameters.or(raw.parameters);

            Some(ChatToolDef {
                name,
                description,
                parameters,
            })
        } else {
            None
        };

        Ok(Self {
            tool_type: raw.tool_type,
            function,
        })
    }
}

// ============================================================================
// Chat Completions 响应（非流式）
// ============================================================================

#[derive(Debug, Serialize)]
pub struct ChatCompletionsResponse {
    pub id: String,
    pub object: &'static str, // "chat.completion"
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: ChatUsage,
}

#[derive(Debug, Serialize)]
pub struct ChatChoice {
    pub index: i32,
    pub message: ChatChoiceMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatChoiceMessage {
    pub role: &'static str, // "assistant"
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
}

#[derive(Debug, Serialize, Clone, Copy)]
pub struct ChatUsage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
}

// ============================================================================
// Chat Completions 响应（流式 chunk）
// ============================================================================

#[derive(Debug, Serialize)]
pub struct ChatCompletionsChunk {
    pub id: String,
    pub object: &'static str, // "chat.completion.chunk"
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Serialize)]
pub struct ChatChunkChoice {
    pub index: i32,
    pub delta: ChatChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct ChatChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatChunkDeltaToolCall>>,
}

#[derive(Debug, Serialize)]
pub struct ChatChunkDeltaToolCall {
    pub index: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<&'static str>,
    pub function: ChatChunkDeltaFunction,
}

#[derive(Debug, Serialize, Default)]
pub struct ChatChunkDeltaFunction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

// ============================================================================
// Responses 请求 / 响应（OpenAI Responses API）
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesRequest {
    #[serde(
        default = "default_responses_model",
        deserialize_with = "deserialize_responses_model"
    )]
    pub model: String,
    /// 可为字符串、消息数组、或包含 input_text/input_image 的多模态结构
    pub input: serde_json::Value,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default = "default_stream_true")]
    pub stream: bool,
    #[serde(default)]
    pub max_output_tokens: Option<i32>,
    #[serde(default)]
    #[allow(dead_code)]
    pub temperature: Option<f32>,
    #[serde(default)]
    #[allow(dead_code)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
    /// 透传字段，本实现暂未做 stateful 会话恢复，仅在响应中回填便于客户端跟踪
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub store: Option<bool>,
    /// reasoning: { effort: "low|medium|high" }（仅做兼容透传）
    #[serde(default)]
    #[allow(dead_code)]
    pub reasoning: Option<serde_json::Value>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

fn default_stream_true() -> bool {
    false
}

fn default_responses_model() -> String {
    "claude-sonnet-4.5".to_string()
}

fn deserialize_responses_model<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let model = Option::<String>::deserialize(deserializer)?;
    Ok(match model {
        Some(model) if !model.trim().is_empty() => model,
        _ => default_responses_model(),
    })
}
