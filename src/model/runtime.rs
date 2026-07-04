//! 运行时共享配置
//!
//! 这些配置可在运行时被 Admin API 修改并即时生效（无需重启）。
//! 与 `Config` 不同，运行时配置使用 `Arc<RwLock<...>>` 在 Anthropic
//! 请求处理器和 Admin 服务之间共享。
//!
//! 当 Admin API 写入这些配置时，会同步回写到 `config.json`，确保下次重启
//! 也能保留更改。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::RwLock;

use super::config::{Config, ModelMappingRule, SystemPromptPosition, UserPreset};

/// 用户模型映射运行时
///
/// 在硬编码 `map_model_with_thinking_suffix` 之前作为 OVERRIDE 层：命中首条
/// 启用且 `source_model` 精确匹配、`target_models` 非空的规则后，把入站模型名
/// 改写为目标模型。仅在 OpenAI / OpenAI-Responses 协议路径生效。
///
/// - "replace" / "alias"：取 `target_models[0]`
/// - "loadbalance"：`weights` 为空或与 `target_models` 长度不符 → 轮询
///   （per-rule `AtomicUsize` 计数器）；否则按 `weights` 加权随机
///
/// 规则列表为空时 `resolve` 恒返回 `None`，与未启用此模块行为一致。
#[derive(Debug, Default)]
pub struct ModelMappingRuntime {
    rules: Vec<ModelMappingRule>,
    /// loadbalance 轮询计数器（key = rule.id）
    counters: HashMap<String, AtomicUsize>,
}

impl ModelMappingRuntime {
    pub fn new(rules: Vec<ModelMappingRule>) -> Self {
        let counters = rules
            .iter()
            .filter(|r| r.rule_type == "loadbalance")
            .map(|r| (r.id.clone(), AtomicUsize::new(0)))
            .collect();
        Self { rules, counters }
    }

    pub fn from_config(cfg: &Config) -> Self {
        Self::new(cfg.model_mappings.clone())
    }

    pub fn rules(&self) -> &[ModelMappingRule] {
        &self.rules
    }

    /// 用新规则替换运行时状态（丢弃旧的轮询计数器）
    pub fn replace(&mut self, rules: Vec<ModelMappingRule>) {
        *self = Self::new(rules);
    }

    /// 解析请求模型名的 OVERRIDE 目标。
    ///
    /// 返回 `Some(target)` 表示命中规则并改写；`None` 表示无匹配规则，
    /// 调用方应保持入站模型名不变（后续仍走硬编码归一化）。
    pub fn resolve(&self, requested_model: &str) -> Option<String> {
        let rule = self.rules.iter().find(|r| {
            r.enabled && r.source_model == requested_model && !r.target_models.is_empty()
        })?;

        match rule.rule_type.as_str() {
            "loadbalance" => Some(self.pick_loadbalance(rule)),
            // "replace" | "alias" | 其它 → 取第一个目标
            _ => Some(rule.target_models[0].clone()),
        }
    }

    fn pick_loadbalance(&self, rule: &ModelMappingRule) -> String {
        let targets = &rule.target_models;

        // weights 为空或长度不匹配 → 轮询
        if rule.weights.is_empty() || rule.weights.len() != targets.len() {
            let idx = self
                .counters
                .get(&rule.id)
                .map(|c| c.fetch_add(1, Ordering::Relaxed))
                .unwrap_or(0)
                % targets.len();
            return targets[idx].clone();
        }

        // 加权随机
        let total: u64 = rule.weights.iter().map(|w| *w as u64).sum();
        if total == 0 {
            return targets[0].clone();
        }
        let mut pick = fastrand::u64(0..total);
        for (i, w) in rule.weights.iter().enumerate() {
            let w = *w as u64;
            if pick < w {
                return targets[i].clone();
            }
            pick -= w;
        }
        targets[targets.len() - 1].clone()
    }
}

/// 跨模块共享的可变模型映射运行时句柄
pub type SharedModelMappingConfig = Arc<RwLock<ModelMappingRuntime>>;

/// 从 `Config` 构建共享模型映射句柄
pub fn model_mapping_from_config(cfg: &Config) -> SharedModelMappingConfig {
    Arc::new(RwLock::new(ModelMappingRuntime::from_config(cfg)))
}

