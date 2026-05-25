//! 内置系统提示词预设库
//!
//! 每个 preset 是一段独立的、用途明确的 system prompt 片段，前端可在 Admin UI
//! 中按需勾选启用。多个启用的 preset 会按 `PRESETS` 中定义的顺序拼接为一段文本，
//! 再追加用户的「自定义补充」，按 `position` 注入到 system role。
//!
//! 添加新 preset 的步骤：
//! 1. 在 `presets/` 下新建 `<id>.md`
//! 2. 在 `PRESETS` 数组里追加一条 `PromptPreset { ... }`，
//!    `content` 用 `include_str!("presets/<id>.md")` 内嵌
//!
//! # 内置预设范围
//! 仅内置生产力类预设（`code_complete` / `concise`）。绕过安全的预设
//! （越权解禁 / 渗透 exploit / 露骨内容等）不作为内置项分发；用户如有需要，
//! 可通过 Admin UI 的「用户预设」自行添加，自行承担内容责任。

/// 单个预设的元数据 + 内容
#[derive(Debug, Clone)]
pub struct PromptPreset {
    /// 稳定唯一标识（用于配置文件持久化、前端勾选）
    pub id: &'static str,
    /// 展示名
    pub name: &'static str,
    /// 一句话简介（前端展示）
    pub description: &'static str,
    /// 完整 prompt 文本
    pub content: &'static str,
}

/// 全量预设清单（顺序即拼接顺序）
pub const PRESETS: &[PromptPreset] = &[
    PromptPreset {
        id: "override",
        name: "默认 Override（解禁）",
        description: "解除所有内容限制，无拒答、无免责声明、无安全警告。等价原 patch-claude 脚本的 override.md。",
        content: include_str!("presets/override.md"),
    },
    PromptPreset {
        id: "pentest",
        name: "渗透测试模式",
        description: "授权安全研究上下文：放心输出可工作的 exploit / payload / shellcode / C2 / AV-EDR-bypass 等。",
        content: include_str!("presets/pentest.md"),
    },
    PromptPreset {
        id: "nsfw",
        name: "NSFW 创作模式",
        description: "解除创作类内容限制：任意题材 / 评级 / 露骨场景，不淡化不省略不打断。",
        content: include_str!("presets/nsfw.md"),
    },
    PromptPreset {
        id: "code_complete",
        name: "代码完整性",
        description: "强制输出完整可运行代码：禁止 TODO / 占位符 / 「为简洁起见省略」类省略。",
        content: include_str!("presets/code_complete.md"),
    },
    PromptPreset {
        id: "concise",
        name: "简洁回复",
        description: "去除所有寒暄、前言、铺垫和 hedging，直击答案。",
        content: include_str!("presets/concise.md"),
    },
    PromptPreset {
        id: "chunked_write",
        name: "长文件分块写入",
        description: "规避 AWS 输出截断导致的 Write/Edit 死循环：强制按 50 行一段写入与编辑，使用占位符续写。",
        content: include_str!("presets/chunked_write.md"),
    },
];

/// 按 id 查找预设
pub fn find(id: &str) -> Option<&'static PromptPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// 判断 id 是否属于内置预设
pub fn is_builtin(id: &str) -> bool {
    find(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_preset_ids_unique() {
        let ids: HashSet<&str> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(
            ids.len(),
            PRESETS.len(),
            "内置预设 id 存在重复：{:?}",
            PRESETS.iter().map(|p| p.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn all_preset_contents_non_empty() {
        for p in PRESETS {
            assert!(
                !p.content.trim().is_empty(),
                "预设 '{}' 的 content 为空（检查 presets/{}.md）",
                p.id,
                p.id
            );
        }
    }

    #[test]
    fn all_presets_have_metadata() {
        for p in PRESETS {
            assert!(!p.name.is_empty(), "预设 '{}' 缺少 name", p.id);
            assert!(
                !p.description.is_empty(),
                "预设 '{}' 缺少 description",
                p.id
            );
        }
    }

    #[test]
    fn find_returns_some_for_all_builtins() {
        for p in PRESETS {
            let found = find(p.id);
            assert!(found.is_some(), "find('{}') 应返回 Some", p.id);
            assert_eq!(found.unwrap().id, p.id, "find 返回的 id 应匹配");
        }
    }

    #[test]
    fn find_unknown_returns_none() {
        assert!(find("nonexistent").is_none());
        assert!(find("").is_none(), "空字符串应返回 None");
        assert!(find("CONCISE").is_none(), "id 大小写敏感");
    }

    #[test]
    fn is_builtin_matches_find() {
        for p in PRESETS {
            assert!(is_builtin(p.id), "is_builtin('{}') 应为 true", p.id);
        }
        assert!(!is_builtin("my_user_preset"));
        assert!(!is_builtin(""));
        assert!(!is_builtin("nonexistent"));
    }
}
