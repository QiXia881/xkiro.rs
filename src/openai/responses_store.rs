use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::converter::parse_responses_input_messages;
use super::types::{ChatMessage, ChatToolCall, ChatToolCallFunction};

const RESPONSES_DIR_NAME: &str = "responses";
const RESPONSES_DEFAULT_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_RESPONSES_HISTORY_DEPTH: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredResponseDoc {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub status: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<Value>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub usage: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub stored_input: Value,
    pub stored_at: i64,
}

#[derive(Debug, Default)]
pub struct ExpandedResponsesHistory {
    pub messages: Vec<ChatMessage>,
}

pub fn responses_store_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(RESPONSES_DIR_NAME)
}

pub fn save_response(store_dir: &Path, mut doc: StoredResponseDoc) -> anyhow::Result<()> {
    if doc.id.trim().is_empty() {
        anyhow::bail!("response missing id");
    }
    std::fs::create_dir_all(store_dir)?;
    if doc.stored_at == 0 {
        doc.stored_at = now_unix();
    }

    let path = response_path(store_dir, &doc.id);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(&doc)?;
    crate::common::io::atomic_write_string(&tmp, &data)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_response(store_dir: &Path, id: &str) -> anyhow::Result<StoredResponseDoc> {
    if id.trim().is_empty() {
        anyhow::bail!("empty response id");
    }
    let path = response_path(store_dir, id);
    let data = std::fs::read_to_string(&path)?;
    let doc: StoredResponseDoc = serde_json::from_str(&data)?;
    if doc.stored_at > 0 {
        let age = now_unix().saturating_sub(doc.stored_at);
        if age > RESPONSES_DEFAULT_TTL.as_secs() as i64 {
            let _ = std::fs::remove_file(path);
            anyhow::bail!("stored response expired");
        }
    }
    Ok(doc)
}

pub fn expand_previous_response_history(
    store_dir: &Path,
    previous_response_id: &str,
) -> anyhow::Result<ExpandedResponsesHistory> {
    let prev = load_response(store_dir, previous_response_id)?;
    let chain = collect_ancestor_chain(store_dir, prev);
    let mut expanded = ExpandedResponsesHistory::default();

    for node in chain {
        if let Some(instructions) = node.instructions.as_deref()
            && !instructions.trim().is_empty()
        {
            expanded.messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(Value::String(instructions.to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }

        if !node.stored_input.is_null() {
            if let Ok(messages) = parse_responses_input_messages(&node.stored_input) {
                expanded.messages.extend(messages);
            }
        }

        expanded.messages.extend(output_to_messages(&node.output));
    }

    Ok(expanded)
}

fn collect_ancestor_chain(store_dir: &Path, prev: StoredResponseDoc) -> Vec<StoredResponseDoc> {
    let mut stack = vec![prev];
    let mut visited: HashSet<String> = stack.iter().map(|doc| doc.id.clone()).collect();

    for _ in 0..MAX_RESPONSES_HISTORY_DEPTH {
        let Some(cursor) = stack.last() else {
            break;
        };
        let Some(parent_id) = cursor.previous_response_id.as_deref() else {
            break;
        };
        if !visited.insert(parent_id.to_string()) {
            break;
        }
        let Ok(parent) = load_response(store_dir, parent_id) else {
            break;
        };
        stack.push(parent);
    }

    stack.reverse();
    stack
}

fn output_to_messages(items: &[Value]) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "message" => {
                let role = item
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("assistant");
                let text = join_output_text_parts(item.get("content"));
                if text.is_empty() && role == "assistant" {
                    continue;
                }
                out.push(ChatMessage {
                    role: role.to_string(),
                    content: Some(Value::String(text)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            "function_call" => {
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("id").and_then(Value::as_str))
                    .unwrap_or_default();
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                let arguments = match item.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                out.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(Value::String(String::new())),
                    tool_calls: Some(vec![ChatToolCall {
                        id: id.to_string(),
                        call_type: "function".to_string(),
                        function: ChatToolCallFunction {
                            name: name.to_string(),
                            arguments,
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                });
            }
            _ => {}
        }
    }
    out
}

fn join_output_text_parts(content: Option<&Value>) -> String {
    let Some(Value::Array(parts)) = content else {
        return String::new();
    };
    let mut out = String::new();
    for part in parts {
        let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(part_type, "output_text" | "text" | "input_text")
            && let Some(text) = part.get("text").and_then(Value::as_str)
        {
            out.push_str(text);
        }
    }
    out
}

fn response_path(store_dir: &Path, id: &str) -> PathBuf {
    store_dir.join(format!("{}.json", sanitize_response_id(id)))
}

fn sanitize_response_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if cleaned.is_empty() {
        "invalid".to_string()
    } else {
        cleaned
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_previous_response_chain_like_kiro_go() {
        let dir = std::env::temp_dir().join(format!(
            "xkiro-responses-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        save_response(
            &dir,
            StoredResponseDoc {
                id: "resp_a".to_string(),
                object: "response".to_string(),
                created_at: 1,
                status: "completed".to_string(),
                model: "claude-sonnet-4.5".to_string(),
                output: vec![serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "turn A assistant" }]
                })],
                usage: Value::Null,
                previous_response_id: None,
                metadata: None,
                instructions: Some("be terse".to_string()),
                stored_input: serde_json::json!("turn A user"),
                stored_at: now_unix(),
            },
        )
        .expect("save A");
        save_response(
            &dir,
            StoredResponseDoc {
                id: "resp_b".to_string(),
                object: "response".to_string(),
                created_at: 2,
                status: "completed".to_string(),
                model: "claude-sonnet-4.5".to_string(),
                output: vec![serde_json::json!({
                    "type": "function_call",
                    "id": "call_b",
                    "name": "lookup",
                    "arguments": "{\"q\":\"x\"}"
                })],
                usage: Value::Null,
                previous_response_id: Some("resp_a".to_string()),
                metadata: None,
                instructions: None,
                stored_input: serde_json::json!("turn B user"),
                stored_at: now_unix(),
            },
        )
        .expect("save B");

        let expanded = expand_previous_response_history(&dir, "resp_b").expect("expand");

        assert_eq!(expanded.messages.len(), 5);
        assert_eq!(expanded.messages[0].role, "system");
        assert_eq!(
            expanded.messages[0].content,
            Some(Value::String("be terse".to_string()))
        );
        assert_eq!(expanded.messages[1].role, "user");
        assert_eq!(
            expanded.messages[1].content,
            Some(Value::String("turn A user".to_string()))
        );
        assert_eq!(expanded.messages[2].role, "assistant");
        assert_eq!(
            expanded.messages[2].content,
            Some(Value::String("turn A assistant".to_string()))
        );
        assert_eq!(expanded.messages[3].role, "user");
        assert_eq!(
            expanded.messages[3].content,
            Some(Value::String("turn B user".to_string()))
        );
        let tool_calls = expanded.messages[4]
            .tool_calls
            .as_ref()
            .expect("assistant tool calls");
        assert_eq!(expanded.messages[4].role, "assistant");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_b");
        assert_eq!(tool_calls[0].call_type, "function");
        assert_eq!(tool_calls[0].function.name, "lookup");
        assert_eq!(tool_calls[0].function.arguments, "{\"q\":\"x\"}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stores_response_metadata_like_kiro_go() {
        let dir = std::env::temp_dir().join(format!(
            "xkiro-responses-metadata-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        save_response(
            &dir,
            StoredResponseDoc {
                id: "resp_meta".to_string(),
                object: "response".to_string(),
                created_at: 1,
                status: "completed".to_string(),
                model: "claude-sonnet-4.5".to_string(),
                output: Vec::new(),
                usage: Value::Null,
                previous_response_id: None,
                metadata: Some(serde_json::json!({ "trace_id": "abc" })),
                instructions: None,
                stored_input: Value::Null,
                stored_at: now_unix(),
            },
        )
        .expect("save metadata");

        let loaded = load_response(&dir, "resp_meta").expect("load metadata");

        assert_eq!(
            loaded.metadata,
            Some(serde_json::json!({ "trace_id": "abc" }))
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
