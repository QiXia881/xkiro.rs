//! Anthropic API 类型定义

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// === 缓存控制 ===

/// 缓存控制配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<serde_json::Value>,
}

// === 错误响应 ===

/// API 错误响应
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

/// 错误详情
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl ErrorResponse {
    /// 创建新的错误响应
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                error_type: error_type.into(),
                message: message.into(),
            },
        }
    }

    /// 创建认证错误响应
    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid or missing API key")
    }

    /// 创建速率限制错误响应
    pub fn rate_limit_error(message: impl Into<String>) -> Self {
        Self::new("rate_limit_error", message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_error_message_matches_claude_route() {
        let response = ErrorResponse::authentication_error();

        assert_eq!(response.error.error_type, "authentication_error");
        assert_eq!(response.error.message, "Invalid or missing API key");
    }

    #[test]
    fn tool_input_schema_accepts_non_object() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "bad_schema_tool",
                "description": "desc",
                "input_schema": "not an object"
            }]
        }))
        .expect("non-object input_schema should parse");

        let tools = req.tools.expect("tools should be present");
        assert!(tools[0].input_schema.is_empty());
    }

    #[test]
    fn count_tokens_request_accepts_missing_model_and_messages() {
        let req: CountTokensRequest = serde_json::from_value(serde_json::json!({}))
            .expect("count_tokens should accept ClaudeRequest zero-value shape");

        assert_eq!(req.model, "");
        assert_eq!(req.max_tokens, 0);
        assert!(req.messages.is_empty());
    }

    #[test]
    fn messages_request_accepts_missing_fields_as_zero_values() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({}))
            .expect("messages should accept ClaudeRequest zero-value shape");

        assert_eq!(req.model, "");
        assert_eq!(req.max_tokens, 0);
        assert!(req.messages.is_empty());
    }

    #[test]
    fn messages_request_accepts_null_scalars_as_zero_values() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": null,
            "max_tokens": null,
            "messages": [
                { "role": null, "content": null }
            ],
            "thinking": {
                "type": null
            },
            "tools": [{
                "name": null,
                "description": null,
                "input_schema": null
            }]
        }))
        .expect("messages should accept null zero-value fields");

        assert_eq!(req.model, "");
        assert_eq!(req.max_tokens, 0);
        assert_eq!(req.messages[0].role, "");
        assert!(req.messages[0].content.is_null());
        assert_eq!(req.thinking.as_ref().unwrap().thinking_type, "");
        let tool = &req.tools.as_ref().unwrap()[0];
        assert_eq!(tool.name, "");
        assert_eq!(tool.description, "");
        assert!(tool.input_schema.is_empty());
    }

    #[test]
    fn count_tokens_request_accepts_null_model_and_messages() {
        let req: CountTokensRequest = serde_json::from_value(serde_json::json!({
            "model": null,
            "max_tokens": null,
            "messages": null
        }))
        .expect("count_tokens should accept null zero-value shape");

        assert_eq!(req.model, "");
        assert_eq!(req.max_tokens, 0);
        assert!(req.messages.is_empty());
    }

    #[test]
    fn thinking_helpers_trim_and_fold_case() {
        let thinking = Thinking {
            thinking_type: " Adaptive ".to_string(),
            budget_tokens: None,
            display: Some(" omitted ".to_string()),
        };

        assert_eq!(thinking.normalized_type(), "adaptive");
        assert!(thinking.is_enabled());
        assert_eq!(thinking.effective_display(), "omitted");

        let empty_display = Thinking {
            thinking_type: "enabled".to_string(),
            budget_tokens: Some(1024),
            display: Some("  ".to_string()),
        };
        assert_eq!(empty_display.effective_display(), "summarized");
    }
}

// === Models 端点类型 ===

/// 模型信息
#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub id: String,
    pub object: String,
    pub owned_by: String,
    pub supports_image: bool,
    pub input_modalities: Vec<String>,
    pub modalities: ModelModalities,
    pub capabilities: ModelCapabilities,
    pub info: ModelInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelModalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCapabilities {
    pub vision: bool,
    pub image: bool,
    pub image_vision: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub meta: ModelInfoMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfoMeta {
    pub capabilities: ModelInfoCapabilities,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfoCapabilities {
    pub vision: bool,
    pub image_vision: bool,
}

/// 模型列表响应
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<Model>,
}

// === Messages 端点类型 ===

/// Thinking 配置
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Thinking {
    #[serde(
        rename = "type",
        default,
        deserialize_with = "deserialize_nullable_string"
    )]
    pub thinking_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<i32>,
    /// `summarized` / `omitted`，未提供时由后端按模型默认填入。
    #[serde(
        default,
        deserialize_with = "deserialize_display",
        skip_serializing_if = "Option::is_none"
    )]
    pub display: Option<String>,
}

