use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::types::{Message, MessagesRequest};

const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);
const ONE_HOUR_CACHE_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, Default)]
pub struct CacheResult {
    pub cache_read_input_tokens: i32,
    pub cache_creation_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
}

#[derive(Debug, Clone)]
pub struct CacheProfile {
    total_input_tokens: i32,
    min_cacheable_tokens: i32,
    breakpoints: Vec<CacheBreakpoint>,
}

#[derive(Debug, Clone)]
struct CacheBreakpoint {
    prefix_fingerprint: [u8; 32],
    cumulative_tokens: i32,
    ttl: Duration,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    token_count: i32,
    ttl: Duration,
    expires_at: Instant,
}

struct CachedCheckpointStore {
    entries_by_credential: HashMap<u64, HashMap<[u8; 32], CacheEntry>>,
    max_cache_read_ratio: f64,
    dirty: bool,
}

pub struct CacheTracker {
    entries: Mutex<CachedCheckpointStore>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PromptCacheDiskFile {
    version: i32,
    entries: Vec<PromptCacheDiskEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptCacheDiskEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_id: Option<u64>,
    fingerprint: [u8; 32],
    expires_at: i64,
    ttl_seconds: i64,
}

impl CacheTracker {
    pub fn new(_max_supported_ttl: Duration) -> Self {
        Self::new_with_max_ratio(_max_supported_ttl, 0.85)
    }

    pub fn new_with_max_ratio(_max_supported_ttl: Duration, max_cache_read_ratio: f64) -> Self {
        Self {
            entries: Mutex::new(CachedCheckpointStore {
                entries_by_credential: HashMap::new(),
                max_cache_read_ratio: normalize_prompt_cache_max_ratio(max_cache_read_ratio),
                dirty: false,
            }),
        }
    }

    pub fn set_max_cache_read_ratio(&self, max_cache_read_ratio: f64) {
        self.entries.lock().max_cache_read_ratio =
            normalize_prompt_cache_max_ratio(max_cache_read_ratio);
    }

    pub fn load_from_path(&self, path: &Path) -> usize {
        let Ok(data) = fs::read(path) else {
            return 0;
        };
        let Ok(disk) = serde_json::from_slice::<PromptCacheDiskFile>(&data) else {
            return 0;
        };

        let now_system = SystemTime::now();
        let now_instant = Instant::now();
        let mut loaded = 0usize;
        let mut entries = self.entries.lock();
        for item in disk.entries {
            let Some(credential_id) = item.credential_id.filter(|id| *id != 0) else {
                continue;
            };
            if item.ttl_seconds <= 0 {
                continue;
            }
            let expiry_system = UNIX_EPOCH + Duration::from_secs(item.expires_at.max(0) as u64);
            let Ok(remaining) = expiry_system.duration_since(now_system) else {
                continue;
            };
            entries
                .entries_by_credential
                .entry(credential_id)
                .or_default()
                .insert(
                    item.fingerprint,
                    CacheEntry {
                        token_count: 0,
                        ttl: Duration::from_secs(item.ttl_seconds as u64),
                        expires_at: now_instant + remaining,
                    },
                );
            loaded += 1;
        }
        entries.dirty = false;
        loaded
    }

    pub fn flush_to_path(&self, path: &Path) -> anyhow::Result<usize> {
        let now_instant = Instant::now();
        let now_system = SystemTime::now();
        let mut entries = self.entries.lock();
        if !entries.dirty {
            return Ok(0);
        }

        prune_expired(&mut entries.entries_by_credential, now_instant);
        let entry_count: usize = entries
            .entries_by_credential
            .values()
            .map(HashMap::len)
            .sum();
        let mut disk_entries = Vec::with_capacity(entry_count);
        for (credential_id, credential_entries) in &entries.entries_by_credential {
            for (fingerprint, entry) in credential_entries {
                let remaining = entry.expires_at.saturating_duration_since(now_instant);
                let expires_at = now_system
                    .checked_add(remaining)
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs() as i64)
                    .unwrap_or(0);
                disk_entries.push(PromptCacheDiskEntry {
                    credential_id: Some(*credential_id),
                    fingerprint: *fingerprint,
                    expires_at,
                    ttl_seconds: entry.ttl.as_secs() as i64,
                });
            }
        }
        entries.dirty = false;
        drop(entries);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(&PromptCacheDiskFile {
            version: 1,
            entries: disk_entries,
        })?;
        fs::write(path, data)?;
        Ok(entry_count)
    }

    pub fn build_profile(
        &self,
        payload: &MessagesRequest,
        total_input_tokens: i32,
    ) -> CacheProfile {
        let flattened = flatten_cacheable_blocks(payload);
        let mut hasher = Sha256::new();
        let mut breakpoints = Vec::new();
        let mut cumulative_tokens = 0i32;
        let mut active_ttl = Duration::ZERO;

        for block in flattened {
            let canonical = canonicalize_cache_value(&block.value);
            write_hash_chunk(&mut hasher, &canonical);
            cumulative_tokens = cumulative_tokens.saturating_add(block.tokens);

            let breakpoint_ttl = if block.ttl > Duration::ZERO {
                let ttl = normalize_prompt_cache_ttl(block.ttl);
                active_ttl = ttl;
                ttl
            } else if block.is_message_end && active_ttl > Duration::ZERO {
                active_ttl
            } else {
                Duration::ZERO
            };

            if breakpoint_ttl > Duration::ZERO {
                let prefix_fingerprint: [u8; 32] = hasher.clone().finalize().into();
                breakpoints.push(CacheBreakpoint {
                    prefix_fingerprint,
                    cumulative_tokens,
                    ttl: breakpoint_ttl,
                });
            }
        }

        CacheProfile {
            total_input_tokens: total_input_tokens.max(cumulative_tokens).max(0),
            min_cacheable_tokens: minimum_cacheable_tokens_for_model(&payload.model),
            breakpoints,
        }
    }

