//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};

use super::{
    handlers::{
        add_credential, batch_operation, cancel_kiro_sso_login, clear_request_logs,
        complete_builder_id_login, complete_iam_sso_login, complete_kiro_sso_login,
        complete_social_login, complete_social_login_callback, create_api_key, delete_api_key,
        delete_credential, delete_user_preset, export_accounts_kiro_go, export_kam,
        export_token_json, force_refresh_balances_batch, force_refresh_token,
        force_refresh_tokens_batch, generate_machine_id, get_account_full_kiro_go,
        get_all_credentials, get_api_key, get_api_keys, get_cached_balances,
        get_compression_config, get_credential_balance, get_credential_models,
        get_credential_overage, get_endpoint_config, get_global_config, get_prompt_filter_config,
        get_proxy_config, get_request_logs, get_runtime_stats, get_settings, get_stats,
        get_system_prompt, get_system_status, get_thinking_config, get_version,
        import_credentials_kiro_go, import_sso_token, import_token_json, poll_builder_id_login,
        poll_builder_id_login_by_body, poll_idc_login, poll_kiro_sso_login, poll_social_login,
        refresh_all_credential_models, refresh_credential_models, reset_api_key_usage,
        reset_failure_count, reset_stats, set_compression_config, set_credential_concurrency,
        set_credential_disabled, set_credential_endpoint, set_credential_overage,
        set_credential_priority, set_credential_region, start_builder_id_login,
        start_iam_sso_login, start_idc_login, start_kiro_sso_login, start_social_login,
        test_credential, update_account_kiro_go, update_api_key, update_endpoint_config,
        update_global_config, update_prompt_filter_config, update_proxy_config, update_settings,
        update_system_prompt, update_thinking_config, upsert_user_preset,
    },
    middleware::{AdminState, admin_auth_middleware},
};

