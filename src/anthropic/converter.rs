//! Anthropic → Kiro 协议转换器
//!
//! 负责将 Anthropic API 请求格式转换为 Kiro API 请求格式

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use base64::{Engine, engine::general_purpose};
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::requests::conversation::{
    AssistantMessage, ConversationState, CurrentMessage, HistoryAssistantMessage,
    HistoryUserMessage, KiroImage, Message, UserInputMessage, UserInputMessageContext, UserMessage,
};
use crate::kiro::model::requests::tool::{
    InputSchema, Tool, ToolResult, ToolSpecification, ToolUseEntry,
};
use crate::model::claude::kiro_upstream_claude_model_id;
use crate::model::config::{CompressionConfig, PromptFilterConfig};

use super::prompt_filter::apply_prompt_filters;
use super::types::{ContentBlock, MessagesRequest};

/// 规范化 JSON Schema，修复 MCP 工具定义中常见的类型问题
///
/// Schema 规范化规则:
/// - 确保顶层 `type: "object"`
/// - 递归删除 `additionalProperties`
/// - 递归删除空 `required` 数组
fn normalize_json_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return serde_json::json!({"type": "object"});
    };

    // 递归清洗
    clean_schema(&mut obj);

    // 确保顶层有 type
    if !obj.contains_key("type") {
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("object".to_string()),
        );
    }

    serde_json::Value::Object(obj)
}

/// 递归清洗 schema
///
/// - 删除 `additionalProperties`
/// - `required` 为空数组或非数组时删除
fn clean_schema(obj: &mut serde_json::Map<String, serde_json::Value>) {
    // 直接删除 additionalProperties
    obj.remove("additionalProperties");

    // required 必须是非空数组，否则删除
    let drop_required = match obj.get("required") {
        Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Array(arr)) => arr.is_empty(),
        Some(_) => true,
        None => false,
    };
    if drop_required {
        obj.remove("required");
    }

    // 递归处理子结构
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        match obj.get_mut(&key) {
            Some(serde_json::Value::Object(sub)) => {
                clean_schema(sub);
            }
            Some(serde_json::Value::Array(arr)) => {
                for item in arr.iter_mut() {
                    if let serde_json::Value::Object(sub) = item {
                        clean_schema(sub);
                    }
                }
            }
            _ => {}
        }
    }
}

const MINIMAL_FALLBACK_USER_CONTENT: &str = ".";

/// Kiro Agentic 模型系统提示
const KIRO_AGENTIC_SYSTEM_PROMPT: &str = "\
You are operating in agentic mode. Work autonomously to complete the user's request.
- Break complex tasks into steps
- Use tools proactively when needed
- Verify your work before responding
- If you encounter errors, try alternative approaches
";

/// 在不主动改变内容语义的前提下，对空文本兜底返回 ""。
///
/// - 含非文本载荷（图片 / tool_result）时保留原文，最终是否需要补占位符由调用方决定
/// - 不含非文本载荷时同样保留原文，由调用方在最末做兜底
fn non_empty_content_or_space(content: String, has_non_text_payload: bool) -> String {
    if has_non_text_payload {
        return content;
    }
    content
}

/// 判断模型名是否为 agentic 变体（以 `-agentic` 结尾，忽略大小写）
pub fn is_agentic_model(model: &str) -> bool {
    model.to_lowercase().ends_with("-agentic")
}

/// build_history 的参数包，避免长签名
struct BuildHistoryContext<'a> {
    model_id: &'a str,
    prompt_filter: &'a PromptFilterConfig,
    is_agentic: bool,
    tool_name_map: &'a mut HashMap<String, String>,
    preserve_tool_names: bool,
}

struct BuildHistoryResult {
    history: Vec<Message>,
    has_system_priming: bool,
}

const TOOL_RESULTS_CONTINUATION_PREFIX: &str = "Tool results:";
const TOOL_RESULTS_CONTINUATION_MAX_LEN: usize = 4000;

pub fn map_model(model: &str) -> String {
    map_model_with_thinking_suffix(model, "-thinking")
}

pub fn map_model_with_thinking_suffix(model: &str, thinking_suffix: &str) -> String {
    let mut model = model.to_string();
    let model_lower = model.to_lowercase();
    let mut lower = model_lower.as_str();
    let suffix_lower = thinking_suffix.to_lowercase();

    if !suffix_lower.is_empty() && lower.ends_with(&suffix_lower) {
        let new_len = model.len().saturating_sub(thinking_suffix.len());
        model.truncate(new_len);
        lower = &model_lower[..model_lower.len().saturating_sub(thinking_suffix.len())];
    }

    for (key, value) in [
        ("claude-sonnet-4-20250514", "claude-sonnet-4"),
        ("claude-3-5-sonnet", "claude-sonnet-4.5"),
        ("claude-3-opus", "claude-sonnet-4.5"),
        ("claude-3-sonnet", "claude-sonnet-4"),
        ("claude-3-haiku", "claude-haiku-4.5"),
        ("gpt-4-turbo", "claude-sonnet-4.5"),
        ("gpt-4o", "claude-sonnet-4.5"),
        ("gpt-4", "claude-sonnet-4.5"),
        ("gpt-3.5-turbo", "claude-sonnet-4.5"),
    ] {
        if lower.contains(key) {
            return value.to_string();
        }
    }

    kiro_upstream_claude_model_id(&model)
}

/// 上下文窗口覆盖值（0 = 未设置，用模型默认 1M/200K）
static CONTEXT_WINDOW_OVERRIDE: AtomicI32 = AtomicI32::new(0);
/// 最终上报 input_tokens 的放大系数（千分比存储，1000 = 1.0x）
static CONTEXT_USAGE_MULTIPLIER_MILLI: AtomicI32 = AtomicI32::new(1000);

/// 设置上下文窗口覆盖值；`<= 0` 表示清除覆盖，回落模型默认
pub fn set_context_window_override(value: i32) {
    CONTEXT_WINDOW_OVERRIDE.store(value.max(0), Ordering::Relaxed);
}

/// 设置最终上报 input_tokens 的放大系数；clamp 到 `0.1..=10.0`
pub fn set_context_usage_multiplier(value: f64) {
    let clamped = value.clamp(0.1, 10.0);
    CONTEXT_USAGE_MULTIPLIER_MILLI.store((clamped * 1000.0).round() as i32, Ordering::Relaxed);
}

/// 返回用于「上下文占比→tokens」换算的基准窗口大小。
///
/// 默认与上游一致（大窗口模型 1M，其余 200K），可经 admin 配置 override 覆盖。
/// 注意：放大系数不在此处叠加——它作用于最终上报值（见
/// [`apply_context_usage_multiplier`]），以覆盖 pct 换算 / 上游 raw 帧 / 本地估算
/// 三条来源，避免上游 raw token 帧旁路系数。
pub fn get_context_window_size(model: &str) -> i32 {
    let ov = CONTEXT_WINDOW_OVERRIDE.load(Ordering::Relaxed);
    if ov > 0 {
        ov
    } else if is_large_context_model(model) {
        1_000_000
    } else {
        200_000
    }
}

/// 对最终上报的 input_tokens 施加放大系数。
///
/// 与 `get_context_window_size` 的 override 正交：override 决定「上游占比→tokens」的
/// 基准窗口，本函数对三条来源（pct 换算 / 上游 raw 帧 / 本地估算）汇合后的最终值统一放大，
/// 使上游 raw token 帧无法旁路系数。对 pct 换算路径而言，`pct*base*mult` 与
/// `(pct*base)*mult` 算术等价，行为不漂移。默认 multiplier=1.0 时原值返回。
pub fn apply_context_usage_multiplier(tokens: i32) -> i32 {
    apply_multiplier_milli(tokens, CONTEXT_USAGE_MULTIPLIER_MILLI.load(Ordering::Relaxed))
}

/// 纯算术核：千分比系数应用。抽出以便无需触碰进程级全局即可单测。
fn apply_multiplier_milli(tokens: i32, milli: i32) -> i32 {
    if milli == 1000 {
        return tokens;
    }
    ((i64::from(tokens) * i64::from(milli)) / 1000).clamp(1, i64::from(i32::MAX)) as i32
}

/// 共享测试守卫：串行化对 `CONTEXT_USAGE_MULTIPLIER_MILLI` 等进程级 atomic 的读写测试，
/// 避免跨模块并行测试互相污染。跨模块读取者（stream/handlers 的 usage 测试）需持同一锁。
#[cfg(test)]
pub(crate) static CONTEXT_GLOBAL_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn is_large_context_model(model: &str) -> bool {
    static CLAUDE_VERSION_EXTRACTOR: OnceLock<Regex> = OnceLock::new();

    let model_lower = model.to_lowercase();
    let version_re = CLAUDE_VERSION_EXTRACTOR
        .get_or_init(|| Regex::new(r"claude-(?:opus|sonnet|haiku)-(\d+)[.-](\d+)").unwrap());

    if let Some(captures) = version_re.captures(&model_lower) {
        let major = captures.get(1).and_then(|m| m.as_str().parse::<i32>().ok());
        let minor = captures.get(2).and_then(|m| m.as_str().parse::<i32>().ok());

        if let (Some(major), Some(minor)) = (major, minor) {
            return major > 4 || (major == 4 && minor >= 6);
        }
    }

    ["4.6", "4-6", "4.7", "4-7", "4.8", "4-8", "4.9", "4-9"]
        .iter()
        .any(|tag| model_lower.contains(tag))
}

/// 转换结果
#[derive(Debug)]
pub struct ConversionResult {
    /// 转换后的 Kiro 请求
    pub conversation_state: ConversationState,
    pub has_system_priming: bool,
    /// 工具名称映射（短名称 → 原始名称），仅当存在超长工具名时非空
    pub tool_name_map: HashMap<String, String>,
}

/// 转换错误
#[derive(Debug)]
pub enum ConversionError {
    UnsupportedModel(String),
    EmptyMessages,
    EmptyMessageContent,
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::UnsupportedModel(model) => write!(f, "模型不支持: {}", model),
            ConversionError::EmptyMessages => write!(f, "消息列表为空"),
            ConversionError::EmptyMessageContent => write!(f, "消息内容为空"),
        }
    }
}

impl std::error::Error for ConversionError {}

/// 从 metadata.user_id 中提取 session UUID
///
/// 支持两种格式:
/// 1. 字符串格式: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
/// 2. JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
///
/// 提取 session UUID 作为 conversationId
pub fn extract_session_id(user_id: &str) -> Option<String> {
    // 先尝试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
            if is_valid_uuid(session_id) {
                return Some(session_id.to_string());
            }
        }
    }

    // 回退到字符串格式: 查找 "session_" 后面的内容
    if let Some(pos) = user_id.find("session_") {
        let session_part = &user_id[pos + 8..]; // "session_" 长度为 8
        if session_part.len() >= 36 {
            let uuid_str = &session_part[..36];
            if is_valid_uuid(uuid_str) {
                return Some(uuid_str.to_string());
            }
        }
    }
    None
}

/// 简单验证 UUID 格式（36 字符，包含 4 个连字符）
fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

fn system_prompt_for_conversation_id(req: &MessagesRequest) -> String {
    req.system
        .as_ref()
        .map(|system| {
            system
                .iter()
                .map(|block| block.text.trim())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn first_conversation_anchor(messages: &[super::types::Message]) -> String {
    for msg in messages {
        if msg.role != "user" {
            continue;
        }
        let text = user_anchor_text(&msg.content);
        if !text.trim().is_empty() {
            return text.trim().to_string();
        }
    }
    String::new()
}

fn user_anchor_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                let block_type = item.get("type").and_then(|v| v.as_str());
                match block_type {
                    Some("text" | "input_text") => item.get("text").and_then(|v| v.as_str()),
                    Some("tool_result") => None,
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        serde_json::Value::Object(obj) => obj
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn build_conversation_id(model_id: &str, system_prompt: &str, anchor: &str) -> String {
    let anchor = anchor.trim();
    if is_synthetic_conversation_anchor(anchor) {
        return Uuid::new_v4().to_string();
    }
    let seed = format!("{}\n{}\n{}", model_id, system_prompt.trim(), anchor);
    Uuid::new_v5(&Uuid::NAMESPACE_URL, seed.as_bytes()).to_string()
}

fn is_synthetic_conversation_anchor(anchor: &str) -> bool {
    if anchor.trim().is_empty() {
        return true;
    }
    let normalized = anchor
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    matches!(
        normalized.as_str(),
        "." | "begin conversation" | "please analyze the attached image."
    ) || normalized == MINIMAL_FALLBACK_USER_CONTENT.to_lowercase()
}

/// 收集历史消息中使用的所有工具名称
fn collect_history_tool_names(history: &[Message]) -> Vec<String> {
    let mut tool_names = Vec::new();

    for msg in history {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                for tool_use in tool_uses {
                    if !tool_names.contains(&tool_use.name) {
                        tool_names.push(tool_use.name.clone());
                    }
                }
            }
        }
    }

    tool_names
}

/// 为历史中使用但不在 tools 列表中的工具创建占位符定义
/// Kiro API 要求：历史消息中引用的工具必须在 currentMessage.tools 中有定义
fn create_placeholder_tool(name: &str) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: name.to_string(),
            description: "Tool used in conversation history".to_string(),
            input_schema: InputSchema::from_json(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {}
            })),
        },
    }
}

