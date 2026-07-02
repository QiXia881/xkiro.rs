//! 工具使用事件
//!
//! 处理 toolUseEvent 类型的事件
//! 接受 toolUseEvent 的多种上游字段命名：
//! - toolUseId / toolUseID / tool_use_id / id 字段别名
//! - name / toolName / tool_name 字段别名
//! - stop / isStop / done 字段别名
//! - input 支持 string 和 JSON object 两种类型

use serde::Deserialize;
use serde_json::Value;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 工具使用事件
///
/// 包含工具调用的流式数据。
/// 使用自定义反序列化识别 Kiro 上游的不同字段命名约定。
#[derive(Debug, Clone)]
pub struct ToolUseEvent {
    /// 工具名称
    pub name: String,
    /// 工具调用 ID（如果上游未提供，由调用方生成 fallback ID）
    pub tool_use_id: String,
    /// 工具输入数据 (JSON 字符串，可能是流式的部分数据)
    pub input: String,
    pub input_is_json_object: bool,
    /// 是否是最后一个块
    pub stop: bool,
}

/// 按优先级尝试多个字段名，跳过空字符串。
fn first_string_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(Value::String(s)) = obj.get(*key) {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

/// 按优先级尝试多个布尔字段名。
fn first_bool_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    for key in keys {
        if let Some(Value::Bool(b)) = obj.get(*key) {
            if *b {
                return true;
            }
        }
    }
    false
}

impl<'de> Deserialize<'de> for ToolUseEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // 先反序列化为原始 JSON Value，手动实现“跳过空主字段继续尝试别名”的语义。
        let raw_value: serde_json::Value = serde_json::Value::deserialize(deserializer)?;
        let obj = match raw_value.as_object() {
            Some(o) => o,
            None => {
                return Ok(ToolUseEvent {
                    name: "unknown".to_string(),
                    tool_use_id: String::new(),
                    input: String::new(),
                    input_is_json_object: false,
                    stop: false,
                });
            }
        };

        // 按优先级尝试多个字段名，跳过空字符串。
        let tool_use_id = first_string_field(obj, &["toolUseId", "toolUseID", "tool_use_id", "id"])
            .unwrap_or_default();
        let name = first_string_field(obj, &["name", "toolName", "tool_name"])
            .unwrap_or_else(|| "unknown".to_string());

        // input: 可能是 string 或 JSON object
        let (input, input_is_json_object) = match obj.get("input") {
            Some(serde_json::Value::String(s)) => (s.clone(), false),
            Some(value @ serde_json::Value::Object(_)) => {
                (serde_json::to_string(value).unwrap_or_default(), true)
            }
            Some(other) => (serde_json::to_string(other).unwrap_or_default(), false),
            None => (String::new(), false),
        };

        // stop: 按优先级尝试多个布尔字段名。
        let stop = first_bool_field(obj, &["stop", "isStop", "done"]);

        Ok(ToolUseEvent {
            name,
            tool_use_id,
            input,
            input_is_json_object,
            stop,
        })
    }
}

impl EventPayload for ToolUseEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl ToolUseEvent {
    /// ID 是否为空（需要由调用方生成 fallback ID）
    pub fn needs_generated_id(&self) -> bool {
        self.tool_use_id.is_empty()
    }

    /// 生成 fallback ID（toolu_ + UUID）
    pub fn generate_fallback_id() -> String {
        format!("toolu_{}", uuid::Uuid::new_v4())
    }

    pub fn apply_input_to_buffer(&self, buffer: &mut String) {
        if self.input.is_empty() {
            return;
        }
        if self.input_is_json_object {
            buffer.clear();
        }
        buffer.push_str(&self.input);
    }
}

impl std::fmt::Display for ToolUseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.stop {
            write!(
                f,
                "ToolUse[{}] (id={}, complete): {}",
                self.name, self.tool_use_id, self.input
            )
        } else {
            write!(
                f,
                "ToolUse[{}] (id={}, partial): {}",
                self.name, self.tool_use_id, self.input
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_standard_camel_case() {
        let json = r#"{"name":"read_file","toolUseId":"toolu_123","input":"{}","stop":true}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.name, "read_file");
        assert_eq!(event.tool_use_id, "toolu_123");
        assert_eq!(event.input, "{}");
        assert!(event.stop);
    }

    #[test]
    fn test_deserialize_alternative_id_field() {
        let json = r#"{"name":"write","id":"toolu_456","input":"{}"}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.tool_use_id, "toolu_456");
    }

    #[test]
    fn test_deserialize_tool_use_id_alias() {
        let json = r#"{"name":"write","tool_use_id":"toolu_789","input":"{}"}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.tool_use_id, "toolu_789");
    }

    #[test]
    fn test_deserialize_missing_id() {
        let json = r#"{"name":"write","input":"{}"}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.tool_use_id, "");
        assert!(event.needs_generated_id());
    }

    #[test]
    fn test_deserialize_alternative_name_field() {
        let json = r#"{"toolName":"read_file","toolUseId":"toolu_123"}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.name, "read_file");
    }

    #[test]
    fn test_deserialize_tool_name_alias() {
        let json = r#"{"tool_name":"read_file","toolUseId":"toolu_123"}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.name, "read_file");
    }

    #[test]
    fn test_deserialize_stop_is_stop() {
        let json = r#"{"name":"x","toolUseId":"1","isStop":true}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert!(event.stop);
    }

    #[test]
    fn test_deserialize_stop_done() {
        let json = r#"{"name":"x","toolUseId":"1","done":true}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert!(event.stop);
    }

    #[test]
    fn test_deserialize_input_json_object() {
        let json = r#"{"name":"write","toolUseId":"1","input":{"file_path":"/tmp/test","content":"hello"}}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert!(event.input.contains("file_path"));
        assert!(event.input.contains("/tmp/test"));
        assert!(event.input_is_json_object);
    }

    #[test]
    fn test_deserialize_input_string_is_append_chunk() {
        let json = r#"{"name":"write","toolUseId":"1","input":"{\"file_path\":"}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.input, "{\"file_path\":");
        assert!(!event.input_is_json_object);
    }

    #[test]
    fn test_deserialize_input_missing() {
        let json = r#"{"name":"write","toolUseId":"1"}"#;
        let event: ToolUseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.input, "");
    }

    #[test]
    fn test_generate_fallback_id() {
        let id = ToolUseEvent::generate_fallback_id();
        assert!(id.starts_with("toolu_"));
        assert!(id.len() > 10);
    }
}
