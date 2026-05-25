//! OpenAI 兼容协议（Chat Completions + Responses）
//!
//! 入口路由由 [`router::create_openai_router`] 构建，挂在与 Anthropic 同一份 [`crate::anthropic::middleware::AppState`] 上。
//!
//! 设计原则：
//! - 复用 `anthropic::converter::convert_request`：OpenAI 请求先翻译成 [`crate::anthropic::types::MessagesRequest`]，
//!   再走原有 Kiro 转换链。最大化复用现有压缩 / 工具规整 / 模型映射逻辑。
//! - 流式翻译：直接消费 Kiro 上游 EventStream（assistantResponseEvent / toolUseEvent / meteringEvent /
//!   contextUsageEvent），按 OpenAI 协议格式（chat.completion.chunk / response.* 事件）回放给客户端。
//! - 错误映射：参考 `anthropic::handlers::map_kiro_provider_error_to_response` 但以 OpenAI error 格式输出。

pub mod converter;
pub mod handlers;
pub mod stream;
pub mod types;