/// 将 Anthropic 请求转换为 Kiro 请求
pub fn convert_request(
    req: &MessagesRequest,
    compression_config: &CompressionConfig,
    prompt_filter: &PromptFilterConfig,
    _unused: bool,
) -> Result<ConversionResult, ConversionError> {
    convert_request_with_thinking_suffix(req, compression_config, prompt_filter, false, "-thinking")
}

pub fn convert_request_with_thinking_suffix(
    req: &MessagesRequest,
    _compression_config: &CompressionConfig,
    prompt_filter: &PromptFilterConfig,
    _unused: bool,
    thinking_suffix: &str,
) -> Result<ConversionResult, ConversionError> {
    // 1. 映射模型
    let model_id = map_model_with_thinking_suffix(&req.model, thinking_suffix);

    // 2. 检查消息列表
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    let source_messages: &[super::types::Message] = &req.messages;

    // 2.5. Handler 已在边界拒绝真实 assistant-final prefill。
    // 转换层保留修剪能力，兜底处理内部路径混入的孤立 assistant 尾部。
    let messages: &[_] = if source_messages.last().is_some_and(|m| m.role != "user") {
        tracing::info!("检测到末尾 assistant 消息（prefill），静默丢弃");
        let last_user_idx = source_messages
            .iter()
            .rposition(|m| m.role == "user")
            .ok_or(ConversionError::EmptyMessages)?;
        &source_messages[..=last_user_idx]
    } else {
        source_messages
    };

    // 2.6. 验证最后一条消息内容不为空
    // 检查最后一条消息是否有有效内容
    let last_message = messages.last().unwrap();
    let has_valid_content = match &last_message.content {
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Array(arr) => arr.iter().any(|item| {
            let Some(block_type) = item.get("type").and_then(|v| v.as_str()) else {
                return false;
            };
            match block_type {
                "text" | "input_text" => item
                    .get("text")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| !t.trim().is_empty()),
                "image" | "image_url" | "input_image" | "file" | "input_file" | "tool_use"
                | "tool_result" => true,
                _ => false,
            }
        }),
        _ => false,
    };
    if !has_valid_content {
        tracing::warn!("最后一条消息内容为空（仅包含空白文本或无内容）");
        return Err(ConversionError::EmptyMessageContent);
    }

    // 3. 生成会话 ID 和代理 ID
    // 优先从 metadata.user_id 中提取 session UUID；否则基于首个真实 user anchor 稳定派生。
    let conversation_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .and_then(|user_id| extract_session_id(user_id))
        .unwrap_or_else(|| {
            build_conversation_id(
                &model_id,
                &system_prompt_for_conversation_id(req),
                &first_conversation_anchor(messages),
            )
        });
    let agent_continuation_id = Uuid::new_v4().to_string();

    // 4. 确定触发类型
    let chat_trigger_type = determine_chat_trigger_type(req);

    // 6. 处理最后一条消息作为 current_message（此处末尾应为 user）
    let last_message = messages.last().unwrap();
    let (text_content, images, tool_results) = process_message_content(&last_message.content)?;

    // 7. 转换工具定义：Claude 路径 sanitize+shorten，OpenAI 路径只 shorten。
    let mut tool_name_map = HashMap::new();
    let preserve_tool_names = req
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.preserve_tool_names);
    let mut tools = convert_tools(&req.tools, &mut tool_name_map, preserve_tool_names);

    // 8. 构建历史消息（需要先构建，以便收集历史中使用的工具）
    let BuildHistoryResult {
        mut history,
        has_system_priming,
    } = build_history(
        req,
        messages,
        BuildHistoryContext {
            model_id: &model_id,
            prompt_filter,
            is_agentic: is_agentic_model(&req.model),
            tool_name_map: &mut tool_name_map,
            preserve_tool_names,
        },
    )?;

    // 8. 清洗历史消息
    // 只有当前 toolResults 正好回答最后一个 history assistant toolUse 时，才保留结构化结果。
    let current_tool_result_ids = collect_tool_result_ids(&tool_results);
    let keep_current_tool_results =
        current_tool_results_match_last_assistant(&history, &current_tool_result_ids);
    if keep_current_tool_results {
        sanitize_kiro_history(&mut history, &current_tool_result_ids);
    } else {
        sanitize_kiro_history(&mut history, &std::collections::HashSet::new());
    }

    // 10. 收集历史中使用的工具名称，为缺失的工具生成占位符定义
    // Kiro API 要求：历史消息中引用的工具必须在 tools 列表中有定义
    // 注意：Kiro 匹配工具名称时忽略大小写，所以这里也需要忽略大小写比较
    let history_tool_names = collect_history_tool_names(&history);
    let mut existing_tool_names: std::collections::HashSet<_> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_lowercase())
        .collect();

    for tool_name in history_tool_names {
        let lower = tool_name.to_lowercase();
        if !existing_tool_names.contains(&lower) {
            tools.push(create_placeholder_tool(&tool_name));
            existing_tool_names.insert(lower);
        }
    }

    // 10.5. 工具统计诊断日志
    {
        let original_tool_count = req.tools.as_ref().map(|t| t.len()).unwrap_or(0);
        let placeholder_count = tools.len().saturating_sub(original_tool_count);

        // 大小写不敏感的重复检测
        let mut name_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for t in &tools {
            *name_counts
                .entry(t.tool_specification.name.to_lowercase())
                .or_insert(0) += 1;
        }
        let duplicates: Vec<_> = name_counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(name, count)| format!("{}(x{})", name, count))
            .collect();

        if !duplicates.is_empty() {
            tracing::warn!(
                tool_count = tools.len(),
                duplicates = ?duplicates,
                "检测到重复工具名称（大小写不敏感）"
            );
        }
        tracing::info!(
            tool_count = tools.len(),
            placeholder_count = placeholder_count,
            "工具定义统计"
        );
    }

    // 11. 构建 UserInputMessageContext
    let mut context = UserInputMessageContext::new();
    if !tools.is_empty() {
        context = context.with_tools(std::mem::take(&mut tools));
    }
    let attach_tool_results = if keep_current_tool_results {
        tool_results.clone()
    } else {
        Vec::new()
    };
    let has_tool_results = !attach_tool_results.is_empty();
    if has_tool_results {
        context = context.with_tool_results(attach_tool_results);
    }

    // 12. 构建当前消息
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let content = non_empty_content_or_space(text_content, !images.is_empty() || has_tool_results);
    let normalized_content = normalize_user_content(&content, !images.is_empty());
    let content = if !normalized_content.is_empty() {
        normalized_content
    } else if !tool_results.is_empty() {
        build_tool_results_continuation(&tool_results)
    } else if !has_tool_results {
        tracing::warn!("currentMessage content 为空，已使用占位符修复");
        MINIMAL_FALLBACK_USER_CONTENT.to_string()
    } else {
        content
    };

    let mut user_input = UserInputMessage::new(content, &model_id)
        .with_context(context)
        .with_origin("AI_EDITOR");

    if !images.is_empty() {
        user_input = user_input.with_images(images);
    }

    let current_message = CurrentMessage::new(user_input);

    // 13. 构建 ConversationState
    let conversation_state = ConversationState::new(conversation_id)
        .with_agent_continuation_id(agent_continuation_id)
        .with_agent_task_type("vibe")
        .with_chat_trigger_type(chat_trigger_type)
        .with_current_message(current_message)
        .with_history(history);

    if !tool_name_map.is_empty() {
        tracing::info!("工具名称映射: {} 个超长名称已缩短", tool_name_map.len());
    }

    Ok(ConversionResult {
        conversation_state,
        has_system_priming,
        tool_name_map,
    })
}

/// 确定聊天触发类型
/// "AUTO" 模式可能会导致 400 Bad Request 错误
fn determine_chat_trigger_type(_req: &MessagesRequest) -> String {
    "MANUAL".to_string()
}

/// 处理消息内容，提取文本、图片和工具结果
fn process_message_content(
    content: &serde_json::Value,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                let block_type = item.get("type").and_then(|v| v.as_str());
                if matches!(
                    block_type,
                    Some("image" | "image_url" | "input_image" | "file" | "input_file")
                ) {
                    images.extend(process_content_image_value(item));
                    continue;
                }

                if let Ok(block) = ContentBlock::deserialize(item) {
                    match block.block_type.as_str() {
                        "text" | "input_text" => {
                            if let Some(text) = block.text {
                                text_parts.push(text);
                            }
                        }
                        "tool_result" => {
                            if let Some(tool_use_id) = block.tool_use_id {
                                let (mut result_content, result_images) =
                                    extract_tool_result_content_and_images(&block.content);
                                if !result_images.is_empty() && result_content.trim().is_empty() {
                                    result_content =
                                        "[Tool returned an image; the image is attached to this message.]"
                                            .to_string();
                                }
                                images.extend(result_images);
                                let is_error = block.is_error.unwrap_or(false);

                                let mut result = if is_error {
                                    ToolResult::error(&tool_use_id, result_content)
                                } else {
                                    ToolResult::success(&tool_use_id, result_content)
                                };
                                result.status =
                                    Some(if is_error { "error" } else { "success" }.to_string());

                                tool_results.push(result);
                            }
                        }
                        "tool_use" => {
                            // tool_use 在 assistant 消息中处理，这里忽略
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    let mut text = text_parts.join("\n");
    if !images.is_empty() {
        text = sanitize_image_placeholders(&text);
    }

    Ok((text, images, tool_results))
}

fn normalize_user_content(text: &str, has_images: bool) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() && has_images {
        return "Please analyze the attached image.".to_string();
    }
    trimmed.to_string()
}

fn sanitize_image_placeholders(text: &str) -> String {
    static IMAGE_PLACEHOLDER_RE: OnceLock<Regex> = OnceLock::new();

    let placeholder = IMAGE_PLACEHOLDER_RE.get_or_init(|| Regex::new(r"\[Image\s+\d+\]").unwrap());
    let cleaned = placeholder.replace_all(text, "");
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn process_content_image_value(item: &serde_json::Value) -> Vec<KiroImage> {
    if let Some((data, format)) = extract_image_data_from_value(item) {
        return process_image_data(&data, &format);
    }

    if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
        return process_content_image_block(&block);
    }

    Vec::new()
}

fn process_content_image_block(block: &ContentBlock) -> Vec<KiroImage> {
    let Some(source) = block.source.as_ref() else {
        return Vec::new();
    };
    let Some(format) = image_format_from_mime(&source.media_type) else {
        return Vec::new();
    };

    process_image_data(&source.data, &format)
}

fn extract_image_data_from_value(item: &serde_json::Value) -> Option<(String, String)> {
    let obj = item.as_object()?;
    if let Some(block_type) = obj.get("type").and_then(|v| v.as_str()) {
        match block_type {
            "image" | "image_url" | "input_image" | "file" | "input_file" => {}
            _ => return None,
        }
    }

    if let Some(file) = obj.get("file").and_then(|v| v.as_object())
        && let Some(img) = extract_image_data_from_value(&serde_json::Value::Object(file.clone()))
    {
        return Some(img);
    }
    if let Some(source) = obj.get("source").and_then(|v| v.as_object()) {
        if let Some(img) = extract_image_data_from_value(&serde_json::Value::Object(source.clone()))
        {
            return Some(img);
        }
        if let Some(data) = source.get("data").and_then(|v| v.as_str()) {
            if let Some(img) = parse_data_url_image(data) {
                return Some(img);
            }
            let format = source
                .get("media_type")
                .or_else(|| source.get("mediaType"))
                .or_else(|| source.get("mime_type"))
                .or_else(|| source.get("mime"))
                .and_then(|v| v.as_str())
                .and_then(image_format_from_mime)
                .unwrap_or_else(|| "png".to_string());
            if is_valid_base64_image_data(data) {
                return Some((data.to_string(), format));
            }
        }
        if let Some(url) = source.get("url").and_then(|v| v.as_str())
            && let Some(img) = parse_data_url_image(url)
        {
            return Some(img);
        }
    }

    for key in ["mime", "media_type", "mime_type"] {
        if let Some(raw) = obj.get(key).and_then(|v| v.as_str())
            && image_format_from_mime(raw).is_none()
        {
            return None;
        }
    }

    if let Some(url) = obj.get("url").and_then(|v| v.as_str())
        && let Some(img) = parse_data_url_image(url)
    {
        return Some(img);
    }
    if let Some(raw) = obj.get("image_url") {
        match raw {
            serde_json::Value::String(url) => {
                if let Some(img) = parse_data_url_image(url) {
                    return Some(img);
                }
            }
            serde_json::Value::Object(map) => {
                if let Some(url) = map.get("url").and_then(|v| v.as_str())
                    && let Some(img) = parse_data_url_image(url)
                {
                    return Some(img);
                }
            }
            _ => {}
        }
    }
    for key in ["b64_json", "image_base64"] {
        if let Some(data) = obj.get(key).and_then(|v| v.as_str())
            && is_valid_base64_image_data(data)
        {
            return Some((data.to_string(), "png".to_string()));
        }
    }
    if let Some(data) = obj.get("data").and_then(|v| v.as_str()) {
        if let Some(img) = parse_data_url_image(data) {
            return Some(img);
        }
        if is_valid_base64_image_data(data) {
            return Some((data.to_string(), "png".to_string()));
        }
    }

    None
}

fn parse_data_url_image(raw: &str) -> Option<(String, String)> {
    let cleaned = raw.trim().replace(['\n', '\r'], "");
    if cleaned.contains("[Image") {
        return None;
    }
    let rest = cleaned.strip_prefix("data:image/")?;
    let (metadata, data) = rest.split_once(',')?;
    let metadata_lower = metadata.to_lowercase();
    if !metadata_lower.contains(";base64") {
        return None;
    }
    let format = metadata
        .split(';')
        .next()
        .map(normalize_image_format)
        .filter(|s| !s.is_empty())?;
    if !is_valid_base64_image_data(data) {
        return None;
    }
    Some((data.to_string(), format))
}

fn image_format_from_mime(raw: &str) -> Option<String> {
    let lower = raw.trim().to_lowercase();
    let format = lower.strip_prefix("image/")?;
    Some(normalize_image_format(format))
}

fn normalize_image_format(format: &str) -> String {
    match format.trim().to_lowercase().as_str() {
        "jpg" => "jpeg".to_string(),
        other => other.to_string(),
    }
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

fn process_image_data(data: &str, format: &str) -> Vec<KiroImage> {
    if !is_valid_base64_image_data(data) {
        tracing::warn!("图片 base64 校验失败，跳过图片");
        return Vec::new();
    }

    let format = normalize_image_format(format);
    let format = if format.is_empty() { "png" } else { &format };
    vec![KiroImage::from_base64(format, data)]
}

/// 若 `s` 看起来是 JSON（首个非空白字符为 `{` 或 `[`），尝试 parse + 紧凑化，
/// 否则或 parse 失败原样返回。用于压缩 MCP 工具返回的 pretty-printed JSON，节省 token。
///
/// 判断策略（先廉价 sniff，再走 parse 兜底）：
/// 1. trim_start 后首字符不是 `{` / `[` → 当作纯文本（Markdown / stdout / 错误消息 / 标量）直接返回
/// 2. 首字符匹配但 parse 失败 → 原样返回（保守：不破坏带噪声的伪 JSON）
/// 3. parse 成功 → `serde_json::to_string` 输出紧凑形式（无空白、无换行）
///
/// 性能：serde_json round-trip。MCP 返回通常 <100KB，亚毫秒级；不引入 SIMD 依赖。
fn compact_json_if_possible(s: &str) -> String {
    let trimmed = s.trim_start();
    let first = trimmed.as_bytes().first().copied();
    if matches!(first, Some(b'{') | Some(b'[')) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
            if let Ok(compact) = serde_json::to_string(&v) {
                return compact;
            }
        }
    }
    s.to_string()
}

fn extract_tool_result_content(content: &Option<serde_json::Value>) -> String {
    use serde_json::Value;

    let _ = match content {
        None => return String::new(),
        Some(Value::Null) => return String::new(),
        Some(Value::String(s)) => return s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts: Vec<String> = Vec::new();
            for item in arr {
                if let Value::Object(map) = item {
                    if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
                        parts.push(text.to_string());
                    }
                }
            }
            return parts.join("");
        }
        Some(_) => return String::new(),
    };

    #[allow(unreachable_code)]
    String::new()
}

