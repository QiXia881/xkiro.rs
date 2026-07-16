//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的 AWS CodeWhisperer 端点：
//! - API: `https://q.{api_region}.amazonaws.com/generateAssistantResponse`
//! - MCP: `https://q.{api_region}.amazonaws.com/mcp`
//! - Usage: `https://codewhisperer.us-east-1.amazonaws.com/getUsageLimits` 或非 us-east-1 的 `https://q.{api_region}.amazonaws.com/getUsageLimits`
//!
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体会在根对象上注入已解析的 `profileArn`。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{
    KiroEndpoint, PreferenceRequestParts, RequestContext, UsageRequestParts,
    apply_stream_token_type_headers, codewhisperer_rest_host_for_region, q_rest_host_for_region,
};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::region::is_known_bad_q_transport_region;

/// Kiro IDE 端点名称
pub const IDE_ENDPOINT_NAME: &str = "ide";

/// Kiro IDE 端点
pub struct IdeEndpoint;

impl IdeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_kiro_api_region(ctx.config)
    }

    fn management_region(&self, ctx: &RequestContext<'_>) -> String {
        let profile_region = ctx
            .credentials
            .management_profile_arn()
            .and_then(KiroCredentials::profile_arn_region_from_value);
        match profile_region {
            Some(region) if !is_known_bad_q_transport_region(region) => region.to_string(),
            _ => self.api_region(ctx).to_string(),
        }
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    fn rest_host(&self, ctx: &RequestContext<'_>) -> String {
        codewhisperer_rest_host_for_region(&self.management_region(ctx))
    }

    fn preference_host(&self, ctx: &RequestContext<'_>) -> String {
        q_rest_host_for_region(&self.management_region(ctx))
    }

    pub(crate) fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            ctx.config.kiro_version, ctx.machine_id
        )
    }

    pub(crate) fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }

    /// 返回 MCP 请求需要附带的 profileArn header 值
    pub(crate) fn mcp_profile_arn_header_value(credentials: &KiroCredentials) -> Option<&str> {
        credentials.profile_arn_trimmed()
    }

    /// 将 profileArn 注入或从请求体根对象移除
    ///
    /// - 其它凭据有 profile_arn：解析 JSON 并 insert
    /// - 其它凭据无 profile_arn：保留并修剪 body 中已有 profileArn
    /// - 解析失败：返回错误，由 provider 立即终止该次调用
    pub(crate) fn inject_profile_arn(
        request_body: &str,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<String> {
        if let Some(profile_arn) = Self::mcp_profile_arn_header_value(credentials) {
            let mut request: serde_json::Value = serde_json::from_str(request_body)?;
            let obj = request
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("request body is not a JSON object"))?;
            obj.insert(
                "profileArn".to_string(),
                serde_json::Value::String(profile_arn.to_string()),
            );
            return Ok(serde_json::to_string(&request)?);
        }

        // body 不含 profileArn 键的子串时，必然无该键可修剪，跳过整 body 解析。
        // 子串若仅出现在字符串值中会漏判为需解析（保守，回退慢路径），不会误短路。
        if !request_body.contains("\"profileArn\"") {
            return Ok(request_body.to_string());
        }

        let Ok(mut request) = serde_json::from_str::<serde_json::Value>(request_body) else {
            return Ok(request_body.to_string());
        };
        let Some(obj) = request.as_object_mut() else {
            return Ok(request_body.to_string());
        };
        if let Some(serde_json::Value::String(profile_arn)) = obj.get_mut("profileArn") {
            let trimmed = profile_arn.trim();
            if trimmed.is_empty() || !KiroCredentials::is_valid_profile_arn(trimmed) {
                obj.remove("profileArn");
            } else if trimmed.len() != profile_arn.len() {
                *profile_arn = trimmed.to_string();
            } else {
                return Ok(request_body.to_string());
            }
            return Ok(serde_json::to_string(&request)?);
        }
        Ok(request_body.to_string())
    }
}

