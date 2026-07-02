//! 账号级 machineId 生成与兼容归一化

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;
use uuid::Uuid;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

/// 兜底 machineId 缓存（按凭据 id 分桶，进程生命周期内稳定）
///
/// key 为 `credentials.id`；无 id 的凭据共享同一个兜底值（正常流程不会出现）。
static FALLBACK_MACHINE_IDS: OnceLock<Mutex<HashMap<Option<u64>, String>>> = OnceLock::new();

pub fn generate_account_machine_id() -> String {
    Uuid::new_v4().to_string().to_ascii_lowercase()
}

/// 归一化存量 machineId。
///
/// xkiro 旧版本生成过 `000...<uuid32>` 与 `<uuid32><uuid32>` 两种 64hex
/// 错误形态；这里只迁移这两类可确定错误，其他非空 legacy 值继续保留。
pub fn normalize_machine_id(machine_id: &str) -> Option<String> {
    let trimmed = machine_id.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if is_hex(&lower) && lower.len() == 64 {
        let first = &lower[..32];
        let second = &lower[32..];
        if first.chars().all(|c| c == '0') {
            return Some(format_hex32_as_uuid(second));
        }
        if first == second {
            return Some(format_hex32_as_uuid(first));
        }
        return Some(lower);
    }

    if let Ok(uuid) = Uuid::parse_str(trimmed) {
        return Some(uuid.hyphenated().to_string());
    }

    Some(lower)
}

pub fn normalize_optional_machine_id(machine_id: Option<String>) -> Option<String> {
    machine_id.as_deref().and_then(normalize_machine_id)
}

pub fn ensure_credential_machine_id(credentials: &mut KiroCredentials) -> bool {
    let normalized = credentials
        .machine_id
        .as_deref()
        .and_then(normalize_machine_id)
        .unwrap_or_else(generate_account_machine_id);
    let changed = credentials.machine_id.as_deref().map(str::trim) != Some(normalized.as_str());
    credentials.machine_id = Some(normalized);
    changed
}

/// 根据凭据信息生成唯一的机器 ID
///
/// 优先级：
/// 1. 凭据级 `machineId`（若配置且非空）
/// 2. 全局 `config.machineId`（仅 legacy 兜底）
/// 3. 随机兜底，按 `credentials.id` 在进程内缓存（正常加载/导入流程会先持久化）
pub fn generate_from_credentials(credentials: &KiroCredentials, config: &Config) -> String {
    if let Some(ref machine_id) = credentials.machine_id {
        if let Some(normalized) = normalize_machine_id(machine_id) {
            return normalized;
        }
    }

    if let Some(ref machine_id) = config.machine_id {
        if let Some(normalized) = normalize_machine_id(machine_id) {
            return normalized;
        }
    }

    fallback_machine_id(credentials)
}

/// 为缺失派生材料的凭据生成兜底 machineId
///
/// 按 `credentials.id` 在进程内缓存；同一凭据多次调用返回同一值。
/// 进程重启会重新随机；正常路径应由 `ensure_credential_machine_id` 先持久化。
fn fallback_machine_id(credentials: &KiroCredentials) -> String {
    let cache = FALLBACK_MACHINE_IDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock();
    if let Some(existing) = map.get(&credentials.id) {
        return existing.clone();
    }

    let machine_id = generate_account_machine_id();
    tracing::warn!(
        credential_id = ?credentials.id,
        "凭据缺少持久化 machineId，使用随机兜底 machineId（进程内稳定）"
    );
    map.insert(credentials.id, machine_id.clone());
    machine_id
}

fn is_hex(value: &str) -> bool {
    value.chars().all(|c| c.is_ascii_hexdigit())
}

