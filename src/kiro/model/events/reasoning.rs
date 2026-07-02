//! 推理内容事件
//!
//! 处理 reasoningContentEvent 类型的事件
//! Kiro 上游在 thinking 模式下会发送此事件类型，携带推理/思考内容。

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 推理内容事件
///
/// 当 Kiro 上游启用 thinking 模式时，推理内容通过此事件类型独立发送，
/// 而非嵌入在 assistantResponseEvent 的 `<thinking>` 标签中。
///
/// # 示例
///
/// ```json
/// {"text": "Let me think about this...", "contentType": "REASONING"}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningContentEvent {
    /// 推理/思考文本内容
    #[serde(default)]
    pub text: String,

    /// 捕获其他未使用的字段，确保反序列化容错
    #[serde(flatten)]
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    extra: serde_json::Value,
}

impl EventPayload for ReasoningContentEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl Default for ReasoningContentEvent {
    fn default() -> Self {
        Self {
            text: String::new(),
            extra: serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_simple() {
        let json = r#"{"text":"Let me reason about this..."}"#;
        let event: ReasoningContentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.text, "Let me reason about this...");
    }

    #[test]
    fn test_deserialize_with_extra_fields() {
        let json = r#"{
            "text": "Thinking content",
            "contentType": "REASONING",
            "someOtherField": 42
        }"#;
        let event: ReasoningContentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.text, "Thinking content");
    }

    #[test]
    fn test_deserialize_empty() {
        let json = r#"{}"#;
        let event: ReasoningContentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.text, "");
    }
}
