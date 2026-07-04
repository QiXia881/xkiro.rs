//! Kiro `ListAvailableModels` 客户端
//!
//! 查询单个凭据可用的上游模型清单（区分 IDE / CLI 端点 user-agent；不区分 Internal provider）。
//! 完整行为对齐上游 profile/model 列表流程：
//! - URL: `https://q.{api_region}.amazonaws.com/ListAvailableModels`
//! - 查询参数：`origin=AI_EDITOR`、`maxResults=50`、可选 `profileArn` / `modelProvider` / `nextToken`
//! - 翻页直到 `nextToken` 为空，把 `default_model` 标记到列表
//! - 失败语义复用 `Err(String)`：401 / 403 / Other

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{
    AMAZONQ_ENDPOINT_NAME, AmazonQEndpoint, CLI_ENDPOINT_NAME, CODEWHISPERER_ENDPOINT_NAME,
    CliEndpoint, CodewhispererEndpoint, IDE_ENDPOINT_NAME, IdeEndpoint, KiroEndpoint,
    RequestContext, UsageRequestParts,
};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

const DEFAULT_PROFILE_REGIONS: &[&str] = &["us-east-1", "eu-central-1"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModelTokenLimits {
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModelPromptCaching {
    pub maximum_cache_checkpoints_per_request: Option<i64>,
    pub minimum_tokens_per_cache_checkpoint: Option<i64>,
    pub supports_prompt_caching: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModel {
    pub model_id: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub description: String,
    pub provider: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub context_window: Option<i64>,
    pub is_default: Option<bool>,
    pub rate_multiplier: Option<f64>,
    pub rate_unit: Option<String>,
    pub prompt_caching: Option<AvailableModelPromptCaching>,
    #[serde(default)]
    pub supported_input_types: Vec<String>,
    pub token_limits: Option<AvailableModelTokenLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListAvailableModelsResponse {
    #[serde(default, alias = "models")]
    pub available_models: Vec<AvailableModel>,
    pub next_token: Option<String>,
    pub default_model: Option<AvailableModel>,
}

fn build_endpoint(
    credentials: &KiroCredentials,
    config: &Config,
) -> anyhow::Result<Box<dyn KiroEndpoint>> {
    match credentials.effective_endpoint_name(Some(&config.default_endpoint)) {
        IDE_ENDPOINT_NAME => Ok(Box::new(IdeEndpoint::new())),
        CODEWHISPERER_ENDPOINT_NAME => Ok(Box::new(CodewhispererEndpoint::new())),
        AMAZONQ_ENDPOINT_NAME => Ok(Box::new(AmazonQEndpoint::new())),
        CLI_ENDPOINT_NAME => Ok(Box::new(CliEndpoint::new())),
        name => anyhow::bail!("未知端点: {}", name),
    }
}

fn build_list_models_url(
    api_host: &str,
    profile_arn: Option<&str>,
    model_provider: Option<&str>,
    next_token: Option<&str>,
) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(&format!("https://{api_host}/"))?;
    url.set_path("ListAvailableModels");
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("origin", "AI_EDITOR");
        pairs.append_pair("maxResults", "50");
        if let Some(arn) = profile_arn
            .map(str::trim)
            .filter(|v| KiroCredentials::is_valid_profile_arn(v))
        {
            pairs.append_pair("profileArn", arn);
        }
        if let Some(provider) = model_provider.map(str::trim).filter(|v| !v.is_empty()) {
            pairs.append_pair("modelProvider", provider);
        }
        if let Some(nt) = next_token.map(str::trim).filter(|v| !v.is_empty()) {
            pairs.append_pair("nextToken", nt);
        }
    }
    Ok(url.into())
}

fn list_models_request_parts(
    endpoint: &dyn KiroEndpoint,
    ctx: &RequestContext<'_>,
) -> anyhow::Result<UsageRequestParts> {
    let mut parts = endpoint.usage_request_parts(ctx, false)?;
    let host = parts
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| {
            format!(
                "q.{}.amazonaws.com",
                ctx.credentials.effective_kiro_api_region(ctx.config)
            )
        });
    parts.url = build_list_models_url(&host, ctx.credentials.profile_arn_trimmed(), None, None)?;
    Ok(parts)
}

fn list_profiles_request_parts(
    endpoint: &dyn KiroEndpoint,
    ctx: &RequestContext<'_>,
) -> anyhow::Result<UsageRequestParts> {
    let mut parts = endpoint.usage_request_parts(ctx, false)?;
    let host = parts
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| {
            crate::kiro::endpoint::codewhisperer_rest_host_for_region(
                ctx.credentials.effective_kiro_api_region(ctx.config),
            )
        });
    parts.url = format!("https://{host}/ListAvailableProfiles");
    Ok(parts)
}

