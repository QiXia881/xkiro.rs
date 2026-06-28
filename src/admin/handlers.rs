//! Admin API HTTP 处理器

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;

use super::{
    middleware::AdminState,
    types::{
        AddCredentialRequest, BatchOperationRequest, BatchRefreshRequest,
        CompleteIamSsoLoginRequest, CompleteKiroSsoLoginRequest, CompleteSocialCallbackRequest,
        CompleteSocialLoginRequest, CreateApiKeyRequest, ExportKamRequest, ExportTokenJsonRequest,
        ImportSsoTokenRequest, ImportTokenJsonRequest, KiroGoImportCredentialsRequest,
        KiroGoUpdateAccountRequest, PollBuilderIdLoginRequest, PollBuilderIdLoginResponse,
        PollKiroSsoLoginRequest, SetConcurrencyRequest, SetDisabledRequest, SetEndpointRequest,
        SetOverageRequest, SetPriorityRequest, SetRegionRequest, StartBuilderIdLoginRequest,
        StartIdcLoginRequest, StartKiroSsoLoginRequest, StartSocialLoginRequest, SuccessResponse,
        UpdateApiKeyRequest, UpdateEndpointConfigRequest, UpdateGlobalConfigRequest,
        UpdatePromptFilterConfigRequest, UpdateProxyConfigRequest, UpdateSettingsRequest,
        UpdateSystemPromptRequest, UpdateThinkingConfigRequest, UpsertUserPresetRequest,
    },
};
use crate::model::config::CompressionConfig;

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct KiroGoExportRequest {
    #[serde(default)]
    ids: Vec<serde_json::Value>,
}

fn parse_expires_at_millis(value: Option<&str>) -> Option<i64> {
    value
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
}

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials();
    Json(response)
}

/// GET /api/admin/credentials/balances/cached
/// 获取所有凭据的缓存余额（双源合并：token_manager 运行时缓存 + AdminService 磁盘缓存）
pub async fn get_cached_balances(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_cached_balances())
}

