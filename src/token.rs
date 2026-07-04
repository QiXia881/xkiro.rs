//! Token 计算模块
//!
//! 提供文本 token 数量计算功能。
//!
//! 默认本地估算使用 xkiro.rs 启发式计数。

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// 精确计数（cl100k_base）总开关
static PRECISE_COUNTING: AtomicBool = AtomicBool::new(false);

/// 设置是否启用 cl100k_base 精确 token 计数
pub fn set_precise_counting(enabled: bool) {
    PRECISE_COUNTING.store(enabled, Ordering::Relaxed);
}

fn precise_counting_enabled() -> bool {
    PRECISE_COUNTING.load(Ordering::Relaxed)
}

/// 全局共享的 cl100k_base BPE 实例
fn cl100k_bpe() -> &'static tiktoken_rs::CoreBPE {
    static BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base BPE 加载失败"))
}

/// Count Tokens API 配置
#[derive(Clone, Default)]
pub struct CountTokensConfig {
    /// 外部 count_tokens API 地址
    pub api_url: Option<String>,
    /// count_tokens API 密钥
    pub api_key: Option<String>,
    /// count_tokens API 认证类型（"x-api-key" 或 "bearer"）
    pub auth_type: String,
    /// 代理配置
    pub proxy: Option<ProxyConfig>,

    pub tls_backend: TlsBackend,
}

/// 全局配置存储
static COUNT_TOKENS_CONFIG: OnceLock<CountTokensConfig> = OnceLock::new();

/// 初始化 count_tokens 配置
///
/// 应在应用启动时调用一次
pub fn init_config(config: CountTokensConfig) {
    let _ = COUNT_TOKENS_CONFIG.set(config);
}

/// 获取配置
fn get_config() -> Option<&'static CountTokensConfig> {
    COUNT_TOKENS_CONFIG.get()
}

/// 计算文本的 token 数量
pub fn count_tokens(text: &str) -> u64 {
    if precise_counting_enabled() {
        return cl100k_bpe().encode_with_special_tokens(text).len() as u64;
    }

    estimate_approx_tokens(text)
}

pub(crate) fn estimate_approx_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }

    let length = text.chars().count();
    if length == 0 {
        return 0;
    }
    if length < 5 {
        return ((length as f64) / 3.0).ceil().max(1.0) as u64;
    }

    let mut regular_ascii = 0usize;
    let mut digits = 0usize;
    let mut symbols = 0usize;
    let mut non_ascii = 0usize;

    for ch in text.chars() {
        match ch {
            '\u{80}'.. => non_ascii += 1,
            '0'..='9' => digits += 1,
            '!'..='/' | ':'..='@' | '['..='`' | '{'..='~' => symbols += 1,
            _ => regular_ascii += 1,
        }
    }

    ((regular_ascii as f64) / 4.5
        + (digits as f64) / 2.0
        + (symbols as f64) / 1.5
        + (non_ascii as f64) / 1.5)
        .ceil()
        .max(1.0) as u64
}

/// 估算请求的输入 tokens
///
/// 优先调用远程 API，失败时回退到本地计算
pub(crate) fn count_all_tokens(
    model: String,
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    // 检查是否配置了远程 API
    if let Some(config) = get_config() {
        if let Some(api_url) = &config.api_url {
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    api_url, config, model, &system, &messages, &tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!("远程 count_tokens API 返回: {}", tokens);
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!("远程 count_tokens API 调用失败，回退到本地计算: {}", e);
                }
            }
        }
    }

    // 本地计算
    count_all_tokens_local(system, messages, tools)
}

