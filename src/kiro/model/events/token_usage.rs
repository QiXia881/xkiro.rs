//! Token 使用量提取
//!
//! 从 Kiro 事件 payload 中递归提取 token 使用量统计。
//! 递归收集 usage map 并读取多种 token 数值字段。
//!
//! Kiro 上游可能在任意事件的 payload 中嵌套 `usage` / `tokenUsage` / `token_usage` 字段，
//! 包含 inputTokens / outputTokens 等实际 token 计数。

use serde_json::Value;

/// Token 使用量统计
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// 输入 tokens
    pub input_tokens: Option<i64>,
    /// 输出 tokens
    pub output_tokens: Option<i64>,
    /// 缓存创建 tokens
    pub cache_creation_tokens: Option<i64>,
    /// 缓存读取 tokens
    pub cache_read_tokens: Option<i64>,
}

impl TokenUsage {
    /// 是否有任何有效的 token 数据
    pub fn has_data(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_creation_tokens.is_some()
            || self.cache_read_tokens.is_some()
    }
}

/// 从事件 JSON payload 中提取 token 使用量
///
/// 递归查找 `usage` / `tokenUsage` / `token_usage` 字段，
/// 然后从中提取 inputTokens / outputTokens 等值。
/// 递归收集 usage map，并同时读取 JSON number 与字符串形式数值。
pub fn extract_token_usage(event: &Value) -> Option<TokenUsage> {
    extract_token_usage_with_current(event, None, None)
}

pub fn extract_token_usage_with_current(
    event: &Value,
    current_input_tokens: Option<i64>,
    current_output_tokens: Option<i64>,
) -> Option<TokenUsage> {
    let mut candidates = Vec::new();
    if let Value::Object(root) = event {
        candidates.push(root.clone());
    }
    collect_usage_maps(event, &mut candidates);

    let mut usage = TokenUsage {
        input_tokens: current_input_tokens,
        output_tokens: current_output_tokens,
        cache_creation_tokens: None,
        cache_read_tokens: None,
    };
    let mut found = false;

    for map in &candidates {
        // Output tokens
        if let Some(v) = read_token_number(
            map,
            &[
                "outputTokens",
                "completionTokens",
                "totalOutputTokens",
                "output_tokens",
                "completion_tokens",
                "total_output_tokens",
            ],
        ) {
            usage.output_tokens = Some(v);
            found = true;
        }

        // Input tokens
        if let Some(v) = read_token_number(
            map,
            &[
                "inputTokens",
                "promptTokens",
                "totalInputTokens",
                "input_tokens",
                "prompt_tokens",
                "total_input_tokens",
            ],
        ) {
            usage.input_tokens = Some(v);
            found = true;
            continue;
        }

        // Cache token breakdown
        let uncached = read_token_number(map, &["uncachedInputTokens", "uncached_input_tokens"]);
        let cache_read =
            read_token_number(map, &["cacheReadInputTokens", "cache_read_input_tokens"]);
        let cache_write = read_token_number(
            map,
            &[
                "cacheWriteInputTokens",
                "cache_write_input_tokens",
                "cacheCreationInputTokens",
                "cache_creation_input_tokens",
            ],
        );

        let has_cache = uncached.is_some() || cache_read.is_some() || cache_write.is_some();
        if has_cache {
            let total_input =
                uncached.unwrap_or(0) + cache_read.unwrap_or(0) + cache_write.unwrap_or(0);
            if total_input > 0 {
                usage.input_tokens = Some(total_input);
                usage.cache_creation_tokens = cache_write;
                usage.cache_read_tokens = cache_read;
                found = true;
                continue;
            }
        }

        // Fallback: totalTokens - outputTokens
        if let Some(total) = read_token_number(map, &["totalTokens", "total_tokens"]) {
            if total > 0 {
                let out = usage.output_tokens.unwrap_or(0);
                if total - out > 0 {
                    usage.input_tokens = Some(total - out);
                    found = true;
                }
            }
        }
    }

    if found { Some(usage) } else { None }
}

/// 递归收集所有 usage / tokenUsage / token_usage map
fn collect_usage_maps(v: &Value, out: &mut Vec<serde_json::Map<String, Value>>) {
    match v {
        Value::Object(map) => {
            for (key, child) in map {
                let lk = key.to_lowercase();
                if lk == "usage" || lk == "tokenusage" || lk == "token_usage" {
                    if let Value::Object(usage_map) = child {
                        out.push(usage_map.clone());
                    }
                }
                collect_usage_maps(child, out);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                collect_usage_maps(child, out);
            }
        }
        _ => {}
    }
}

/// 从 map 中按多个候选 key 读取数值
///
/// 同时处理 JSON number 和 string-encoded number（如 `"100"`）
fn read_token_number(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(v) = map.get(*key) {
            match v {
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        return Some(i);
                    }
                    if let Some(f) = n.as_f64() {
                        return Some(f as i64);
                    }
                }
                // 上游可能以字符串形式返回数值
                Value::String(s) => {
                    if let Ok(i) = s.parse::<i64>() {
                        return Some(i);
                    }
                    if let Ok(f) = s.parse::<f64>() {
                        return Some(f as i64);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_from_direct_usage() {
        let event = json!({
            "usage": {
                "inputTokens": 100,
                "outputTokens": 50
            }
        });
        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
    }

    #[test]
    fn test_extract_from_root_fields() {
        let event = json!({
            "inputTokens": 100,
            "outputTokens": 50
        });
        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
    }

    #[test]
    fn test_extract_from_nested_usage() {
        let event = json!({
            "content": "hello",
            "metadata": {
                "tokenUsage": {
                    "input_tokens": 200,
                    "output_tokens": 80
                }
            }
        });
        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.input_tokens, Some(200));
        assert_eq!(usage.output_tokens, Some(80));
    }

    #[test]
    fn test_extract_cache_tokens() {
        let event = json!({
            "usage": {
                "uncachedInputTokens": 50,
                "cacheReadInputTokens": 100,
                "cacheCreationInputTokens": 30
            }
        });
        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.input_tokens, Some(180));
        assert_eq!(usage.cache_creation_tokens, Some(30));
        assert_eq!(usage.cache_read_tokens, Some(100));
    }

    #[test]
    fn test_no_usage_returns_none() {
        let event = json!({
            "content": "hello"
        });
        assert!(extract_token_usage(&event).is_none());
    }

    #[test]
    fn test_total_tokens_fallback() {
        let event = json!({
            "usage": {
                "totalTokens": 150,
                "outputTokens": 50
            }
        });
        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
    }

    #[test]
    fn test_root_total_tokens_fallback() {
        let event = json!({
            "totalTokens": 150,
            "outputTokens": 50
        });
        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
    }

    #[test]
    fn test_total_tokens_fallback_uses_current_output() {
        let event = json!({
            "usage": {
                "totalTokens": 150
            }
        });
        let usage = extract_token_usage_with_current(&event, Some(20), Some(50)).unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
    }
}