fn extract_tool_result_content_and_images(
    content: &Option<serde_json::Value>,
) -> (String, Vec<KiroImage>) {
    use serde_json::Value;

    let Some(value) = content else {
        return (extract_tool_result_content(content), Vec::new());
    };

    let items: Vec<&Value> = match value {
        Value::Array(arr) => arr.iter().collect(),
        Value::Object(_) => vec![value],
        _ => return (extract_tool_result_content(content), Vec::new()),
    };

    let mut parts = Vec::new();
    let mut images = Vec::new();
    let mut saw_image = false;

    for item in items {
        let explicit_image = matches!(
            item.get("type").and_then(|v| v.as_str()),
            Some("image" | "image_url" | "input_image" | "file" | "input_file")
        );
        if explicit_image && let Some(processed_images) = extract_tool_result_image(item) {
            saw_image = true;
            images.extend(processed_images);
            continue;
        }

        match item {
            Value::Object(map) => {
                if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        parts.push(text.to_string());
                    }
                    continue;
                }
            }
            _ => {}
        }

        if let Some(processed_images) = extract_tool_result_image(item) {
            saw_image = true;
            images.extend(processed_images);
        }
    }

    if saw_image {
        return (parts.join(""), images);
    }

    (extract_tool_result_content(content), Vec::new())
}

fn extract_tool_result_image(item: &serde_json::Value) -> Option<Vec<KiroImage>> {
    let images = process_content_image_value(item);
    if !images.is_empty() {
        return Some(images);
    }

    if let Some(obj) = item.as_object()
        && obj.get("type").is_some()
    {
        let mut untyped = obj.clone();
        untyped.remove("type");
        let images = process_content_image_value(&serde_json::Value::Object(untyped));
        if !images.is_empty() {
            return Some(images);
        }
    }

    None
}

fn collect_tool_result_ids(
    tool_results: &[crate::kiro::model::requests::tool::ToolResult],
) -> std::collections::HashSet<String> {
    tool_results
        .iter()
        .filter_map(|tr| {
            let id = tr.tool_use_id.trim();
            (!id.is_empty()).then(|| id.to_string())
        })
        .collect()
}

fn current_tool_results_match_last_assistant(
    history: &[Message],
    current_tool_result_ids: &std::collections::HashSet<String>,
) -> bool {
    if current_tool_result_ids.is_empty() || history.is_empty() {
        return false;
    }
    let Some(Message::Assistant(last)) = history.last() else {
        return false;
    };
    let Some(tool_uses) = last.assistant_response_message.tool_uses.as_ref() else {
        return false;
    };
    if tool_uses.is_empty() {
        return false;
    }
    tool_uses
        .iter()
        .all(|tu| current_tool_result_ids.contains(&tu.tool_use_id))
}

/// Kiro API 工具名称最大长度限制
///
/// 工具名缩短阈值: 64 字节
const TOOL_NAME_MAX_LEN: usize = 64;

/// Kiro API 工具描述最大长度限制
///
/// 工具描述最大长度: 10237 字符
const MAX_TOOL_DESC_LEN: usize = 10237;

/// 将工具名称标准化为 camelCase
///
/// 工具名标准化规则:
/// Kiro 工具名必须是纯 camelCase（无下划线和短横线）。
/// 分隔符（_, -, 多下划线命名空间前缀）转换为 camelCase 边界。
fn sanitize_tool_name(name: &str) -> String {
    let parts: Vec<&str> = name
        .split(|c: char| c == '_' || c == '-')
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return "tool".to_string();
    }
    let mut result = String::with_capacity(name.len());
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            // 第一段：首字母小写
            let mut chars = part.chars();
            if let Some(first) = chars.next() {
                result.extend(first.to_lowercase());
                result.extend(chars);
            }
        } else {
            // 后续段：首字母大写
            let mut chars = part.chars();
            if let Some(first) = chars.next() {
                result.extend(first.to_uppercase());
                result.extend(chars);
            }
        }
    }
    if result.is_empty() {
        "tool".to_string()
    } else {
        result
    }
}

/// 缩短超长工具名称
///
/// 工具名缩短规则:
/// - MCP 工具: mcp__server__tool → mcp__tool
/// - 其他: 硬截断到 64 字节
fn shorten_tool_name(name: &str) -> String {
    if name.len() <= TOOL_NAME_MAX_LEN {
        return name.to_string();
    }
    // MCP fast-path: mcp__<server>__<tool> → mcp__<tool>
    if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some((_, last)) = rest.rsplit_once("__") {
            if !last.is_empty() {
                let candidate = format!("mcp__{}", last);
                if candidate.len() <= TOOL_NAME_MAX_LEN {
                    return candidate;
                }
            }
        }
    }
    truncate_to_utf8_boundary(name, TOOL_NAME_MAX_LEN).to_string()
}

fn truncate_to_utf8_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// 工具名称处理：Claude 路径先 camelCase 标准化再缩短；OpenAI 路径只缩短。
///
/// Claude 工具名使用 sanitize+shorten，OpenAI 工具名只使用 shorten。
fn map_tool_name(
    name: &str,
    tool_name_map: &mut HashMap<String, String>,
    preserve_tool_names: bool,
) -> String {
    let sanitized = if preserve_tool_names {
        name.to_string()
    } else {
        sanitize_tool_name(name)
    };
    let shortened = shorten_tool_name(&sanitized);
    if shortened != name {
        tool_name_map.insert(shortened.clone(), name.to_string());
    }
    shortened
}

/// 转换工具定义
///
/// 工具定义转换规则:
/// - Claude 工具名先 sanitizeToolName（camelCase）再 shortenToolName（64 字节截断）
/// - OpenAI 工具名只 shortenToolName（64 字节截断）
/// - 描述硬截断到 10237 字符
/// - Schema 通过 ensureObjectSchema + cleanSchema 清洗
fn convert_tools(
    tools: &Option<Vec<super::types::Tool>>,
    tool_name_map: &mut HashMap<String, String>,
    preserve_tool_names: bool,
) -> Vec<Tool> {
    let Some(tools) = tools else {
        return Vec::new();
    };

    tools
        .iter()
        .map(|t| {
            // 先 camelCase 标准化，再缩短超长名称。
            let sanitized_name = map_tool_name(&t.name, tool_name_map, preserve_tool_names);

            // 空描述使用标准化后的工具名。
            let description = if t.description.trim().is_empty() {
                format!("Tool: {}", sanitized_name)
            } else {
                t.description.clone()
            };

            // 硬截断描述到 MAX_TOOL_DESC_LEN 字符。
            let final_description = if description.len() > MAX_TOOL_DESC_LEN {
                format!("{}...", &description[..MAX_TOOL_DESC_LEN])
            } else {
                description
            };

            Tool {
                tool_specification: ToolSpecification {
                    name: sanitized_name,
                    description: final_description,
                    input_schema: InputSchema::from_json(normalize_json_schema(serde_json::json!(
                        t.input_schema
                    ))),
                },
            }
        })
        .collect()
}

/// 生成thinking标签前缀
///
/// Opus 4.7 特殊性：
/// - 不支持 `type: "enabled"` —— handlers 已自动降级为 `adaptive`
/// - 默认 `display: "omitted"` —— 不主动吐 thinking 文本，需显式声明 `summarized`
/// - instruction-following 严，加 `IMPORTANT` 兜底确保始终用 `<thinking>` 标签
pub(crate) fn generate_thinking_prefix(req: &MessagesRequest) -> Option<String> {
    let t = req.thinking.as_ref()?;
    let model_lower = req.model.to_lowercase();
    let is_opus_4_7 = model_lower.contains("opus")
        && (model_lower.contains("4-7") || model_lower.contains("4.7"));

    let thinking_type = t.normalized_type();
    match thinking_type.as_str() {
        "enabled" => Some(format!(
            "<thinking_mode>enabled</thinking_mode>\n<max_thinking_length>{}</max_thinking_length>",
            t.budget_tokens.unwrap_or(20000)
        )),
        "adaptive" => {
            let effort = req
                .output_config
                .as_ref()
                .map(|c| c.effort.as_str())
                .unwrap_or("high");
            let display = t.effective_display();
            let base = format!(
                "<thinking_mode>adaptive</thinking_mode><thinking_effort>{}</thinking_effort><thinking_display>{}</thinking_display>",
                effort, display
            );
            if is_opus_4_7 && display == "summarized" {
                Some(format!(
                    "{}\nIMPORTANT: Wrap your full reasoning inside <thinking>...</thinking> tags BEFORE the final answer. This wrapping is required even when adaptive thinking decides the task is simple — always emit at least a brief <thinking>...</thinking> block.",
                    base
                ))
            } else {
                Some(base)
            }
        }
        _ => None,
    }
}

/// 检查内容是否已包含thinking标签
fn has_thinking_tags(content: &str) -> bool {
    content.contains("<thinking_mode>") || content.contains("<max_thinking_length>")
}

