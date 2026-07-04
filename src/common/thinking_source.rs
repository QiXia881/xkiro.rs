//! Thinking 来源仲裁
//!
//! Kiro 上游可能通过两种方式发送思考内容：
//! - `reasoningContentEvent`（独立事件，thinking 模式）
//! - assistant 文本里内联的 `<thinking>...</thinking>` 标签
//!
//! 同一次响应只能采信其中一种来源，否则会重复输出思考内容。此仲裁器锁定
//! 首个出现的来源：一旦选定 ReasoningEvent 就拒绝 TagBlock，反之亦然。
//!
//! Anthropic `StreamContext` 与 OpenAI `OpenAIChatStream` 共用同一套仲裁逻辑，
//! 抽取到此处消除重复实现（ER-3）。

/// 思考内容来源仲裁状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingSourceArbiter {
    /// 尚未见到任何思考来源。
    #[default]
    Unknown,
    /// 已锁定为 `reasoningContentEvent`。
    ReasoningEvent,
    /// 已锁定为内联 `<thinking>` 标签。
    TagBlock,
}

impl ThinkingSourceArbiter {
    /// reasoning 事件请求发言权。
    ///
    /// 若已锁定 TagBlock 则拒绝（返回 false）；否则锁定为 ReasoningEvent 并放行。
    pub fn allow_reasoning(&mut self) -> bool {
        if *self == Self::TagBlock {
            return false;
        }
        *self = Self::ReasoningEvent;
        true
    }

    /// `<thinking>` 标签请求发言权。
    ///
    /// 若已锁定 ReasoningEvent 则拒绝；Unknown 时锁定为 TagBlock；已是 TagBlock 时放行。
    pub fn allow_tag(&mut self) -> bool {
        if *self == Self::ReasoningEvent {
            return false;
        }
        if *self == Self::Unknown {
            *self = Self::TagBlock;
        }
        *self == Self::TagBlock
    }

    /// 当前来源是否已锁定为 reasoning 事件。
    pub fn is_reasoning(&self) -> bool {
        *self == Self::ReasoningEvent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_first_locks_out_tag() {
        let mut a = ThinkingSourceArbiter::default();
        assert!(a.allow_reasoning());
        assert!(a.is_reasoning());
        assert!(!a.allow_tag());
        // 后续 reasoning 仍放行
        assert!(a.allow_reasoning());
    }

    #[test]
    fn tag_first_locks_out_reasoning() {
        let mut a = ThinkingSourceArbiter::default();
        assert!(a.allow_tag());
        assert!(!a.is_reasoning());
        assert!(!a.allow_reasoning());
        // 后续 tag 仍放行
        assert!(a.allow_tag());
    }

    #[test]
    fn unknown_starts_neutral() {
        let a = ThinkingSourceArbiter::default();
        assert_eq!(a, ThinkingSourceArbiter::Unknown);
        assert!(!a.is_reasoning());
    }
}