/// POST /api/admin/credentials/:id/disabled
/// 设置凭据禁用状态
pub async fn set_credential_disabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetDisabledRequest>,
) -> impl IntoResponse {
    match state.service.set_disabled(id, payload.disabled) {
        Ok(_) => {
            let action = if payload.disabled { "禁用" } else { "启用" };
            Json(SuccessResponse::new(format!("凭据 #{} 已{}", id, action))).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/priority
/// 设置凭据优先级
pub async fn set_credential_priority(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetPriorityRequest>,
) -> impl IntoResponse {
    match state.service.set_priority(id, payload.priority) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 优先级已设置为 {}",
            id, payload.priority
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/concurrency
/// 设置单凭据独立并发上限（None=回退全局）
pub async fn set_credential_concurrency(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetConcurrencyRequest>,
) -> impl IntoResponse {
    match state.service.set_concurrency(id, payload.concurrency) {
        Ok(_) => {
            let msg = match payload.concurrency {
                Some(n) => format!("凭据 #{} 独立并发上限已设置为 {}", id, n),
                None => format!("凭据 #{} 已恢复使用全局并发上限", id),
            };
            Json(SuccessResponse::new(msg)).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/reset
/// 重置失败计数并重新启用
pub async fn reset_failure_count(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reset_and_enable(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 失败计数已重置并重新启用",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/balance?force=1
/// 获取指定凭据的余额；force=1 跳过缓存强制走云端
pub async fn get_credential_balance(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let force = params
        .get("force")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    match state.service.get_balance(id, force).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/models?provider=anthropic&force=1
/// 拉取指定凭据可用模型列表（30 分钟内缓存复用）
pub async fn get_credential_models(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let force = params
        .get("force")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let provider = params
        .get("provider")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match state
        .service
        .list_available_models(id, provider.as_deref(), force)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials
/// 添加新凭据
pub async fn add_credential(
    State(state): State<AdminState>,
    Json(payload): Json<AddCredentialRequest>,
) -> impl IntoResponse {
    match state.service.add_credential(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/credentials
/// Kiro-Go 兼容导入路径：支持 external_idp trust-on-import 和回退刷新。
pub async fn import_credentials_kiro_go(
    State(state): State<AdminState>,
    Json(payload): Json<KiroGoImportCredentialsRequest>,
) -> impl IntoResponse {
    match state.service.import_kiro_go_credential(payload).await {
        Ok(resp) => Json(serde_json::json!({
            "success": true,
            "account": {
                "id": resp.credential_id,
                "email": resp.email,
            },
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/credentials/:id
/// 删除凭据
pub async fn delete_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/accounts/:id/full
/// Kiro-Go 兼容完整账号信息，仅返回本地可导出的 OAuth 凭据字段。
pub async fn get_account_full_kiro_go(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let snapshot = state.service.token_manager_snapshot();
    let entry = match snapshot.entries.iter().find(|entry| entry.id == id) {
        Some(entry) => entry,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Account not found"})),
            )
                .into_response();
        }
    };

    let mut exported = state.service.export_credentials_to_kam(&[id]);
    let account = match exported.pop() {
        Some(account) => account,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "credential is not exportable"})),
            )
                .into_response();
        }
    };

    Json(serde_json::json!({
        "id": id,
        "email": account.email,
        "userId": account.user_id,
        "nickname": account.label,
        "accessToken": account.access_token,
        "refreshToken": account.refresh_token,
        "clientId": account.client_id,
        "clientSecret": account.client_secret,
        "authMethod": account.auth_method,
        "provider": account.provider,
        "region": account.region,
        "expiresAt": parse_expires_at_millis(account.expires_at.as_deref()),
        "machineId": account.machine_id,
        "weight": entry.weight,
        "profileArn": account.profile_arn,
        "proxyURL": entry.proxy_url,
        "enabled": !entry.disabled,
        "requestCount": entry.success_count,
        "errorCount": entry.failure_count,
        "totalTokens": 0,
        "totalCredits": 0.0,
        "lastUsed": entry.last_used_at,
    }))
    .into_response()
}

/// PUT /api/admin/accounts/:id
/// Kiro-Go 兼容更新路径；仅适配本地已有等价语义的字段。
pub async fn update_account_kiro_go(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<KiroGoUpdateAccountRequest>,
) -> impl IntoResponse {
    match state.service.update_kiro_go_account(id, payload) {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/refresh
/// 强制刷新凭据 Token
pub async fn force_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.force_refresh_token(id).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} Token 已强制刷新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/compression
/// 获取压缩配置
pub async fn get_compression_config(State(state): State<AdminState>) -> impl IntoResponse {
    let config = state.compression_config.read().clone();
    Json(config)
}

/// PUT /api/admin/config/compression
/// 更新压缩配置
pub async fn set_compression_config(
    State(state): State<AdminState>,
    Json(payload): Json<CompressionConfig>,
) -> impl IntoResponse {
    *state.compression_config.write() = payload.clone();
    tracing::info!("压缩配置已通过 Admin API 更新");
    Json(payload)
}

/// POST /api/admin/credentials/import-token-json
/// 批量导入 token.json
pub async fn import_token_json(
    State(state): State<AdminState>,
    Json(payload): Json<ImportTokenJsonRequest>,
) -> impl IntoResponse {
    let response = state.service.import_token_json(payload).await;
    Json(response)
}

/// POST /api/admin/credentials/export-token-json
/// 按 ID 列表导出 token.json 兼容格式（可被 import-token-json 直接吃回）
pub async fn export_token_json(
    State(state): State<AdminState>,
    Json(payload): Json<ExportTokenJsonRequest>,
) -> impl IntoResponse {
    if payload.ids.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "ids 不能为空"})),
        )
            .into_response();
    }
    let items = state.service.export_credentials_to_token_json(&payload.ids);
    Json(items).into_response()
}

/// POST /api/admin/credentials/export-kam
/// 按 ID 列表导出 KAM (`kiro-account-manager`) 兼容格式（Account[] JSON）
pub async fn export_kam(
    State(state): State<AdminState>,
    Json(payload): Json<ExportKamRequest>,
) -> impl IntoResponse {
    if payload.ids.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "ids 不能为空"})),
        )
            .into_response();
    }
    let items = state.service.export_credentials_to_kam(&payload.ids);
    Json(items).into_response()
}

/// POST /api/admin/export
/// Kiro-Go 兼容导出路径：空 body 或空 ids 导出全部可导出的 OAuth 凭据。
pub async fn export_accounts_kiro_go(
    State(state): State<AdminState>,
    body: Bytes,
) -> impl IntoResponse {
    let req = serde_json::from_slice::<KiroGoExportRequest>(&body).unwrap_or_default();
    let mut ids: Vec<u64> = req
        .ids
        .iter()
        .filter_map(|id| {
            id.as_u64()
                .or_else(|| id.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
        .collect();
    if ids.is_empty() {
        ids = state
            .service
            .token_manager_snapshot()
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect();
    }

    let accounts = state.service.export_credentials_to_kam(&ids);
    let now_ms = Utc::now().timestamp_millis();
    let accounts: Vec<serde_json::Value> = accounts
        .into_iter()
        .map(|account| {
            let provider = account
                .provider
                .clone()
                .unwrap_or_else(|| "Google".to_string());
            let auth_method = account
                .auth_method
                .clone()
                .unwrap_or_else(|| "social".to_string());
            serde_json::json!({
                "id": account.id,
                "email": account.email,
                "nickname": account.label,
                "idp": provider,
                "userId": account.user_id,
                "machineId": account.machine_id,
                "credentials": {
                    "accessToken": account.access_token,
                    "csrfToken": "",
                    "refreshToken": account.refresh_token,
                    "clientId": account.client_id,
                    "clientSecret": account.client_secret,
                    "region": account.region,
                    "expiresAt": account.expires_at,
                    "authMethod": auth_method,
                    "provider": provider,
                },
                "subscription": {
                    "type": "Free",
                    "title": null,
                },
                "usage": {
                    "current": 0.0,
                    "limit": 0.0,
                    "percentUsed": 0.0,
                    "lastUpdated": now_ms,
                },
                "tags": [],
                "status": if account.enabled { "active" } else { "disabled" },
                "createdAt": now_ms,
                "lastUsedAt": now_ms,
            })
        })
        .collect();

    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "exportedAt": now_ms,
        "accounts": accounts,
        "groups": [],
        "tags": [],
    }))
    .into_response()
}

/// POST /api/admin/credentials/:id/region
/// 设置凭据 Region
pub async fn set_credential_region(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetRegionRequest>,
) -> impl IntoResponse {
    match state
        .service
        .set_region(id, payload.region, payload.api_region)
    {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} Region 已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/endpoint
/// 设置凭据 endpoint
pub async fn set_credential_endpoint(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetEndpointRequest>,
) -> impl IntoResponse {
    match state.service.set_endpoint(id, payload.endpoint) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} endpoint 已更新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/proxy
/// 获取全局代理配置（脱敏）
pub async fn get_proxy_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_kiro_go_proxy_config())
}

/// POST /api/admin/proxy
/// 更新全局代理配置（热更新）
pub async fn update_proxy_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdateProxyConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_kiro_go_proxy_config(req).await {
        Ok(_) => Json(SuccessResponse::new("全局代理配置已更新")).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

pub async fn get_settings(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_settings())
}

pub async fn update_settings(
    State(state): State<AdminState>,
    Json(req): Json<UpdateSettingsRequest>,
) -> impl IntoResponse {
    match state.service.update_settings(req).await {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

pub async fn get_thinking_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_thinking_config())
}

pub async fn update_thinking_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdateThinkingConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_thinking_config(req).await {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

pub async fn get_endpoint_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_endpoint_config())
}