/// 创建 Admin API 路由
///
/// # 端点
/// ## 凭据管理
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `DELETE /credentials/:id` - 删除凭据
/// - `POST /credentials/:id/disabled` - 设置凭据禁用状态
/// - `POST /credentials/:id/priority` - 设置凭据优先级
/// - `POST /credentials/:id/concurrency` - 设置凭据并发
/// - `POST /credentials/:id/reset` - 重置失败计数
/// - `POST /credentials/:id/refresh` - 强制刷新 Token
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `GET /credentials/:id/models` - 获取可用模型
/// - `POST /credentials/:id/region` - 设置凭据 Region
/// - `POST /credentials/:id/endpoint` - 设置凭据 endpoint
/// - `POST /credentials/:id/overage` - 切换 overage 开关
/// - `POST /credentials/:id/test` - 测试凭据连通性
/// - `GET /credentials/balances/cached` - 获取所有凭据的缓存余额
/// - `GET /credentials/runtime-stats` - 获取运行时状态
/// - `POST /credentials/refresh-batch` - 批量刷新 Token
/// - `POST /credentials/refresh-balances-batch` - 批量刷新余额
/// - `POST /credentials/batch` - 批量操作（enable/disable/refresh）
/// - `POST /credentials/import-token-json` - 批量导入 token.json
/// - `POST /credentials/export-token-json` - 导出 token.json
/// - `POST /credentials/export-kam` - 导出 KAM 格式
///
/// 兼容 Kiro-Go Admin API 的同义路径：
/// - `GET/POST /accounts` -> `/credentials`
/// - `POST /accounts/batch` -> `/credentials/batch`
/// - `POST /accounts/:id/refresh` -> `/credentials/:id/refresh`
/// - `POST /accounts/:id/test` -> `/credentials/:id/test`
/// - `GET /accounts/:id/models` -> `/credentials/:id/models`
/// - `GET /accounts/:id/models/cached` -> `/credentials/:id/models`
/// - `POST /accounts/:id/models/refresh` -> `/credentials/:id/models/refresh`
/// - `POST /accounts/models/refresh` -> `/credentials/models/refresh`
/// - `GET/POST /accounts/:id/overage` -> overage 状态读取/切换
/// - `GET /accounts/:id/full` -> Kiro-Go full account shape for exportable OAuth credentials
/// - `PUT /accounts/:id` -> enabled/weight/proxyURL updates
///
/// ## 配置管理
/// - `GET/PUT /config/compression` - 压缩配置
/// - `GET/PUT /config/global` - 全局配置
/// - `GET/POST /proxy` - 代理配置
/// - `GET/PUT /config/system-prompt` - 系统提示配置
/// - `POST /config/user-presets` - 创建/更新用户预设
/// - `DELETE /config/user-presets/:id` - 删除用户预设
///
/// ## 认证流程
/// - `POST /auth/social/start` - 启动 Social OAuth
/// - `POST /auth/social/poll/:session_id` - 轮询 Social OAuth
/// - `POST /auth/social/callback/:session_id` - 提交 Social OAuth 回调 URL
/// - `POST /auth/social/complete/:session_id` - 完成 Social OAuth（helper 模式）
/// - `POST /auth/idc/start` - 启动 IdC 登录
/// - `POST /auth/idc/poll/:session_id` - 轮询 IdC 登录
/// - `POST /auth/sso-token` - SSO Token 导入
/// - `POST /auth/builderid/start` - 启动 Builder ID 登录
/// - `POST /auth/builderid/poll/:session_id` - 轮询 Builder ID 登录
///
/// ## API Key 管理
/// - `GET /api-keys` - 获取所有 API Keys
/// - `POST /api-keys` - 创建 API Key
/// - `GET /api-keys/:id` - 获取单个 API Key
/// - `PUT /api-keys/:id` - 更新 API Key
/// - `DELETE /api-keys/:id` - 删除 API Key
/// - `POST /api-keys/:id/reset-usage` - 重置使用量
///
/// ## 系统信息
/// - `GET /logs` - 获取请求日志
/// - `DELETE /logs` - 清空请求日志
/// - `GET /status` - 获取系统状态
/// - `GET /stats` - 获取详细统计
/// - `POST /stats/reset` - 重置统计
/// - `GET /version` - 获取版本信息
/// - `GET /generate-machine-id` - 生成 Machine ID
///
/// # 认证
/// 需要 Admin API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    Router::new()
        // 凭据管理
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route("/accounts", get(get_all_credentials).post(add_credential))
        .route("/credentials/balances/cached", get(get_cached_balances))
        .route("/credentials/import-token-json", post(import_token_json))
        .route("/credentials/export-token-json", post(export_token_json))
        .route("/credentials/export-kam", post(export_kam))
        .route("/accounts/batch", post(batch_operation))
        .route(
            "/accounts/models/refresh",
            post(refresh_all_credential_models),
        )
        .route(
            "/accounts/{id}",
            put(update_account_kiro_go).delete(delete_credential),
        )
        .route("/accounts/{id}/refresh", post(force_refresh_token))
        .route("/accounts/{id}/test", post(test_credential))
        .route("/accounts/{id}/full", get(get_account_full_kiro_go))
        .route("/accounts/{id}/models", get(get_credential_models))
        .route("/accounts/{id}/models/cached", get(get_credential_models))
        .route(
            "/accounts/{id}/models/refresh",
            post(refresh_credential_models),
        )
        .route(
            "/accounts/{id}/overage",
            get(get_credential_overage).post(set_credential_overage),
        )
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route(
            "/credentials/{id}/concurrency",
            post(set_credential_concurrency),
        )
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route("/credentials/{id}/models", get(get_credential_models))
        .route(
            "/credentials/{id}/models/refresh",
            post(refresh_credential_models),
        )
        .route(
            "/credentials/models/refresh",
            post(refresh_all_credential_models),
        )
        .route("/credentials/{id}/region", post(set_credential_region))
        .route("/credentials/{id}/endpoint", post(set_credential_endpoint))
        .route("/credentials/{id}/overage", post(set_credential_overage))
        .route("/credentials/{id}/test", post(test_credential))
        .route("/credentials/runtime-stats", get(get_runtime_stats))
        .route(
            "/credentials/refresh-batch",
            post(force_refresh_tokens_batch),
        )
        .route(
            "/credentials/refresh-balances-batch",
            post(force_refresh_balances_batch),
        )
        .route("/credentials/batch", post(batch_operation))
        // 配置管理
        .route(
            "/config/compression",
            get(get_compression_config).put(set_compression_config),
        )
        .route(
            "/config/global",
            get(get_global_config).put(update_global_config),
        )
        .route("/export", post(export_accounts_kiro_go))
        .route("/settings", get(get_settings).post(update_settings))
        .route(
            "/thinking",
            get(get_thinking_config).post(update_thinking_config),
        )
        .route(
            "/endpoint",
            get(get_endpoint_config).post(update_endpoint_config),
        )
        .route("/proxy", get(get_proxy_config).post(update_proxy_config))
        .route(
            "/prompt-filter",
            get(get_prompt_filter_config).post(update_prompt_filter_config),
        )
        .route(
            "/config/system-prompt",
            get(get_system_prompt).put(update_system_prompt),
        )
        .route("/config/user-presets", post(upsert_user_preset))
        .route("/config/user-presets/{id}", delete(delete_user_preset))
        // 认证流程
        .route("/auth/idc/start", post(start_idc_login))
        .route("/auth/iam-sso/start", post(start_iam_sso_login))
        .route("/auth/iam-sso/complete", post(complete_iam_sso_login))
        .route("/auth/idc/poll/{session_id}", post(poll_idc_login))
        .route("/auth/social/start", post(start_social_login))
        .route("/auth/social/poll/{session_id}", post(poll_social_login))
        .route(
            "/auth/social/callback/{session_id}",
            post(complete_social_login_callback),
        )
        .route(
            "/auth/social/complete/{session_id}",
            post(complete_social_login),
        )
        .route("/auth/sso-token", post(import_sso_token))
        .route("/auth/credentials", post(import_credentials_kiro_go))
        .route("/auth/builderid/start", post(start_builder_id_login))
        .route("/auth/builderid/complete", post(complete_builder_id_login))
        .route("/auth/builderid/poll", post(poll_builder_id_login_by_body))
        .route(
            "/auth/builderid/poll/{session_id}",
            post(poll_builder_id_login),
        )
        .route("/auth/kiro-sso/start", post(start_kiro_sso_login))
        .route("/auth/kiro-sso/poll", post(poll_kiro_sso_login))
        .route("/auth/kiro-sso/complete", post(complete_kiro_sso_login))
        .route("/auth/kiro-sso/cancel", post(cancel_kiro_sso_login))
        // API Key 管理
        .route("/api-keys", get(get_api_keys).post(create_api_key))
        .route(
            "/api-keys/{id}",
            get(get_api_key).put(update_api_key).delete(delete_api_key),
        )
        .route("/api-keys/{id}/reset-usage", post(reset_api_key_usage))
        // 系统信息
        .route("/logs", get(get_request_logs).delete(clear_request_logs))
        .route("/status", get(get_system_status))
        .route("/stats", get(get_stats))
        .route("/stats/reset", post(reset_stats))
        .route("/version", get(get_version))
        .route("/generate-machine-id", get(generate_machine_id))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ))
        .with_state(state)
}