impl Thinking {
    pub fn normalized_type(&self) -> String {
        self.thinking_type.trim().to_lowercase()
    }

    /// 是否启用了 thinking（enabled 或 adaptive）
    pub fn is_enabled(&self) -> bool {
        matches!(self.normalized_type().as_str(), "enabled" | "adaptive")
    }

    /// 有效 display 值（None 时回退 "summarized"，确保 Kiro 能吐 thinking 文本）
    pub fn effective_display(&self) -> &str {
        self.display
            .as_deref()
            .map(str::trim)
            .filter(|display| !display.is_empty())
            .unwrap_or("summarized")
    }
}

/// 保留客户端原始 `display` 值，边界校验层负责返回 400。
fn deserialize_display<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

/// OutputConfig 配置
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OutputConfig {
    #[serde(default = "default_effort")]
    pub effort: String,
}

fn default_effort() -> String {
    "high".to_string()
}

/// Claude Code 请求中的 metadata
#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    /// 用户 ID，格式如: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
    pub user_id: Option<String>,
    #[serde(skip)]
    pub preserve_tool_names: bool,
}

/// Messages 请求体
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct MessagesRequest {
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub model: String,
    #[serde(default, deserialize_with = "deserialize_nullable_i32")]
    pub max_tokens: i32,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default, deserialize_with = "deserialize_nullable_vec")]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, deserialize_with = "deserialize_system")]
    pub system: Option<Vec<SystemMessage>>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<serde_json::Value>,
    pub thinking: Option<Thinking>,
    pub output_config: Option<OutputConfig>,
    /// Claude Code 请求中的 metadata，包含 session 信息
    pub metadata: Option<Metadata>,
}

/// 反序列化 system 字段，支持字符串或数组格式
fn deserialize_system<'de, D>(deserializer: D) -> Result<Option<Vec<SystemMessage>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // 创建一个 visitor 来处理 string 或 array
    struct SystemVisitor;

    impl<'de> serde::de::Visitor<'de> for SystemVisitor {
        type Value = Option<Vec<SystemMessage>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string or an array of system messages")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(vec![SystemMessage {
                text: value.to_string(),
                block_type: None,
                cache_control: None,
            }]))
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut messages = Vec::new();
            while let Some(msg) = seq.next_element()? {
                messages.push(msg);
            }
            Ok(if messages.is_empty() {
                None
            } else {
                Some(messages)
            })
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            serde::de::Deserialize::deserialize(deserializer)
        }
    }

    deserializer.deserialize_any(SystemVisitor)
}

/// 消息
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub role: String,
    /// 可以是 string 或 ContentBlock 数组
    #[serde(default)]
    pub content: serde_json::Value,
}

/// 系统消息
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemMessage {
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub text: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub block_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// 工具定义
///
/// 支持两种格式：
/// 1. 普通工具：{ name, description, input_schema }
/// 2. WebSearch 工具：{ type: "web_search_20250305", name: "web_search", max_uses: 8 }
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    /// 工具类型，如 "web_search_20250305"（可选，仅 WebSearch 工具）
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<String>,
    /// 工具名称
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub name: String,
    /// 工具描述（普通工具必需，WebSearch 工具可选）
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub description: String,
    /// 输入参数 schema（普通工具必需，WebSearch 工具无此字段）
    #[serde(default, deserialize_with = "deserialize_input_schema")]
    pub input_schema: HashMap<String, serde_json::Value>,
    /// 最大使用次数（仅 WebSearch 工具）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<i32>,
    /// 缓存控制
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

fn deserialize_input_schema<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::Object(obj)) = value else {
        return Ok(HashMap::new());
    };
    Ok(obj.into_iter().collect())
}

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

fn deserialize_nullable_i32<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<i32>::deserialize(deserializer)?.unwrap_or_default())
}

fn deserialize_nullable_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

impl Tool {
    /// 检查是否为 WebSearch 工具
    #[allow(dead_code)]
    pub fn is_web_search(&self) -> bool {
        self.tool_type
            .as_ref()
            .is_some_and(|t| t.starts_with("web_search"))
    }
}

/// 内容块
#[derive(Debug, Deserialize, Serialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ImageSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// 图片数据源
#[derive(Debug, Deserialize, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

// === Count Tokens 端点类型 ===

/// Token 计数请求
#[derive(Debug, Serialize, Deserialize)]
pub struct CountTokensRequest {
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub model: String,
    #[serde(default, deserialize_with = "deserialize_nullable_i32")]
    pub max_tokens: i32,
    #[serde(default, deserialize_with = "deserialize_nullable_vec")]
    pub messages: Vec<Message>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_system"
    )]
    pub system: Option<Vec<SystemMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
}

/// Token 计数响应
#[derive(Debug, Serialize, Deserialize)]
pub struct CountTokensResponse {
    pub input_tokens: i32,
}