pub async fn update_endpoint_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdateEndpointConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_endpoint_config(req).await {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

pub async fn get_prompt_filter_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_prompt_filter_config())
}

pub async fn update_prompt_filter_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdatePromptFilterConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_prompt_filter_config(req).await {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/global
/// 获取全局配置
pub async fn get_global_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_global_config())
}

/// PUT /api/admin/config/global
/// 更新全局配置（热更新）
pub async fn update_global_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdateGlobalConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_global_config(req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/runtime-stats
/// 轻量运行时状态：返回每个凭据的并发占用 K/N + last_used_at（5s 轮询）
pub async fn get_runtime_stats(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_runtime_stats()).into_response()
}

/// POST /api/admin/credentials/refresh-batch
/// 批量刷新 Token：服务端 Semaphore(8) 并发，前端一次往返
pub async fn force_refresh_tokens_batch(
    State(state): State<AdminState>,
    Json(req): Json<BatchRefreshRequest>,
) -> impl IntoResponse {
    Json(state.service.force_refresh_tokens_batch(req.ids).await).into_response()
}

/// POST /api/admin/credentials/refresh-balances-batch
/// 批量刷新余额：服务端 Semaphore(8) 并发，前端一次往返
pub async fn force_refresh_balances_batch(
    State(state): State<AdminState>,
    Json(req): Json<BatchRefreshRequest>,
) -> impl IntoResponse {
    Json(state.service.force_refresh_balances_batch(req.ids).await).into_response()
}

