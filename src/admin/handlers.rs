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
        CompleteSocialLoginRequest, CreateApiKeyRequest, CredentialAliasUpdateRequest,
        CredentialBatchRequest, CredentialProbeRequest, CredentialSnapshotExportRequest,
        ExportCredentialBackupRequest, ImportCredentialRecordRequest,
        ImportCredentialRecordResponse, ImportCredentialsRequest, ImportSsoTokenRequest,
        PollBuilderIdLoginByBodyResponse, PollBuilderIdLoginRequest, PollKiroSsoLoginRequest,
        ProxyAutoAssignRequest, ProxyImportRequest, ProxyUpsertRequest,
        RefreshAllCredentialModelsResponse, RefreshCredentialModelsResponse, SetConcurrencyRequest,
        SetCredentialProxyByRegionRequest, SetCredentialProxyRequest, SetDisabledRequest,
        SetEndpointRequest, SetOverageRequest, SetPriorityRequest, SetRegionRequest,
        StartBuilderIdLoginRequest, StartIdcLoginRequest, StartKiroSsoLoginRequest,
        StartSocialLoginRequest, SuccessResponse, UpdateAccessSettingsRequest, UpdateApiKeyRequest,
        UpdateCommonConfigRequest, UpdateEndpointConfigRequest, UpdateGlobalConfigRequest,
        UpdateModelMappingsRequest, UpdatePromptFilterConfigRequest, UpdateProxyConfigRequest,
        UpdateSystemPromptRequest, UpdateThinkingConfigRequest, UpsertUserPresetRequest,
    },
};
use crate::model::config::CompressionConfig;

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials();
    Json(response)
}

