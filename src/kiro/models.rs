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

fn build_endpoint(credentials: &KiroCredentials, config: &Config) -> anyhow::Result<Box<dyn KiroEndpoint>> {
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
    let mut parts = endpoint.usage_request_parts(ctx)?;
    let host = parts
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| format!("q.{}.amazonaws.com", ctx.credentials.effective_api_region(ctx.config)));
    parts.url = build_list_models_url(
        &host,
        ctx.credentials.profile_arn.as_deref(),
        None,
        None,
    )?;
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
        credentials.profile_arn.as_deref(),
        model_provider,
        next_token,
    )
    .map_err(|e| e.to_string())?;

    // ListAvailableModels 不要 tokentype header
    parts.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("tokentype"));
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