/// POST /api/admin/credentials/:id/overage
/// 切换上游 overage 开关（调用 Kiro setUserPreference）
pub async fn set_credential_overage(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetOverageRequest>,
) -> impl IntoResponse {
    match state.service.set_overage_status(id, payload.enabled).await {
        Ok(_) => {
            let action = if payload.enabled {
                "已开启"
            } else {
                "已关闭"
            };
            Json(SuccessResponse::new(format!(
                "凭据 #{} 超额开关{}",
                id, action
            )))
            .into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/accounts/:id/overage
/// Kiro-Go 兼容路径：拉取并返回单个账号的上游 overage 状态。
pub async fn get_credential_overage(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.get_balance(id, true).await {
        Ok(balance) => Json(serde_json::json!({
            "success": true,
            "overageStatus": balance.overage_status,
            "overageCapability": balance.overage_capability,
            "subscriptionTitle": balance.subscription_title,
            "overageCap": balance.overage_cap,
            "overageRate": 0.0,
            "currentOverages": (balance.current_usage - balance.usage_limit).max(0.0),
            "overageCheckedAt": Utc::now().timestamp(),
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/system-prompt
pub async fn get_system_prompt(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_system_prompt())
}

/// PUT /api/admin/config/system-prompt
pub async fn update_system_prompt(
    State(state): State<AdminState>,
    Json(payload): Json<UpdateSystemPromptRequest>,
) -> impl IntoResponse {
    match state.service.update_system_prompt(payload) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/config/user-presets
pub async fn upsert_user_preset(
    State(state): State<AdminState>,
    Json(payload): Json<UpsertUserPresetRequest>,
) -> impl IntoResponse {
    match state.service.upsert_user_preset(payload) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/config/user-presets/:id
pub async fn delete_user_preset(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_user_preset(&id) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/social/start
pub async fn start_social_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartSocialLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_social_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/social/poll/:session_id
pub async fn poll_social_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match state.service.poll_social_login(&session_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/social/callback/:session_id
/// manual 模式：提交浏览器地址栏中的 OAuth 回调 URL
pub async fn complete_social_login_callback(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Json(payload): Json<CompleteSocialCallbackRequest>,
) -> impl IntoResponse {
    match state
        .service
        .complete_social_login_callback(&session_id, payload)
        .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/social/complete/:session_id
/// helper 模式：本机 helper 完成 OAuth 后回传最终 token
pub async fn complete_social_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Json(payload): Json<CompleteSocialLoginRequest>,
) -> impl IntoResponse {
    match state
        .service
        .complete_social_login(&session_id, payload)
        .await
    {
        Ok(_) => Json(SuccessResponse::new("凭据已回传并添加")).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/idc/start
pub async fn start_idc_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartIdcLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_idc_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/iam-sso/start
/// Kiro-Go 兼容 IAM SSO authorization-code 登录开始。
pub async fn start_iam_sso_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartIdcLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_iam_sso_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/iam-sso/complete
/// Kiro-Go 兼容 IAM SSO callbackUrl 完成。
pub async fn complete_iam_sso_login(
    State(state): State<AdminState>,
    Json(payload): Json<CompleteIamSsoLoginRequest>,
) -> impl IntoResponse {
    match state.service.complete_iam_sso_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/idc/poll/:session_id
pub async fn poll_idc_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match state.service.poll_idc_login(&session_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

// ============ 请求日志和统计 ============

/// GET /api/admin/logs
/// 获取请求日志（最新在前）
pub async fn get_request_logs(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_request_logs())
}

/// DELETE /api/admin/logs
/// 清空请求日志
pub async fn clear_request_logs(State(state): State<AdminState>) -> impl IntoResponse {
    state.service.clear_request_logs();
    Json(SuccessResponse::new("请求日志已清空"))
}

/// GET /api/admin/status
/// 获取系统状态
pub async fn get_system_status(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_system_status())
}

/// GET /api/admin/stats
/// 获取详细统计
pub async fn get_stats(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_stats())
}

/// POST /api/admin/stats/reset
/// 重置统计
pub async fn reset_stats(State(state): State<AdminState>) -> impl IntoResponse {
    state.service.reset_stats();
    Json(SuccessResponse::new("统计已重置"))
}

/// GET /api/admin/version
/// 获取版本信息
pub async fn get_version(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_version())
}

/// GET /api/admin/generate-machine-id
/// 生成 Machine ID
pub async fn generate_machine_id(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.generate_machine_id())
}

// ============ 凭据连通性测试 ============

/// POST /api/admin/credentials/:id/test
/// 测试凭据连通性
pub async fn test_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.test_credential(id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

// ============ 批量操作 ============

/// POST /api/admin/credentials/batch
/// 批量操作凭据（enable/disable/refresh）
pub async fn batch_operation(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::BatchOperationRequest>,
) -> impl IntoResponse {
    match state.service.batch_operation(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

// ============ SSO Token 导入 ============

/// POST /api/admin/auth/sso-token
/// 从 SSO Token 导入凭据
pub async fn import_sso_token(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::ImportSsoTokenRequest>,
) -> impl IntoResponse {
    match state.service.import_sso_token(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

// ============ Builder ID 登录 ============

/// POST /api/admin/auth/builderid/start
/// 启动 Builder ID 登录
pub async fn start_builder_id_login(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::StartBuilderIdLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_builder_id_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/builderid/complete
/// 使用浏览器回调 URL 完成 Builder ID authorization-code 登录
pub async fn complete_builder_id_login(
    State(state): State<AdminState>,
    Json(payload): Json<CompleteIamSsoLoginRequest>,
) -> impl IntoResponse {
    match state.service.complete_builder_id_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/builderid/poll/:session_id
/// 轮询 Builder ID 登录状态
pub async fn poll_builder_id_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match state.service.poll_builder_id_login(&session_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/builderid/poll
/// Kiro-Go 兼容路径：从 body.sessionId 读取会话 ID。
pub async fn poll_builder_id_login_by_body(
    State(state): State<AdminState>,
    Json(payload): Json<PollBuilderIdLoginRequest>,
) -> impl IntoResponse {
    match state
        .service
        .poll_builder_id_login(&payload.session_id)
        .await
    {
        Ok(PollBuilderIdLoginResponse::Pending { interval }) => Json(serde_json::json!({
            "success": true,
            "completed": false,
            "status": "pending",
            "interval": interval,
        }))
        .into_response(),
        Ok(PollBuilderIdLoginResponse::Success {
            credential_id,
            email,
        }) => Json(serde_json::json!({
            "success": true,
            "completed": true,
            "account": {
                "id": credential_id,
                "email": email,
            },
        }))
        .into_response(),
        Ok(PollBuilderIdLoginResponse::Expired) => Json(serde_json::json!({
            "success": false,
            "completed": false,
            "status": "expired",
            "error": "expired",
        }))
        .into_response(),
        Ok(PollBuilderIdLoginResponse::Error { message }) => Json(serde_json::json!({
            "success": false,
            "completed": false,
            "error": message,
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/kiro-sso/start
/// 启动 Kiro hosted SSO 登录（Microsoft 365 / Entra ID）
pub async fn start_kiro_sso_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartKiroSsoLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_kiro_sso_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/kiro-sso/poll
/// 轮询 Kiro hosted SSO 登录状态
pub async fn poll_kiro_sso_login(
    State(state): State<AdminState>,
    Json(payload): Json<PollKiroSsoLoginRequest>,
) -> impl IntoResponse {
    match state.service.poll_kiro_sso_login(&payload.session_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/kiro-sso/complete
/// 提交远程浏览器中的 localhost 回调 URL。
pub async fn complete_kiro_sso_login(
    State(state): State<AdminState>,
    Json(payload): Json<CompleteKiroSsoLoginRequest>,
) -> impl IntoResponse {
    match state
        .service
        .complete_kiro_sso_login_callback(payload)
        .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/kiro-sso/cancel
/// 取消 Kiro hosted SSO 登录并释放回调端口
pub async fn cancel_kiro_sso_login(
    State(state): State<AdminState>,
    Json(payload): Json<PollKiroSsoLoginRequest>,
) -> impl IntoResponse {
    state
        .service
        .cancel_kiro_sso_login(&payload.session_id)
        .await;
    Json(serde_json::json!({ "success": true }))
}

// ============ API Key 管理 ============

/// GET /api/admin/api-keys
/// 获取所有 API Keys
pub async fn get_api_keys(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_api_keys())
}

/// POST /api/admin/api-keys
/// 创建 API Key
pub async fn create_api_key(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::CreateApiKeyRequest>,
) -> impl IntoResponse {
    match state.service.create_api_key(payload) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/api-keys/:id
/// 获取单个 API Key
pub async fn get_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.get_api_key(&id) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// PUT /api/admin/api-keys/:id
/// 更新 API Key
pub async fn update_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(payload): Json<super::types::UpdateApiKeyRequest>,
) -> impl IntoResponse {
    match state.service.update_api_key(&id, payload) {
        Ok(api_key) => Json(serde_json::json!({
            "success": true,
            "apiKey": api_key,
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/api-keys/:id
/// 删除 API Key
pub async fn delete_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_api_key(&id) {
        Ok(_) => Json(SuccessResponse::new("API Key 已删除")).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/api-keys/:id/reset-usage
/// 重置 API Key 使用量
pub async fn reset_api_key_usage(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.reset_api_key_usage(&id) {
        Ok(api_key) => Json(serde_json::json!({
            "success": true,
            "apiKey": api_key,
        }))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

// ============ 模型缓存刷新 ============

/// POST /api/admin/credentials/:id/models/refresh
/// 刷新指定凭据的模型缓存
pub async fn refresh_credential_models(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.list_available_models(id, None, true).await {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 模型缓存已刷新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/models/refresh
/// 刷新所有凭据的模型缓存
pub async fn refresh_all_credential_models(State(state): State<AdminState>) -> impl IntoResponse {
    let snapshot = state.service.token_manager_snapshot();
    let mut success_count = 0;
    let mut failure_count = 0;

    for entry in &snapshot.entries {
        if entry.disabled {
            continue;
        }
        match state
            .service
            .list_available_models(entry.id, None, true)
            .await
        {
            Ok(_) => success_count += 1,
            Err(_) => failure_count += 1,
        }
    }

    Json(SuccessResponse::new(format!(
        "模型缓存刷新完成：成功 {}，失败 {}",
        success_count, failure_count
    )))
}