/// GET /api/admin/accounts
/// `/accounts` 凭据别名视图列表
pub async fn list_credential_alias_views(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.list_credential_alias_views())
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/credentials/import/record
/// 单条凭据记录导入：支持 external_idp trust-on-import 和回退刷新。
pub async fn import_credential_record(
    State(state): State<AdminState>,
    Json(payload): Json<ImportCredentialRecordRequest>,
) -> impl IntoResponse {
    match state.service.import_credential_record(payload).await {
        Ok(resp) => Json(ImportCredentialRecordResponse::new(resp)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/accounts/:id/models
/// `/accounts` 凭据别名模型刷新，支持 sourceAccountId 或本地数字 ID
pub async fn refresh_credential_alias_models(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state
        .service
        .refresh_credential_models_by_path_id(&id)
        .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/accounts/:id/models/cached
/// `/accounts` 凭据别名缓存模型列表，支持 sourceAccountId 或本地数字 ID
pub async fn get_cached_credential_alias_models(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    Json(state.service.get_cached_credential_models_by_path_id(&id))
}

/// DELETE /api/admin/credentials/:id
/// 删除凭据
pub async fn delete_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已删除", id))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// DELETE /api/admin/accounts/:id
/// `/accounts` 凭据别名删除路径，支持 sourceAccountId 或本地数字 ID
pub async fn delete_credential_alias(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_credential_by_path_id(&id) {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/accounts/:id/full
/// `/accounts` 凭据别名完整导出视图，支持 sourceAccountId 或本地数字 ID。
pub async fn get_credential_alias_full(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.get_credential_full_export_by_path_id(&id) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// PUT /api/admin/accounts/:id
/// `/accounts` 凭据别名更新路径；仅适配本地已有等价语义的字段。
pub async fn update_credential_alias(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(payload): Json<CredentialAliasUpdateRequest>,
) -> impl IntoResponse {
    match state
        .service
        .update_credential_alias_by_path_id(&id, payload)
    {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/credentials/:id/refresh
/// 强制刷新凭据令牌
pub async fn force_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.force_refresh_token(id).await {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 令牌已强制刷新", id))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/accounts/:id/refresh
/// `/accounts` 凭据别名信息刷新，支持 sourceAccountId 或本地数字 ID
pub async fn refresh_credential_alias(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.refresh_credential_by_path_id(&id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
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

/// POST /api/admin/credentials/export
/// 按 ID 列表导出 xkiro.rs 完整备份
pub async fn export_credential_backup(
    State(state): State<AdminState>,
    Json(payload): Json<ExportCredentialBackupRequest>,
) -> impl IntoResponse {
    if payload.ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "ids 不能为空"})),
        )
            .into_response();
    }
    Json(state.service.export_credential_backup(&payload.ids)).into_response()
}

/// POST /api/admin/export
/// 凭据快照导出：空 body 或解析失败时导出全部
pub async fn export_credentials_snapshot(
    State(state): State<AdminState>,
    body: Bytes,
) -> impl IntoResponse {
    let payload = if body.is_empty() {
        CredentialSnapshotExportRequest::default()
    } else {
        serde_json::from_slice::<CredentialSnapshotExportRequest>(&body).unwrap_or_default()
    };
    Json(state.service.export_credential_snapshot(&payload.ids))
}

/// POST /api/admin/credentials/import
/// 自动识别并导入扁平凭据 / 完整备份
pub async fn import_credentials(
    State(state): State<AdminState>,
    Json(payload): Json<ImportCredentialsRequest>,
) -> impl IntoResponse {
    Json(state.service.import_credentials(payload))
}

/// POST /api/admin/credentials/:id/region
/// 设置凭据区域
pub async fn set_credential_region(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetRegionRequest>,
) -> impl IntoResponse {
    match state
        .service
        .set_region(id, payload.region, payload.api_region)
    {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 区域已更新", id))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/credentials/:id/endpoint
/// 设置凭据端点
pub async fn set_credential_endpoint(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetEndpointRequest>,
) -> impl IntoResponse {
    match state.service.set_endpoint(id, payload.endpoint) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 端点已更新", id))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/proxy
/// 获取简化代理 URL 配置
pub async fn get_proxy_url_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_proxy_url_config())
}

/// POST /api/admin/proxy
/// 更新简化代理 URL 配置
pub async fn update_proxy_url_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdateProxyConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_proxy_url_config(req).await {
        Ok(_) => Json(SuccessResponse::new("全局代理配置已更新")).into_response(),
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/config/proxy
/// 获取全局代理配置（脱敏）
pub async fn get_proxy_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_proxy_config())
}

/// POST /api/admin/config/proxy
/// 更新全局代理配置（热更新）
pub async fn update_proxy_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdateProxyConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_proxy_config(req).await {
        Ok(_) => Json(SuccessResponse::new("全局代理配置已更新")).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn list_proxies(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.list_proxies())
}

pub async fn add_proxy(
    State(state): State<AdminState>,
    Json(req): Json<ProxyUpsertRequest>,
) -> impl IntoResponse {
    match state.service.add_proxy(req).await {
        Ok(id) => Json(SuccessResponse::new(format!("代理 #{} 已新增", id))).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn update_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(req): Json<ProxyUpsertRequest>,
) -> impl IntoResponse {
    match state.service.update_proxy(id, req) {
        Ok(_) => Json(SuccessResponse::new(format!("代理 #{} 已更新", id))).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn delete_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_proxy(id) {
        Ok(unbound) => Json(SuccessResponse::new(format!(
            "代理 #{} 已删除，解绑 {} 个凭据",
            id, unbound
        )))
        .into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn test_proxy(State(state): State<AdminState>, Path(id): Path<u64>) -> impl IntoResponse {
    Json(state.service.test_proxy(id).await)
}

pub async fn import_proxies(
    State(state): State<AdminState>,
    Json(req): Json<ProxyImportRequest>,
) -> impl IntoResponse {
    Json(state.service.import_proxies(req).await)
}

pub async fn auto_assign_proxies(
    State(state): State<AdminState>,
    Json(req): Json<ProxyAutoAssignRequest>,
) -> impl IntoResponse {
    Json(state.service.auto_assign_proxies(req))
}

pub async fn set_credential_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(req): Json<SetCredentialProxyRequest>,
) -> impl IntoResponse {
    match state.service.set_credential_proxy(id, req.proxy_id) {
        Ok(_) => {
            let message = match req.proxy_id {
                Some(proxy_id) => format!("凭据 #{} 已绑定代理 #{}", id, proxy_id),
                None => format!("凭据 #{} 已解绑代理", id),
            };
            Json(SuccessResponse::new(message)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

pub async fn set_credential_proxy_by_region(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(req): Json<SetCredentialProxyByRegionRequest>,
) -> impl IntoResponse {
    match state
        .service
        .set_credential_proxy_by_region(id, req.region.as_deref())
    {
        Ok(proxy_id) => {
            let message = match proxy_id {
                Some(proxy_id) => format!("凭据 #{} 已绑定代理 #{}", id, proxy_id),
                None => format!("凭据 #{} 已解绑代理", id),
            };
            Json(serde_json::json!({ "message": message, "proxyId": proxy_id })).into_response()
        }
        Err(e) => e.into_response(),
    }
}

pub async fn get_access_settings(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_access_settings())
}

pub async fn update_access_settings(
    State(state): State<AdminState>,
    Json(req): Json<UpdateAccessSettingsRequest>,
) -> impl IntoResponse {
    match state.service.update_access_settings(req).await {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn get_common_config(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.get_common_config() {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn update_common_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdateCommonConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_common_config(req) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
    }
}

pub async fn get_model_mappings(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_model_mappings())
}

pub async fn update_model_mappings(
    State(state): State<AdminState>,
    Json(req): Json<UpdateModelMappingsRequest>,
) -> impl IntoResponse {
    match state.service.update_model_mappings(req).await {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/credentials/runtime-stats
/// 轻量运行时状态：返回每个凭据的并发占用 K/N + last_used_at（5s 轮询）
pub async fn get_runtime_stats(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_runtime_stats()).into_response()
}

/// POST /api/admin/credentials/refresh-batch
/// 批量刷新令牌：服务端 Semaphore(8) 并发，前端一次往返
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
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/accounts/:id/overage
/// `/accounts` 凭据别名路径：支持 sourceAccountId 或本地数字 ID，返回 overage 快照。
pub async fn set_credential_alias_overage(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(payload): Json<SetOverageRequest>,
) -> impl IntoResponse {
    match state
        .service
        .set_credential_overage_by_path_id(&id, payload.enabled)
        .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/accounts/:id/overage
/// `/accounts` 凭据别名路径：支持 sourceAccountId 或本地数字 ID，返回 overage 快照。
pub async fn get_credential_alias_overage(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.get_credential_overage_by_path_id(&id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/credentials/:id/overage
/// xkiro 原生路径：数字 ID，拉取并返回上游 overage 状态。
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/config/user-presets
pub async fn upsert_user_preset(
    State(state): State<AdminState>,
    Json(payload): Json<UpsertUserPresetRequest>,
) -> impl IntoResponse {
    match state.service.upsert_user_preset(payload) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// DELETE /api/admin/config/user-presets/:id
pub async fn delete_user_preset(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_user_preset(&id) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/auth/social/start
pub async fn start_social_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartSocialLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_social_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/auth/social/poll/:session_id
pub async fn poll_social_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match state.service.poll_social_login(&session_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/auth/social/complete/:session_id
/// helper 模式：本机 helper 完成 OAuth 后回传最终令牌
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
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/auth/idc/start
pub async fn start_idc_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartIdcLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_idc_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/auth/iam-sso/start
/// IAM SSO authorization-code 登录开始。
pub async fn start_iam_sso_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartIdcLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_iam_sso_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/auth/iam-sso/complete
/// IAM SSO callbackUrl 完成。
pub async fn complete_iam_sso_login(
    State(state): State<AdminState>,
    Json(payload): Json<CompleteIamSsoLoginRequest>,
) -> impl IntoResponse {
    match state.service.complete_iam_sso_login(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/auth/idc/poll/:session_id
pub async fn poll_idc_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match state.service.poll_idc_login(&session_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
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

/// GET /api/admin/system/machine-id
/// 生成机器 ID
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
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/accounts/:id/test
/// `/accounts` 凭据别名测试：支持 sourceAccountId 或本地数字 ID。
pub async fn test_credential_alias(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let payload = if body.is_empty() {
        CredentialProbeRequest::default()
    } else {
        serde_json::from_slice::<CredentialProbeRequest>(&body).unwrap_or_default()
    };

    match state
        .service
        .test_credential_by_path_id(&id, payload.model)
        .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/accounts/batch
/// `/accounts` 凭据别名批量操作（enable/disable/refresh）
pub async fn batch_credentials(
    State(state): State<AdminState>,
    Json(payload): Json<CredentialBatchRequest>,
) -> impl IntoResponse {
    match state.service.batch_credentials(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

// ============ SSO 令牌导入 ============

/// POST /api/admin/auth/sso-token
/// 从 SSO 令牌导入凭据
pub async fn import_sso_token(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::ImportSsoTokenRequest>,
) -> impl IntoResponse {
    match state.service.import_sso_token(payload).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/auth/builderid/poll
/// 保留 body.sessionId 轮询入口。
pub async fn poll_builder_id_login_by_body(
    State(state): State<AdminState>,
    Json(payload): Json<PollBuilderIdLoginRequest>,
) -> impl IntoResponse {
    match state
        .service
        .poll_builder_id_login(&payload.session_id)
        .await
    {
        Ok(resp) => Json(PollBuilderIdLoginByBodyResponse::new(resp)).into_response(),
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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
        Err(e) => e.into_response(),
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

// ============ API 密钥管理 ============

/// GET /api/admin/api-keys
/// 获取所有 API 密钥
pub async fn get_api_keys(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_api_keys())
}

/// POST /api/admin/api-keys
/// 创建 API 密钥
pub async fn create_api_key(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::CreateApiKeyRequest>,
) -> impl IntoResponse {
    match state.service.create_api_key(payload) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// GET /api/admin/api-keys/:id
/// 获取单个 API 密钥
pub async fn get_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.get_api_key(&id) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

/// PUT /api/admin/api-keys/:id
/// 更新 API 密钥
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
        Err(e) => e.into_response(),
    }
}

/// DELETE /api/admin/api-keys/:id
/// 删除 API 密钥
pub async fn delete_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_api_key(&id) {
        Ok(_) => Json(SuccessResponse::new("API 密钥已删除")).into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/api-keys/:id/reset-usage
/// 重置 API 密钥使用量
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
        Err(e) => e.into_response(),
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
        Ok(response) => Json(RefreshCredentialModelsResponse::new(
            format!("凭据 #{} 模型缓存已刷新", id),
            response.available_models.len(),
        ))
        .into_response(),
        Err(e) => e.into_response(),
    }
}

/// POST /api/admin/credentials/models/refresh
/// 刷新所有凭据的模型缓存
pub async fn refresh_all_credential_models(State(state): State<AdminState>) -> impl IntoResponse {
    let snapshot = state.service.token_manager_snapshot();
    let mut refreshed = 0;
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
            Ok(response) => refreshed += response.available_models.len(),
            Err(_) => failure_count += 1,
        }
    }

    Json(RefreshAllCredentialModelsResponse::new(
        format!(
            "模型缓存刷新完成：刷新 {} 个模型，失败 {} 个凭据",
            refreshed, failure_count
        ),
        refreshed,
        failure_count,
    ))
}