/// 构建历史消息
///
/// # Arguments
/// * `req` - 原始请求，用于读取 `system`、`thinking` 等配置字段
/// * `messages` - 转换入口归一化后的消息切片，末尾必定是 user 消息。
///   调用方应始终使用此参数而非 `req.messages`。
/// * `model_id` - 已映射的 Kiro 模型 ID
fn build_history(
    req: &MessagesRequest,
    messages: &[super::types::Message],
    ctx: BuildHistoryContext<'_>,
) -> Result<BuildHistoryResult, ConversionError> {
    let BuildHistoryContext {
        model_id,
        prompt_filter,
        is_agentic,
        tool_name_map,
        preserve_tool_names,
    } = ctx;
    let mut history = Vec::new();

    // 生成thinking前缀（如果需要）
    let thinking_prefix = generate_thinking_prefix(req);

    // 1. 处理系统消息：只应用配置驱动的 applyPromptFilters，再拼接 thinking prefix。
    let base_system = req.system.as_ref().map(|system| {
        system
            .iter()
            .map(|s| apply_prompt_filters(prompt_filter, &s.text))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    });

    let needs_inject = thinking_prefix.is_some();

    let final_system = match base_system {
        Some(s) if !s.is_empty() => {
            let mut content = s;
            if let Some(ref prefix) = thinking_prefix {
                if !has_thinking_tags(&content) {
                    content = format!("{}\n{}", prefix, content);
                }
            }
            Some(content)
        }
        _ if needs_inject => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(ref prefix) = thinking_prefix {
                parts.push(prefix.clone());
            }
            let content = parts.join("\n");
            Some(content)
        }
        _ => None,
    };

    let has_system_priming = final_system.is_some();
    if let Some(content) = final_system {
        let user_msg = HistoryUserMessage::new(content, model_id);
        history.push(Message::User(user_msg));
        let assistant_msg = HistoryAssistantMessage::new("I will follow these instructions.");
        history.push(Message::Assistant(assistant_msg));
    }

    // Agentic 模型：追加专用系统提示
    if is_agentic {
        let user_msg = HistoryUserMessage::new(KIRO_AGENTIC_SYSTEM_PROMPT, model_id);
        history.push(Message::User(user_msg));

        let assistant_msg =
            HistoryAssistantMessage::new("I will work autonomously following these principles.");
        history.push(Message::Assistant(assistant_msg));
    }

    // 2. 处理常规消息历史
    // 最后一条消息作为 currentMessage，不加入历史
    // messages 末尾必定是 user，故直接截掉最后一条即可
    let history_end_index = messages.len().saturating_sub(1);

    // 收集并配对消息
    let mut user_buffer: Vec<&super::types::Message> = Vec::new();
    let mut assistant_buffer: Vec<&super::types::Message> = Vec::new();

    for msg in messages.iter().take(history_end_index) {
        if msg.role == "user" {
            // 先处理累积的 assistant 消息
            if !assistant_buffer.is_empty() {
                let merged = merge_assistant_messages(
                    &assistant_buffer,
                    tool_name_map,
                    preserve_tool_names,
                )?;
                history.push(Message::Assistant(merged));
                assistant_buffer.clear();
            }
            user_buffer.push(msg);
        } else if msg.role == "assistant" {
            // 先处理累积的 user 消息
            if !user_buffer.is_empty() {
                let merged_user = merge_user_messages(&user_buffer, model_id)?;
                history.push(Message::User(merged_user));
                user_buffer.clear();
            }
            // 只有 history 末尾是 User 时才允许接 assistant，
            // 否则该 assistant 是孤立的（无前置 user），静默丢弃避免上游 400
            if !matches!(history.last(), Some(Message::User(_))) {
                tracing::warn!("检测到无前置 user 的孤立 assistant 消息，已丢弃");
                continue;
            }
            // 累积 assistant 消息（支持连续多条）
            assistant_buffer.push(msg);
        }
    }

    // 处理末尾累积的 assistant 消息：同样要求紧邻前一条是 User
    if !assistant_buffer.is_empty() {
        if matches!(history.last(), Some(Message::User(_))) {
            let merged =
                merge_assistant_messages(&assistant_buffer, tool_name_map, preserve_tool_names)?;
            history.push(Message::Assistant(merged));
        } else {
            tracing::warn!(
                "末尾 assistant_buffer 无前置 user 配对（{} 条），已丢弃",
                assistant_buffer.len()
            );
        }
    }

    // 处理结尾的孤立 user 消息
    if !user_buffer.is_empty() {
        let merged_user = merge_user_messages(&user_buffer, model_id)?;
        history.push(Message::User(merged_user));
    }

    Ok(BuildHistoryResult {
        history,
        has_system_priming,
    })
}

/// 清洗历史消息
///
/// 核心逻辑：
/// 1. 构建 tool_use_id → tool_name 映射表
/// 2. 识别"活跃"工具轮次（最后一个 assistant 的 tool_uses 全部被当前 message 的 tool_results 覆盖）
/// 3. 非活跃 assistant 工具轮次：剥离 tool_uses，仅保留文本
/// 4. 用户 tool_results 轮次：将结构化结果叙述为纯文本
/// 5. 剥离被污染的工具调用文本
/// 6. 丢弃空心 assistant 轮次
/// 7. 丢弃连续重复的 user 轮次
/// 8. 重新修剪开头的 assistant 消息
pub fn sanitize_kiro_history(
    history: &mut Vec<Message>,
    current_tool_result_ids: &std::collections::HashSet<String>,
) {
    if history.is_empty() {
        return;
    }

    // 1. 构建 tool_use_id → tool_name 映射
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in history.iter() {
        if let Message::Assistant(assistant) = msg {
            if let Some(ref tool_uses) = assistant.assistant_response_message.tool_uses {
                for tu in tool_uses {
                    if !tu.tool_use_id.is_empty() && !tu.name.is_empty() {
                        tool_names.insert(tu.tool_use_id.clone(), tu.name.clone());
                    }
                }
            }
        }
    }

    // 2. 识别"活跃"工具轮次索引
    let mut active_idx: Option<usize> = None;
    if !current_tool_result_ids.is_empty() {
        if let Some(Message::Assistant(last)) = history.last() {
            if let Some(ref tool_uses) = last.assistant_response_message.tool_uses {
                if !tool_uses.is_empty() {
                    let all_covered = tool_uses
                        .iter()
                        .all(|tu| current_tool_result_ids.contains(&tu.tool_use_id));
                    if all_covered {
                        active_idx = Some(history.len() - 1);
                    }
                }
            }
        }
    }

    // 3-5. 处理每条消息
    for (i, msg) in history.iter_mut().enumerate() {
        match msg {
            Message::Assistant(assistant) => {
                let arm = &mut assistant.assistant_response_message;

                // 剥离被污染的工具调用文本
                if !arm.content.is_empty() {
                    arm.content = strip_polluted_tool_call_text(&arm.content);
                }

                // 非活跃工具轮次：剥离 tool_uses
                if let Some(ref tool_uses) = arm.tool_uses {
                    if !tool_uses.is_empty() && active_idx != Some(i) {
                        arm.tool_uses = None;
                    }
                }
            }
            Message::User(user) => {
                let uim = &mut user.user_input_message;
                let ctx = &mut uim.user_input_message_context;

                // 将结构化 tool_results 叙述为纯文本
                if !ctx.tool_results.is_empty() {
                    let narrated = narrate_tool_results(&ctx.tool_results, &tool_names);
                    uim.content = join_history_text(&uim.content, &narrated);
                    ctx.tool_results.clear();
                }

                // 剥离历史中的工具定义
                ctx.tools.clear();

                if uim.content.trim().is_empty() && uim.images.is_empty() {
                    uim.content = MINIMAL_FALLBACK_USER_CONTENT.to_string();
                }
            }
        }
    }

    // 6. 丢弃空心 assistant 轮次 + 7. 连续重复 user 轮次
    let mut cleaned: Vec<Message> = Vec::with_capacity(history.len());
    for msg in history.drain(..) {
        match &msg {
            Message::Assistant(assistant) => {
                let arm = &assistant.assistant_response_message;
                let content_trimmed = arm.content.trim();
                let has_tool_uses = arm.tool_uses.as_ref().is_some_and(|t| !t.is_empty());
                // 空心 assistant（无内容或仅 "." 且无 tool_uses）→ 丢弃
                // "." 是 minimal fallback user content 占位符。
                if (content_trimmed.is_empty() || content_trimmed == MINIMAL_FALLBACK_USER_CONTENT)
                    && !has_tool_uses
                {
                    continue;
                }
            }
            Message::User(user) => {
                let content = user.user_input_message.content.trim().to_string();
                // 连续重复 user 轮次 → 丢弃
                if !content.is_empty() {
                    if let Some(Message::User(prev)) = cleaned.last() {
                        if prev.user_input_message.content.trim() == content
                            && user.user_input_message.images.is_empty()
                        {
                            continue;
                        }
                    }
                }
            }
        }
        cleaned.push(msg);
    }

    // 8. 重新修剪开头的 assistant 消息
    while matches!(cleaned.first(), Some(Message::Assistant(_))) {
        cleaned.remove(0);
    }

    *history = cleaned;
}

