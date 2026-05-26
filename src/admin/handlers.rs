//! Admin API HTTP 处理器

use axum::{
    Json,
    extract::{Path, Query, State},
    response::IntoResponse,
};

use super::{
    middleware::AdminState,
    types::{
        AddCredentialRequest, BatchRefreshRequest, ExportTokenJsonRequest, ImportTokenJsonRequest,
        SetConcurrencyRequest, SetDisabledRequest, SetEndpointRequest, SetOverageRequest,
        SetPriorityRequest, SetRegionRequest, SuccessResponse, UpdateGlobalConfigRequest,
        UpdateProxyConfigRequest, UpdateSystemPromptRequest, UpsertUserPresetRequest,
    },
};
use crate::model::config::CompressionConfig;

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
    Json(state.service.get_proxy_config())
}

/// POST /api/admin/proxy
/// 更新全局代理配置（热更新）
pub async fn update_proxy_config(
    State(state): State<AdminState>,
    Json(req): Json<UpdateProxyConfigRequest>,
) -> impl IntoResponse {
    match state.service.update_proxy_config(req).await {
        Ok(_) => Json(SuccessResponse::new("全局代理配置已更新")).into_response(),
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
            let action = if payload.enabled { "已开启" } else { "已关闭" };
            Json(SuccessResponse::new(format!(
                "凭据 #{} 超额开关{}",
                id, action
            )))
            .into_response()
        }
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