/// 调用远程 count_tokens API
async fn call_remote_count_tokens(
    api_url: &str,
    config: &CountTokensConfig,
    model: String,
    system: &Option<Vec<SystemMessage>>,
    messages: &Vec<Message>,
    tools: &Option<Vec<Tool>>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = build_client(config.proxy.as_ref(), 300, config.tls_backend)?;

    // 构建请求体
    let request = CountTokensRequest {
        model: model, // 模型名称用于 token 计算
        max_tokens: 0,
        messages: messages.clone(),
        system: system.clone(),
        tools: tools.clone(),
        thinking: None,
        output_config: None,
    };

    // 构建请求
    let mut req_builder = client.post(api_url);

    // 设置认证头
    if let Some(api_key) = &config.api_key {
        if config.auth_type == "bearer" {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            req_builder = req_builder.header("x-api-key", api_key);
        }
    }

    // 发送请求
    let response = req_builder
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("API 返回错误状态: {}", response.status()).into());
    }

    let result: CountTokensResponse = response.json().await?;
    Ok(result.input_tokens as u64)
}

/// 本地计算请求的输入 tokens
fn count_all_tokens_local(
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    let mut total = 0;

    // 系统消息
    if let Some(ref system) = system {
        for msg in system {
            total += count_tokens(&msg.text);
        }
    }

    // 消息内容按 xkiro.rs 本地估算语义递归计算。
    for msg in &messages {
        total += count_message_content_tokens(&msg.content);
    }

    // 工具定义
    if let Some(ref tools) = tools {
        for tool in tools {
            total += count_tokens(&tool.name);
            total += count_tokens(&tool.description);
            let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            total += count_tokens(&input_schema_json);
        }
    }

    total
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    total += count_tokens(text) as i32;
                }
            }
            Some("thinking") => {
                if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
                    total += count_tokens(thinking) as i32;
                }
            }
            Some("tool_use") => {
                if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                    total += count_tokens(name) as i32;
                }
                if let Some(input) = block.get("input") {
                    let input_str = serde_json::to_string(input).unwrap_or_default();
                    total += count_tokens(&input_str) as i32;
                }
            }
            _ => {
                total += count_message_content_tokens(block) as i32;
            }
        }
    }

    total
}

/// 计算系统消息的 tokens（cache_tracker 使用）
pub fn count_system_message_tokens(message: &SystemMessage) -> u64 {
    count_tokens(&message.text)
}

/// 计算工具定义的 tokens（cache_tracker 使用）
pub fn count_tool_definition_tokens(tool: &Tool) -> u64 {
    let mut total = count_tokens(&tool.name) + count_tokens(&tool.description);
    let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
    total += count_tokens(&input_schema_json);
    total
}