fn remove_api_key_token_type_header(headers: &mut Vec<(&'static str, String)>) {
    headers.retain(|(k, _)| *k != "tokentype");
}

fn refresh_invocation_id(headers: &mut [(&'static str, String)]) {
    if let Some(slot) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("amz-sdk-invocation-id"))
    {
        slot.1 = Uuid::new_v4().to_string();
    }
}

async fn fetch_page(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    model_provider: Option<&str>,
    next_token: Option<&str>,
) -> Result<ListAvailableModelsResponse, String> {
    let machine_id_value = machine_id::generate_from_credentials(credentials, config);
    let endpoint = build_endpoint(credentials, config).map_err(|e| e.to_string())?;
    let ctx = RequestContext {
        credentials,
        token,
        machine_id: &machine_id_value,
        config,
    };
    let mut parts =
        list_models_request_parts(endpoint.as_ref(), &ctx).map_err(|e| e.to_string())?;

    let host = parts
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    parts.url = build_list_models_url(
        &host,
        credentials.profile_arn_trimmed(),
        model_provider,
        next_token,
    )
    .map_err(|e| e.to_string())?;

    remove_api_key_token_type_header(&mut parts.headers);
    refresh_invocation_id(&mut parts.headers);

    let client = build_client(proxy, 60, config.tls_backend).map_err(|e| e.to_string())?;
    let mut req = client.get(&parts.url).header("accept", "application/json");
    for (name, value) in &parts.headers {
        req = req.header(*name, value);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("ListAvailableModels 请求失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 401 {
            return Err(format!("AUTH_ERROR: ListAvailableModels {status}: {body}"));
        }
        if status.as_u16() == 403 {
            if body.contains("AccessDeniedException") && body.contains("TemporarilySuspended") {
                return Err(format!("BANNED: ListAvailableModels 403: {body}"));
            }
            return Err(format!("AUTH_ERROR: ListAvailableModels 403: {body}"));
        }
        return Err(format!("ListAvailableModels failed ({status}): {body}"));
    }
    let body_text = resp
        .text()
        .await
        .map_err(|e| format!("读取响应失败: {e}"))?;
    serde_json::from_str::<ListAvailableModelsResponse>(&body_text)
        .map_err(|e| format!("解析 ListAvailableModels 响应失败: {e}; body={body_text}"))
}

fn mark_default(models: &mut [AvailableModel], default_id: Option<&str>) {
    if let Some(id) = default_id {
        for m in models {
            if m.model_id == id && m.is_default.is_none() {
                m.is_default = Some(true);
            }
        }
    }
}

fn sort_for_display(models: &mut [AvailableModel]) {
    models.sort_by_key(|m| !m.is_default.unwrap_or(false));
}

/// 拉全量模型列表（自动翻页）
pub async fn fetch_all_available_models(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    model_provider: Option<&str>,
) -> Result<ListAvailableModelsResponse, String> {
    let mut aggregated = ListAvailableModelsResponse::default();
    let mut next_token: Option<String> = None;
    loop {
        let mut page = fetch_page(
            credentials,
            config,
            token,
            proxy,
            model_provider,
            next_token.as_deref(),
        )
        .await?;
        if aggregated.default_model.is_none() {
            aggregated.default_model = page.default_model.clone();
        }
        let default_id = aggregated
            .default_model
            .as_ref()
            .map(|m| m.model_id.as_str());
        mark_default(&mut page.available_models, default_id);
        if let Some(default_model) = aggregated.default_model.as_mut() {
            default_model.is_default = Some(true);
        }
        aggregated.available_models.extend(page.available_models);
        next_token = page.next_token;
        if next_token.is_none() {
            break;
        }
    }
    sort_for_display(&mut aggregated.available_models);
    aggregated.next_token = None;
    Ok(aggregated)
}

// ============================================================================
// ListAvailableProfiles API
// ============================================================================

/// ListAvailableProfiles 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAvailableProfilesResponse {
    #[serde(default)]
    pub profiles: Vec<ProfileEntry>,
}

/// Profile 条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileEntry {
    pub arn: String,
    #[serde(default)]
    pub profile_name: Option<String>,
    #[serde(default)]
    pub profile_status: Option<String>,
}

