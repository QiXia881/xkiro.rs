//! CodeWhisperer 端点
//!
//! 对应 CodeWhisperer API 端点：
//! - API: `https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse` 或区域化后的 `https://q.{api_region}.amazonaws.com/generateAssistantResponse`
//! - X-Amz-Target: `AmazonCodeWhispererStreamingService.GenerateAssistantResponse`
//!
//! 此端点与 IDE 端点共享大部分逻辑，主要区别在于：
//! - 不同的 host（codewhisperer. vs q.）
//! - 需要 X-Amz-Target header
//! - User-Agent 使用不同的 API 名称

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{
    KiroEndpoint, PreferenceRequestParts, RequestContext, UsageRequestParts,
    codewhisperer_rest_host_for_region, q_rest_host_for_region,
};
use crate::kiro::model::credentials::KiroCredentials;

/// CodeWhisperer 端点名称
pub const CODEWHISPERER_ENDPOINT_NAME: &str = "codewhisperer";

/// CodeWhisperer 端点
pub struct CodewhispererEndpoint;

impl CodewhispererEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_kiro_api_region(ctx.config)
    }

    fn host_for_region(api_region: &str) -> String {
        codewhisperer_rest_host_for_region(api_region)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        Self::host_for_region(self.api_region(ctx))
    }

    fn preference_host(&self, ctx: &RequestContext<'_>) -> String {
        q_rest_host_for_region(self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            ctx.config.kiro_version, ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }

    fn mcp_profile_arn_header_value(credentials: &KiroCredentials) -> Option<&str> {
        if credentials.is_aws_sso_oidc_credential() {
            return None;
        }
        credentials.profile_arn_trimmed()
    }

    fn inject_profile_arn(
        request_body: &str,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<String> {
        if credentials.is_aws_sso_oidc_credential() {
            let mut request: serde_json::Value = serde_json::from_str(request_body)?;
            if let Some(obj) = request.as_object_mut() {
                obj.remove("profileArn");
            }
            return Ok(serde_json::to_string(&request)?);
        }

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

        let Ok(mut request) = serde_json::from_str::<serde_json::Value>(request_body) else {
            return Ok(request_body.to_string());
        };
        let Some(obj) = request.as_object_mut() else {
            return Ok(request_body.to_string());
        };
        if let Some(serde_json::Value::String(profile_arn)) = obj.get_mut("profileArn") {
            let trimmed = profile_arn.trim();
            if trimmed.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::CodewhispererEndpoint;
    use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;
    use serde_json::Value;

    fn header_value<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn test_inject_profile_arn_preserves_and_trims_explicit_payload_arn() {
        let body =
            r#"{"conversationState":{},"profileArn":" arn:aws:codewhisperer:profile/explicit "}"#;
        let credentials = KiroCredentials::default();
        let result = CodewhispererEndpoint::inject_profile_arn(body, &credentials).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(json["profileArn"], "arn:aws:codewhisperer:profile/explicit");
    }

    #[test]
    fn codewhisperer_endpoint_uses_codewhisperer_host_for_us_east_1() {
        let endpoint = CodewhispererEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            api_region: Some("us-east-1".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };

        assert_eq!(
            endpoint.api_url(&ctx),
            "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            endpoint.mcp_url(&ctx),
            "https://codewhisperer.us-east-1.amazonaws.com/mcp"
        );
    }

    #[test]
    fn codewhisperer_api_headers_include_streaming_accept_header() {
        let endpoint = CodewhispererEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
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
    fn codewhisperer_usage_uses_codewhisperer_rest_host_for_us_east_1() {
        let endpoint = CodewhispererEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            api_region: Some("us-east-1".to_string()),
            profile_arn: Some("arn:aws:codewhisperer:us-east-1:123:profile/test".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };

        let usage = endpoint.usage_request_parts(&ctx, false).unwrap();

        assert!(
            usage
                .url
                .starts_with("https://codewhisperer.us-east-1.amazonaws.com/getUsageLimits?")
        );
        assert!(
            usage.url.contains(
                "profileArn=arn%3Aaws%3Acodewhisperer%3Aus-east-1%3A123%3Aprofile%2Ftest"
            )
        );
        assert_eq!(
            header_value(&usage.headers, "Accept"),
            Some("application/json")
        );
        assert_eq!(
            header_value(&usage.headers, "host"),
            Some("codewhisperer.us-east-1.amazonaws.com")
        );
        assert!(
            header_value(&usage.headers, "user-agent")
                .is_some_and(|value| value.contains("api/codewhispererruntime#1.0.0"))
        );
    }

    #[test]
    fn codewhisperer_set_preference_uses_q_host_and_profile_arn() {
        let endpoint = CodewhispererEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            api_region: Some("us-east-1".to_string()),
            profile_arn: Some("arn:aws:codewhisperer:us-east-1:123:profile/test".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };

        let parts = endpoint
            .set_preference_request_parts(&ctx, "DISABLED")
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
        assert_eq!(body["overageConfiguration"]["overageStatus"], "DISABLED");
        assert_eq!(
            body["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/test"
        );
    }

    #[test]
    fn codewhisperer_endpoint_regionalizes_non_us_east_1_to_q_host() {
        let endpoint = CodewhispererEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            api_region: Some("eu-central-1".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
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

        let usage = endpoint.usage_request_parts(&ctx, false).unwrap();
        assert!(
            usage
                .url
                .starts_with("https://q.eu-central-1.amazonaws.com/")
        );
        assert!(!usage.url.contains("codewhisperer.eu-central-1"));
        assert!(
            usage
                .headers
                .iter()
                .any(|(key, value)| { *key == "host" && value == "q.eu-central-1.amazonaws.com" })
        );
    }

    #[test]
    fn codewhisperer_endpoint_prefers_profile_arn_region() {
        let endpoint = CodewhispererEndpoint::new();
        let mut config = Config::default();
        config.api_region = Some("us-east-1".to_string());
        let credentials = KiroCredentials {
            api_region: Some("us-east-1".to_string()),
            profile_arn: Some("arn:aws:codewhisperer:eu-central-1:123:profile/test".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
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
            .set_preference_request_parts(&ctx, "DISABLED")
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
}

impl Default for CodewhispererEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for CodewhispererEndpoint {
    fn name(&self) -> &'static str {
        CODEWHISPERER_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/generateAssistantResponse", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("Accept", "*/*")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header(
                "X-Amz-Target",
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
            )
            .header("Authorization", format!("Bearer {}", ctx.token));

        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        }
        if ctx.credentials.is_external_idp_credential() {
            req = req.header("TokenType", "EXTERNAL_IDP");
        }
        req
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
        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        }
        if ctx.credentials.is_external_idp_credential() {
            req = req.header("TokenType", "EXTERNAL_IDP");
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> anyhow::Result<String> {
        Self::inject_profile_arn(body, ctx.credentials)
    }

    fn usage_request_parts(
        &self,
        ctx: &RequestContext<'_>,
        need_email: bool,
    ) -> anyhow::Result<UsageRequestParts> {
        let host = self.host(ctx);
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
        if let Some(profile_arn) = ctx.credentials.profile_arn_trimmed() {
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
        if let Some(profile_arn) = ctx.credentials.profile_arn_trimmed() {
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
