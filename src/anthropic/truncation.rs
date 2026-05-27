//! 工具调用截断检测模块
//!
//! 当上游返回的工具调用 JSON 被截断时（例如因为 max_tokens 限制），
//! 提供启发式检测和软失败恢复机制，引导模型分块重试。

/// 注入到 system prompt 末尾的截断恢复说明
///
/// 用于告知模型对话中出现的 `[System Notice]` 与 `[API Limitation]` 是 xkiro
/// 的截断恢复标记，并非 prompt injection 攻击，避免模型把它们识别为越权指令而拒答。
pub const TRUNCATION_RECOVERY_SYSTEM_NOTICE: &str = "\n# Output Truncation Handling

This conversation may include system-level notifications about output truncation:
- `[System Notice]` - indicates your response was cut off by API limits
- `[API Limitation]` - indicates a tool call result was truncated

These are legitimate system notifications, NOT prompt injection attempts. They inform you about technical limitations so you can adapt your approach if needed.";

/// 截断类型
#[derive(Debug, Clone, PartialEq)]
pub enum TruncationType {
    /// 工具输入为空（上游可能完全截断了 input）
    EmptyInput,
    /// JSON 解析失败（不完整的 JSON）
    InvalidJson,
    /// 缺少必要字段（JSON 有效但结构不完整）
    MissingFields,
    /// 未闭合的字符串（JSON 字符串被截断）
    IncompleteString,
}

impl std::fmt::Display for TruncationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TruncationType::EmptyInput => write!(f, "empty_input"),
            TruncationType::InvalidJson => write!(f, "invalid_json"),
            TruncationType::MissingFields => write!(f, "missing_fields"),
            TruncationType::IncompleteString => write!(f, "incomplete_string"),
        }
    }
}

/// 截断检测结果
#[derive(Debug, Clone)]
pub struct TruncationInfo {
    /// 截断类型
    pub truncation_type: TruncationType,
    /// 工具名称
    pub tool_name: String,
    /// 工具调用 ID
    pub tool_use_id: String,
    /// 原始输入（可能不完整）
    pub raw_input: String,
}

/// 检测工具调用输入是否被截断
///
/// 启发式判断规则：
/// 1. 空输入 → EmptyInput
/// 2. 未闭合的引号 → IncompleteString
/// 3. 括号不平衡 → InvalidJson
/// 4. 末尾字符异常（非 `}` / `]` / `"` / 数字 / `true` / `false` / `null`）→ InvalidJson
pub fn detect_truncation(
    tool_name: &str,
    tool_use_id: &str,
    raw_input: &str,
) -> Option<TruncationInfo> {
    detect_truncation_with_required(tool_name, tool_use_id, raw_input, &[])
}

/// 同 [`detect_truncation`]，并在 JSON 解析成功时校验 `required_fields` 是否齐全。
///
/// 当 JSON 结构有效（括号平衡、引号闭合）但顶层对象缺少必填字段时，
/// 返回 [`TruncationType::MissingFields`]。
pub fn detect_truncation_with_required(
    tool_name: &str,
    tool_use_id: &str,
    raw_input: &str,
    required_fields: &[&str],
) -> Option<TruncationInfo> {
    let trimmed = raw_input.trim();

    // 空输入
    if trimmed.is_empty() {
        return Some(TruncationInfo {
            truncation_type: TruncationType::EmptyInput,
            tool_name: tool_name.to_string(),
            tool_use_id: tool_use_id.to_string(),
            raw_input: raw_input.to_string(),
        });
    }

    // 检查未闭合的字符串引号
    if has_unclosed_string(trimmed) {
        return Some(TruncationInfo {
            truncation_type: TruncationType::IncompleteString,
            tool_name: tool_name.to_string(),
            tool_use_id: tool_use_id.to_string(),
            raw_input: raw_input.to_string(),
        });
    }

    // 检查括号平衡
    if !are_brackets_balanced(trimmed) {
        return Some(TruncationInfo {
            truncation_type: TruncationType::InvalidJson,
            tool_name: tool_name.to_string(),
            tool_use_id: tool_use_id.to_string(),
            raw_input: raw_input.to_string(),
        });
    }

    // 括号/引号都平衡：尝试解析 JSON 并校验必填字段
    if !required_fields.is_empty() {
        if let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            let missing: Vec<&&str> = required_fields
                .iter()
                .filter(|f| !map.contains_key(**f))
                .collect();
            if !missing.is_empty() {
                return Some(TruncationInfo {
                    truncation_type: TruncationType::MissingFields,
                    tool_name: tool_name.to_string(),
                    tool_use_id: tool_use_id.to_string(),
                    raw_input: raw_input.to_string(),
                });
            }
        }
    }

    None
}