/// 叙述 tool_results 为纯文本
///
/// Tool result 叙述规则:
/// 格式: "Tool results:\n\n[tool_name] content\n\n[tool_name] content"
/// 空内容: "[tool_name] (no output)"
fn narrate_tool_results(
    tool_results: &[crate::kiro::model::requests::tool::ToolResult],
    tool_names: &std::collections::HashMap<String, String>,
) -> String {
    let mut parts = Vec::new();
    for tr in tool_results {
        // 如果 tool_use_id 不在映射中，省略 [name] 前缀。
        let tool_name_opt = tool_names.get(&tr.tool_use_id).map(|s| s.as_str());
        // 提取文本内容，跳过空白文本。
        let content_text: String = tr
            .content
            .iter()
            .filter_map(|m| m.get("text").and_then(|v| v.as_str()))
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        // 有名称时用 [name] 前缀，无名称时省略前缀。
        match tool_name_opt {
            Some(name) => {
                if content_text.is_empty() {
                    parts.push(format!("[{}] (no output)", name));
                } else {
                    parts.push(format!("[{}] {}", name, content_text));
                }
            }
            None => {
                if content_text.is_empty() {
                    parts.push("(no output)".to_string());
                } else {
                    parts.push(content_text);
                }
            }
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("Tool results:\n\n{}", parts.join("\n\n"))
    }
}

fn build_tool_results_continuation(
    tool_results: &[crate::kiro::model::requests::tool::ToolResult],
) -> String {
    if tool_results.is_empty() {
        return MINIMAL_FALLBACK_USER_CONTENT.to_string();
    }

    let mut parts = Vec::with_capacity(tool_results.len());
    for tr in tool_results {
        for content in &tr.content {
            if let Some(text) = content.get("text").and_then(|v| v.as_str()) {
                let text = text.trim();
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }

    if parts.is_empty() {
        return MINIMAL_FALLBACK_USER_CONTENT.to_string();
    }

    let joined = format!(
        "{}\n\n{}",
        TOOL_RESULTS_CONTINUATION_PREFIX,
        parts.join("\n\n")
    );
    if joined.len() > TOOL_RESULTS_CONTINUATION_MAX_LEN {
        let cut = joined
            .char_indices()
            .map(|(idx, _)| idx)
            .take_while(|idx| *idx <= TOOL_RESULTS_CONTINUATION_MAX_LEN)
            .last()
            .unwrap_or(0);
        joined[..cut].to_string()
    } else {
        joined
    }
}

fn join_history_text(existing: &str, narrated: &str) -> String {
    let existing = existing.trim();
    let narrated = narrated.trim();
    match (existing.is_empty(), narrated.is_empty()) {
        (false, false) => format!("{}\n\n{}", existing, narrated),
        (true, false) => narrated.to_string(),
        (false, true) => existing.to_string(),
        (true, true) => String::new(),
    }
}

/// 剥离被污染的工具调用文本
///
/// 清理历史 assistant 工具调用污染文本:
/// - 移除 `[Called tool ...]` 模式（不仅在行首，也处理嵌入的情况）
/// - 折叠 3+ 连续空行为 2 个空行
fn strip_polluted_tool_call_text(content: &str) -> String {
    use std::sync::OnceLock;
    // 包级别静态编译的正则。
    static POLLUTED_RE: OnceLock<regex::Regex> = OnceLock::new();
    static BLANK_RE: OnceLock<regex::Regex> = OnceLock::new();

    let polluted =
        POLLUTED_RE.get_or_init(|| regex::Regex::new(r"\[Called tool [^\]]*\]").unwrap());
    let blank = BLANK_RE.get_or_init(|| regex::Regex::new(r"\n{3,}").unwrap());

    // 快速路径：无匹配则直接返回
    if !content.contains("[Called tool ") {
        return content.to_string();
    }
    let cleaned = polluted.replace_all(content, "");
    let result = blank.replace_all(&cleaned, "\n\n");
    result.trim().to_string()
}

/// 合并多个 user 消息
fn merge_user_messages(
    messages: &[&super::types::Message],
    model_id: &str,
) -> Result<HistoryUserMessage, ConversionError> {
    let mut content_parts = Vec::new();
    let mut all_images = Vec::new();
    let mut all_tool_results = Vec::new();

    for msg in messages {
        let (text, images, tool_results) = process_message_content(&msg.content)?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        all_images.extend(images);
        all_tool_results.extend(tool_results);
    }

    let content = content_parts.join("\n");
    let final_content =
        if content.trim().is_empty() && all_images.is_empty() && all_tool_results.is_empty() {
            tracing::warn!("history user 消息为空，使用占位符修复");
            MINIMAL_FALLBACK_USER_CONTENT.to_string()
        } else {
            content
        };
    let mut user_msg = UserMessage::new(&final_content, model_id);

    if !all_images.is_empty() {
        user_msg = user_msg.with_images(all_images);
    }

    if !all_tool_results.is_empty() {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(all_tool_results);
        user_msg = user_msg.with_context(ctx);
    }

    Ok(HistoryUserMessage {
        user_input_message: user_msg,
    })
}

/// 转换 assistant 消息
fn convert_assistant_message(
    msg: &super::types::Message,
    tool_name_map: &mut HashMap<String, String>,
    preserve_tool_names: bool,
) -> Result<HistoryAssistantMessage, ConversionError> {
    let mut thinking_content = String::new();
    let mut text_content = String::new();
    let mut tool_uses = Vec::new();

    match &msg.content {
        serde_json::Value::String(s) => {
            text_content = s.clone();
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = ContentBlock::deserialize(item) {
                    match block.block_type.as_str() {
                        "thinking" => {
                            if let Some(thinking) = block.thinking {
                                thinking_content.push_str(&thinking);
                            }
                        }
                        "text" => {
                            if let Some(text) = block.text {
                                text_content.push_str(&text);
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (block.id, block.name) {
                                // input 必须是 JSON Object；客户端传 string/array/null 时回退 `{}`，
                                // 避免上游 400 "malformed message/tool sequences"
                                let input = match block.input {
                                    Some(serde_json::Value::Object(_)) => block.input.unwrap(),
                                    _ => serde_json::json!({}),
                                };
                                let mapped_name =
                                    map_tool_name(&name, tool_name_map, preserve_tool_names);
                                tool_uses
                                    .push(ToolUseEntry::new(id, mapped_name).with_input(input));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    let final_content = if !thinking_content.is_empty() {
        if !text_content.is_empty() {
            format!(
                "<thinking>{}</thinking>\n\n{}",
                thinking_content, text_content
            )
        } else {
            format!("<thinking>{}</thinking>", thinking_content)
        }
    } else {
        text_content
    };

    let mut assistant = AssistantMessage::new(final_content);
    if !tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(tool_uses);
    }

    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 合并多个连续的 assistant 消息为一条
/// 用于处理网络不稳定时产生的连续 assistant 消息（Issue #79）
fn merge_assistant_messages(
    messages: &[&super::types::Message],
    tool_name_map: &mut HashMap<String, String>,
    preserve_tool_names: bool,
) -> Result<HistoryAssistantMessage, ConversionError> {
    assert!(!messages.is_empty());
    if messages.len() == 1 {
        return convert_assistant_message(messages[0], tool_name_map, preserve_tool_names);
    }

    let mut all_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();

    for msg in messages {
        let converted = convert_assistant_message(msg, tool_name_map, preserve_tool_names)?;
        let am = converted.assistant_response_message;
        if !am.content.trim().is_empty() {
            content_parts.push(am.content);
        }
        if let Some(tus) = am.tool_uses {
            all_tool_uses.extend(tus);
        }
    }

    let content = content_parts.join("\n\n");

    let mut assistant = AssistantMessage::new(content);
    if !all_tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(all_tool_uses);
    }
    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_schema_top_level_defaults() {
        // 只确保 type: "object"，不注入 properties/required/additionalProperties。
        let out = normalize_json_schema(serde_json::json!({}));
        assert_eq!(out["type"], "object");
        assert!(
            out.get("properties").is_none(),
            "schema cleaner must not inject properties"
        );
        assert!(
            out.get("required").is_none(),
            "schema cleaner must not inject required"
        );
        assert!(
            out.get("additionalProperties").is_none(),
            "schema cleaner removes additionalProperties"
        );
    }

    #[test]
    fn test_normalize_schema_required_null_top_level() {
        // required: null -> 删除
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "required": null
        }));
        assert!(out.get("required").is_none(), "required:null 应被删除");
    }

    #[test]
    fn test_normalize_schema_required_empty_array() {
        // required: [] -> 删除
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "required": []
        }));
        assert!(out.get("required").is_none(), "required:[] 应被删除");
    }

    #[test]
    fn test_normalize_schema_required_keeps_non_empty() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "required": ["a", "b"]
        }));
        let req = out["required"].as_array().unwrap();
        assert_eq!(req.len(), 2);
        assert_eq!(req[0], "a");
        assert_eq!(req[1], "b");
    }

    #[test]
    fn test_normalize_schema_recurses_properties_required_null() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "foo": {
                    "type": "object",
                    "required": null,
                    "properties": {"x": {"type": "string"}}
                }
            }
        }));
        let foo = &out["properties"]["foo"];
        assert!(foo.get("required").is_none(), "嵌套 required:null 应被删除");
    }

    #[test]
    fn test_normalize_schema_recurses_properties_required_empty() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "foo": {
                    "type": "object",
                    "required": []
                }
            }
        }));
        assert!(out["properties"]["foo"].get("required").is_none());
    }

    #[test]
    fn test_normalize_schema_recurses_items() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "list": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": null
                    }
                }
            }
        }));
        assert!(out["properties"]["list"]["items"].get("required").is_none());
    }

    #[test]
    fn test_normalize_schema_recurses_items_array() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "tuple": {
                    "type": "array",
                    "items": [
                        {"type": "object", "required": null},
                        {"type": "string"}
                    ]
                }
            }
        }));
        let arr = out["properties"]["tuple"]["items"].as_array().unwrap();
        assert!(arr[0].get("required").is_none());
    }

    #[test]
    fn test_normalize_schema_recurses_all_of() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "allOf": [
                {"type": "object", "required": null},
                {"type": "object", "required": []}
            ]
        }));
        let arr = out["allOf"].as_array().unwrap();
        assert!(arr[0].get("required").is_none());
        assert!(arr[1].get("required").is_none());
    }

    #[test]
    fn test_normalize_schema_recurses_one_of_any_of() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "oneOf": [{"type": "object", "required": null}],
            "anyOf": [{"type": "object", "required": null}]
        }));
        assert!(out["oneOf"][0].get("required").is_none());
        assert!(out["anyOf"][0].get("required").is_none());
    }

    #[test]
    fn test_normalize_schema_additional_properties_always_removed() {
        // additionalProperties 无条件删除。
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "required": null
            }
        }));
        assert!(
            out.get("additionalProperties").is_none(),
            "additionalProperties 应被无条件删除"
        );
    }

    #[test]
    fn test_normalize_schema_additional_properties_invalid_dropped_in_nested() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "foo": {
                    "type": "object",
                    "additionalProperties": "wrong"
                }
            }
        }));
        assert!(
            out["properties"]["foo"]
                .get("additionalProperties")
                .is_none(),
            "嵌套层非法 additionalProperties 应被删除（不注入兜底）"
        );
    }

    #[test]
    fn test_normalize_schema_deep_nesting() {
        let out = normalize_json_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": null,
                                "properties": {
                                    "deep": {
                                        "type": "object",
                                        "required": []
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }));
        let item = &out["properties"]["outer"]["properties"]["inner"]["items"];
        assert!(item.get("required").is_none());
        assert!(item["properties"]["deep"].get("required").is_none());
    }

    #[test]
    fn test_sanitize_tool_name_camel_cases_expected_inputs() {
        assert_eq!(sanitize_tool_name("short_name"), "shortName");
        assert_eq!(sanitize_tool_name("FOO_BAR-baz"), "fOOBARBaz");
        assert_eq!(sanitize_tool_name("mcp__server__tool"), "mcpServerTool");
        assert_eq!(sanitize_tool_name("__--"), "tool");
    }

    #[test]
    fn test_shorten_mcp_uses_last_segment_when_fits() {
        let long = format!("mcp__filesystem__{}", "a".repeat(55));
        assert!(long.len() > TOOL_NAME_MAX_LEN);
        let short = shorten_tool_name(&long);
        assert_eq!(short, format!("mcp__{}", "a".repeat(55)));
        assert!(short.len() <= TOOL_NAME_MAX_LEN);
    }

    #[test]
    fn test_shorten_mcp_uses_last_segment_with_multi_double_underscore() {
        let long = format!("mcp__group__server__{}", "x".repeat(50));
        let short = shorten_tool_name(&long);
        assert_eq!(short, format!("mcp__{}", "x".repeat(50)));
    }

    #[test]
    fn test_shorten_mcp_hard_truncates_when_last_segment_too_long() {
        let long = format!("mcp__server__{}", "z".repeat(70));
        let short = shorten_tool_name(&long);
        assert_eq!(short, long[..TOOL_NAME_MAX_LEN]);
    }

    #[test]
    fn test_shorten_mcp_no_double_underscore_hard_truncates() {
        let long = format!("mcp__{}", "y".repeat(70));
        let short = shorten_tool_name(&long);
        assert_eq!(short, long[..TOOL_NAME_MAX_LEN]);
    }

    #[test]
    fn test_shorten_non_mcp_hard_truncates() {
        let long = "x".repeat(80);
        let short = shorten_tool_name(&long);
        assert_eq!(short, long[..TOOL_NAME_MAX_LEN]);
    }

    #[test]
    fn test_shorten_mcp_empty_last_segment_hard_truncates() {
        let long = format!("mcp__{}__", "q".repeat(70));
        let short = shorten_tool_name(&long);
        assert_eq!(short, long[..TOOL_NAME_MAX_LEN]);
    }

    fn make_tool(name: &str, description: &str) -> super::super::types::Tool {
        super::super::types::Tool {
            tool_type: None,
            name: name.to_string(),
            description: description.to_string(),
            input_schema: HashMap::new(),
            max_uses: None,
            cache_control: None,
        }
    }

    fn make_tool_with_schema(
        name: &str,
        description: &str,
        schema: serde_json::Value,
    ) -> super::super::types::Tool {
        let input_schema = schema
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        super::super::types::Tool {
            tool_type: None,
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
            max_uses: None,
            cache_control: None,
        }
    }

    #[test]
    fn test_convert_tools_hard_truncates_long_description() {
        // 描述超过 10237 字符时硬截断 + "..."。
        let mut map = HashMap::new();
        let tools = Some(vec![make_tool("LongTool", &"x".repeat(11000))]);
        let out = convert_tools(&tools, &mut map, false);
        assert_eq!(out.len(), 1);
        assert!(out[0].tool_specification.description.len() <= MAX_TOOL_DESC_LEN + 3); // +3 for "..."
        assert!(out[0].tool_specification.description.ends_with("..."));
    }

    #[test]
    fn test_convert_tools_keeps_short_description_inline() {
        let mut map = HashMap::new();
        let tools = Some(vec![make_tool("ShortTool", "small desc")]);
        let out = convert_tools(&tools, &mut map, false);
        assert_eq!(out[0].tool_specification.description, "small desc");
    }

    #[test]
    fn test_convert_tools_preserves_web_search() {
        let mut map = HashMap::new();
        let mut web_search = make_tool("web_search", "");
        web_search.tool_type = Some("web_search_20250305".to_string());
        let tools = Some(vec![web_search, make_tool("Read", "read files")]);

        let out = convert_tools(&tools, &mut map, false);

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].tool_specification.name, "webSearch");
        assert_eq!(out[0].tool_specification.description, "Tool: webSearch");
        assert_eq!(out[1].tool_specification.name, "read");
    }

    #[test]
    fn test_convert_tools_write_description_is_not_xkiro_augmented() {
        let mut map = HashMap::new();
        let tools = Some(vec![make_tool("Write", "write files")]);

        let out = convert_tools(&tools, &mut map, false);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tool_specification.description, "write files");
        assert!(
            !out[0]
                .tool_specification
                .description
                .contains("CHUNKED WRITE PROTOCOL")
        );
    }

    #[test]
    fn test_convert_tools_non_object_schema_becomes_object() {
        let mut map = HashMap::new();
        let tools = Some(vec![make_tool_with_schema(
            "BadSchema",
            "desc",
            serde_json::json!("not an object"),
        )]);

        let out = convert_tools(&tools, &mut map, false);

        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].tool_specification.input_schema.json,
            serde_json::json!({"type": "object"})
        );
    }

    #[test]
    fn test_convert_tools_preserves_desc_under_limit() {
        let mut map = HashMap::new();
        let desc = "y".repeat(10000); // under 10237 limit
        let tools = Some(vec![make_tool("Big", &desc)]);
        let out = convert_tools(&tools, &mut map, false);
        assert_eq!(out[0].tool_specification.description, desc);
    }

    #[test]
    fn test_convert_tools_does_not_apply_xkiro_bulk_tool_compression() {
        let mut map = HashMap::new();
        let tools = Some(
            (0..15)
                .map(|idx| {
                    make_tool_with_schema(
                        &format!("Tool{idx}"),
                        &"d".repeat(2000),
                        serde_json::json!({
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "schema descriptions are part of the tool contract"
                                }
                            }
                        }),
                    )
                })
                .collect::<Vec<_>>(),
        );

        let out = convert_tools(&tools, &mut map, false);

        assert_eq!(out.len(), 15);
        assert_eq!(out[0].tool_specification.description, "d".repeat(2000));
        assert_eq!(
            out[0].tool_specification.input_schema.json["properties"]["path"]["description"],
            "schema descriptions are part of the tool contract"
        );
    }

    #[test]
    fn test_convert_tools_mixed_long_short() {
        let mut map = HashMap::new();
        let tools = Some(vec![
            make_tool("Short", "ok"),
            make_tool("Long", &"z".repeat(11000)),
            make_tool("Short2", "fine"),
        ]);
        let out = convert_tools(&tools, &mut map, false);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].tool_specification.description, "ok");
        assert!(out[1].tool_specification.description.ends_with("..."));
        assert_eq!(out[2].tool_specification.description, "fine");
    }

    #[test]
    fn test_convert_tools_threshold_boundary_keeps_inline() {
        let mut map = HashMap::new();
        // 描述恰好 = 阈值字符数（不超） → 不抽离
        let desc = "a".repeat(100);
        let tools = Some(vec![make_tool("Boundary", &desc)]);
        let out = convert_tools(&tools, &mut map, false);
        assert_eq!(out[0].tool_specification.description, desc);
    }

    #[test]
    fn test_convert_tools_threshold_boundary_plus_one_truncates() {
        // 超过 10237 字符时硬截断。
        let mut map = HashMap::new();
        let desc = "a".repeat(MAX_TOOL_DESC_LEN + 1);
        let tools = Some(vec![make_tool("Boundary", &desc)]);
        let out = convert_tools(&tools, &mut map, false);
        assert!(out[0].tool_specification.description.ends_with("..."));
        assert!(out[0].tool_specification.description.len() <= MAX_TOOL_DESC_LEN + 3);
    }

    #[test]
    fn test_convert_tools_empty_returns_empty() {
        let mut map = HashMap::new();
        let out = convert_tools(&None, &mut map, false);
        assert!(out.is_empty());
    }

    #[test]
    fn test_convert_tools_long_with_mcp_shortening() {
        // 先 camelCase 标准化，再缩短。
        // mcp__server__kkk... → sanitize → mcpServerKkk... → 缩短（非 MCP 前缀，硬截断）
        let mut map = HashMap::new();
        let long_name = format!("mcp__server__{}", "k".repeat(60));
        assert!(long_name.len() > TOOL_NAME_MAX_LEN);
        let tools = Some(vec![make_tool(&long_name, &"y".repeat(100))]);
        let out = convert_tools(&tools, &mut map, false);
        let sanitized = &out[0].tool_specification.name;
        assert!(sanitized.len() <= TOOL_NAME_MAX_LEN);
        // camelCase 后 mcp__ 前缀消失，变为 mcpServerKkk...
        assert!(sanitized.starts_with("mcpServer"));
    }

    #[test]
    fn test_write_tool_does_not_inject_xkiro_chunked_system_policy() {
        use super::super::types::{
            Message as AnthropicMessage, SystemMessage, Tool as AnthropicTool,
        };

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("write it"),
            }],
            system: Some(vec![SystemMessage {
                text: "You are concise.".to_string(),
                block_type: None,
                cache_control: None,
            }]),
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: "Write".to_string(),
                description: "write files".to_string(),
                input_schema: HashMap::new(),
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .unwrap();
        let payload = serde_json::to_string(&result.conversation_state).unwrap();

        assert!(payload.contains("You are concise."));
        assert!(!payload.contains("CHUNKED WRITE PROTOCOL"));
        assert!(!payload.contains("NEVER write more than 50 lines"));
    }

    #[test]
    fn test_map_model_handles_aliases_and_thinking_suffix() {
        let cases = [
            ("claude-opus-4-8", "claude-opus-4.8"),
            ("claude-opus-4.8", "claude-opus-4.8"),
            ("claude-opus-4-7", "claude-opus-4.7"),
            ("claude-opus-4.7", "claude-opus-4.7"),
            ("claude-sonnet-4-6", "claude-sonnet-4.6"),
            ("claude-sonnet-4.6", "claude-sonnet-4.6"),
            ("claude-haiku-4-5", "claude-haiku-4.5"),
            ("claude-haiku-4.5", "claude-haiku-4.5"),
            ("claude-sonnet-5-0", "claude-sonnet-5.0"),
            ("claude-sonnet-4", "claude-sonnet-4"),
            ("claude-sonnet-4-20250514", "claude-sonnet-4"),
            ("claude-3-5-sonnet", "claude-sonnet-4.5"),
            ("claude-3-opus", "claude-sonnet-4.5"),
            ("claude-3-sonnet", "claude-sonnet-4"),
            ("claude-3-haiku", "claude-haiku-4.5"),
            ("gpt-4-turbo", "claude-sonnet-4.5"),
            ("gpt-4o", "claude-sonnet-4.5"),
            ("gpt-4", "claude-sonnet-4.5"),
            ("gpt-3.5-turbo", "claude-sonnet-4.5"),
            ("claude-opus-4-8-thinking", "claude-opus-4.8"),
            ("claude-sonnet-4.5-thinking", "claude-sonnet-4.5"),
            ("claude-3-5-sonnet-thinking", "claude-sonnet-4.5"),
            ("some-other-model", "some-other-model"),
            ("claude-opux-4-8", "claude-opux-4-8"),
        ];

        for (input, expected) in cases {
            assert_eq!(map_model(input), expected, "{input}");
        }
    }

    #[test]
    fn test_map_model_honors_custom_thinking_suffix() {
        assert_eq!(
            map_model_with_thinking_suffix("claude-opus-4-8-think", "-think"),
            "claude-opus-4.8"
        );
        assert_eq!(
            map_model_with_thinking_suffix("claude-sonnet-4.5-think", "-think"),
            "claude-sonnet-4.5"
        );
    }

    #[test]
    fn test_convert_request_honors_custom_thinking_suffix() {
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": "hello"}
        ]));
        let mut req = req;
        req.model = "claude-opus-4-8-think".to_string();
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();

        let result =
            convert_request_with_thinking_suffix(&req, &cfg, &pf, false, "-think").unwrap();

        assert_eq!(
            result
                .conversation_state
                .current_message
                .user_input_message
                .model_id,
            "claude-opus-4.8"
        );
    }

    #[test]
    fn test_convert_request_preserves_user_content_until_payload_truncation() {
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": "line1\n\n\nline2"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "older context that would exceed the old history char budget"},
            {"role": "assistant", "content": "still here"},
            {"role": "user", "content": "current"}
        ]));
        let cfg = crate::model::config::CompressionConfig {
            max_request_body_bytes: 1,
        };
        let pf = crate::model::config::PromptFilterConfig::default();

        let result = convert_request(&req, &cfg, &pf, false).expect("convert");

        assert!(
            result.conversation_state.history.len() > 1,
            "conversion must not apply the removed history rewriter"
        );
        assert!(
            result.conversation_state.history.iter().any(|msg| matches!(
                msg,
                Message::User(user)
                    if user.user_input_message.content.contains("line1\n\n\nline2")
            )),
            "conversion preserves user text until handler-level payload truncation"
        );
    }

    #[test]
    fn test_convert_request_preserves_system_content_for_prompt_filtering() {
        let req: super::super::types::MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "system": "<thinking_mode>enabled</thinking_mode>\n<execution_discipline>keep this</execution_discipline>\n[Context: Current time is 2026-06-27]",
            "messages": [
                {"role": "user", "content": "current"}
            ],
        }))
        .expect("test fixture should parse");
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();

        let result = convert_request(&req, &cfg, &pf, false).expect("convert");

        let Message::User(system_message) = &result.conversation_state.history[0] else {
            panic!("system priming should be a user history message");
        };
        let content = &system_message.user_input_message.content;
        assert!(content.contains("<thinking_mode>enabled</thinking_mode>"));
        assert!(content.contains("<execution_discipline>keep this</execution_discipline>"));
        assert!(content.contains("[Context: Current time is 2026-06-27]"));
    }

    #[test]
    fn apply_multiplier_milli_pure_arithmetic() {
        // 默认 1.0（1000‰）：原值返回，无漂移
        assert_eq!(apply_multiplier_milli(250_000, 1000), 250_000);
        // 放大：250K * 4.0 = 1M（把 opus-4-8 的 ~250K 有效上下文抬到 CC 的 1M 基准）
        assert_eq!(apply_multiplier_milli(250_000, 4000), 1_000_000);
        // 缩小
        assert_eq!(apply_multiplier_milli(200_000, 500), 100_000);
        // clamp 下限：不产生 0/负
        assert_eq!(apply_multiplier_milli(1, 100), 1);
        // 溢出安全：i64 中间量 + clamp 到 i32::MAX
        assert_eq!(apply_multiplier_milli(i32::MAX, 10_000), i32::MAX);
    }

    #[test]
    fn multiplier_applies_to_all_three_sources_including_raw_frame() {
        let _g = CONTEXT_GLOBAL_TEST_GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_context_usage_multiplier(4.0);

        // 三条来源分别驱动 apply_context_usage_multiplier：pct 换算 / 上游 raw 帧 / 本地估算
        // 都汇合到同一个整数值上，系数统一放大，raw 帧不再旁路。
        assert_eq!(apply_context_usage_multiplier(250_000), 1_000_000);
        assert_eq!(apply_context_usage_multiplier(50_000), 200_000);

        // 复位，避免污染其他串行测试
        set_context_usage_multiplier(1.0);
    }

    #[test]
    fn test_context_window_size_model_matrix() {
        let cases = [
            ("claude-opus-4.8", 1_000_000),
            ("claude-opus-4-8", 1_000_000),
            ("claude-opus-4.7", 1_000_000),
            ("claude-opus-4.6", 1_000_000),
            ("claude-sonnet-4.6", 1_000_000),
            ("claude-opus-4.8-thinking", 1_000_000),
            ("CLAUDE-OPUS-4.8", 1_000_000),
            ("claude-opus-4.9", 1_000_000),
            ("claude-sonnet-5.0", 1_000_000),
            ("claude-opus-4.5", 200_000),
            ("claude-sonnet-4.5", 200_000),
            ("claude-sonnet-4", 200_000),
            ("claude-haiku-4.5", 200_000),
            ("claude-3-5-sonnet", 200_000),
            ("unknown-model", 200_000),
        ];

        for (model, expected) in cases {
            assert_eq!(get_context_window_size(model), expected, "{model}");
        }
    }

    #[test]
    fn test_generate_thinking_prefix_enabled() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-5".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: Some(crate::anthropic::types::Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: Some(12345),
                display: None,
            }),
            output_config: None,
            metadata: None,
        };
        let prefix = generate_thinking_prefix(&req).unwrap();
        assert_eq!(
            prefix,
            "<thinking_mode>enabled</thinking_mode>\n<max_thinking_length>12345</max_thinking_length>"
        );
        assert!(!prefix.contains("IMPORTANT"));
    }

    #[test]
    fn test_generate_thinking_prefix_adaptive_4_7_summarized_appends_important() {
        let req = MessagesRequest {
            model: "claude-opus-4-7".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: Some(crate::anthropic::types::Thinking {
                thinking_type: "adaptive".to_string(),
                budget_tokens: None,
                display: Some("summarized".to_string()),
            }),
            output_config: Some(crate::anthropic::types::OutputConfig {
                effort: "high".to_string(),
            }),
            metadata: None,
        };
        let prefix = generate_thinking_prefix(&req).unwrap();
        assert!(prefix.contains("<thinking_mode>adaptive</thinking_mode>"));
        assert!(prefix.contains("<thinking_effort>high</thinking_effort>"));
        assert!(prefix.contains("<thinking_display>summarized</thinking_display>"));
        assert!(prefix.contains("IMPORTANT: Wrap your full reasoning"));
    }

    #[test]
    fn test_generate_thinking_prefix_adaptive_4_7_omitted_no_important() {
        let req = MessagesRequest {
            model: "claude-opus-4-7".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: Some(crate::anthropic::types::Thinking {
                thinking_type: "adaptive".to_string(),
                budget_tokens: None,
                display: Some("omitted".to_string()),
            }),
            output_config: None,
            metadata: None,
        };
        let prefix = generate_thinking_prefix(&req).unwrap();
        assert!(prefix.contains("<thinking_display>omitted</thinking_display>"));
        assert!(!prefix.contains("IMPORTANT"));
    }

    #[test]
    fn test_generate_thinking_prefix_trims_type_and_display() {
        let req = MessagesRequest {
            model: "claude-opus-4-7".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: Some(crate::anthropic::types::Thinking {
                thinking_type: " Adaptive ".to_string(),
                budget_tokens: None,
                display: Some(" omitted ".to_string()),
            }),
            output_config: None,
            metadata: None,
        };
        let prefix = generate_thinking_prefix(&req).unwrap();
        assert!(prefix.contains("<thinking_mode>adaptive</thinking_mode>"));
        assert!(prefix.contains("<thinking_display>omitted</thinking_display>"));
        assert!(!prefix.contains("IMPORTANT"));
    }

    #[test]
    fn test_generate_thinking_prefix_adaptive_4_6_no_important() {
        let req = MessagesRequest {
            model: "claude-opus-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: Some(crate::anthropic::types::Thinking {
                thinking_type: "adaptive".to_string(),
                budget_tokens: None,
                display: None,
            }),
            output_config: None,
            metadata: None,
        };
        let prefix = generate_thinking_prefix(&req).unwrap();
        assert!(prefix.contains("<thinking_mode>adaptive</thinking_mode>"));
        assert!(prefix.contains("<thinking_display>summarized</thinking_display>"));
        assert!(!prefix.contains("IMPORTANT"));
    }

    #[test]
    fn test_generate_thinking_prefix_none_when_thinking_absent() {
        let req = MessagesRequest {
            model: "claude-opus-4-7".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        assert!(generate_thinking_prefix(&req).is_none());
    }

    #[test]
    fn test_map_model_thinking_suffix_haiku() {
        let result = map_model("claude-haiku-4-5-20251001-thinking");
        assert_eq!(result, "claude-haiku-4.5-20251001");
    }

    #[test]
    fn test_determine_chat_trigger_type() {
        // 无工具时返回 MANUAL
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        assert_eq!(determine_chat_trigger_type(&req), "MANUAL");
    }

    #[test]
    fn test_collect_history_tool_names() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 创建包含工具使用的历史消息
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
            ToolUseEntry::new("tool-2", "write")
                .with_input(serde_json::json!({"path": "/out.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_names = collect_history_tool_names(&history);
        assert_eq!(tool_names.len(), 2);
        assert!(tool_names.contains(&"read".to_string()));
        assert!(tool_names.contains(&"write".to_string()));
    }

    #[test]
    fn test_create_placeholder_tool() {
        let tool = create_placeholder_tool("my_custom_tool");

        assert_eq!(tool.tool_specification.name, "my_custom_tool");
        assert!(!tool.tool_specification.description.is_empty());

        // 验证 JSON 序列化正确
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"my_custom_tool\""));
        assert!(!json.contains("additionalProperties"));
        assert!(!json.contains("\"required\""));
    }

    #[test]
    fn test_shorten_tool_name_deterministic() {
        let long_name =
            "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
        assert!(long_name.len() > TOOL_NAME_MAX_LEN);

        let short1 = shorten_tool_name(long_name);
        let short2 = shorten_tool_name(long_name);
        assert_eq!(short1, short2, "相同输入应产生相同的短名称");
        assert!(
            short1.len() <= TOOL_NAME_MAX_LEN,
            "短名称长度应 <= 64，实际 {}",
            short1.len()
        );
    }

    #[test]
    fn test_shorten_tool_name_allows_hard_truncation_collision() {
        let prefix = "tool_name_that_is_very_long_and_exceeds_the_kiro_limit_with_same_prefix_";
        let name_a = format!("{prefix}a");
        let name_b = format!("{prefix}b");
        let short_a = shorten_tool_name(&name_a);
        let short_b = shorten_tool_name(&name_b);
        assert_eq!(short_a, short_b);
    }

    #[test]
    fn test_map_tool_name_short_passthrough() {
        // 所有名称先 camelCase 标准化。
        let mut map = HashMap::new();
        let result = map_tool_name("short_name", &mut map, false);
        assert_eq!(result, "shortName", "应转为 camelCase");
        // camelCase 后名称不同，会记录映射
        assert_eq!(map.get("shortName"), Some(&"short_name".to_string()));
    }

    #[test]
    fn test_map_tool_name_long_creates_mapping() {
        let mut map = HashMap::new();
        let long_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_64";
        let result = map_tool_name(long_name, &mut map, false);
        assert!(result.len() <= TOOL_NAME_MAX_LEN);
        assert_eq!(map.get(&result), Some(&long_name.to_string()));
    }

    #[test]
    fn test_tool_name_mapping_in_convert_request() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name =
            "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_64";
        assert!(long_tool_name.len() > TOOL_NAME_MAX_LEN);

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .unwrap();

        // 应该有映射
        assert_eq!(result.tool_name_map.len(), 1);

        // 映射中的值应该是原始名称
        let (short, original) = result.tool_name_map.iter().next().unwrap();
        assert_eq!(original, long_tool_name);
        assert!(short.len() <= TOOL_NAME_MAX_LEN);

        // Kiro 请求中的工具名应该是短名称
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;
        assert_eq!(tools[0].tool_specification.name, *short);
    }

    #[test]
    fn test_tool_name_mapping_in_history() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name =
            "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_64";

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("use the tool"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "calling tool"},
                        {"type": "tool_use", "id": "toolu_01", "name": long_tool_name, "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .unwrap();
        let short_name = result.tool_name_map.iter().next().unwrap().0.clone();

        // 历史中 assistant 消息的 tool_use name 也应该被映射
        let history = &result.conversation_state.history;
        let mut found = false;
        for msg in history {
            if let Message::Assistant(a) = msg {
                if let Some(ref tool_uses) = a.assistant_response_message.tool_uses {
                    for tu in tool_uses {
                        if tu.tool_use_id == "toolu_01" {
                            assert_eq!(tu.name, short_name, "历史中的 tool_use name 应该是短名称");
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "应该在历史中找到 tool_use");
    }

    #[test]
    fn test_current_tool_results_match_last_assistant_are_attached() {
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("read file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "read_file", "input": {"path": "/tmp/a"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file body"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .unwrap();
        let current = &result.conversation_state.current_message.user_input_message;

        assert_eq!(current.content, "Tool results:\n\nfile body");
        assert_eq!(current.user_input_message_context.tool_results.len(), 1);
        assert_eq!(
            current.user_input_message_context.tool_results[0].tool_use_id,
            "tool-1"
        );
    }

    #[test]
    fn test_current_orphan_tool_results_are_flattened_not_dropped() {
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("read file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "read_file", "input": {"path": "/tmp/a"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "orphan-1", "content": "orphan output"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .unwrap();
        let current = &result.conversation_state.current_message.user_input_message;

        assert_eq!(current.content, "Tool results:\n\norphan output");
        assert!(current.user_input_message_context.tool_results.is_empty());
        assert!(
            result
                .conversation_state
                .history
                .iter()
                .all(|msg| match msg {
                    Message::Assistant(a) => a.assistant_response_message.tool_uses.is_none(),
                    _ => true,
                })
        );
    }

    #[test]
    fn test_history_tools_added_to_tools_list() {
        use super::super::types::Message as AnthropicMessage;

        // 创建一个请求，历史中有工具使用，但 tools 列表为空
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "I'll read the file."},
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/test.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None, // 没有提供工具定义
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .unwrap();

        // 验证 tools 列表中包含了历史中使用的工具的占位符定义
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;

        assert!(!tools.is_empty(), "tools 列表不应为空");
        assert!(
            tools.iter().any(|t| t.tool_specification.name == "read"),
            "tools 列表应包含 'read' 工具的占位符定义"
        );
    }

    #[test]
    fn test_extract_session_id_valid() {
        // 测试有效的 user_id 格式
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_format() {
        // 测试 JSON 格式的 user_id
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_invalid_session() {
        // 测试 JSON 格式但 session_id 不是有效 UUID
        let user_id = r#"{"device_id":"abc","session_id":"not-a-uuid"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_no_session() {
        // 测试没有 session 的 user_id
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_invalid_uuid() {
        // 测试无效的 UUID 格式
        let user_id = "user_xxx_session_invalid-uuid";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_convert_request_with_session_metadata() {
        use super::super::types::{Message as AnthropicMessage, Metadata};

        // 测试带有 metadata 的请求，应该使用 session UUID 作为 conversationId
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(
                    "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
                ),
                preserve_tool_names: false,
            }),
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .unwrap();
        assert_eq!(
            result.conversation_state.conversation_id,
            "a0662283-7fd3-4399-a7eb-52b9a717ae88"
        );
    }

    #[test]
    fn test_convert_request_without_metadata() {
        use super::super::types::Message as AnthropicMessage;

        // 没有 metadata 时，用真实 user anchor 派生稳定 conversationId。
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        )
        .unwrap();
        // 验证生成的是有效的 UUID 格式
        assert_eq!(result.conversation_state.conversation_id.len(), 36);
        assert_eq!(
            result
                .conversation_state
                .conversation_id
                .chars()
                .filter(|c| *c == '-')
                .count(),
            4
        );
    }

    #[test]
    fn test_conversation_id_stable_from_user_anchor() {
        use super::super::types::{Message as AnthropicMessage, SystemMessage};

        let req_a = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Build calculator"),
            }],
            stream: false,
            system: Some(vec![SystemMessage {
                text: "You are helpful".to_string(),
                block_type: None,
                cache_control: None,
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let mut req_b = req_a.clone();
        req_b.messages.push(AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!("Sure"),
        });
        req_b.messages.push(AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("Continue"),
        });

        let cfg = CompressionConfig::default();
        let pf = PromptFilterConfig::default();
        let id_a = convert_request(&req_a, &cfg, &pf, false)
            .unwrap()
            .conversation_state
            .conversation_id;
        let id_b = convert_request(&req_b, &cfg, &pf, false)
            .unwrap()
            .conversation_state
            .conversation_id;

        assert_eq!(id_a, id_b);
    }

    #[test]
    fn test_conversation_id_random_for_synthetic_anchor() {
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("."),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let cfg = CompressionConfig::default();
        let pf = PromptFilterConfig::default();
        let id_a = convert_request(&req, &cfg, &pf, false)
            .unwrap()
            .conversation_state
            .conversation_id;
        let id_b = convert_request(&req, &cfg, &pf, false)
            .unwrap()
            .conversation_state
            .conversation_id;

        assert_ne!(id_a, id_b);
    }

    #[test]
    fn test_convert_assistant_message_tool_use_only() {
        use super::super::types::Message as AnthropicMessage;

        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let result =
            convert_assistant_message(&msg, &mut HashMap::new(), false).expect("应该成功转换");

        assert_eq!(
            result.assistant_response_message.content, "",
            "仅 tool_use 时不应注入文本占位符"
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
        // 工具名被 camelCase 标准化。
        assert_eq!(tool_uses[0].name, "readFile");
    }

    #[test]
    fn test_convert_assistant_message_with_text_and_tool_use() {
        use super::super::types::Message as AnthropicMessage;

        // 测试同时包含 text 和 tool_use 的 assistant 消息
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Let me read that file for you."},
                {"type": "tool_use", "id": "toolu_02XYZ", "name": "read_file", "input": {"path": "/data.json"}}
            ]),
        };

        let result =
            convert_assistant_message(&msg, &mut HashMap::new(), false).expect("应该成功转换");

        // 验证 content 使用原始文本（不是占位符）
        assert_eq!(
            result.assistant_response_message.content,
            "Let me read that file for you."
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_02XYZ");
    }

    #[test]
    fn test_merge_consecutive_assistant_messages() {
        // 测试连续 assistant 消息被正确合并（Issue #79）
        use super::super::types::Message as AnthropicMessage;

        let msg1 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "Let me think about this..."},
                {"type": "text", "text": " "}
            ]),
        };

        let msg2 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "I should read the file."},
                {"type": "text", "text": "Let me read that file."},
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let messages: Vec<&AnthropicMessage> = vec![&msg1, &msg2];
        let result =
            merge_assistant_messages(&messages, &mut HashMap::new(), false).expect("合并应成功");

        let content = &result.assistant_response_message.content;
        assert!(content.contains("<thinking>"), "应包含 thinking 标签");
        assert!(
            content.contains("Let me read that file"),
            "应包含第二条消息的 text 内容"
        );

        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
    }

    #[test]
    fn test_consecutive_assistant_with_tool_use_result_pairing() {
        // 测试 Issue #79 的完整场景
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the config file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "I need to read the file..."},
                        {"type": "text", "text": " "}
                    ]),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "Let me read the config."},
                        {"type": "text", "text": "I'll read the config file for you."},
                        {"type": "tool_use", "id": "toolu_01XYZ", "name": "read_file", "input": {"path": "/config.json"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01XYZ", "content": "{\"key\": \"value\"}"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(
            &req,
            &CompressionConfig::default(),
            &PromptFilterConfig::default(),
            false,
        );
        assert!(
            result.is_ok(),
            "连续 assistant 消息场景不应报错: {:?}",
            result.err()
        );

        let state = result.unwrap().conversation_state;
        let mut found_tool_use = false;
        for msg in &state.history {
            if let Message::Assistant(assistant_msg) = msg {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    if tool_uses.iter().any(|t| t.tool_use_id == "toolu_01XYZ") {
                        found_tool_use = true;
                        break;
                    }
                }
            }
        }
        assert!(found_tool_use, "合并后的 assistant 消息应包含 tool_use");
    }

    // ----- extract_tool_result_content behavior -----

    #[test]
    fn test_tool_result_none_is_empty() {
        assert_eq!(extract_tool_result_content(&None), "");
    }

    #[test]
    fn test_tool_result_null_is_empty() {
        assert_eq!(
            extract_tool_result_content(&Some(serde_json::Value::Null)),
            ""
        );
    }

    #[test]
    fn test_tool_result_string_is_preserved() {
        let raw = "{\n  \"key\": \"value\"\n}";
        assert_eq!(
            extract_tool_result_content(&Some(serde_json::json!(raw))),
            raw
        );
    }

    #[test]
    fn test_tool_result_empty_array_is_empty() {
        assert_eq!(
            extract_tool_result_content(&Some(serde_json::json!([]))),
            ""
        );
    }

    #[test]
    fn test_tool_result_array_empty_text_is_empty() {
        assert_eq!(
            extract_tool_result_content(&Some(serde_json::json!([
                {"type": "text", "text": ""}
            ]))),
            ""
        );
    }

    #[test]
    fn test_tool_result_object_text_block_is_empty() {
        assert_eq!(
            extract_tool_result_content(&Some(serde_json::json!({
                "type": "text", "text": "hello"
            }))),
            ""
        );
    }

    #[test]
    fn test_tool_result_object_text_field_is_empty() {
        assert_eq!(
            extract_tool_result_content(&Some(serde_json::json!({"text": "x"}))),
            ""
        );
    }

    #[test]
    fn test_tool_result_array_uses_text_fields_only() {
        let out = extract_tool_result_content(&Some(serde_json::json!([
            "first",
            {"type": "text", "text": "second"},
            {"text": "third"},
            {"custom": "ignored"},
            ""
        ])));
        assert_eq!(out, "secondthird");
    }

    #[test]
    fn test_tool_result_unknown_object_does_not_pollute_text() {
        assert_eq!(
            extract_tool_result_content(&Some(serde_json::json!({"custom": "ignored"}))),
            ""
        );
    }

    #[test]
    fn test_tool_result_image_attaches_to_current_message() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": "read image"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tool_1", "name": "read", "input": {"path": "a.png"}}
            ]},
            {"role": "user", "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "tool_1",
                    "content": [{
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": IMG_DATA
                        }
                    }]
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.images.len(), 1);
        assert_eq!(current.images[0].format, "png");
        assert_eq!(current.user_input_message_context.tool_results.len(), 1);
        assert_eq!(
            current.user_input_message_context.tool_results[0].content[0]["text"],
            "[Tool returned an image; the image is attached to this message.]"
        );
    }

    #[test]
    fn test_tool_result_untyped_source_image_attaches() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": "read image"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tool_1", "name": "read", "input": {"path": "a.png"}}
            ]},
            {"role": "user", "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "tool_1",
                    "content": [{
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": IMG_DATA
                        }
                    }]
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.images.len(), 1);
        assert_eq!(current.images[0].format, "png");
        assert_eq!(current.images[0].source.bytes, IMG_DATA);
        assert_eq!(
            current.user_input_message_context.tool_results[0].content[0]["text"],
            "[Tool returned an image; the image is attached to this message.]"
        );
    }

    #[test]
    fn test_empty_tool_result_does_not_inject_success_placeholder() {
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": "run"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tool_1", "name": "read", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tool_1", "content": null}
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.content, MINIMAL_FALLBACK_USER_CONTENT);
        assert_eq!(current.user_input_message_context.tool_results.len(), 1);
        assert_eq!(
            current.user_input_message_context.tool_results[0].content[0]["text"],
            ""
        );
    }

    #[test]
    fn test_image_passthrough_keeps_declared_format() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": IMG_DATA
                    }
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.images.len(), 1);
        assert_eq!(current.images[0].format, "jpeg");
        assert_eq!(current.images[0].source.bytes, IMG_DATA);
    }

    #[test]
    fn test_tool_result_image_ignores_unknown_object_text_pollution() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let content = Some(serde_json::json!([
            "raw-string",
            123,
            {"payload": {"answer": 42}},
            {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": IMG_DATA
                }
            }
        ]));
        let (text, images) = extract_tool_result_content_and_images(&content);

        assert_eq!(text, "");
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn test_tool_result_image_text_parts_join() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let content = Some(serde_json::json!([
            {"type": "text", "text": "{\n  \"alpha\": true\n}"},
            {"text": "beta"},
            {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": IMG_DATA
                }
            }
        ]));
        let (text, images) = extract_tool_result_content_and_images(&content);

        assert_eq!(text, "{\n  \"alpha\": true\n}beta");
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn test_image_url_data_url_attaches() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {
                    "type": "image_url",
                    "image_url": {"url": format!("data:image/png;base64,{}", IMG_DATA)}
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.content, "Please analyze the attached image.");
        assert_eq!(current.images.len(), 1);
        assert_eq!(current.images[0].format, "png");
        assert_eq!(current.images[0].source.bytes, IMG_DATA);
    }

    #[test]
    fn test_images_are_not_capped_at_twenty() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let blocks: Vec<serde_json::Value> = (0..25)
            .map(|_| {
                serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": IMG_DATA
                    }
                })
            })
            .collect();
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": blocks}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.images.len(), 25);
    }

    #[test]
    fn test_image_placeholder_text_is_removed_from_current_message() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {"type": "text", "text": "[Image 1]"},
                {
                    "type": "image_url",
                    "image_url": {"url": format!("data:image/png;base64,{}", IMG_DATA)}
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.content, "Please analyze the attached image.");
        assert_eq!(current.images.len(), 1);
    }

    #[test]
    fn test_image_placeholder_text_is_removed_from_history() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {"type": "text", "text": "see [Image 1] now"},
                {
                    "type": "image_url",
                    "image_url": {"url": format!("data:image/png;base64,{}", IMG_DATA)}
                }
            ]},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "next"}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let first_user = kr
            .conversation_state
            .history
            .iter()
            .find_map(|msg| match msg {
                Message::User(user) => Some(user),
                Message::Assistant(_) => None,
            })
            .expect("history should contain the image user turn");
        assert_eq!(first_user.user_input_message.content, "see now");
        assert_eq!(first_user.user_input_message.images.len(), 1);
    }

    #[test]
    fn test_image_source_accepts_media_type_aliases() {
        const IMG_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {
                    "type": "input_image",
                    "source": {
                        "type": "base64",
                        "mediaType": "image/png",
                        "data": IMG_DATA
                    }
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.images.len(), 1);
        assert_eq!(current.images[0].format, "png");
    }

    #[test]
    fn test_image_placeholder_is_not_treated_as_image() {
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "[Image 1]"}}
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert!(current.images.is_empty());
        assert_eq!(current.content, MINIMAL_FALLBACK_USER_CONTENT);
    }

    #[test]
    fn test_non_image_mime_file_is_ignored() {
        const TXT_DATA: &str = "aGVsbG8=";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {
                    "type": "file",
                    "mime": "text/plain",
                    "data": TXT_DATA
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert!(current.images.is_empty());
        assert_eq!(current.content, MINIMAL_FALLBACK_USER_CONTENT);
    }

    #[test]
    fn test_gif_image_is_passed_through() {
        const GIF_DATA: &str = "R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/gif",
                        "data": GIF_DATA
                    }
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.images.len(), 1);
        assert_eq!(current.images[0].format, "gif");
        assert_eq!(current.images[0].source.bytes, GIF_DATA);
    }

    #[test]
    fn test_arbitrary_image_mime_is_passed_through() {
        const IMG_DATA: &str = "aGVsbG8=";
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/heic",
                        "data": IMG_DATA
                    }
                }
            ]}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let current = &kr.conversation_state.current_message.user_input_message;
        assert_eq!(current.images.len(), 1);
        assert_eq!(current.images[0].format, "heic");
        assert_eq!(current.images[0].source.bytes, IMG_DATA);
    }

    // ----- 400 hardening: 历史孤立 assistant 丢弃 + web_search 过滤 -----

    fn make_request_with_messages(
        messages: serde_json::Value,
    ) -> super::super::types::MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": messages,
        }))
        .expect("test fixture should parse")
    }

    #[test]
    fn test_history_drops_orphan_leading_assistant() {
        let req = make_request_with_messages(serde_json::json!([
            {"role": "assistant", "content": "leading orphan"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "now"}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");
        let history = kr.conversation_state.history;
        // leading orphan assistant 必须被丢弃；history 不应以 assistant 开头
        if let Some(first) = history.first() {
            assert!(
                matches!(first, Message::User(_)),
                "history 首条应为 User，实际：{:?}",
                first
            );
        }
        // 不应包含 "leading orphan" 的 assistant
        for m in &history {
            if let Message::Assistant(a) = m {
                assert!(
                    !a.assistant_response_message
                        .content
                        .contains("leading orphan"),
                    "孤立 assistant 内容不应进入 history"
                );
            }
        }
    }

    #[test]
    fn test_history_keeps_web_search_tool_use_for_placeholder_pairing() {
        // 非活跃 tool turn 的 tool_uses 被剥离，
        // tool_result 被叙述为纯文本。
        // web_search tool_result 仍会出现在叙述文本中，用于 placeholder 配对。
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": "search"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_ws", "name": "web_search", "input": {"q": "x"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_ws", "content": "result"}
            ]},
            {"role": "user", "content": "next"}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");
        // sanitize_kiro_history 会将 tool_result 叙述为纯文本
        let mut found_narrated = false;
        for m in &kr.conversation_state.history {
            if let Message::User(u) = m {
                if u.user_input_message.content.contains("Tool results") {
                    found_narrated = true;
                }
            }
        }
        assert!(found_narrated, "web_search tool_result 应被叙述为纯文本");
    }

    #[test]
    fn test_tool_use_input_non_object_falls_back_to_empty_object() {
        // 非活跃 tool turn 的 tool_uses 被剥离。
        // 这里 tool_result 在历史中，但当前消息没有 tool_result，所以 tool turn 不是"活跃"的
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu1", "name": "do_it", "input": "raw_string"}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1", "content": "done"}
            ]},
            {"role": "user", "content": "n"}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");
        // sanitize_kiro_history 会将非活跃 tool turn 的 tool_uses 剥离
        // tool_result 会被叙述为纯文本
        let mut found_narrated = false;
        for m in &kr.conversation_state.history {
            if let Message::User(u) = m {
                if u.user_input_message.content.contains("Tool results") {
                    found_narrated = true;
                }
            }
        }
        assert!(found_narrated, "tool_result 应被叙述为纯文本");
    }

    #[test]
    fn test_sanitize_history_joins_existing_user_text_and_tool_results() {
        let mut user_msg = HistoryUserMessage::new("existing user text", "claude-sonnet-4.5");
        user_msg
            .user_input_message
            .user_input_message_context
            .tool_results
            .push(crate::kiro::model::requests::tool::ToolResult::success(
                "tool-1",
                "tool output",
            ));

        let mut assistant_msg = AssistantMessage::new("running");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            crate::kiro::model::requests::tool::ToolUseEntry::new("tool-1", "exec_command"),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("start", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
            Message::User(user_msg),
        ];

        sanitize_kiro_history(&mut history, &std::collections::HashSet::new());

        let Message::User(user) = &history[2] else {
            panic!("history[2] should remain user");
        };
        assert_eq!(
            user.user_input_message.content,
            "existing user text\n\nTool results:\n\n[exec_command] tool output"
        );
    }

    #[test]
    fn test_sanitize_history_collapses_empty_user_turns() {
        let mut history = vec![
            Message::User(HistoryUserMessage::new("", "claude-sonnet-4.5")),
            Message::User(HistoryUserMessage::new("", "claude-sonnet-4.5")),
        ];

        sanitize_kiro_history(&mut history, &std::collections::HashSet::new());

        assert_eq!(history.len(), 1);
        let Message::User(user) = &history[0] else {
            panic!("history[0] should remain user");
        };
        assert_eq!(
            user.user_input_message.content,
            MINIMAL_FALLBACK_USER_CONTENT
        );
    }

    #[test]
    fn test_sanitize_history_drops_polluted_and_dot_assistant_turns() {
        let req = make_request_with_messages(serde_json::json!([
            {"role": "user", "content": "start"},
            {"role": "assistant", "content": "[Called tool exec_command with input {\"cmd\":\"x\"}]"},
            {"role": "user", "content": "continue"},
            {"role": "assistant", "content": "."},
            {"role": "user", "content": "go on"},
            {"role": "assistant", "content": "Let me check.\n\n[Called tool exec_command with input {\"cmd\":\"pwd\"}]"},
            {"role": "user", "content": "final question"}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let mut assistant_text = String::new();
        for m in &kr.conversation_state.history {
            if let Message::Assistant(a) = m {
                let content = a.assistant_response_message.content.trim();
                assert_ne!(content, "");
                assert_ne!(content, MINIMAL_FALLBACK_USER_CONTENT);
                assert!(!content.contains("[Called tool"));
                assistant_text.push_str(content);
                assistant_text.push('\n');
            }
        }
        assert!(assistant_text.contains("Let me check."));
    }

    // ----- Kiro-Go role 行为：未知 role 不注入 Kiro history -----

    #[test]
    fn test_unknown_role_developer_is_ignored_like_kiro_go() {
        let req = make_request_with_messages(serde_json::json!([
            {"role": "developer", "content": "context A"},
            {"role": "developer", "content": "context B"},
            {"role": "user", "content": "question"}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");

        let cur_text = &kr
            .conversation_state
            .current_message
            .user_input_message
            .content;
        assert!(
            cur_text.contains("question"),
            "currentMessage 应包含 question"
        );

        let has_context_a = kr.conversation_state.history.iter().any(|m| {
            if let Message::User(u) = m {
                u.user_input_message.content.contains("context A")
            } else {
                false
            }
        });
        assert!(!has_context_a, "developer role 不应注入 Kiro history");
    }

    #[test]
    fn test_message_system_role_is_ignored_like_kiro_go() {
        let req = make_request_with_messages(serde_json::json!([
            {"role": "system", "content": "ignore me"},
            {"role": "user", "content": "real question"}
        ]));
        let cfg = crate::model::config::CompressionConfig::default();
        let pf = crate::model::config::PromptFilterConfig::default();
        let kr = convert_request(&req, &cfg, &pf, false).expect("convert");
        let cur_text = &kr
            .conversation_state
            .current_message
            .user_input_message
            .content;
        assert!(cur_text.contains("real question"));

        let has_system_text = kr.conversation_state.history.iter().any(|m| {
            if let Message::User(u) = m {
                u.user_input_message.content.contains("ignore me")
            } else {
                false
            }
        });
        assert!(
            !has_system_text,
            "message-level system role 不应注入 Kiro history"
        );
    }
}