/// 计算消息内容的 tokens（cache_tracker 使用）
///
/// 支持 string / array / object 三种 JSON 形态，
/// 递归处理 ContentBlock 内的 text/thinking/input/content 字段。
pub fn count_message_content_tokens(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Null => 0,
        serde_json::Value::String(s) => count_tokens(s),
        serde_json::Value::Array(arr) => arr.iter().map(count_message_content_tokens).sum(),
        serde_json::Value::Object(obj) => {
            match obj.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                        return count_tokens(text);
                    }
                }
                Some("thinking") => {
                    if let Some(thinking) = obj.get("thinking").and_then(|v| v.as_str()) {
                        return count_tokens(thinking);
                    }
                }
                Some("tool_use") => {
                    let mut total = 0;
                    if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                        total += count_tokens(name);
                    }
                    if let Some(input) = obj.get("input") {
                        let json = serde_json::to_string(input).unwrap_or_default();
                        total += count_tokens(&json);
                    }
                    if total > 0 {
                        return total;
                    }
                }
                Some("tool_result") => {
                    if let Some(content) = obj.get("content") {
                        return count_message_content_tokens(content);
                    }
                }
                _ => {}
            }

            let mut total = 0;
            if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                total += count_tokens(text);
            }
            if let Some(thinking) = obj.get("thinking").and_then(|v| v.as_str()) {
                total += count_tokens(thinking);
            }
            if let Some(content) = obj.get("content") {
                total += count_message_content_tokens(content);
            }
            if total > 0 {
                return total;
            }

            serde_json::to_string(value)
                .map(|json| count_tokens(&json))
                .unwrap_or(0)
        }
        _ => serde_json::to_string(value)
            .map(|json| count_tokens(&json))
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 共享 atomic 的测试需串行化，避免并行污染
    static GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn precise_counting_matches_known_cl100k_lengths() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_precise_counting(true);
        assert_eq!(count_tokens("hello world"), 2);
        assert_eq!(count_tokens(""), 0);
        set_precise_counting(false);
    }

    #[test]
    fn heuristic_counting_matches_local_estimator() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_precise_counting(false);
        assert_eq!(count_tokens(""), 0);
        assert_eq!(count_tokens("abc"), 1);
        assert_eq!(count_tokens("1234"), 2);
        assert_eq!(count_tokens("hello world"), 3);
        assert_eq!(count_tokens("!!!!!!"), 4);
        assert_eq!(count_tokens("你好世界"), 2);
        assert_eq!(count_tokens("你好世界啊"), 4);
    }

    #[test]
    fn message_content_tokens_falls_back_to_json_for_unknown_content() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_precise_counting(false);
        let value = serde_json::json!({
            "type": "custom_block",
            "payload": {"answer": 42}
        });
        let expected = count_tokens(&serde_json::to_string(&value).unwrap());

        assert_eq!(count_message_content_tokens(&value), expected);
        assert!(count_message_content_tokens(&value) > 0);
    }

    #[test]
    fn local_request_tokens_count_tool_use_and_tool_result() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_precise_counting(false);

        let tool_use_block = serde_json::json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "exec_command",
            "input": {"cmd": "pwd"}
        });
        let tool_use_expected = count_tokens("exec_command")
            + count_tokens(&serde_json::to_string(&tool_use_block["input"]).unwrap());
        assert_eq!(
            count_message_content_tokens(&tool_use_block),
            tool_use_expected
        );

        let text_only = vec![Message {
            role: "user".to_string(),
            content: serde_json::json!([{
                "type": "text",
                "text": "hello"
            }]),
        }];
        let with_tools = vec![
            Message {
                role: "assistant".to_string(),
                content: serde_json::json!([{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "exec_command",
                    "input": {"cmd": "pwd"}
                }]),
            },
            Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": [{"type": "text", "text": "workspace path"}]
                }]),
            },
        ];

        let base = count_all_tokens_local(None, text_only, None);
        let counted = count_all_tokens_local(None, with_tools, None);

        assert!(counted > base);
        assert!(counted >= count_tokens("exec_command"));
        assert!(counted >= count_tokens("workspace path"));
    }

    #[test]
    fn local_request_tokens_allow_zero_input() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_precise_counting(false);

        assert_eq!(count_all_tokens_local(None, Vec::new(), None), 0);
    }

    #[test]
    fn output_tokens_allow_zero() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_precise_counting(false);

        assert_eq!(estimate_output_tokens(&[]), 0);
        assert_eq!(
            estimate_output_tokens(&[
                serde_json::json!({"type": "text", "text": ""}),
                serde_json::json!({"type": "thinking", "thinking": ""})
            ]),
            0
        );
    }

    #[test]
    fn output_tokens_count_thinking_and_tool_name() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_precise_counting(false);

        let tool_input = serde_json::json!({"cmd": "pwd"});
        let content = vec![
            serde_json::json!({"type": "text", "text": "hello"}),
            serde_json::json!({"type": "thinking", "thinking": "reasoning"}),
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_1",
                "name": "exec_command",
                "input": tool_input
            }),
        ];
        let expected = count_tokens("hello")
            + count_tokens("reasoning")
            + count_tokens("exec_command")
            + count_tokens(&serde_json::to_string(&content[2]["input"]).unwrap());

        assert_eq!(estimate_output_tokens(&content), expected as i32);
    }

    #[test]
    fn output_tokens_fall_back_to_json_for_unknown_blocks() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_precise_counting(false);

        let block = serde_json::json!({
            "type": "custom_block",
            "payload": {"answer": 42}
        });
        let expected = count_tokens(&serde_json::to_string(&block).unwrap());

        assert_eq!(estimate_output_tokens(&[block]), expected as i32);
    }
}