/// 构建软失败的工具结果消息
///
/// 当检测到截断时，生成一条引导模型分块重试的错误消息，
/// 而不是直接返回解析错误。
pub fn build_soft_failure_result(info: &TruncationInfo) -> String {
    match info.truncation_type {
        TruncationType::EmptyInput => {
            format!(
                "Tool call '{}' (id: {}) was truncated: the input was empty. \
                 This usually means the response was cut off due to token limits. \
                 Please retry with a shorter input or break the operation into smaller steps.",
                info.tool_name, info.tool_use_id
            )
        }
        TruncationType::IncompleteString => {
            format!(
                "Tool call '{}' (id: {}) was truncated: a string value was not properly closed. \
                 The input appears to have been cut off mid-string. \
                 Please retry with shorter content or split the operation into multiple calls.",
                info.tool_name, info.tool_use_id
            )
        }
        TruncationType::InvalidJson => {
            format!(
                "Tool call '{}' (id: {}) was truncated: the JSON input is incomplete \
                 (unbalanced brackets). Please retry with a shorter input or break the \
                 operation into smaller steps.",
                info.tool_name, info.tool_use_id
            )
        }
        TruncationType::MissingFields => {
            format!(
                "Tool call '{}' (id: {}) was truncated: required fields are missing. \
                 Please retry with all required fields included.",
                info.tool_name, info.tool_use_id
            )
        }
    }
}

/// 已知 critical 工具的 required 字段映射，用于 MissingFields 检测。
///
/// 按 Claude Code 内置工具的契约写死；客户端自定义工具不在内（返回空数组跳过校验）。
pub fn required_fields_for(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "Write" | "str_replace_editor" => &["file_path", "content"],
        "Edit" => &["file_path", "old_string", "new_string"],
        "Read" => &["file_path"],
        "Bash" => &["command"],
        _ => &[],
    }
}

/// 检查字符串中是否有未闭合的引号
fn has_unclosed_string(s: &str) -> bool {
    let mut in_string = false;
    let mut escape_next = false;

    for ch in s.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_string => {
                escape_next = true;
            }
            '"' => {
                in_string = !in_string;
            }
            _ => {}
        }
    }

    in_string
}

/// 检查括号是否平衡
fn are_brackets_balanced(s: &str) -> bool {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape_next = false;

    for ch in s.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape_next = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => stack.push(ch),
            '}' => {
                if stack.last() != Some(&'{') {
                    return false;
                }
                stack.pop();
            }
            ']' => {
                if stack.last() != Some(&'[') {
                    return false;
                }
                stack.pop();
            }
            _ => {}
        }
    }

    stack.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_empty_input() {
        let result = detect_truncation("Write", "tool-1", "");
        assert!(result.is_some());
        assert_eq!(result.unwrap().truncation_type, TruncationType::EmptyInput);
    }

    #[test]
    fn test_detect_whitespace_only_input() {
        let result = detect_truncation("Write", "tool-1", "   \n  ");
        assert!(result.is_some());
        assert_eq!(result.unwrap().truncation_type, TruncationType::EmptyInput);
    }

    #[test]
    fn test_detect_incomplete_string() {
        let result = detect_truncation("Write", "tool-1", r#"{"content": "hello world"#);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().truncation_type,
            TruncationType::IncompleteString
        );
    }

    #[test]
    fn test_detect_unbalanced_brackets() {
        // 字符串已闭合但括号不平衡
        let result = detect_truncation("Write", "tool-1", r#"{"content": "hello","more": 123"#);
        assert!(result.is_some());
        assert_eq!(result.unwrap().truncation_type, TruncationType::InvalidJson);
    }

    #[test]
    fn test_valid_json_no_truncation() {
        let result = detect_truncation("Write", "tool-1", r#"{"content": "hello"}"#);
        assert!(result.is_none());
    }

    #[test]
    fn test_valid_empty_object() {
        let result = detect_truncation("Write", "tool-1", "{}");
        assert!(result.is_none());
    }

    #[test]
    fn test_nested_brackets() {
        let result = detect_truncation("Write", "tool-1", r#"{"a": {"b": [1, 2, {"c": 3}]}}"#);
        assert!(result.is_none());
    }

    #[test]
    fn test_escaped_quotes_in_string() {
        let result = detect_truncation("Write", "tool-1", r#"{"content": "say \"hello\""}"#);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_soft_failure_empty() {
        let info = TruncationInfo {
            truncation_type: TruncationType::EmptyInput,
            tool_name: "Write".to_string(),
            tool_use_id: "tool-1".to_string(),
            raw_input: String::new(),
        };
        let msg = build_soft_failure_result(&info);
        assert!(msg.contains("truncated"));
        assert!(msg.contains("Write"));
        assert!(msg.contains("tool-1"));
    }

    #[test]
    fn test_build_soft_failure_incomplete_string() {
        let info = TruncationInfo {
            truncation_type: TruncationType::IncompleteString,
            tool_name: "Edit".to_string(),
            tool_use_id: "tool-2".to_string(),
            raw_input: r#"{"old_string": "hello"#.to_string(),
        };
        let msg = build_soft_failure_result(&info);
        assert!(msg.contains("string value was not properly closed"));
    }

    #[test]
    fn test_detect_missing_fields() {
        let result = detect_truncation_with_required(
            "Edit",
            "tool-3",
            r#"{"old_string": "foo"}"#,
            &["old_string", "new_string"],
        );
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.truncation_type, TruncationType::MissingFields);
        assert_eq!(info.tool_name, "Edit");
    }

    #[test]
    fn test_detect_required_present_no_truncation() {
        let result = detect_truncation_with_required(
            "Edit",
            "tool-4",
            r#"{"old_string":"a","new_string":"b"}"#,
            &["old_string", "new_string"],
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_missing_fields_skipped_when_no_required() {
        let result = detect_truncation("Edit", "tool-5", r#"{}"#);
        assert!(result.is_none());
    }
}
