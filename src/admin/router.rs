//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post},
};

use super::{
    handlers::{
        add_credential, delete_credential, delete_user_preset, export_token_json,
        force_refresh_balances_batch, force_refresh_token, force_refresh_tokens_batch,
        get_all_credentials, get_cached_balances, get_compression_config, get_credential_balance,
        get_credential_models, get_global_config, get_proxy_config, get_runtime_stats,
        get_system_prompt, import_token_json, reset_failure_count, set_compression_config,
        set_credential_concurrency, set_credential_disabled, set_credential_endpoint,
        set_credential_overage, set_credential_priority, set_credential_region,
        update_global_config, update_proxy_config, update_system_prompt, upsert_user_preset,
    },
    middleware::{AdminState, admin_auth_middleware},
};

/// 创建 Admin API 路由
///
/// # 端点
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `GET /credentials/balances/cached` - 获取所有凭据的缓存余额
/// - `POST /credentials/import-token-json` - 批量导入 token.json
/// - `POST /credentials/export-token-json` - 按 ID 列表导出 token.json（可直接吃回 import）
/// - `DELETE /credentials/:id` - 删除凭据
/// - `POST /credentials/:id/disabled` - 设置凭据禁用状态
/// - `POST /credentials/:id/priority` - 设置凭据优先级
/// - `POST /credentials/:id/reset` - 重置失败计数
/// - `POST /credentials/:id/refresh` - 强制刷新 Token
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `POST /credentials/:id/region` - 设置凭据 Region
/// - `POST /credentials/:id/endpoint` - 设置凭据 endpoint
/// - `GET /credentials/runtime-stats` - 获取所有凭据运行时状态（K/N + lastUsed 内存快照）
/// - `POST /credentials/refresh-batch` - 批量强制刷新 Token（后端并发，单次往返）
/// - `GET /config/compression` - 获取压缩配置
/// - `PUT /config/compression` - 更新压缩配置
/// - `GET /config/global` - 获取全局配置
/// - `PUT /config/global` - 更新全局配置（热更新）
/// - `GET /proxy` - 获取全局代理配置
/// - `POST /proxy` - 更新全局代理配置（热更新）
///
/// # 认证
/// 需要 Admin API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    Router::new()
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route("/credentials/balances/cached", get(get_cached_balances))
        .route(
            "/credentials/import-token-json",
            post(import_token_json),
        )
        .route(
            "/credentials/export-token-json",
            post(export_token_json),
        )
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/concurrency", post(set_credential_concurrency))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route("/credentials/{id}/models", get(get_credential_models))
        .route("/credentials/{id}/region", post(set_credential_region))
        .route("/credentials/{id}/endpoint", post(set_credential_endpoint))
        .route("/credentials/{id}/overage", post(set_credential_overage))
        .route("/credentials/runtime-stats", get(get_runtime_stats))
        .route(
            "/credentials/refresh-batch",
            post(force_refresh_tokens_batch),
        )
        .route(
            "/credentials/refresh-balances-batch",
            post(force_refresh_balances_batch),
        )
        .route(
            "/config/compression",
            get(get_compression_config).put(set_compression_config),
        )
        .route(
            "/config/global",
            get(get_global_config).put(update_global_config),
        )
        .route("/proxy", get(get_proxy_config).post(update_proxy_config))
        .route(
            "/config/system-prompt",
            get(get_system_prompt).put(update_system_prompt),
        )
        .route("/config/user-presets", post(upsert_user_preset))
        .route("/config/user-presets/{id}", delete(delete_user_preset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ))
        .with_state(state)
}
