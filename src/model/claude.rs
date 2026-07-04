use regex::Regex;
use std::sync::OnceLock;

pub fn native_claude_model_id(model: &str) -> String {
    static VERSION_PATTERN: OnceLock<Regex> = OnceLock::new();

    let model = model.trim().to_ascii_lowercase();
    let version_re = VERSION_PATTERN
        .get_or_init(|| Regex::new(r"claude-(opus|sonnet|haiku)-(\d+)[.-](\d{1,2})\b").unwrap());

    version_re
        .replace_all(&model, "claude-$1-$2-$3")
        .into_owned()
}

pub fn kiro_upstream_claude_model_id(model: &str) -> String {
    static DASH_VERSION_PATTERN: OnceLock<Regex> = OnceLock::new();

    let model = model.trim();
    let lower = model.to_ascii_lowercase();
    let version_re = DASH_VERSION_PATTERN
        .get_or_init(|| Regex::new(r"claude-(opus|sonnet|haiku)-(\d+)-(\d{1,2})\b").unwrap());

    if version_re.is_match(&lower) {
        return version_re
            .replace_all(&lower, "claude-$1-$2.$3")
            .into_owned();
    }

    model.to_string()
}

pub fn claude_model_match_key(model: &str) -> Option<String> {
    let model = model.trim();
    if model.is_empty() {
        None
    } else {
        Some(native_claude_model_id(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_claude_model_id_normalizes_dot_and_hyphen_versions() {
        assert_eq!(native_claude_model_id("claude-opus-4.8"), "claude-opus-4-8");
        assert_eq!(native_claude_model_id("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(
            native_claude_model_id("claude-haiku-4.5-20251001"),
            "claude-haiku-4-5-20251001"
        );
        assert_eq!(
            native_claude_model_id("some-other-model"),
            "some-other-model"
        );
    }

    #[test]
    fn kiro_upstream_claude_model_id_normalizes_hyphen_versions_to_dot() {
        assert_eq!(
            kiro_upstream_claude_model_id("claude-opus-4-8"),
            "claude-opus-4.8"
        );
        assert_eq!(
            kiro_upstream_claude_model_id("claude-opus-4.8"),
            "claude-opus-4.8"
        );
        assert_eq!(
            kiro_upstream_claude_model_id("claude-sonnet-4-20250514"),
            "claude-sonnet-4-20250514"
        );
        assert_eq!(
            kiro_upstream_claude_model_id("some-other-model"),
            "some-other-model"
        );
    }

    #[test]
    fn claude_model_match_key_rejects_blank_models() {
        assert_eq!(claude_model_match_key("   "), None);
        assert_eq!(
            claude_model_match_key(" Claude-Sonnet-4.6 ").as_deref(),
            Some("claude-sonnet-4-6")
        );
    }
}