impl Default for IdeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn name(&self) -> &'static str {
        IDE_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/generateAssistantResponse", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let req = req
            .header("Accept", "*/*")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        apply_stream_token_type_headers(req, ctx.credentials)
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(profile_arn) = Self::mcp_profile_arn_header_value(ctx.credentials) {
            req = req.header("x-amzn-kiro-profile-arn", profile_arn);
        }
        apply_stream_token_type_headers(req, ctx.credentials)
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> anyhow::Result<String> {
        Self::inject_profile_arn(body, ctx.credentials)
    }

    fn usage_request_parts(
        &self,
        ctx: &RequestContext<'_>,
        need_email: bool,
    ) -> anyhow::Result<UsageRequestParts> {
        let host = self.rest_host(ctx);
        let mut url = if need_email {
            format!(
                "https://{}/getUsageLimits?isEmailRequired=true&origin=AI_EDITOR&resourceType=AGENTIC_REQUEST",
                host
            )
        } else {
            format!(
                "https://{}/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST",
                host
            )
        };
        if let Some(profile_arn) = ctx.credentials.management_profile_arn() {
            url.push_str(&format!("&profileArn={}", urlencoding::encode(profile_arn)));
        }

        let mut headers = vec![
            ("Accept", "application/json".to_string()),
            (
                "x-amz-user-agent",
                format!(
                    "aws-sdk-js/1.0.0 KiroIDE-{}-{}",
                    ctx.config.kiro_version, ctx.machine_id
                ),
            ),
            (
                "user-agent",
                format!(
                    "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
                    ctx.config.system_version,
                    ctx.config.node_version,
                    ctx.config.kiro_version,
                    ctx.machine_id
                ),
            ),
            ("host", host),
            ("amz-sdk-invocation-id", Uuid::new_v4().to_string()),
            ("amz-sdk-request", "attempt=1; max=1".to_string()),
            ("Authorization", format!("Bearer {}", ctx.token)),
            ("Connection", "close".to_string()),
        ];

        if ctx.credentials.is_api_key_credential() {
            headers.push(("tokentype", "API_KEY".to_string()));
        }
        if ctx.credentials.is_external_idp_credential() {
            headers.push(("TokenType", "EXTERNAL_IDP".to_string()));
        }

        Ok(UsageRequestParts { url, headers })
    }

    fn set_preference_request_parts(
        &self,
        ctx: &RequestContext<'_>,
        overage_status: &str,
    ) -> anyhow::Result<PreferenceRequestParts> {
        let host = self.preference_host(ctx);
        let url = format!("https://{}/setUserPreference", host);

        let mut body = serde_json::json!({
            "overageConfiguration": { "overageStatus": overage_status },
        });
        if let Some(profile_arn) = ctx.credentials.management_profile_arn() {
            body["profileArn"] = serde_json::Value::String(profile_arn.to_string());
        }

        let mut headers = vec![
            ("Accept", "application/json".to_string()),
            ("content-type", "application/json".to_string()),
            (
                "x-amz-user-agent",
                format!(
                    "aws-sdk-js/1.0.0 KiroIDE-{}-{}",
                    ctx.config.kiro_version, ctx.machine_id
                ),
            ),
            (
                "user-agent",
                format!(
                    "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
                    ctx.config.system_version,
                    ctx.config.node_version,
                    ctx.config.kiro_version,
                    ctx.machine_id
                ),
            ),
            ("host", host),
            ("amz-sdk-invocation-id", Uuid::new_v4().to_string()),
            ("amz-sdk-request", "attempt=1; max=1".to_string()),
            ("Authorization", format!("Bearer {}", ctx.token)),
            ("Connection", "close".to_string()),
        ];

        if ctx.credentials.is_api_key_credential() {
            headers.push(("tokentype", "API_KEY".to_string()));
        }
        if ctx.credentials.is_external_idp_credential() {
            headers.push(("TokenType", "EXTERNAL_IDP".to_string()));
        }

        Ok(PreferenceRequestParts {
            url,
            headers,
            body: serde_json::to_string(&body)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::IdeEndpoint;
    use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;
    use serde_json::Value;

    fn cred_with_arn(arn: Option<&str>) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.profile_arn = arn.map(|s| s.to_string());
        c
    }

    fn cred_sso(auth_method: &str) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.auth_method = Some(auth_method.to_string());
        c.profile_arn = Some("arn:aws:codewhisperer:profile/sso".to_string());
        c
    }

    fn cred_sso_oauth() -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.client_id = Some("cid".to_string());
        c.client_secret = Some("csec".to_string());
        c.profile_arn = Some("arn:aws:codewhisperer:profile/sso".to_string());
        c
    }

    fn header_value<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn test_streaming_header_values_use_ide_api_format() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-123",
            config: &config,
        };

        let user_agent = endpoint.user_agent(&ctx);
        let amz_user_agent = endpoint.x_amz_user_agent(&ctx);

        assert!(user_agent.contains("aws-sdk-js/1.0.34"));
        assert!(user_agent.contains("api/codewhispererstreaming#1.0.34"));
        assert!(user_agent.contains("KiroIDE-0.11.107-machine-123"));
        assert!(amz_user_agent.contains("aws-sdk-js/1.0.34 KiroIDE-0.11.107-machine-123"));
    }

    #[test]
    fn ide_api_headers_include_streaming_accept_header() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-123",
            config: &config,
        };

        let request = endpoint
            .decorate_api(reqwest::Client::new().post(endpoint.api_url(&ctx)), &ctx)
            .build()
            .unwrap();

        assert_eq!(
            request
                .headers()
                .get("accept")
                .and_then(|v| v.to_str().ok()),
            Some("*/*")
        );
    }

    #[test]
    fn test_runtime_header_values_use_runtime_api_format() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-456",
            config: &config,
        };

        let parts = endpoint.usage_request_parts(&ctx, false).unwrap();
        let user_agent = parts
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("user-agent"))
            .map(|(_, value)| value.as_str())
            .expect("user-agent should exist");

        assert!(user_agent.contains("aws-sdk-js/1.0.0"));
        assert!(user_agent.contains("api/codewhispererruntime#1.0.0"));
        assert!(user_agent.contains("m/N,E"));
    }

    #[test]
    fn ide_usage_uses_codewhisperer_rest_host_for_us_east_1() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.auth_method = Some("builder-id".to_string());
        credentials.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:123:profile/test".to_string());
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-usage",
            config: &config,
        };

        let parts = endpoint.usage_request_parts(&ctx, false).unwrap();

        assert!(
            parts
                .url
                .starts_with("https://codewhisperer.us-east-1.amazonaws.com/getUsageLimits?")
        );
        assert!(
            parts.url.contains(
                "profileArn=arn%3Aaws%3Acodewhisperer%3Aus-east-1%3A123%3Aprofile%2Ftest"
            )
        );
        assert_eq!(
            header_value(&parts.headers, "Accept"),
            Some("application/json")
        );
        assert_eq!(
            header_value(&parts.headers, "host"),
            Some("codewhisperer.us-east-1.amazonaws.com")
        );
        assert!(
            header_value(&parts.headers, "user-agent")
                .is_some_and(|value| value.contains("api/codewhispererruntime#1.0.0"))
        );
    }

    #[test]
    fn ide_usage_omits_profile_arn_when_builder_id_has_no_cached_arn() {
        let endpoint = IdeEndpoint::new();
        let mut config = Config::default();
        config.api_region = Some("eu-central-1".to_string());
        let credentials = KiroCredentials {
            auth_method: Some("idc".to_string()),
            provider: Some("BuilderId".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-usage",
            config: &config,
        };

        let parts = endpoint.usage_request_parts(&ctx, false).unwrap();

        assert!(
            parts
                .url
                .starts_with("https://q.eu-central-1.amazonaws.com/getUsageLimits?")
        );
        assert!(!parts.url.contains("profileArn="));
        assert_eq!(
            header_value(&parts.headers, "host"),
            Some("q.eu-central-1.amazonaws.com")
        );
    }

    #[test]
    fn ide_set_preference_uses_q_host_and_profile_arn() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:123:profile/test".to_string());
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-pref",
            config: &config,
        };

        let parts = endpoint
            .set_preference_request_parts(&ctx, "ENABLED")
            .unwrap();
        let body: Value = serde_json::from_str(&parts.body).unwrap();

        assert_eq!(
            parts.url,
            "https://q.us-east-1.amazonaws.com/setUserPreference"
        );
        assert_eq!(
            header_value(&parts.headers, "Accept"),
            Some("application/json")
        );
        assert_eq!(
            header_value(&parts.headers, "content-type"),
            Some("application/json")
        );
        assert_eq!(
            header_value(&parts.headers, "host"),
            Some("q.us-east-1.amazonaws.com")
        );
        assert_eq!(body["overageConfiguration"]["overageStatus"], "ENABLED");
        assert_eq!(
            body["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/test"
        );
    }

    #[test]
    fn ide_set_preference_keeps_enterprise_cached_profile_arn() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            auth_method: Some("idc".to_string()),
            provider: Some("Enterprise".to_string()),
            profile_arn: Some("arn:aws:codewhisperer:us-east-1:123:profile/ignored".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-pref",
            config: &config,
        };

        let parts = endpoint
            .set_preference_request_parts(&ctx, "DISABLED")
            .unwrap();
        let body: Value = serde_json::from_str(&parts.body).unwrap();

        assert_eq!(
            body["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ignored"
        );
    }

    #[test]
    fn ide_endpoint_prefers_profile_arn_region() {
        let endpoint = IdeEndpoint::new();
        let mut config = Config::default();
        config.api_region = Some("us-east-1".to_string());
        let mut credentials = KiroCredentials::default();
        credentials.api_region = Some("us-west-2".to_string());
        credentials.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:123:profile/test".to_string());
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-region",
            config: &config,
        };

        assert_eq!(
            endpoint.api_url(&ctx),
            "https://q.eu-central-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            endpoint.mcp_url(&ctx),
            "https://q.eu-central-1.amazonaws.com/mcp"
        );

        let request = endpoint
            .decorate_api(reqwest::Client::new().post(endpoint.api_url(&ctx)), &ctx)
            .build()
            .unwrap();
        assert_eq!(
            request.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("q.eu-central-1.amazonaws.com")
        );

        let usage = endpoint.usage_request_parts(&ctx, false).unwrap();
        assert!(
            usage
                .url
                .starts_with("https://q.eu-central-1.amazonaws.com/getUsageLimits?")
        );
        assert_eq!(
            header_value(&usage.headers, "host"),
            Some("q.eu-central-1.amazonaws.com")
        );

        let preference = endpoint
            .set_preference_request_parts(&ctx, "ENABLED")
            .unwrap();
        assert_eq!(
            preference.url,
            "https://q.eu-central-1.amazonaws.com/setUserPreference"
        );
        assert_eq!(
            header_value(&preference.headers, "host"),
            Some("q.eu-central-1.amazonaws.com")
        );
    }

    #[test]
    fn ide_endpoint_repairs_known_bad_profile_region_without_changing_arn() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            api_region: Some("us-east-1".to_string()),
            profile_arn: Some("arn:aws:codewhisperer:eu-north-1:123:profile/test".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-region",
            config: &config,
        };

        assert_eq!(
            endpoint.api_url(&ctx),
            "https://q.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            endpoint.mcp_url(&ctx),
            "https://q.us-east-1.amazonaws.com/mcp"
        );

        let request = endpoint
            .decorate_api(reqwest::Client::new().post(endpoint.api_url(&ctx)), &ctx)
            .build()
            .unwrap();
        assert_eq!(
            request.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("q.us-east-1.amazonaws.com")
        );

        let usage = endpoint.usage_request_parts(&ctx, false).unwrap();
        assert!(
            usage
                .url
                .starts_with("https://codewhisperer.us-east-1.amazonaws.com/getUsageLimits?")
        );
        assert!(
            usage.url.contains(
                "profileArn=arn%3Aaws%3Acodewhisperer%3Aeu-north-1%3A123%3Aprofile%2Ftest"
            )
        );
        assert_eq!(
            header_value(&usage.headers, "host"),
            Some("codewhisperer.us-east-1.amazonaws.com")
        );

        let preference = endpoint
            .set_preference_request_parts(&ctx, "ENABLED")
            .unwrap();
        let body: Value = serde_json::from_str(&preference.body).unwrap();
        assert_eq!(
            preference.url,
            "https://q.us-east-1.amazonaws.com/setUserPreference"
        );
        assert_eq!(
            header_value(&preference.headers, "host"),
            Some("q.us-east-1.amazonaws.com")
        );
        assert_eq!(
            body["profileArn"],
            "arn:aws:codewhisperer:eu-north-1:123:profile/test"
        );
    }

    #[test]
    fn test_external_idp_usage_headers_include_token_type() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.auth_method = Some("external_idp".to_string());
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-789",
            config: &config,
        };

        let parts = endpoint.usage_request_parts(&ctx, false).unwrap();
        assert!(parts.headers.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("TokenType") && value == "EXTERNAL_IDP"
        }));
    }

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let cred = cred_with_arn(Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC"));
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_trims_cached_value() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let cred = cred_with_arn(Some(" arn:aws:codewhisperer:profile/test "));
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "arn:aws:codewhisperer:profile/test");
    }

    #[test]
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let cred = cred_with_arn(None);
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        // 没 arn 也没 SSO，原 body 透传
        assert_eq!(result, body);
    }

    #[test]
    fn test_inject_profile_arn_preserves_and_trims_explicit_payload_arn() {
        let body =
            r#"{"conversationState":{},"profileArn":" arn:aws:codewhisperer:profile/explicit "}"#;
        let cred = cred_with_arn(None);
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "arn:aws:codewhisperer:profile/explicit");
    }

    #[test]
    fn test_inject_profile_arn_removes_blank_explicit_payload_arn() {
        let body = r#"{"conversationState":{},"profileArn":"   "}"#;
        let cred = cred_with_arn(None);
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let cred = cred_with_arn(Some("arn:aws:codewhisperer:us-east-1:123:profile/new"));
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/new"
        );
    }

    #[test]
    fn test_inject_profile_arn_removes_invalid_explicit_payload_arn() {
        let body =
            r#"{"conversationState":{},"profileArn":"e3438419-4424-4e57-8990-ef76bd749a44"}"#;
        let cred = cred_with_arn(None);
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let cred = cred_with_arn(Some("arn:aws:codewhisperer:profile/test"));
        assert!(IdeEndpoint::inject_profile_arn(body, &cred).is_err());
    }

    #[test]
    fn test_sso_builder_id_uses_cached_profile_arn() {
        let body = r#"{"profileArn":"old","other":1}"#;
        let cred = cred_sso("builder-id");
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "arn:aws:codewhisperer:profile/sso");
        assert_eq!(json["other"], 1);
    }

    #[test]
    fn test_sso_idc_uses_cached_profile_arn() {
        let body = r#"{"profileArn":"old"}"#;
        let cred = cred_sso("idc");
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "arn:aws:codewhisperer:profile/sso");
    }

    #[test]
    fn test_sso_oauth_uses_cached_profile_arn() {
        let body = r#"{"profileArn":"old"}"#;
        let cred = cred_sso_oauth();
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "arn:aws:codewhisperer:profile/sso");
    }

    #[test]
    fn test_mcp_profile_arn_header_value_sso_returns_cached_arn() {
        let cred = cred_sso("builder-id");
        assert_eq!(
            IdeEndpoint::mcp_profile_arn_header_value(&cred),
            Some("arn:aws:codewhisperer:profile/sso")
        );
    }

    #[test]
    fn test_mcp_profile_arn_header_value_normal_returns_arn() {
        let cred = cred_with_arn(Some(" arn:aws:codewhisperer:profile/test "));
        assert_eq!(
            IdeEndpoint::mcp_profile_arn_header_value(&cred),
            Some("arn:aws:codewhisperer:profile/test")
        );
    }

    #[test]
    fn test_blank_profile_arn_header_value_returns_none() {
        let cred = cred_with_arn(Some("   "));
        assert!(IdeEndpoint::mcp_profile_arn_header_value(&cred).is_none());
    }
}