    pub fn compute(&self, credential_id: u64, profile: &CacheProfile) -> CacheResult {
        if credential_id == 0 {
            return CacheResult::default();
        }
        let Some(last_breakpoint) = profile.last_breakpoint() else {
            return CacheResult::default();
        };
        let mut last_breakpoint_tokens = last_breakpoint
            .cumulative_tokens
            .min(profile.total_input_tokens);

        let now = Instant::now();
        let mut entries = self.entries.lock();
        prune_expired(&mut entries.entries_by_credential, now);
        let max_cache_read_ratio = entries.max_cache_read_ratio;

        let Some(credential_entries) = entries.entries_by_credential.get_mut(&credential_id) else {
            tracing::debug!(credential_id, "首次请求，无缓存条目");
            let effective_creation = if last_breakpoint_tokens < profile.min_cacheable_tokens {
                0
            } else {
                last_breakpoint_tokens
            };
            let (cache_5m, cache_1h) = compute_ttl_breakdown(profile, 0);
            return CacheResult {
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: effective_creation,
                cache_creation_5m_input_tokens: cache_5m,
                cache_creation_1h_input_tokens: cache_1h,
            };
        };

        if credential_entries.is_empty() {
            tracing::debug!(credential_id, "首次请求，无缓存条目");
            let effective_creation = if last_breakpoint_tokens < profile.min_cacheable_tokens {
                0
            } else {
                last_breakpoint_tokens
            };
            let (cache_5m, cache_1h) = compute_ttl_breakdown(profile, 0);
            return CacheResult {
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: effective_creation,
                cache_creation_5m_input_tokens: cache_5m,
                cache_creation_1h_input_tokens: cache_1h,
            };
        };

        tracing::debug!(
            credential_id,
            entry_count = credential_entries.len(),
            "查找缓存匹配"
        );

        let max_cacheable = ((profile.total_input_tokens as f64) * max_cache_read_ratio) as i32;
        if last_breakpoint_tokens > max_cacheable {
            last_breakpoint_tokens = max_cacheable;
        }

        let mut matched_tokens = 0;

        'outer: for breakpoint in profile.breakpoints.iter().rev() {
            if breakpoint.cumulative_tokens < profile.min_cacheable_tokens {
                continue;
            }
            if let Some(entry) = credential_entries.get_mut(&breakpoint.prefix_fingerprint) {
                if entry.expires_at <= now {
                    continue;
                }
                entry.expires_at = now + entry.ttl;
                matched_tokens = breakpoint
                    .cumulative_tokens
                    .min(profile.total_input_tokens)
                    .min(last_breakpoint_tokens);
                break 'outer;
            }
        }

        let new_tokens = last_breakpoint_tokens.saturating_sub(matched_tokens).max(0);
        let (cache_5m, cache_1h) = compute_ttl_breakdown(profile, matched_tokens);

        tracing::debug!(
            credential_id,
            matched_tokens,
            new_tokens,
            cache_5m,
            cache_1h,
            "缓存计算结果"
        );

        CacheResult {
            cache_read_input_tokens: matched_tokens.max(0),
            cache_creation_input_tokens: new_tokens,
            cache_creation_5m_input_tokens: cache_5m,
            cache_creation_1h_input_tokens: cache_1h,
        }
    }

    pub fn update(&self, credential_id: u64, profile: &CacheProfile) {
        if credential_id == 0 || profile.breakpoints.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut entries = self.entries.lock();
        prune_expired(&mut entries.entries_by_credential, now);
        let credential_entries = entries
            .entries_by_credential
            .entry(credential_id)
            .or_default();

        let mut inserted = false;
        for breakpoint in &profile.breakpoints {
            if breakpoint.cumulative_tokens < profile.min_cacheable_tokens {
                continue;
            }
            credential_entries.insert(
                breakpoint.prefix_fingerprint,
                CacheEntry {
                    token_count: breakpoint.cumulative_tokens,
                    ttl: breakpoint.ttl,
                    expires_at: now + breakpoint.ttl,
                },
            );
            inserted = true;
        }
        if inserted {
            entries.dirty = true;
        }
    }
}

fn normalize_prompt_cache_max_ratio(ratio: f64) -> f64 {
    if ratio > 0.0 && ratio <= 1.0 {
        ratio
    } else {
        0.85
    }
}

