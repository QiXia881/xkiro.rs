//! Bracket 风格工具调用回退解析器
//!
//! 部分模型在响应文本里以 `[Called name with args: {...}]` 形式输出工具调用，
//! 而非走结构化 toolUseEvent 通道。本模块在主解析路径之后兜底扫描，
//! 把这种内嵌格式抽出转成标准 tool_use 块，避免工具调用被丢失。
//!
//! 行为参考自 jwadow/kiro-gateway `kiro/parsers.py::parse_bracket_tool_calls`。
//!
//! 输出文本会被原样保留，由调用方决定是否清理。
//!
//! 仅识别完整闭合的 JSON 参数；解析失败的片段记日志后跳过。

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// 抽出的 bracket 形式工具调用
#[derive(Debug, Clone)]
pub struct BracketToolCall {
    pub name: String,
    pub input: Value,
    /// 在原文本中的字节范围 `[start, end)`，包含起始 `[` 与末尾 `]`
    pub span: (usize, usize),
}

fn header_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\[Called\s+([A-Za-z_][\w\-]*)\s+with\s+args:\s*").unwrap())
}

/// 从 `start_pos`（必须指向 `{`）开始定位匹配的 `}`，处理字符串/转义/嵌套。
/// 返回闭合 `}` 的字节位置；找不到返回 None。
fn find_matching_brace(text: &str, start_pos: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if start_pos >= bytes.len() || bytes[start_pos] != b'{' {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut i = start_pos;
    while i < bytes.len() {
        let c = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if in_string {
            match c {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// 扫描文本，抽出所有 `[Called name with args: {...}]` 形式的工具调用。
/// 末尾的 `]` 必须紧跟在 JSON 结束之后（中间允许空白）；否则忽略此匹配，
/// 避免把普通文档中的方括号当成工具调用。
pub fn parse_bracket_tool_calls(text: &str) -> Vec<BracketToolCall> {
    if !text.contains("[Called") && !text.contains("[called") && !text.contains("[CALLED") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for m in header_regex().captures_iter(text) {
        let full = m.get(0).expect("regex group 0 always present");
        let name = m.get(1).expect("name group always present").as_str().to_string();
        let args_start = full.end();
        let json_start = match text[args_start..].find('{') {
            Some(rel) => args_start + rel,
            None => continue,
        };
        let json_end = match find_matching_brace(text, json_start) {
            Some(p) => p,
            None => continue,
        };
        let after_json = &text[json_end + 1..];
        let trimmed = after_json.trim_start();
        if !trimmed.starts_with(']') {
            continue;
        }
        let close_offset = (after_json.len() - trimmed.len()) + 1;
        let span_end = json_end + 1 + close_offset;
        let json_str = &text[json_start..=json_end];
        match serde_json::from_str::<Value>(json_str) {
            Ok(input) => out.push(BracketToolCall {
                name,
                input,
                span: (full.start(), span_end),
            }),
            Err(e) => {
                tracing::warn!(
                    name = %name,
                    err = %e,
                    snippet = %&json_str.chars().take(80).collect::<String>(),
                    "bracket 工具调用 JSON 解析失败，跳过"
                );
            }
        }
    }
    out
}

/// 把所有匹配的 bracket span 从原文本里剔除，返回纯净文本。
/// span 必须由 [`parse_bracket_tool_calls`] 返回，已按文本顺序排列。
pub fn strip_bracket_spans(text: &str, calls: &[BracketToolCall]) -> String {
    if calls.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for c in calls {
        let (s, e) = c.span;
        if s < cursor || e > text.len() {
            continue;
        }
        out.push_str(&text[cursor..s]);
        cursor = e;
    }
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_call() {
        let t = r#"hi [Called get_weather with args: {"city":"London"}] bye"#;
        let calls = parse_bracket_tool_calls(t);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].input["city"], "London");
        let stripped = strip_bracket_spans(t, &calls);
        assert_eq!(stripped, "hi  bye");
    }

    #[test]
    fn parses_multiple_calls_with_nested_json() {
        let t = r#"a [Called f with args: {"a":{"b":1}}] mid [Called g with args: {"x":[1,2]}] z"#;
        let calls = parse_bracket_tool_calls(t);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "f");
        assert_eq!(calls[1].name, "g");
        assert_eq!(calls[1].input["x"], serde_json::json!([1, 2]));
    }

    #[test]
    fn ignores_strings_with_braces() {
        let t = r#"[Called f with args: {"msg":"oops {not real}"}]"#;
        let calls = parse_bracket_tool_calls(t);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["msg"], "oops {not real}");
    }

    #[test]
    fn ignores_unclosed() {
        let t = r#"[Called f with args: {"a":1"#;
        assert!(parse_bracket_tool_calls(t).is_empty());
    }

    #[test]
    fn ignores_invalid_json() {
        let t = r#"[Called f with args: {bad json}]"#;
        assert!(parse_bracket_tool_calls(t).is_empty());
    }

    #[test]
    fn ignores_no_closing_bracket_after_json() {
        let t = r#"[Called f with args: {"a":1} continue text"#;
        assert!(parse_bracket_tool_calls(t).is_empty());
    }

    #[test]
    fn skips_when_marker_absent() {
        assert!(parse_bracket_tool_calls("nothing here").is_empty());
    }

    #[test]
    fn case_insensitive_marker() {
        let t = r#"[called f with args: {"a":1}]"#;
        let calls = parse_bracket_tool_calls(t);
        assert_eq!(calls.len(), 1);
    }
}
