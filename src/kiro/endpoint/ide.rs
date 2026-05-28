//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的 AWS CodeWhisperer 端点：
//! - API: `https://q.{api_region}.amazonaws.com/generateAssistantResponse`
//! - MCP: `https://q.{api_region}.amazonaws.com/mcp`
//! - Usage: `https://q.{api_region}.amazonaws.com/getUsageLimits`
//!
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体会在根对象上注入 `profileArn`，
//! 但 AWS SSO OIDC（Builder ID / IAM Identity Center）凭据不携带 profileArn，
//! 此时反而需要从 body / header 移除该字段。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, PreferenceRequestParts, RequestContext, UsageRequestParts};
use crate::kiro::model::credentials::KiroCredentials;

/// Kiro IDE 端点名称
pub const IDE_ENDPOINT_NAME: &str = "ide";

/// Kiro IDE 端点
pub struct IdeEndpoint;

impl IdeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
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

    /// 判断是否为 AWS SSO OIDC 凭据（Builder ID / IAM Identity Center）
    ///
    /// SSO OIDC 凭据不携带 profileArn，发送时需要从请求体/请求头中移除。
    fn is_aws_sso_oidc_credentials(credentials: &KiroCredentials) -> bool {
        let auth_method = credentials.auth_method.as_deref();
        matches!(auth_method, Some("builder-id") | Some("idc"))
            || (credentials.client_id.is_some() && credentials.client_secret.is_some())
    }

    /// 返回 MCP 请求需要附带的 profileArn header 值，SSO OIDC 凭据返回 None
    fn mcp_profile_arn_header_value(credentials: &KiroCredentials) -> Option<&str> {
        if Self::is_aws_sso_oidc_credentials(credentials) {
            return None;
        }
        credentials.profile_arn.as_deref()
    }

    /// 将 profileArn 注入或从请求体根对象移除
    ///
    /// - SSO OIDC 凭据：解析 JSON 并 remove `profileArn`
    /// - 其它凭据有 profile_arn：解析 JSON 并 insert
    /// - 其它凭据无 profile_arn：原 body 透传
    /// - 解析失败：返回错误，由 provider 立即终止该次调用
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

        let Some(profile_arn) = Self::mcp_profile_arn_header_value(credentials) else {
            return Ok(request_body.to_string());
        };

        let mut request: serde_json::Value = serde_json::from_str(request_body)?;
        let obj = request
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("request body is not a JSON object"))?;
        obj.insert(
            "profileArn".to_string(),
            serde_json::Value::String(profile_arn.to_string()),
        );
        Ok(serde_json::to_string(&request)?)
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
        format!(
            "https://q.{}.amazonaws.com/generateAssistantResponse",
            self.api_region(ctx)
        )
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://q.{}.amazonaws.com/mcp", self.api_region(ctx))
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
            .header("Authorization", format!("Bearer {}", ctx.token));

        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
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
    use crate::kiro::model::credentials::KiroCredentials;
    use serde_json::Value;

    fn cred_with_arn(arn: Option<&str>) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.profile_arn = arn.map(|s| s.to_string());
        c
    }

    fn cred_sso(auth_method: &str) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.auth_method = Some(auth_method.to_string());
        c.profile_arn = Some("ignored-arn".to_string());
        c
    }

    fn cred_sso_oauth() -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.client_id = Some("cid".to_string());
        c.client_secret = Some("csec".to_string());
        c.profile_arn = Some("ignored-arn".to_string());
        c
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
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let cred = cred_with_arn(None);
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        // 没 arn 也没 SSO，原 body 透传
        assert_eq!(result, body);
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let cred = cred_with_arn(Some("new-arn"));
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let cred = cred_with_arn(Some("arn:test"));
        // SSO 与非 SSO 路径都要先 parse；非法 JSON 应返 Err
        assert!(IdeEndpoint::inject_profile_arn(body, &cred).is_err());
    }

    #[test]
    fn test_sso_builder_id_strips_profile_arn() {
        let body = r#"{"profileArn":"old","other":1}"#;
        let cred = cred_sso("builder-id");
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
        assert_eq!(json["other"], 1);
    }

    #[test]
    fn test_sso_idc_strips_profile_arn() {
        let body = r#"{"profileArn":"old"}"#;
        let cred = cred_sso("idc");
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
    }

    #[test]
    fn test_sso_oauth_strips_profile_arn() {
        let body = r#"{"profileArn":"old"}"#;
        let cred = cred_sso_oauth();
        let result = IdeEndpoint::inject_profile_arn(body, &cred).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
    }

    #[test]
    fn test_mcp_profile_arn_header_value_sso_returns_none() {
        let cred = cred_sso("builder-id");
        assert!(IdeEndpoint::mcp_profile_arn_header_value(&cred).is_none());
    }

    #[test]
    fn test_mcp_profile_arn_header_value_normal_returns_arn() {
        let cred = cred_with_arn(Some("arn:test"));
        assert_eq!(
            IdeEndpoint::mcp_profile_arn_header_value(&cred),
            Some("arn:test")
        );
    }
}
