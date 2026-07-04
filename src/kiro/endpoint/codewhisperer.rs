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
    IdeEndpoint, KiroEndpoint, PreferenceRequestParts, RequestContext, UsageRequestParts,
    apply_stream_token_type_headers, codewhisperer_rest_host_for_region,
};
use crate::kiro::model::credentials::KiroCredentials;

/// CodeWhisperer 端点名称
pub const CODEWHISPERER_ENDPOINT_NAME: &str = "codewhisperer";

/// CodeWhisperer 请求的 `X-Amz-Target`
const CODEWHISPERER_API_TARGET: &str =
    "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";

/// CodeWhisperer 端点
///
/// 与 IDE 端点共享绝大部分逻辑（profileArn 注入、User-Agent、runtime headers、
/// usage / preference 请求），仅在以下三点真正不同，故内部持有一个 [`IdeEndpoint`]
/// 委托共享部分，只保留差异：
/// - host：`codewhisperer.` / 区域化 `q.` 而非 IDE 固定的 `q.`
/// - `decorate_api` 需要额外的 `X-Amz-Target` header
/// - streaming API 名称（实际字节与 IDE 相同，直接复用 IDE 的 UA 构造器）
pub struct CodewhispererEndpoint {
    ide: IdeEndpoint,
}

impl CodewhispererEndpoint {
    pub fn new() -> Self {
        Self {
            ide: IdeEndpoint::new(),
        }
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_kiro_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        codewhisperer_rest_host_for_region(self.api_region(ctx))
    }

    /// profileArn 注入逻辑与 IDE 完全一致，直接委托。
    ///
    /// 保留此关联函数是为了让既有测试 `CodewhispererEndpoint::inject_profile_arn`
    /// 继续以相同签名调用。
    fn inject_profile_arn(
        request_body: &str,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<String> {
        IdeEndpoint::inject_profile_arn(request_body, credentials)
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
        // 与 IDE decorate_api 的头集合一致，仅 host 不同并额外附带 X-Amz-Target。
        // reqwest 的 `.header()` 是追加而非覆盖，无法直接委托 IDE（会产生重复 host），
        // 故此处复用 IDE 的 UA 构造器与 tokenType 追加逻辑，只保留真正的差异。
        let req = req
            .header("Accept", "*/*")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.ide.x_amz_user_agent(ctx))
            .header("user-agent", self.ide.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("X-Amz-Target", CODEWHISPERER_API_TARGET)
            .header("Authorization", format!("Bearer {}", ctx.token));

        apply_stream_token_type_headers(req, ctx.credentials)
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.ide.x_amz_user_agent(ctx))
            .header("user-agent", self.ide.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(profile_arn) = IdeEndpoint::mcp_profile_arn_header_value(ctx.credentials) {
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
        // getUsageLimits 请求与 IDE 完全一致：IDE 的 rest_host 与本端点 host 都解析为
        // codewhisperer_rest_host_for_region，产出字节相同，直接委托。
        self.ide.usage_request_parts(ctx, need_email)
    }

    fn set_preference_request_parts(
        &self,
        ctx: &RequestContext<'_>,
        overage_status: &str,
    ) -> anyhow::Result<PreferenceRequestParts> {
        // setUserPreference 请求与 IDE 完全一致：两端都用 q_rest_host_for_region，直接委托。
        self.ide.set_preference_request_parts(ctx, overage_status)
    }
}