/// 计算不同 TTL 的缓存创建 token 数
fn compute_ttl_breakdown(profile: &CacheProfile, matched_tokens: i32) -> (i32, i32) {
    if profile.breakpoints.is_empty() {
        return (0, 0);
    }

    let mut cache_5m = 0;
    let mut cache_1h = 0;
    let mut previous = matched_tokens;

    for breakpoint in &profile.breakpoints {
        let current = breakpoint.cumulative_tokens.min(profile.total_input_tokens);
        if current <= previous {
            continue;
        }
        let delta = current - previous;
        if breakpoint.ttl >= ONE_HOUR_CACHE_TTL {
            cache_1h += delta;
        } else {
            cache_5m += delta;
        }
        previous = current;
    }

    (cache_5m, cache_1h)
}

impl CacheProfile {
    #[cfg(test)]
    pub fn total_input_tokens(&self) -> i32 {
        self.total_input_tokens
    }

    fn cacheable_breakpoints(&self) -> Vec<ResolvedBreakpoint> {
        self.breakpoints
            .iter()
            .filter_map(|breakpoint| {
                if breakpoint.cumulative_tokens < self.min_cacheable_tokens {
                    return None;
                }

                Some(ResolvedBreakpoint {
                    cumulative_tokens: breakpoint.cumulative_tokens,
                    ttl: breakpoint.ttl,
                })
            })
            .collect()
    }

    fn last_breakpoint(&self) -> Option<ResolvedBreakpoint> {
        self.breakpoints
            .last()
            .map(|breakpoint| ResolvedBreakpoint {
                cumulative_tokens: breakpoint.cumulative_tokens,
                ttl: breakpoint.ttl,
            })
    }

    fn last_cacheable_breakpoint(&self) -> Option<ResolvedBreakpoint> {
        self.cacheable_breakpoints().into_iter().last()
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedBreakpoint {
    cumulative_tokens: i32,
    ttl: Duration,
}

#[derive(Debug)]
struct PendingBlock {
    value: serde_json::Value,
    tokens: i32,
    ttl: Duration,
    is_message_end: bool,
}

fn flatten_cacheable_blocks(payload: &MessagesRequest) -> Vec<PendingBlock> {
    let mut blocks = Vec::new();

    let prelude = serde_json::json!({
        "kind": "request_prelude",
        "model": payload.model,
        "tool_choice": payload.tool_choice,
    });
    append_cache_block(&mut blocks, prelude, Duration::ZERO, false);

    if let Some(tools) = &payload.tools {
        for (tool_index, tool) in tools.iter().enumerate() {
            let tool_value = serde_json::json!({
                "kind": "tool",
                "tool_index": tool_index,
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            });
            let ttl = serde_json::to_value(tool)
                .ok()
                .and_then(|value| extract_cache_ttl(&value))
                .unwrap_or(Duration::ZERO);
            append_cache_block(
                &mut blocks,
                strip_cache_position_keys(tool_value),
                ttl,
                false,
            );
        }
    }

    if let Some(system) = &payload.system {
        for (system_index, block) in system.iter().enumerate() {
            let value = serde_json::to_value(block).unwrap_or(serde_json::Value::Null);
            if is_anthropic_billing_header_block(&value) {
                continue;
            }
            let ttl = extract_cache_ttl(&value).unwrap_or(Duration::ZERO);
            append_cache_block(
                &mut blocks,
                strip_cache_position_keys(serde_json::json!({
                    "kind": "system",
                    "system_index": system_index,
                    "block": value,
                })),
                ttl,
                false,
            );
        }
    }

    for (message_index, message) in payload.messages.iter().enumerate() {
        blocks.extend(flatten_message_blocks(message_index, message));
    }

    blocks
}

fn flatten_message_blocks(message_index: usize, message: &Message) -> Vec<PendingBlock> {
    match &message.content {
        serde_json::Value::String(text) => vec![build_message_block(
            message_index,
            &message.role,
            0,
            serde_json::json!({
                "type": "text",
                "text": text,
            }),
            Duration::ZERO,
            true,
        )],
        serde_json::Value::Array(blocks) => {
            let last_block_index = blocks.len().saturating_sub(1);
            blocks
                .iter()
                .enumerate()
                .map(|(block_index, block)| {
                    let ttl = extract_cache_ttl(block).unwrap_or(Duration::ZERO);
                    build_message_block(
                        message_index,
                        &message.role,
                        block_index,
                        block.clone(),
                        ttl,
                        block_index == last_block_index,
                    )
                })
                .collect()
        }
        other => vec![build_message_block(
            message_index,
            &message.role,
            0,
            other.clone(),
            Duration::ZERO,
            true,
        )],
    }
}

fn build_message_block(
    message_index: usize,
    role: &str,
    block_index: usize,
    block: serde_json::Value,
    ttl: Duration,
    is_message_end: bool,
) -> PendingBlock {
    let value = strip_cache_position_keys(serde_json::json!({
            "kind": "message",
            "message_index": message_index,
            "role": role,
            "block_index": block_index,
            "block": block,
    }));
    build_pending_block(value, ttl, is_message_end)
}

fn append_cache_block(
    blocks: &mut Vec<PendingBlock>,
    value: serde_json::Value,
    ttl: Duration,
    is_message_end: bool,
) {
    blocks.push(build_pending_block(value, ttl, is_message_end));
}

fn build_pending_block(
    value: serde_json::Value,
    ttl: Duration,
    is_message_end: bool,
) -> PendingBlock {
    let canonical = canonicalize_cache_value(&value);
    PendingBlock {
        tokens: estimate_approx_tokens(&canonical),
        value,
        ttl: normalize_prompt_cache_ttl(ttl),
        is_message_end,
    }
}

fn extract_cache_ttl(value: &serde_json::Value) -> Option<Duration> {
    let cache_control = value.get("cache_control")?.as_object()?;
    let cache_type = cache_control
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if !cache_type.eq_ignore_ascii_case("ephemeral") {
        return None;
    }

    parse_prompt_cache_ttl_value(cache_control.get("ttl")).or(Some(DEFAULT_CACHE_TTL))
}

fn parse_prompt_cache_ttl_value(value: Option<&serde_json::Value>) -> Option<Duration> {
    match value? {
        serde_json::Value::String(text) => parse_prompt_cache_ttl_string(text),
        serde_json::Value::Number(number) => number
            .as_u64()
            .or_else(|| {
                number.as_f64().and_then(|seconds| {
                    if seconds.is_finite() && seconds > 0.0 {
                        Some(seconds as u64)
                    } else {
                        None
                    }
                })
            })
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs),
        _ => None,
    }
}

fn parse_prompt_cache_ttl_string(value: &str) -> Option<Duration> {
    let trimmed = value.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "1h" {
        return Some(ONE_HOUR_CACHE_TTL);
    }
    if let Some(seconds) = trimmed.strip_suffix('s')
        && let Ok(seconds) = seconds.parse::<u64>()
        && seconds > 0
    {
        return Some(Duration::from_secs(seconds));
    }
    if let Some(minutes) = trimmed.strip_suffix('m')
        && let Ok(minutes) = minutes.parse::<u64>()
        && minutes > 0
    {
        return Some(Duration::from_secs(minutes.saturating_mul(60)));
    }
    if let Some(hours) = trimmed.strip_suffix('h')
        && let Ok(hours) = hours.parse::<u64>()
        && hours > 0
    {
        return Some(Duration::from_secs(hours.saturating_mul(3600)));
    }
    if let Ok(seconds) = trimmed.parse::<u64>()
        && seconds > 0
    {
        return Some(Duration::from_secs(seconds));
    }
    None
}

fn normalize_prompt_cache_ttl(ttl: Duration) -> Duration {
    if ttl <= Duration::ZERO {
        Duration::ZERO
    } else if ttl > ONE_HOUR_CACHE_TTL || ttl > DEFAULT_CACHE_TTL {
        ONE_HOUR_CACHE_TTL
    } else {
        DEFAULT_CACHE_TTL
    }
}

fn strip_cache_position_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "tool_index" | "system_index" | "message_index" | "block_index"
                ) {
                    continue;
                }
                out.insert(key, value);
            }
            serde_json::Value::Object(out)
        }
        other => other,
    }
}

