//! 请求日志和统计系统
//!
//! 提供全局请求统计和环形缓冲区日志，用于 Admin API 查询。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use chrono::Utc;
use parking_lot::RwLock;
use serde::ser::{Serialize, SerializeStruct};

/// 请求日志最大条数（环形缓冲区）
const REQUEST_LOGS_MAX_SIZE: usize = 500;

pub const REQUEST_LOG_STATUS_SUCCESS: &str = "success";
pub const REQUEST_LOG_STATUS_ERROR: &str = "error";
pub const REQUEST_LOG_ERROR_QUOTA: &str = "quota";
pub const REQUEST_LOG_ERROR_OVERAGE: &str = "overage";
pub const REQUEST_LOG_ERROR_SUSPENDED: &str = "suspended";
pub const REQUEST_LOG_ERROR_AUTH: &str = "auth";
pub const REQUEST_LOG_ERROR_PROFILE: &str = "profile";
pub const REQUEST_LOG_ERROR_UNKNOWN: &str = "unknown";

/// 单条请求日志
#[derive(Debug, Clone)]
pub struct RequestLog {
    /// 请求时间（Unix 秒时间戳）
    pub time: i64,
    /// API 端点类型："claude" / "openai" / "responses"
    pub endpoint: String,
    /// 模型名称
    pub model: String,
    /// 凭据 ID
    pub credential_id: String,
    /// 请求状态："success" / "error"
    pub status: String,
    /// 错误信息（成功时为空）
    pub error: String,
    /// 错误分类："quota" / "overage" / "suspended" / "auth" / "profile" / "unknown"
    pub error_type: String,
    /// token 总数（input + output）
    pub tokens: i64,
    /// 消耗的 credits
    pub credits: f64,
    /// 请求耗时（毫秒）
    pub duration: i64,
}

impl Serialize for RequestLog {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("RequestLog", 11)?;
        state.serialize_field("time", &self.time)?;
        state.serialize_field("endpoint", &self.endpoint)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("credentialId", &self.credential_id)?;
        state.serialize_field("accountId", &self.credential_id)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("error", &self.error)?;
        state.serialize_field("errorType", &self.error_type)?;
        state.serialize_field("tokens", &self.tokens)?;
        state.serialize_field("credits", &self.credits)?;
        state.serialize_field("duration", &self.duration)?;
        state.end()
    }
}

/// 全局请求统计
#[derive(Debug)]
pub struct RequestStats {
    /// 总请求数
    pub total_requests: AtomicI64,
    /// 成功请求数
    pub success_requests: AtomicI64,
    /// 失败请求数
    pub failed_requests: AtomicI64,
    /// 总 token 数
    pub total_tokens: AtomicI64,
    /// 总额度
    total_credits: parking_lot::Mutex<f64>,
    /// 启动时间
    pub start_time: Instant,
    /// 请求日志环形缓冲区
    request_logs: RwLock<VecDeque<RequestLog>>,
}

impl RequestStats {
    /// 创建新的统计实例
    pub fn new() -> Self {
        Self {
            total_requests: AtomicI64::new(0),
            success_requests: AtomicI64::new(0),
            failed_requests: AtomicI64::new(0),
            total_tokens: AtomicI64::new(0),
            total_credits: parking_lot::Mutex::new(0.0),
            start_time: Instant::now(),
            request_logs: RwLock::new(VecDeque::with_capacity(REQUEST_LOGS_MAX_SIZE)),
        }
    }

    /// 记录成功请求
    pub fn record_success(
        &self,
        endpoint: &str,
        model: &str,
        credential_id: &str,
        input_tokens: i64,
        output_tokens: i64,
        credits: f64,
        duration_ms: i64,
    ) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.success_requests.fetch_add(1, Ordering::Relaxed);
        let total_tokens = input_tokens + output_tokens;
        self.total_tokens.fetch_add(total_tokens, Ordering::Relaxed);
        {
            let mut c = self.total_credits.lock();
            *c += credits;
        }

