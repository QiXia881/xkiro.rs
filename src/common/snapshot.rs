//! 测试专用 SSE 快照工具
//!
//! 为流式管道去重（ER-1/ER-2/ER-3）提供回归防护网：
//! 在重构前捕获各 stream context 的 SSE 输出为 golden 文件，重构后逐字比对。
//!
//! golden 缺失时首次运行自动写入（基线捕获），存在时严格比对；
//! 不匹配时写出 `.snap.new` 供人工 diff，并 panic。
//!
//! 易变字段（msg_/toolu_/chatcmpl-/resp_/fc_ 后跟随机 UUID）会被掩码为稳定占位符，
//! 避免 UUID 抖动导致假阳性。

use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;

/// 掩码 SSE 输出里的易变 ID，使快照可稳定比对。
///
/// 覆盖：Anthropic `msg_<uuid>` / `toolu_<uuid>`，OpenAI `chatcmpl-<uuid>` /
/// `resp_<uuid>` / `fc_<uuid>`。占位符保留前缀语义，便于阅读 diff。
pub fn mask_volatile_ids(input: &str) -> String {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        vec![
            (Regex::new(r"msg_[0-9a-fA-F]{16,}").unwrap(), "msg_<ID>"),
            (
                Regex::new(r"toolu_[0-9a-fA-F-]{16,}").unwrap(),
                "toolu_<ID>",
            ),
            (
                Regex::new(r"chatcmpl-[0-9a-zA-Z]{16,}").unwrap(),
                "chatcmpl-<ID>",
            ),
            (Regex::new(r"resp_[0-9a-zA-Z]{16,}").unwrap(), "resp_<ID>"),
            (Regex::new(r"fc_[0-9a-zA-Z]{16,}").unwrap(), "fc_<ID>"),
            // OpenAI chunk 的 created / created_at 时间戳每次运行不同，需掩码
            (
                Regex::new(r#""created_at":\s*\d+"#).unwrap(),
                r#""created_at":<TS>"#,
            ),
            (
                Regex::new(r#""created":\s*\d+"#).unwrap(),
                r#""created":<TS>"#,
            ),
            // 输出 token 计数由 token::count_tokens 估算得出，受全局
            // PRECISE_COUNTING 开关影响（并行测试可能翻转它，导致启发式↔cl100k
            // 抖动）。这些数值不是 ER-1/2/3 去重要保护的对象，掩码以保证快照确定性。
            (
                Regex::new(r#""completion_tokens":\s*\d+"#).unwrap(),
                r#""completion_tokens":<N>"#,
            ),
            (
                Regex::new(r#""total_tokens":\s*\d+"#).unwrap(),
                r#""total_tokens":<N>"#,
            ),
            (
                Regex::new(r#""output_tokens":\s*\d+"#).unwrap(),
                r#""output_tokens":<N>"#,
            ),
        ]
    });

    let mut out = input.to_string();
    for (re, replacement) in patterns {
        out = re.replace_all(&out, *replacement).into_owned();
    }
    out
}

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
}

/// golden 文件比对：缺失时写入基线并通过，存在时严格比对。
///
/// `actual` 会先经 [`mask_volatile_ids`] 掩码。不匹配时写出 `<name>.snap.new`
/// 并 panic，提示人工 diff 后决定是接受变更还是修复回归。
pub fn assert_golden(name: &str, actual: &str) {
    let masked = mask_volatile_ids(actual);
    let dir = snapshot_dir();
    std::fs::create_dir_all(&dir).expect("创建快照目录失败");
    let golden_path = dir.join(format!("{name}.snap"));

    if !golden_path.exists() {
        std::fs::write(&golden_path, &masked).expect("写入基线快照失败");
        eprintln!("[snapshot] 基线已捕获: {}", golden_path.display());
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).expect("读取 golden 快照失败");
    if expected != masked {
        let new_path = dir.join(format!("{name}.snap.new"));
        std::fs::write(&new_path, &masked).expect("写入 .snap.new 失败");
        panic!(
            "SSE 快照不匹配: {name}\n  golden: {}\n  actual: {}\n请 diff 两者：变更符合预期则用 .new 覆盖 .snap，否则为回归。",
            golden_path.display(),
            new_path.display(),
        );
    }
}