fn canonicalize_cache_value(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical_json(&mut out, value);
    out
}

fn write_canonical_json(out: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(true) => out.push_str("true"),
        serde_json::Value::Bool(false) => out.push_str("false"),
        serde_json::Value::Number(number) => out.push_str(&number.to_string()),
        serde_json::Value::String(text) => {
            out.push_str(&serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string()));
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(out, item);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<_> = map
                .keys()
                .filter(|key| key.as_str() != "cache_control")
                .collect();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()));
                out.push(':');
                if let Some(item) = map.get(key) {
                    write_canonical_json(out, item);
                }
            }
            out.push('}');
        }
    }
}

fn write_hash_chunk(hasher: &mut Sha256, chunk: &str) {
    hasher.update(chunk.len().to_string().as_bytes());
    hasher.update([0]);
    hasher.update(chunk.as_bytes());
    hasher.update([0]);
}

fn estimate_approx_tokens(text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }

    let length = text.chars().count();
    if length == 0 {
        return 0;
    }
    if length < 5 {
        return ((length as f64) / 3.0).ceil().max(1.0) as i32;
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
        .max(1.0) as i32
}

fn minimum_cacheable_tokens_for_model(model: &str) -> i32 {
    let model_lower = model.to_lowercase();

    if model_lower.contains("opus") {
        4096
    } else {
        1024
    }
}

fn is_anthropic_billing_header_block(value: &serde_json::Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    let is_text_block = obj
        .get("type")
        .and_then(|v| v.as_str())
        .is_none_or(|t| t.is_empty() || t == "text");
    if !is_text_block {
        return false;
    }
    let Some(text) = obj.get("text").and_then(|v| v.as_str()) else {
        return false;
    };
    text.trim_start()
        .to_lowercase()
        .starts_with("x-anthropic-billing-header:")
}

