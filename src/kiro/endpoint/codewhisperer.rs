//! CodeWhisperer 端点
//!
//! 对应 CodeWhisperer API 端点：
//! - API: `https://codewhisperer.{api_region}.amazonaws.com/generateAssistantResponse`
//! - X-Amz-Target: `AmazonCodeWhispererStreamingService.GenerateAssistantResponse`
//!
//! 此端点与 IDE 端点共享大部分逻辑，主要区别在于：
//! - 不同的 host（codewhisperer. vs q.）
//! - 需要 X-Amz-Target header
//! - User-Agent 使用不同的 API 名称

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, PreferenceRequestParts, RequestContext, UsageRequestParts};
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
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("codewhisperer.{}.amazonaws.com", self.api_region(ctx))
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

    fn is_aws_sso_oidc_credentials(credentials: &KiroCredentials) -> bool {
        let auth_method = credentials.auth_method.as_deref();
        matches!(auth_method, Some("builder-id") | Some("idc"))
            || (credentials.client_id.is_some() && credentials.client_secret.is_some())
    }

    fn mcp_profile_arn_header_value(credentials: &KiroCredentials) -> Option<&str> {
        if Self::is_aws_sso_oidc_credentials(credentials) {
            return None;
        }
        credentials.profile_arn_trimmed()
    }

    fn inject_profile_arn(
        request_body: &str,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<String> {
        if Self::is_aws_sso_oidc_credentials(credentials) {
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
    use crate::kiro::model::credentials::KiroCredentials;
    use serde_json::Value;

    #[test]
    fn test_inject_profile_arn_preserves_and_trims_explicit_payload_arn_like_kiro_go() {
        let body =
            r#"{"conversationState":{},"profileArn":" arn:aws:codewhisperer:profile/explicit "}"#;
        let credentials = KiroCredentials::default();
        let result = CodewhispererEndpoint::inject_profile_arn(body, &credentials).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(json["profileArn"], "arn:aws:codewhisperer:profile/explicit");
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
        format!(
            "https://codewhisperer.{}.amazonaws.com/generateAssistantResponse",
            self.api_region(ctx)
        )
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://codewhisperer.{}.amazonaws.com/mcp",
            self.api_region(ctx)
        )
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
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
        if let Some(profile_arn) = Self::mcp_profile_arn_header_value(ctx.credentials) {
            url.push_str(&format!("&profileArn={}", urlencoding::encode(profile_arn)));
        }

        let mut headers = vec![
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
        let host = self.host(ctx);
        let url = format!("https://{}/setUserPreference", host);

        let mut body = serde_json::json!({
            "overageConfiguration": { "overageStatus": overage_status },
        });
        if let Some(profile_arn) = Self::mcp_profile_arn_header_value(ctx.credentials) {
            body["profileArn"] = serde_json::Value::String(profile_arn.to_string());
        }

        let mut headers = vec![
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