        let entry = RequestLog {
            time: Utc::now().timestamp(),
            endpoint: endpoint.to_string(),
            model: model.to_string(),
            credential_id: credential_id.to_string(),
            status: REQUEST_LOG_STATUS_SUCCESS.to_string(),
            error: String::new(),
            error_type: String::new(),
            tokens: total_tokens,
            credits,
            duration: duration_ms,
        };
        self.append_log(entry);
    }

    /// 记录失败请求
    pub fn record_failure(
        &self,
        endpoint: &str,
        model: &str,
        credential_id: &str,
        error: &str,
        error_type: &str,
        duration_ms: i64,
    ) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.failed_requests.fetch_add(1, Ordering::Relaxed);

        let entry = RequestLog {
            time: Utc::now().timestamp(),
            endpoint: endpoint.to_string(),
            model: model.to_string(),
            credential_id: credential_id.to_string(),
            status: REQUEST_LOG_STATUS_ERROR.to_string(),
            error: error.to_string(),
            error_type: error_type.to_string(),
            tokens: 0,
            credits: 0.0,
            duration: duration_ms,
        };
        self.append_log(entry);
    }

    /// 追加日志到环形缓冲区
    fn append_log(&self, entry: RequestLog) {
        let mut logs = self.request_logs.write();
        if logs.len() >= REQUEST_LOGS_MAX_SIZE {
            logs.pop_front();
        }
        logs.push_back(entry);
    }

    /// 获取所有日志（最新在前）
    pub fn get_logs(&self) -> Vec<RequestLog> {
        let logs = self.request_logs.read();
        logs.iter().rev().cloned().collect()
    }

    /// 清空日志
    pub fn clear_logs(&self) {
        let mut logs = self.request_logs.write();
        logs.clear();
    }

    /// 获取总请求数
    pub fn total_requests(&self) -> i64 {
        self.total_requests.load(Ordering::Relaxed)
    }

    /// 获取成功请求数
    pub fn success_requests(&self) -> i64 {
        self.success_requests.load(Ordering::Relaxed)
    }

    /// 获取失败请求数
    pub fn failed_requests(&self) -> i64 {
        self.failed_requests.load(Ordering::Relaxed)
    }

    /// 获取总 token 数
    pub fn total_tokens(&self) -> i64 {
        self.total_tokens.load(Ordering::Relaxed)
    }

    /// 获取总额度
    pub fn total_credits(&self) -> f64 {
        *self.total_credits.lock()
    }

    /// 获取运行时间（秒）
    pub fn uptime(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// 重置统计
    pub fn reset(&self) {
        self.total_requests.store(0, Ordering::Relaxed);
        self.success_requests.store(0, Ordering::Relaxed);
        self.failed_requests.store(0, Ordering::Relaxed);
        self.total_tokens.store(0, Ordering::Relaxed);
        *self.total_credits.lock() = 0.0;
    }
}

/// 错误分类
pub fn classify_error(message: &str) -> &'static str {
    let lower = message.to_lowercase();

    if lower.contains("quota") || lower.contains("429") || lower.contains("rate_limit") {
        return REQUEST_LOG_ERROR_QUOTA;
    }
    if lower.contains("overage") || lower.contains("402") {
        return REQUEST_LOG_ERROR_OVERAGE;
    }
    if lower.contains("temporarily_suspended")
        || lower.contains("temporarily is suspended")
        || lower.contains("account suspended")
    {
        return REQUEST_LOG_ERROR_SUSPENDED;
    }
    if lower.contains("http 401")
        || lower.contains("http 403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authentication failed")
        || lower.contains("token invalid")
        || lower.contains("token expired")
        || lower.contains("invalid_grant")
        || lower.contains("access token expired")
        || lower.contains("refresh token expired")
    {
        return REQUEST_LOG_ERROR_AUTH;
    }
    if lower.contains("no available kiro profile") || lower.contains("profile unavailable") {
        return REQUEST_LOG_ERROR_PROFILE;
    }

    REQUEST_LOG_ERROR_UNKNOWN
}

/// 共享统计引用
pub type SharedRequestStats = Arc<RequestStats>;

/// 创建共享统计实例
pub fn create_shared_stats() -> SharedRequestStats {
    Arc::new(RequestStats::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_log_serializes_credential_id_alias_without_dropping_empty_error_fields() {
        let stats = RequestStats::new();
        let before = Utc::now().timestamp();

        stats.record_success("claude", "claude-sonnet-4.5", "42", 10, 5, 0.25, 123);

        let after = Utc::now().timestamp();
        let logs = stats.get_logs();
        assert_eq!(logs.len(), 1);

        let value = serde_json::to_value(&logs[0]).unwrap();
        assert_eq!(value["endpoint"], "claude");
        assert_eq!(value["model"], "claude-sonnet-4.5");
        assert_eq!(value["credentialId"], "42");
        assert_eq!(value["accountId"], "42");
        assert_eq!(value["status"], "success");
        assert_eq!(value["error"], "");
        assert_eq!(value["errorType"], "");
        assert_eq!(value["tokens"], 15);
        assert_eq!(value["credits"], json!(0.25));
        assert_eq!(value["duration"], 123);

        let time = value["time"].as_i64().unwrap();
        assert!(time >= before && time <= after);
        assert!(time < 100_000_000_000);
    }

    #[test]
    fn request_logs_keep_newest_first_and_ring_size() {
        let stats = RequestStats::new();

        for i in 0..501 {
            stats.record_failure(
                "openai",
                "model",
                &i.to_string(),
                "HTTP 429",
                classify_error("HTTP 429"),
                1,
            );
        }

        let logs = stats.get_logs();
        assert_eq!(logs.len(), REQUEST_LOGS_MAX_SIZE);
        assert_eq!(logs[0].credential_id, "500");
        assert_eq!(logs[499].credential_id, "1");
        assert_eq!(logs[0].error_type, REQUEST_LOG_ERROR_QUOTA);

        let newest = serde_json::to_value(&logs[0]).unwrap();
        let oldest = serde_json::to_value(&logs[499]).unwrap();
        assert_eq!(newest["credentialId"], "500");
        assert_eq!(newest["accountId"], "500");
        assert_eq!(oldest["credentialId"], "1");
        assert_eq!(oldest["accountId"], "1");

        stats.clear_logs();
        assert!(stats.get_logs().is_empty());
    }
}
