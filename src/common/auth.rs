//! 公共认证工具函数

use axum::{
    body::Body,
    http::{Request, header},
};
use subtle::ConstantTimeEq;

/// 从请求中提取 API 密钥
///
/// 支持两种认证方式：
/// - `Authorization: Bearer <token>` header
/// - `x-api-key` header
pub fn extract_api_key(request: &Request<Body>) -> Option<String> {
    if let Some(key) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some(key.to_string());
    }

    request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// 常量时间字符串比较，防止时序攻击
///
/// 无论字符串内容如何，比较所需的时间都是恒定的，
/// 这可以防止攻击者通过测量响应时间来猜测 API 密钥。
///
/// 使用经过安全审计的 `subtle` crate 实现
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_api_key_prefers_bearer_over_x_api_key() {
        let request = Request::builder()
            .header(header::AUTHORIZATION, "Bearer bearer-key")
            .header("X-Api-Key", "x-api-key")
            .body(Body::empty())
            .unwrap();

        assert_eq!(extract_api_key(&request).as_deref(), Some("bearer-key"));
    }

    #[test]
    fn extract_api_key_falls_back_to_x_api_key() {
        let request = Request::builder()
            .header("X-Api-Key", "x-api-key")
            .body(Body::empty())
            .unwrap();

        assert_eq!(extract_api_key(&request).as_deref(), Some("x-api-key"));
    }
}