fn format_hex32_as_uuid(hex: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_account_machine_id_is_uuid() {
        let machine_id = generate_account_machine_id();
        assert!(Uuid::parse_str(&machine_id).is_ok());
        assert_eq!(machine_id, machine_id.to_ascii_lowercase());
    }

    #[test]
    fn test_generate_with_custom_machine_id() {
        let credentials = KiroCredentials::default();
        let mut config = Config::default();
        config.machine_id = Some("2582956E-CC88-4669-B546-07ADBFFCB894".to_string());

        let result = generate_from_credentials(&credentials, &config);
        assert_eq!(result, "2582956e-cc88-4669-b546-07adbffcb894");
    }

    #[test]
    fn test_generate_with_credential_machine_id_overrides_config() {
        let mut credentials = KiroCredentials::default();
        credentials.machine_id = Some("2582956e-cc88-4669-b546-07adbffcb894".to_string());

        let mut config = Config::default();
        config.machine_id = Some("a".repeat(64));

        let result = generate_from_credentials(&credentials, &config);
        assert_eq!(result, "2582956e-cc88-4669-b546-07adbffcb894");
    }

    #[test]
    fn test_generate_without_credentials_uses_fallback() {
        let credentials = KiroCredentials::default();
        let config = Config::default();

        let result = generate_from_credentials(&credentials, &config);
        assert!(Uuid::parse_str(&result).is_ok());
    }

    #[test]
    fn test_fallback_is_stable_per_credential() {
        let mut credentials = KiroCredentials::default();
        credentials.id = Some(u64::MAX - 10);
        let config = Config::default();

        let first = generate_from_credentials(&credentials, &config);
        let second = generate_from_credentials(&credentials, &config);
        assert_eq!(first, second);
    }

    #[test]
    fn test_fallback_differs_across_credentials() {
        let mut cred_a = KiroCredentials::default();
        cred_a.id = Some(u64::MAX - 20);
        let mut cred_b = KiroCredentials::default();
        cred_b.id = Some(u64::MAX - 21);
        let config = Config::default();

        let id_a = generate_from_credentials(&cred_a, &config);
        let id_b = generate_from_credentials(&cred_b, &config);
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn test_normalize_uuid_format() {
        assert_eq!(
            normalize_machine_id("2582956E-CC88-4669-B546-07ADBFFCB894"),
            Some("2582956e-cc88-4669-b546-07adbffcb894".to_string())
        );
    }

    #[test]
    fn test_normalize_32_hex_as_uuid() {
        assert_eq!(
            normalize_machine_id("2582956ecc884669b54607adbffcb894"),
            Some("2582956e-cc88-4669-b546-07adbffcb894".to_string())
        );
    }

    #[test]
    fn test_normalize_zero_padded_64_hex_bug_as_uuid() {
        assert_eq!(
            normalize_machine_id(
                "000000000000000000000000000000002582956ecc884669b54607adbffcb894"
            ),
            Some("2582956e-cc88-4669-b546-07adbffcb894".to_string())
        );
    }

    #[test]
    fn test_normalize_repeated_64_hex_bug_as_uuid() {
        assert_eq!(
            normalize_machine_id(
                "2582956ecc884669b54607adbffcb8942582956ecc884669b54607adbffcb894"
            ),
            Some("2582956e-cc88-4669-b546-07adbffcb894".to_string())
        );
    }

    #[test]
    fn test_normalize_legacy_64_hex_is_preserved() {
        let hex64 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_string();
        assert_eq!(normalize_machine_id(&hex64), Some(hex64));
    }

    #[test]
    fn test_normalize_legacy_freeform_is_preserved() {
        assert_eq!(
            normalize_machine_id(" MACHINE-1 "),
            Some("machine-1".to_string())
        );
    }

    #[test]
    fn test_ensure_credential_machine_id_generates_when_missing() {
        let mut credentials = KiroCredentials::default();
        assert!(ensure_credential_machine_id(&mut credentials));
        assert!(
            credentials
                .machine_id
                .as_deref()
                .is_some_and(|value| Uuid::parse_str(value).is_ok())
        );
    }
}
