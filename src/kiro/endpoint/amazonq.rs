//! Amazon Q data-plane endpoint used by `preferredEndpoint=amazonq`.

use reqwest::RequestBuilder;

use super::{IdeEndpoint, KiroEndpoint, PreferenceRequestParts, RequestContext, UsageRequestParts};

pub const AMAZONQ_ENDPOINT_NAME: &str = "amazonq";
const AMAZONQ_API_TARGET: &str = "AmazonQDeveloperStreamingService.SendMessage";

pub struct AmazonQEndpoint {
    ide: IdeEndpoint,
}

impl AmazonQEndpoint {
    pub fn new() -> Self {
        Self {
            ide: IdeEndpoint::new(),
        }
    }
}

impl Default for AmazonQEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for AmazonQEndpoint {
    fn name(&self) -> &'static str {
        AMAZONQ_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        self.ide.api_url(ctx)
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        self.ide.mcp_url(ctx)
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        self.ide
            .decorate_api(req, ctx)
            .header("X-Amz-Target", AMAZONQ_API_TARGET)
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        self.ide.decorate_mcp(req, ctx)
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> anyhow::Result<String> {
        self.ide.transform_api_body(body, ctx)
    }

    fn transform_mcp_body(&self, body: &str, ctx: &RequestContext<'_>) -> anyhow::Result<String> {
        self.ide.transform_mcp_body(body, ctx)
    }

    fn usage_request_parts(
        &self,
        ctx: &RequestContext<'_>,
        need_email: bool,
    ) -> anyhow::Result<UsageRequestParts> {
        self.ide.usage_request_parts(ctx, need_email)
    }

    fn set_preference_request_parts(
        &self,
        ctx: &RequestContext<'_>,
        overage_status: &str,
    ) -> anyhow::Result<PreferenceRequestParts> {
        self.ide.set_preference_request_parts(ctx, overage_status)
    }

    fn is_monthly_request_limit(&self, body: &str) -> bool {
        self.ide.is_monthly_request_limit(body)
    }

    fn is_bearer_token_invalid(&self, body: &str) -> bool {
        self.ide.is_bearer_token_invalid(body)
    }
}

#[cfg(test)]
mod tests {
    use super::{AMAZONQ_API_TARGET, AmazonQEndpoint};
    use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    #[test]
    fn amazonq_endpoint_uses_data_plane_headers_without_cli_rewrite() {
        let endpoint = AmazonQEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-123",
            config: &config,
        };

        let req = reqwest::Client::new()
            .post(endpoint.api_url(&ctx))
            .header("content-type", "application/json");
        let request = endpoint.decorate_api(req, &ctx).build().unwrap();
        let headers = request.headers();

        assert_eq!(
            request.url().as_str(),
            "https://q.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            headers.get("x-amz-target").and_then(|v| v.to_str().ok()),
            Some(AMAZONQ_API_TARGET)
        );
        assert_eq!(
            headers.get("accept").and_then(|v| v.to_str().ok()),
            Some("*/*")
        );
        let user_agent = headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(user_agent.contains("KiroIDE-"));
        assert!(!user_agent.contains("AmazonQ-For-CLI"));

        let body = endpoint
            .transform_api_body(
                r#"{"conversationState":{"currentMessage":{"userInputMessage":{"origin":"AI_EDITOR"}}}}"#,
                &ctx,
            )
            .unwrap();
        assert!(body.contains(r#""origin":"AI_EDITOR""#));
        assert!(!body.contains("KIRO_CLI"));
    }

    #[test]
    fn amazonq_endpoint_prefers_profile_arn_region() {
        let endpoint = AmazonQEndpoint::new();
        let mut config = Config::default();
        config.api_region = Some("us-east-1".to_string());
        let credentials = KiroCredentials {
            api_region: Some("us-west-2".to_string()),
            profile_arn: Some("arn:aws:codewhisperer:eu-central-1:123:profile/test".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine-123",
            config: &config,
        };

        let req = reqwest::Client::new().post(endpoint.api_url(&ctx));
        let request = endpoint.decorate_api(req, &ctx).build().unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://q.eu-central-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            request.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("q.eu-central-1.amazonaws.com")
        );
        assert_eq!(
            request
                .headers()
                .get("x-amz-target")
                .and_then(|v| v.to_str().ok()),
            Some(AMAZONQ_API_TARGET)
        );
    }

    #[test]
    fn amazonq_endpoint_repairs_known_bad_profile_region() {
        let endpoint = AmazonQEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            api_region: Some("us-east-1".to_string()),
            profile_arn: Some("arn:aws:codewhisperer:eu-north-1:123:profile/test".to_string()),
            ..Default::default()
        };
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
            request.url().as_str(),
            "https://q.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            request.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("q.us-east-1.amazonaws.com")
        );
    }
}