fn prune_expired(
    entries_by_credential: &mut HashMap<u64, HashMap<[u8; 32], CacheEntry>>,
    now: Instant,
) {
    entries_by_credential.retain(|_, entries| {
        entries.retain(|_, entry| entry.expires_at > now);
        !entries.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::{CacheControl, SystemMessage, Tool};
    use crate::token;

    fn build_request(messages: Vec<Message>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            messages,
            stream: false,
            system: Some(vec![SystemMessage {
                block_type: None,
                text: "system".to_string(),
                cache_control: None,
            }]),
            tools: Some(vec![Tool {
                tool_type: None,
                name: "echo".to_string(),
                description: "echo".to_string(),
                input_schema: Default::default(),
                max_uses: None,
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    fn build_request_with_system(
        messages: Vec<Message>,
        system: Vec<SystemMessage>,
    ) -> MessagesRequest {
        let mut request = build_request(messages);
        request.system = Some(system);
        request
    }

    fn msg(role: &str, content: serde_json::Value) -> Message {
        Message {
            role: role.to_string(),
            content,
        }
    }

    fn cache_text(text: &str) -> serde_json::Value {
        serde_json::json!([{
            "type": "text",
            "text": text,
            "cache_control": { "type": "ephemeral" }
        }])
    }

    fn long_cacheable_text() -> String {
        std::iter::repeat_n("cacheable prompt chunk", 256)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn medium_turn_text(label: &str) -> String {
        format!(
            "{} {}",
            label,
            std::iter::repeat_n("conversation growth chunk", 80)
                .collect::<Vec<_>>()
                .join(" ")
        )
    }

    fn temp_cache_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "xkiro-cache-tracker-{name}-{}.json",
            uuid::Uuid::new_v4()
        ))
    }

    fn estimate_input_tokens(request: &MessagesRequest) -> i32 {
        token::count_all_tokens(
            request.model.clone(),
            request.system.clone(),
            request.messages.clone(),
            request.tools.clone(),
        ) as i32
    }

    fn cache_read_cap_tokens(profile: &CacheProfile) -> i32 {
        ((profile.total_input_tokens() as f64) * 0.85) as i32
    }

    #[test]
    fn numeric_ttl_value_sets_breakpoint_ttl() {
        let req = build_request(vec![
            msg(
                "user",
                serde_json::json!([{
                    "type": "text",
                    "text": long_cacheable_text(),
                    "cache_control": { "type": "ephemeral", "ttl": 3600 }
                }]),
            ),
            msg("assistant", serde_json::json!("R1")),
        ]);
        let tracker = CacheTracker::new(Duration::ZERO);
        let profile = tracker.build_profile(&req, estimate_input_tokens(&req));

        let breakpoints = profile.cacheable_breakpoints();
        assert!(!breakpoints.is_empty());
        assert!(
            breakpoints
                .iter()
                .all(|bp| bp.ttl == Duration::from_secs(3600))
        );
    }

    #[test]
    fn system_numeric_ttl_deserializes() {
        let payload = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "system": [{
                "type": "text",
                "text": long_cacheable_text(),
                "cache_control": { "type": "ephemeral", "ttl": 3600 }
            }],
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let req: MessagesRequest = serde_json::from_value(payload).unwrap();
        let tracker = CacheTracker::new(Duration::from_secs(300));
        let profile = tracker.build_profile(&req, estimate_input_tokens(&req));

        assert_eq!(
            profile.last_cacheable_breakpoint().map(|bp| bp.ttl),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn constructor_ttl_does_not_override_explicit_breakpoint_ttl() {
        let req = build_request(vec![msg(
            "user",
            serde_json::json!([{
                "type": "text",
                "text": long_cacheable_text(),
                "cache_control": { "type": "ephemeral", "ttl": "1h" }
            }]),
        )]);
        let tracker = CacheTracker::new(Duration::from_secs(300));
        let profile = tracker.build_profile(&req, estimate_input_tokens(&req));

        assert!(
            profile
                .cacheable_breakpoints()
                .iter()
                .all(|bp| bp.ttl == Duration::from_secs(3600))
        );
    }

    #[test]
    fn attribution_header_drift_does_not_break_cache_hit() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let system1 = vec![
            SystemMessage {
                block_type: Some("text".to_string()),
                text:
                    "x-anthropic-billing-header: cc_version=2.1.87.1; cc_entrypoint=cli; cch=aaaaa;"
                        .to_string(),
                cache_control: None,
            },
            SystemMessage {
                block_type: Some("text".to_string()),
                text: long_cacheable_text(),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            },
        ];
        let system2 = vec![
            SystemMessage {
                block_type: Some("text".to_string()),
                text: "x-anthropic-billing-header: cc_version=2.1.87.222222222222222222; cc_entrypoint=cli; cch=bbbbb; extra_padding=xyzxyzxyzxyz;".to_string(),
                cache_control: None,
            },
            SystemMessage {
                block_type: Some("text".to_string()),
                text: long_cacheable_text(),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            },
        ];

        let req1 =
            build_request_with_system(vec![msg("user", serde_json::json!("hello"))], system1);
        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        tracker.update(1, &profile1);

        let req2 =
            build_request_with_system(vec![msg("user", serde_json::json!("hello"))], system2);
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result = tracker.compute(1, &profile2);
        let expected_match = profile2
            .last_cacheable_breakpoint()
            .map(|bp| {
                bp.cumulative_tokens
                    .min(profile2.total_input_tokens())
                    .min(cache_read_cap_tokens(&profile2))
            })
            .unwrap_or(0);

        assert!(total1 != total2);
        assert!(result.cache_read_input_tokens > 0);
        assert_eq!(result.cache_read_input_tokens, expected_match);
        assert_eq!(result.cache_creation_input_tokens, 0);
    }

    #[test]
    fn attribution_header_block_is_skipped_for_cache_prefix() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let system_with_header = vec![
            SystemMessage {
                block_type: Some("text".to_string()),
                text: "x-anthropic-billing-header: cc_version=2.1.87.1; cch=aaaaa;".to_string(),
                cache_control: None,
            },
            SystemMessage {
                block_type: Some("text".to_string()),
                text: long_cacheable_text(),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            },
        ];
        let system_without_header = vec![SystemMessage {
            block_type: Some("text".to_string()),
            text: long_cacheable_text(),
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        }];

        let with_header = build_request_with_system(
            vec![msg("user", serde_json::json!("hello"))],
            system_with_header,
        );
        let without_header = build_request_with_system(
            vec![msg("user", serde_json::json!("hello"))],
            system_without_header,
        );
        let profile_with = tracker.build_profile(&with_header, estimate_input_tokens(&with_header));
        let profile_without =
            tracker.build_profile(&without_header, estimate_input_tokens(&without_header));

        assert_eq!(
            profile_with
                .last_cacheable_breakpoint()
                .map(|bp| bp.cumulative_tokens),
            profile_without
                .last_cacheable_breakpoint()
                .map(|bp| bp.cumulative_tokens)
        );
    }

    #[test]
    fn opus_models_use_4096_min_cacheable_tokens() {
        assert_eq!(minimum_cacheable_tokens_for_model("claude-opus-4.8"), 4096);
        assert_eq!(
            minimum_cacheable_tokens_for_model("claude-sonnet-4.6"),
            1024
        );
        assert_eq!(minimum_cacheable_tokens_for_model("claude-haiku-4.5"), 1024);
    }

    #[test]
    fn normal_system_text_change_still_misses() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let system1 = vec![SystemMessage {
            block_type: Some("text".to_string()),
            text: long_cacheable_text(),
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        }];
        let system2 = vec![SystemMessage {
            block_type: Some("text".to_string()),
            text: format!("{} extra", long_cacheable_text()),
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        }];

        let req1 =
            build_request_with_system(vec![msg("user", serde_json::json!("hello"))], system1);
        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        tracker.update(1, &profile1);

        let req2 =
            build_request_with_system(vec![msg("user", serde_json::json!("hello"))], system2);
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result = tracker.compute(1, &profile2);

        assert_eq!(result.cache_read_input_tokens, 0);
    }

    #[test]
    fn explicit_breakpoint_without_hit_creates_prefix_only() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let req = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let total = estimate_input_tokens(&req);
        let profile = tracker.build_profile(&req, total);
        let result = tracker.compute(1, &profile);

        assert_eq!(result.cache_read_input_tokens, 0);
        assert_eq!(
            result.cache_creation_input_tokens,
            profile
                .last_cacheable_breakpoint()
                .map(|bp| bp.cumulative_tokens)
                .unwrap_or(0)
        );
    }

    #[test]
    fn empty_credential_cache_after_uncacheable_update_is_treated_as_first_request() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let short_req = build_request(vec![msg("user", cache_text("short"))]);
        let short_profile = tracker.build_profile(&short_req, estimate_input_tokens(&short_req));
        tracker.update(1, &short_profile);

        let long_req = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let long_profile = tracker.build_profile(&long_req, estimate_input_tokens(&long_req));
        let result = tracker.compute(1, &long_profile);

        assert_eq!(result.cache_read_input_tokens, 0);
        assert_eq!(
            result.cache_creation_input_tokens,
            long_profile
                .last_cacheable_breakpoint()
                .map(|bp| bp.cumulative_tokens)
                .unwrap_or(0)
        );
    }

    #[test]
    fn same_content_with_shape_drift_does_not_false_hit() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let req1 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        tracker.update(1, &profile1);

        let req2 = build_request(vec![
            msg("user", serde_json::json!(long_cacheable_text())),
            msg(
                "assistant",
                serde_json::json!([{
                    "type": "text",
                    "text": "ok",
                    "cache_control": { "type": "ephemeral" }
                }]),
            ),
        ]);
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result = tracker.compute(1, &profile2);

        assert_eq!(result.cache_read_input_tokens, 0);
        assert!(result.cache_creation_input_tokens > 0);
    }

    #[test]
    fn same_length_retry_with_same_breakpoint_is_hit() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let req1 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        tracker.update(1, &profile1);

        let req2 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result = tracker.compute(1, &profile2);

        assert_eq!(
            result.cache_read_input_tokens,
            profile1
                .last_cacheable_breakpoint()
                .map(|bp| bp.cumulative_tokens.min(cache_read_cap_tokens(&profile2)))
                .unwrap_or(0)
        );
        assert_eq!(result.cache_creation_input_tokens, 0);
    }

    #[test]
    fn same_prompt_hits_only_same_credential() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let req = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        let total = estimate_input_tokens(&req);
        let profile = tracker.build_profile(&req, total);

        let first = tracker.compute(1, &profile);
        assert!(first.cache_creation_input_tokens > 0);
        tracker.update(1, &profile);

        let other_credential = tracker.compute(2, &profile);
        assert_eq!(other_credential.cache_read_input_tokens, 0);
        assert!(other_credential.cache_creation_input_tokens > 0);

        let same_credential = tracker.compute(1, &profile);
        assert!(same_credential.cache_read_input_tokens > 0);
        assert_eq!(same_credential.cache_creation_input_tokens, 0);
    }

    #[test]
    fn cache_read_cap_ratio_is_configurable() {
        let mut hasher = Sha256::new();
        write_hash_chunk(&mut hasher, "cache-ratio-test");
        let prefix_fingerprint: [u8; 32] = hasher.finalize().into();
        let profile = CacheProfile {
            total_input_tokens: 1100,
            min_cacheable_tokens: 1024,
            breakpoints: vec![CacheBreakpoint {
                prefix_fingerprint,
                cumulative_tokens: 1024,
                ttl: Duration::from_secs(300),
            }],
        };
        let tracker = CacheTracker::new(Duration::from_secs(300));
        tracker.update(1, &profile);

        let default_result = tracker.compute(1, &profile);
        assert_eq!(default_result.cache_read_input_tokens, 935);

        tracker.set_max_cache_read_ratio(0.95);
        let raised_result = tracker.compute(1, &profile);
        assert_eq!(raised_result.cache_read_input_tokens, 1024);
    }

    #[test]
    fn prompt_cache_persists_and_reloads_unexpired_entries() {
        let path = temp_cache_path("reload");
        let req = build_request(vec![
            msg("user", cache_text(&long_cacheable_text())),
            msg("assistant", serde_json::json!("R1")),
        ]);
        let tracker = CacheTracker::new(Duration::from_secs(300));
        let profile = tracker.build_profile(&req, estimate_input_tokens(&req));

        tracker.update(1, &profile);
        let persisted = tracker
            .flush_to_path(&path)
            .expect("cache flush should work");
        assert!(persisted > 0);

        let reloaded = CacheTracker::new(Duration::from_secs(300));
        let loaded = reloaded.load_from_path(&path);
        assert_eq!(loaded, persisted);
        let other_credential = reloaded.compute(2, &profile);
        assert_eq!(other_credential.cache_read_input_tokens, 0);

        let usage = reloaded.compute(1, &profile);
        assert!(usage.cache_read_input_tokens > 0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prompt_cache_load_drops_expired_entries() {
        let path = temp_cache_path("expired");
        let disk = PromptCacheDiskFile {
            version: 1,
            entries: vec![PromptCacheDiskEntry {
                credential_id: Some(1),
                fingerprint: [7u8; 32],
                expires_at: 1,
                ttl_seconds: 300,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();

        let tracker = CacheTracker::new(Duration::from_secs(300));
        assert_eq!(tracker.load_from_path(&path), 0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prefix_match_with_appended_turn_reads_previous_prefix_cache() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let req1 = build_request(vec![
            msg("user", cache_text(&long_cacheable_text())),
            msg("assistant", serde_json::json!("R1")),
            msg("user", serde_json::json!("Follow-up")),
            msg("assistant", serde_json::json!("R2")),
        ]);
        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        tracker.update(1, &profile1);

        let req2 = build_request(vec![
            msg("user", cache_text(&long_cacheable_text())),
            msg("assistant", serde_json::json!("R1")),
            msg("user", serde_json::json!("Follow-up")),
            msg("assistant", serde_json::json!("R2")),
            msg("user", serde_json::json!("New feedback")),
            msg("assistant", serde_json::json!("R3")),
        ]);
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result = tracker.compute(1, &profile2);

        let matched_tokens = profile1
            .last_cacheable_breakpoint()
            .map(|bp| bp.cumulative_tokens)
            .unwrap_or(0);
        let capped_last_tokens = profile2
            .last_cacheable_breakpoint()
            .map(|bp| bp.cumulative_tokens.min(cache_read_cap_tokens(&profile2)))
            .unwrap_or(0);

        assert!(matched_tokens > 0);
        assert_eq!(
            result.cache_read_input_tokens,
            matched_tokens.min(cache_read_cap_tokens(&profile2))
        );
        assert_eq!(
            result.cache_creation_input_tokens,
            capped_last_tokens.saturating_sub(result.cache_read_input_tokens)
        );
    }

    #[test]
    fn prefix_lookback_limits_to_recent_ten_breakpoints() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let mut messages = Vec::new();
        for i in 0..12 {
            messages.push(msg(
                "user",
                cache_text(&format!("{}-{i}", long_cacheable_text())),
            ));
            messages.push(msg("assistant", serde_json::json!(format!("reply-{i}"))));
        }
        let req = build_request(messages);
        let total = estimate_input_tokens(&req);
        let profile = tracker.build_profile(&req, total);
        assert!(profile.cacheable_breakpoints().len() >= 10);
    }

    #[test]
    fn message_end_after_anchor_creates_additional_breakpoint() {
        let req = build_request(vec![
            msg("user", cache_text(&long_cacheable_text())),
            msg("assistant", serde_json::json!("R1")),
        ]);
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let profile = tracker.build_profile(&req, estimate_input_tokens(&req));
        let breakpoints = profile.cacheable_breakpoints();
        assert!(breakpoints.len() >= 2);
    }

    #[test]
    fn multi_turn_history_extends_cacheable_prefix() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let long = long_cacheable_text();

        let req1 = build_request(vec![msg("user", cache_text(&long))]);
        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        let result1 = tracker.compute(1, &profile1);
        assert!(result1.cache_creation_input_tokens > 0);
        tracker.update(1, &profile1);

        let req2 = build_request(vec![
            msg("user", cache_text(&long)),
            msg("assistant", serde_json::json!(medium_turn_text("R1"))),
            msg("user", serde_json::json!(medium_turn_text("R2"))),
        ]);
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result2 = tracker.compute(1, &profile2);
        assert!(result2.cache_read_input_tokens >= result1.cache_creation_input_tokens);
        tracker.update(1, &profile2);

        let req3 = build_request(vec![
            msg("user", cache_text(&long)),
            msg("assistant", serde_json::json!(medium_turn_text("R1"))),
            msg("user", serde_json::json!(medium_turn_text("R2"))),
            msg("assistant", serde_json::json!(medium_turn_text("R2A"))),
            msg("user", serde_json::json!(medium_turn_text("R3"))),
        ]);
        let total3 = estimate_input_tokens(&req3);
        let profile3 = tracker.build_profile(&req3, total3);
        let result3 = tracker.compute(1, &profile3);
        assert!(result3.cache_read_input_tokens > result2.cache_read_input_tokens);
    }

    #[test]
    fn ttl_is_inherited_for_derived_message_breakpoints() {
        let req = build_request(vec![
            msg(
                "user",
                serde_json::json!([{
                    "type": "text",
                    "text": long_cacheable_text(),
                    "cache_control": { "type": "ephemeral", "ttl": "1h" }
                }]),
            ),
            msg("assistant", serde_json::json!("R1")),
            msg("user", serde_json::json!("R2")),
        ]);
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let profile = tracker.build_profile(&req, estimate_input_tokens(&req));
        let breakpoints = profile.cacheable_breakpoints();
        assert!(breakpoints.len() >= 2);
        assert!(
            breakpoints
                .iter()
                .all(|bp| bp.ttl == Duration::from_secs(3600))
        );
    }

    #[test]
    fn tool_changes_invalidate_downstream_prefix() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));
        let mut req1 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        req1.tools.as_mut().unwrap().push(Tool {
            tool_type: None,
            name: "alpha".to_string(),
            description: "alpha".to_string(),
            input_schema: Default::default(),
            max_uses: None,
            cache_control: None,
        });
        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        tracker.update(1, &profile1);

        let mut req2 = build_request(vec![msg("user", cache_text(&long_cacheable_text()))]);
        req2.tools.as_mut().unwrap().push(Tool {
            tool_type: None,
            name: "beta".to_string(),
            description: "beta".to_string(),
            input_schema: Default::default(),
            max_uses: None,
            cache_control: None,
        });
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result = tracker.compute(1, &profile2);

        assert_eq!(result.cache_read_input_tokens, 0);
        assert_eq!(
            result.cache_creation_input_tokens,
            profile2
                .last_cacheable_breakpoint()
                .map(|bp| bp.cumulative_tokens.min(cache_read_cap_tokens(&profile2)))
                .unwrap_or(0)
        );
    }

    #[test]
    fn minimum_cacheable_tokens_model_matrix() {
        assert_eq!(minimum_cacheable_tokens_for_model("claude-opus-4-8"), 4096);
        assert_eq!(minimum_cacheable_tokens_for_model("claude-opus-4-7"), 4096);
        assert_eq!(
            minimum_cacheable_tokens_for_model("claude-sonnet-4-6"),
            1024
        );
        assert_eq!(
            minimum_cacheable_tokens_for_model("claude-sonnet-4-5-20250929"),
            1024
        );
        assert_eq!(
            minimum_cacheable_tokens_for_model("claude-haiku-4-5-20251001"),
            1024
        );
        assert_eq!(minimum_cacheable_tokens_for_model("claude-haiku-4-5"), 1024);
        assert_eq!(minimum_cacheable_tokens_for_model("claude-fable-5"), 1024);
        assert_eq!(minimum_cacheable_tokens_for_model("claude-mythos-5"), 1024);
        assert_eq!(minimum_cacheable_tokens_for_model("claude-haiku-3"), 1024);
    }

    #[test]
    fn lookback_beyond_ten_finds_cache_hit() {
        let tracker = CacheTracker::new(Duration::from_secs(3600));

        // Build a request with a cache_control breakpoint on a large system block
        let long_text = long_cacheable_text();
        let system_block = SystemMessage {
            block_type: Some("text".to_string()),
            text: long_text.clone(),
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        };

        let mut req1 = build_request_with_system(
            vec![msg("user", cache_text(&long_text))],
            vec![system_block.clone()],
        );
        // Replace tools with 15 distinct tools to push the system breakpoint beyond position 10
        req1.tools = Some(
            (0..15)
                .map(|i| Tool {
                    tool_type: None,
                    name: format!("tool_{i}"),
                    description: format!("tool {i}"),
                    input_schema: Default::default(),
                    max_uses: None,
                    cache_control: None,
                })
                .collect(),
        );

        let total1 = estimate_input_tokens(&req1);
        let profile1 = tracker.build_profile(&req1, total1);
        tracker.update(1, &profile1);

        // Second identical request — should hit the system breakpoint even though it's
        // now past position 10 in the block list (tools come first)
        let mut req2 = req1.clone();
        req2.messages = vec![
            msg("user", cache_text(&long_text)),
            msg("assistant", serde_json::json!("ok")),
            msg("user", cache_text(&long_text)),
        ];
        let total2 = estimate_input_tokens(&req2);
        let profile2 = tracker.build_profile(&req2, total2);
        let result = tracker.compute(1, &profile2);

        assert!(
            result.cache_read_input_tokens > 0,
            "expected cache hit with lookback > 10, got read={}",
            result.cache_read_input_tokens
        );
    }
}