/// 查询可用 Profile 列表并返回第一个有效 profileArn
///
/// - POST https://q.{region}.amazonaws.com/ListAvailableProfiles
/// - Body: `{"maxResults": 10}`
/// - 返回第一个非空 arn
///
/// # Arguments
/// * `credentials` - 凭据信息（用于获取 region 和 token）
/// * `config` - 全局配置
/// * `token` - Bearer token
/// * `proxy` - 可选代理配置
pub async fn list_available_profiles(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> Result<String, String> {
    let endpoint =
        build_endpoint(credentials, config).map_err(|e| format!("构建 endpoint 失败: {e}"))?;
    let rctx = RequestContext {
        credentials,
        token,
        machine_id: &machine_id::generate_from_credentials(credentials, config),
        config,
    };

    let parts = list_profiles_request_parts(endpoint.as_ref(), &rctx).map_err(|e| e.to_string())?;

    let client = build_client(proxy, 30, config.tls_backend)
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    let mut request = client
        .post(&parts.url)
        .header("content-type", "application/json")
        .body(r#"{"maxResults":10}"#);
    for (name, value) in &parts.headers {
        request = request.header(*name, value);
    }

    let resp = request
        .send()
        .await
        .map_err(|e| format!("ListAvailableProfiles 请求失败: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "ListAvailableProfiles {} {}: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            body
        ));
    }

    let body_text = resp
        .text()
        .await
        .map_err(|e| format!("读取 ListAvailableProfiles 响应失败: {e}"))?;

    let result: ListAvailableProfilesResponse = serde_json::from_str(&body_text)
        .map_err(|e| format!("解析 ListAvailableProfiles 响应失败: {e}; body={body_text}"))?;

    result
        .profiles
        .iter()
        .find(|p| !p.arn.trim().is_empty())
        .map(|p| p.arn.trim().to_string())
        .ok_or_else(|| "ListAvailableProfiles 返回空 profile 列表".to_string())
}

/// 带重试的 ListAvailableProfiles 调用
///
/// - 最多重试 3 次
/// - 仅对瞬态错误（网络错误、5xx、429）重试
/// - 空 profile 列表和 4xx（非 429）不重试
pub async fn list_available_profiles_with_retry(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> Result<String, String> {
    let mut last_err = String::new();
    for region in profile_region_candidates(credentials, config) {
        let mut regional_credentials = credentials.clone();
        regional_credentials.api_region = Some(region.clone());
        match list_available_profiles_with_retry_in_region(
            &regional_credentials,
            config,
            token,
            proxy,
            &region,
        )
        .await
        {
            Ok(arn) => return Ok(arn),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

async fn list_available_profiles_with_retry_in_region(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    region: &str,
) -> Result<String, String> {
    const MAX_ATTEMPTS: usize = 3;
    let mut backoff_ms: u64 = 200;
    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        match list_available_profiles(credentials, config, token, proxy).await {
            Ok(arn) => return Ok(arn),
            Err(e) => {
                last_err = e;
                if !is_transient_profile_error(&last_err) || attempt == MAX_ATTEMPTS {
                    return Err(last_err);
                }
                tracing::debug!(
                    "ListAvailableProfiles 瞬态失败 region={} (尝试 {}/{}): {}",
                    region,
                    attempt,
                    MAX_ATTEMPTS,
                    last_err
                );
                tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms *= 2;
            }
        }
    }
    Err(last_err)
}

fn profile_region_candidates(credentials: &KiroCredentials, config: &Config) -> Vec<String> {
    let mut out = Vec::new();

    push_unique_region(&mut out, profile_lookup_region(credentials, config));
    if !should_probe_fallback_regions(credentials) {
        return out;
    }

    if let Ok(regions) = std::env::var("KIRO_PROFILE_REGIONS") {
        for region in regions.split(',') {
            push_unique_region(&mut out, region);
        }
        return out;
    }

    for region in DEFAULT_PROFILE_REGIONS {
        push_unique_region(&mut out, region);
    }
    out
}

fn profile_lookup_region<'a>(credentials: &'a KiroCredentials, config: &'a Config) -> &'a str {
    if let Some(region) = credentials.profile_arn_region() {
        return region;
    }
    if let Some(region) = credentials
        .api_region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return region;
    }
    if let Some(region) = credentials
        .region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return region;
    }
    config.effective_api_region()
}

fn push_unique_region(out: &mut Vec<String>, region: &str) {
    let region = region.trim();
    if region.is_empty() || out.iter().any(|existing| existing == region) {
        return;
    }
    out.push(region.to_string());
}

fn should_probe_fallback_regions(credentials: &KiroCredentials) -> bool {
    credentials.is_external_idp_credential()
        || credentials
            .api_region
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
            && credentials
                .region
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
}

/// 判断是否为瞬态错误（值得重试）
fn is_transient_profile_error(err: &str) -> bool {
    if err.contains("空 profile 列表") || err.contains("empty profile") {
        return false;
    }
    // HTTP 5xx / 429 是瞬态的
    if err.contains(" 5") || err.contains(" 429") {
        return true;
    }
    // 网络错误是瞬态的
    !err.contains(" 4")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_value<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn list_models_url_uses_trimmed_profile_arn() {
        let mut credentials = KiroCredentials::default();
        credentials.profile_arn = Some(" arn:aws:codewhisperer:profile/test ".to_string());

        let url = build_list_models_url(
            "q.us-east-1.amazonaws.com",
            credentials.profile_arn_trimmed(),
            Some(" anthropic "),
            Some(" next "),
        )
        .unwrap();

        assert!(url.contains("profileArn=arn%3Aaws%3Acodewhisperer%3Aprofile%2Ftest"));
        assert!(url.contains("modelProvider=anthropic"));
        assert!(url.contains("nextToken=next"));
        assert!(!url.contains("+arn"));
        assert!(!url.contains("test+"));
    }

    #[test]
    fn list_models_url_omits_blank_profile_arn() {
        let mut credentials = KiroCredentials::default();
        credentials.profile_arn = Some("   ".to_string());

        let url = build_list_models_url(
            "q.us-east-1.amazonaws.com",
            credentials.profile_arn_trimmed(),
            None,
            None,
        )
        .unwrap();

        assert!(!url.contains("profileArn="));
    }

    #[test]
    fn list_models_url_omits_microsoft_uuid_profile_id() {
        let url = build_list_models_url(
            "q.us-east-1.amazonaws.com",
            Some("e3438419-4424-4e57-8990-ef76bd749a44"),
            None,
            None,
        )
        .unwrap();

        assert!(!url.contains("profileArn="));
        assert!(!url.contains("e3438419-4424-4e57-8990-ef76bd749a44"));
    }

    #[test]
    fn list_models_request_uses_kiro_go_rest_shape() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };

        let parts = list_models_request_parts(&endpoint, &ctx).unwrap();

        assert_eq!(
            parts.url,
            "https://codewhisperer.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR&maxResults=50"
        );
        assert_eq!(
            header_value(&parts.headers, "host"),
            Some("codewhisperer.us-east-1.amazonaws.com")
        );
        assert!(
            header_value(&parts.headers, "user-agent")
                .is_some_and(|value| value.contains("api/codewhispererruntime#1.0.0"))
        );
        assert!(
            !parts
                .headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("X-Amz-Target"))
        );
    }

    #[test]
    fn list_profiles_request_uses_runtime_rest_shape() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };

        let parts = list_profiles_request_parts(&endpoint, &ctx).unwrap();

        assert_eq!(
            parts.url,
            "https://codewhisperer.us-east-1.amazonaws.com/ListAvailableProfiles"
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
        assert!(
            !parts
                .headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("X-Amz-Target"))
        );
        assert!(
            !parts
                .headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("x-amzn-kiro-agent-mode"))
        );
        assert_ne!(header_value(&parts.headers, "Accept"), Some("*/*"));
    }

    #[test]
    fn external_idp_profile_candidates_include_default_fallback_regions() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.auth_method = Some("external_idp".to_string());

        assert_eq!(
            profile_region_candidates(&credentials, &config),
            vec!["us-east-1".to_string(), "eu-central-1".to_string()]
        );
    }

    #[test]
    fn list_profiles_request_prefers_profile_arn_region() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:123:profile/test".to_string());
        let endpoint = IdeEndpoint::new();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };
        let parts = list_profiles_request_parts(&endpoint, &ctx).unwrap();

        assert_eq!(
            parts.url,
            "https://q.eu-central-1.amazonaws.com/ListAvailableProfiles"
        );
        assert_eq!(
            header_value(&parts.headers, "host"),
            Some("q.eu-central-1.amazonaws.com")
        );
    }

    #[test]
    fn external_idp_requests_include_token_type() {
        let endpoint = IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            auth_method: Some("external_idp".to_string()),
            provider: Some("AzureAD".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };
        let models = list_models_request_parts(&endpoint, &ctx).unwrap();
        let profiles = list_profiles_request_parts(&endpoint, &ctx).unwrap();

        assert_eq!(
            header_value(&models.headers, "TokenType"),
            Some("EXTERNAL_IDP")
        );
        assert_eq!(
            header_value(&profiles.headers, "TokenType"),
            Some("EXTERNAL_IDP")
        );
    }

    #[test]
    fn list_models_removes_api_key_tokentype_without_removing_external_idp_token_type() {
        let mut headers = vec![
            ("tokentype", "API_KEY".to_string()),
            ("TokenType", "EXTERNAL_IDP".to_string()),
        ];

        remove_api_key_token_type_header(&mut headers);

        assert!(!headers.iter().any(|(key, _)| *key == "tokentype"));
        assert_eq!(header_value(&headers, "TokenType"), Some("EXTERNAL_IDP"));
    }

    #[test]
    fn profile_candidates_prefer_profile_arn_region() {
        let mut config = Config::default();
        config.api_region = Some("us-east-1".to_string());
        let mut credentials = KiroCredentials::default();
        credentials.auth_method = Some("external_idp".to_string());
        credentials.api_region = Some("us-west-2".to_string());
        credentials.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:123:profile/test".to_string());

        assert_eq!(
            profile_region_candidates(&credentials, &config),
            vec!["eu-central-1".to_string(), "us-east-1".to_string()]
        );
    }

    #[test]
    fn explicit_non_external_idp_api_region_does_not_probe_fallbacks() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.auth_method = Some("idc".to_string());
        credentials.api_region = Some("ap-southeast-1".to_string());

        assert_eq!(
            profile_region_candidates(&credentials, &config),
            vec!["ap-southeast-1".to_string()]
        );
    }

    #[test]
    fn non_external_idp_with_region_does_not_probe_fallbacks() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.auth_method = Some("idc".to_string());
        credentials.region = Some("ap-southeast-1".to_string());

        assert_eq!(
            profile_region_candidates(&credentials, &config),
            vec!["ap-southeast-1".to_string()]
        );
    }
}