/// Prompt 注入运行时配置
///
/// 字段含义：
/// - `enabled`：注入总开关；关闭后所有 preset + 自定义文本都不注入
/// - `enabled_presets`：启用的 preset id 列表（混合内置 + 用户自定义）
/// - `user_presets`：用户自定义预设清单（与内置 `PRESETS` 并列）
/// - `custom_content`：自由文本补充（追加到所有 preset 之后）
/// - `position`：拼接结果在 system role 中的插入位置
#[derive(Debug, Clone)]
pub struct PromptRuntimeConfig {
    pub enabled: bool,
    pub enabled_presets: Vec<String>,
    pub user_presets: Vec<UserPreset>,
    pub custom_content: Option<String>,
    pub position: SystemPromptPosition,
}

impl PromptRuntimeConfig {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.system_prompt_enabled,
            enabled_presets: cfg.enabled_presets.clone(),
            user_presets: cfg.user_presets.clone(),
            custom_content: cfg.system_prompt.clone(),
            position: cfg.system_prompt_position,
        }
    }

    /// 计算最终要注入的文本。返回 `None` 表示无需注入。
    ///
    /// 拼接顺序：
    /// 1. 内置 preset（按 `PRESETS` 数组顺序）
    /// 2. 用户 preset（按 `user_presets` 顺序）
    /// 3. `custom_content`
    /// 各段之间用空行连接。
    pub fn build_injection_text(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }

        let mut parts: Vec<String> = Vec::new();

        for p in crate::anthropic::prompt_presets::PRESETS {
            if self.enabled_presets.iter().any(|id| id == p.id) {
                parts.push(p.content.trim().to_string());
            }
        }
        for up in &self.user_presets {
            if self.enabled_presets.iter().any(|id| id == &up.id) {
                let trimmed = up.content.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        if let Some(c) = self.custom_content.as_deref() {
            let t = c.trim();
            if !t.is_empty() {
                parts.push(t.to_string());
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }
}

/// 跨模块共享的可变 Prompt 配置句柄
pub type SharedPromptConfig = Arc<RwLock<PromptRuntimeConfig>>;

/// 从 `Config` 构建共享句柄
pub fn shared_from_config(cfg: &Config) -> SharedPromptConfig {
    Arc::new(RwLock::new(PromptRuntimeConfig::from_config(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_cfg() -> PromptRuntimeConfig {
        PromptRuntimeConfig {
            enabled: false,
            enabled_presets: Vec::new(),
            user_presets: Vec::new(),
            custom_content: None,
            position: SystemPromptPosition::Append,
        }
    }

    #[test]
    fn disabled_returns_none() {
        let mut c = empty_cfg();
        c.custom_content = Some("hi".into());
        assert!(c.build_injection_text().is_none());
    }

    #[test]
    fn enabled_but_empty_returns_none() {
        let mut c = empty_cfg();
        c.enabled = true;
        assert!(c.build_injection_text().is_none());
    }

    #[test]
    fn enabled_builtin_concise() {
        let mut c = empty_cfg();
        c.enabled = true;
        c.enabled_presets.push("concise".to_string());
        let out = c.build_injection_text().unwrap();
        assert!(out.contains("CONCISE"));
    }

    #[test]
    fn user_preset_appears_when_enabled() {
        let mut c = empty_cfg();
        c.enabled = true;
        c.user_presets.push(UserPreset {
            id: "u1".into(),
            name: "u1".into(),
            description: String::new(),
            content: "USER_TEXT_MARKER".into(),
        });
        c.enabled_presets.push("u1".to_string());
        let out = c.build_injection_text().unwrap();
        assert!(out.contains("USER_TEXT_MARKER"));
    }

    #[test]
    fn custom_content_appended() {
        let mut c = empty_cfg();
        c.enabled = true;
        c.enabled_presets.push("concise".to_string());
        c.custom_content = Some("EXTRA_NOTE".into());
        let out = c.build_injection_text().unwrap();
        let concise_pos = out.find("CONCISE").unwrap();
        let extra_pos = out.find("EXTRA_NOTE").unwrap();
        assert!(extra_pos > concise_pos, "custom_content 应在 preset 之后");
    }

    #[test]
    fn unknown_preset_id_skipped() {
        let mut c = empty_cfg();
        c.enabled = true;
        c.enabled_presets.push("nonexistent".to_string());
        assert!(c.build_injection_text().is_none());
    }

    #[test]
    fn empty_user_preset_content_skipped() {
        let mut c = empty_cfg();
        c.enabled = true;
        c.user_presets.push(UserPreset {
            id: "blank".into(),
            name: "blank".into(),
            description: String::new(),
            content: "   \n  ".into(),
        });
        c.enabled_presets.push("blank".to_string());
        assert!(c.build_injection_text().is_none());
    }

    // ---- ModelMappingRuntime ----

    fn rule(
        id: &str,
        rule_type: &str,
        source: &str,
        targets: &[&str],
        weights: &[u32],
    ) -> ModelMappingRule {
        ModelMappingRule {
            id: id.into(),
            name: id.into(),
            enabled: true,
            rule_type: rule_type.into(),
            source_model: source.into(),
            target_models: targets.iter().map(|s| s.to_string()).collect(),
            weights: weights.to_vec(),
        }
    }

    #[test]
    fn model_mapping_empty_is_passthrough() {
        let rt = ModelMappingRuntime::new(Vec::new());
        assert_eq!(rt.resolve("gpt-4o"), None);
        assert_eq!(rt.resolve("claude-sonnet-4"), None);
    }

    #[test]
    fn model_mapping_replace_maps_source_to_target() {
        let rt = ModelMappingRuntime::new(vec![rule(
            "r1",
            "replace",
            "gpt-4o",
            &["claude-sonnet-4.5"],
            &[],
        )]);
        assert_eq!(rt.resolve("gpt-4o").as_deref(), Some("claude-sonnet-4.5"));
        // 非匹配源保持 None（passthrough）
        assert_eq!(rt.resolve("gpt-4-turbo"), None);
    }

    #[test]
    fn model_mapping_alias_takes_first_target() {
        let rt = ModelMappingRuntime::new(vec![rule("r1", "alias", "foo", &["bar", "baz"], &[])]);
        assert_eq!(rt.resolve("foo").as_deref(), Some("bar"));
    }

    #[test]
    fn model_mapping_loadbalance_round_robin_cycles() {
        let rt = ModelMappingRuntime::new(vec![rule(
            "lb",
            "loadbalance",
            "src",
            &["a", "b", "c"],
            &[],
        )]);
        // 轮询严格按顺序循环
        assert_eq!(rt.resolve("src").as_deref(), Some("a"));
        assert_eq!(rt.resolve("src").as_deref(), Some("b"));
        assert_eq!(rt.resolve("src").as_deref(), Some("c"));
        assert_eq!(rt.resolve("src").as_deref(), Some("a"));
    }

    #[test]
    fn model_mapping_disabled_rule_skipped() {
        let mut r = rule("r1", "replace", "gpt-4o", &["claude-sonnet-4.5"], &[]);
        r.enabled = false;
        let rt = ModelMappingRuntime::new(vec![r]);
        assert_eq!(rt.resolve("gpt-4o"), None);
    }

    #[test]
    fn model_mapping_empty_targets_skipped() {
        let rt = ModelMappingRuntime::new(vec![rule("r1", "replace", "gpt-4o", &[], &[])]);
        assert_eq!(rt.resolve("gpt-4o"), None);
    }

    #[test]
    fn model_mapping_first_matching_rule_wins() {
        let rt = ModelMappingRuntime::new(vec![
            rule("r1", "replace", "gpt-4o", &["first"], &[]),
            rule("r2", "replace", "gpt-4o", &["second"], &[]),
        ]);
        assert_eq!(rt.resolve("gpt-4o").as_deref(), Some("first"));
    }

    #[test]
    fn model_mapping_weighted_all_weight_on_one() {
        // weights 长度匹配且总和>0：权重全压在索引 1
        let rt =
            ModelMappingRuntime::new(vec![rule("lb", "loadbalance", "src", &["a", "b"], &[0, 1])]);
        for _ in 0..20 {
            assert_eq!(rt.resolve("src").as_deref(), Some("b"));
        }
    }

    #[test]
    fn model_mapping_weight_len_mismatch_falls_back_to_round_robin() {
        // weights 长度与 targets 不符 → 退化为轮询
        let rt =
            ModelMappingRuntime::new(vec![rule("lb", "loadbalance", "src", &["a", "b"], &[5])]);
        assert_eq!(rt.resolve("src").as_deref(), Some("a"));
        assert_eq!(rt.resolve("src").as_deref(), Some("b"));
        assert_eq!(rt.resolve("src").as_deref(), Some("a"));
    }
}
