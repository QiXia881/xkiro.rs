//! Kiro `ListAvailableModels` 客户端
//!
//! 查询单个凭据可用的上游模型清单（区分 IDE / CLI 端点 user-agent；不区分 Internal provider）。
//! 完整行为对齐参考 `kiro-account-manager`：
//! - URL: `https://q.{api_region}.amazonaws.com/ListAvailableModels`
//! - 查询参数：`origin=AI_EDITOR`、`maxResults=50`、可选 `profileArn` / `modelProvider` / `nextToken`
//! - 翻页直到 `nextToken` 为空，把 `default_model` 标记到列表
//! - 失败语义复用 `Err(String)`：401 / 403 / Other

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{
    CLI_ENDPOINT_NAME, CliEndpoint, IDE_ENDPOINT_NAME, IdeEndpoint, KiroEndpoint, RequestContext,
    UsageRequestParts,
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
        CLI_ENDPOINT_NAME => Ok(Box::new(CliEndpoint::new())),
        name => anyhow::bail!("未知 endpoint: {}", name),
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
        if let Some(arn) = profile_arn.filter(|v| !v.trim().is_empty()) {
            pairs.append_pair("profileArn", arn);
        }
        if let Some(provider) = model_provider.filter(|v| !v.trim().is_empty()) {
            pairs.append_pair("modelProvider", provider);
        }
        if let Some(nt) = next_token.filter(|v| !v.trim().is_empty()) {
            pairs.append_pair("nextToken", nt);
        }
    }
    Ok(url.into())
}
/// 提取仅 ListAvailableModels 需要的 host + headers
///
/// 复用 endpoint 的 `usage_request_parts` 拿 host / user-agent 风格，
/// 避免重复维护 IDE / CLI 两套 UA 字符串。
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
                ctx.credentials.effective_api_region(ctx.config)
            )
        });
    parts.url = build_list_models_url(&host, ctx.credentials.profile_arn_trimmed(), None, None)?;
    Ok(parts)
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

    // ListAvailableModels 不要 tokentype header
    parts
        .headers
        .retain(|(k, _)| !k.eq_ignore_ascii_case("tokentype"));
    // 替换 invocation-id 为新值（避免页间重复）
    if let Some(slot) = parts
        .headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("amz-sdk-invocation-id"))
    {
        slot.1 = Uuid::new_v4().to_string();
    }

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
/// 对齐 Kiro-Go `listAvailableProfiles()`:
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

    // 构建 URL
    let api_region = credentials.effective_api_region(config);
    let host = format!("q.{}.amazonaws.com", api_region);
    let url = format!("https://{}/ListAvailableProfiles", host);

    let client = build_client(proxy, 30, config.tls_backend)
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    let base = client
        .post(&url)
        .header("content-type", "application/json")
        .body(r#"{"maxResults":10}"#);

    let request = endpoint.decorate_api(base, &rctx);

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
/// 对齐 Kiro-Go `listAvailableProfilesWithRetry()`:
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

    push_unique_region(&mut out, credentials.effective_api_region(config));
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

    #[test]
    fn list_models_url_uses_trimmed_profile_arn() {
        let mut credentials = KiroCredentials::default();
        credentials.profile_arn = Some(" arn:aws:codewhisperer:profile/test ".to_string());

        let url = build_list_models_url(
            "q.us-east-1.amazonaws.com",
            credentials.profile_arn_trimmed(),
            None,
            None,
        )
        .unwrap();

        assert!(url.contains("profileArn=arn%3Aaws%3Acodewhisperer%3Aprofile%2Ftest"));
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
    fn external_idp_profile_candidates_include_kiro_go_fallback_regions() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.auth_method = Some("external_idp".to_string());

        assert_eq!(
            profile_region_candidates(&credentials, &config),
            vec!["us-east-1".to_string(), "eu-central-1".to_string()]
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
            vec!["us-east-1".to_string()]
        );
    }
}
