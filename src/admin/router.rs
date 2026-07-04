//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};

use super::{
    handlers::{
        add_credential, add_proxy, auto_assign_proxies, batch_credentials, batch_operation,
        cancel_kiro_sso_login, clear_request_logs, complete_builder_id_login,
        complete_iam_sso_login, complete_kiro_sso_login, complete_social_login,
        complete_social_login_callback, create_api_key, delete_api_key, delete_credential,
        delete_credential_alias, delete_proxy, delete_user_preset, export_credential_backup,
        export_credentials_snapshot, force_refresh_balances_batch, force_refresh_token,
        force_refresh_tokens_batch, generate_machine_id, get_access_settings, get_all_credentials,
        get_api_key, get_api_keys, get_cached_balances, get_cached_credential_alias_models,
        get_common_config, get_compression_config, get_credential_alias_full,
        get_credential_alias_overage, get_credential_balance, get_credential_models,
        get_credential_overage, get_endpoint_config, get_global_config, get_model_mappings,
        get_prompt_filter_config, get_proxy_config, get_proxy_url_config, get_request_logs,
        get_runtime_stats, get_stats, get_system_prompt, get_system_status, get_thinking_config,
        get_version, import_credential_record, import_credentials, import_proxies,
        import_sso_token, list_credential_alias_views, list_proxies, poll_builder_id_login,
        poll_builder_id_login_by_body, poll_idc_login, poll_kiro_sso_login, poll_social_login,
        refresh_all_credential_models, refresh_credential_alias, refresh_credential_alias_models,
        refresh_credential_models, reset_api_key_usage, reset_failure_count, reset_stats,
        set_compression_config, set_credential_alias_overage, set_credential_concurrency,
        set_credential_disabled, set_credential_endpoint, set_credential_overage,
        set_credential_priority, set_credential_proxy, set_credential_proxy_by_region,
        set_credential_region, start_builder_id_login, start_iam_sso_login, start_idc_login,
        start_kiro_sso_login, start_social_login, test_credential, test_credential_alias,
        test_proxy, update_access_settings, update_api_key, update_common_config,
        update_credential_alias, update_endpoint_config, update_global_config,
        update_model_mappings, update_prompt_filter_config, update_proxy, update_proxy_config,
        update_proxy_url_config, update_system_prompt, update_thinking_config, upsert_user_preset,
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
/// - `POST /credentials/:id/refresh` - 强制刷新令牌
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `GET /credentials/:id/models` - 获取可用模型
/// - `POST /credentials/:id/region` - 设置凭据区域
/// - `POST /credentials/:id/endpoint` - 设置凭据端点
/// - `GET/POST /credentials/:id/overage` - 读取/切换 overage 开关
/// - `POST /credentials/:id/test` - 测试凭据连通性
/// - `GET /credentials/balances/cached` - 获取所有凭据的缓存余额
/// - `GET /credentials/runtime-stats` - 获取运行时状态
/// - `POST /credentials/refresh-batch` - 批量刷新令牌
/// - `POST /credentials/refresh-balances-batch` - 批量刷新余额
/// - `POST /credentials/batch` - 批量操作（enable/disable/refresh）
/// - `POST /credentials/import` - 自动识别并导入完整备份 / 缓存凭据 / 扁平凭据
/// - `POST /credentials/import/record` - 导入并校验单条凭据记录
/// - `POST /credentials/export` - 导出 xkiro.rs 完整备份
///
/// `/accounts` 凭据别名入口：
/// - `GET/POST /accounts` -> `/credentials`
/// - `POST /accounts/batch` -> `/credentials/batch`
/// - `POST /accounts/:id/refresh` -> `/credentials/:id/refresh`
/// - `POST /accounts/:id/test` -> `/credentials/:id/test`
/// - `GET /accounts/:id/models` -> `/credentials/:id/models`
/// - `GET /accounts/:id/models/cached` -> `/credentials/:id/models`
/// - `POST /accounts/:id/models/refresh` -> `/credentials/:id/models/refresh`
/// - `POST /accounts/models/refresh` -> `/credentials/models/refresh`
/// - `GET/POST /accounts/:id/overage` -> overage 状态读取/切换
/// - `GET /accounts/:id/full` -> 可导出 OAuth 凭据的完整导出视图
/// - `PUT /accounts/:id` -> enabled/weight/proxyUrl updates
///
/// ## 配置管理
/// - `GET/PUT /config/compression` - 压缩配置
/// - `GET/PUT /config/global` - 全局配置
/// - `GET/POST /config/settings` - 访问控制配置
/// - `GET/POST /config/thinking` - Thinking 配置
/// - `GET/POST /config/endpoint` - 端点配置
/// - `GET/POST /config/proxy` - 代理配置
/// - `GET/POST /config/prompt-filter` - Prompt Filter 配置
/// - `GET/PUT /config/system-prompt` - 系统提示配置
/// - `POST /config/user-presets` - 创建/更新用户预设
/// - `DELETE /config/user-presets/:id` - 删除用户预设
///
/// ## 认证流程
/// - `POST /auth/social/start` - 启动社交 OAuth
/// - `POST /auth/social/poll/:session_id` - 轮询社交 OAuth
/// - `POST /auth/social/callback/:session_id` - 提交社交 OAuth 回调 URL
/// - `POST /auth/social/complete/:session_id` - 完成社交 OAuth（helper 模式）
/// - `POST /auth/idc/start` - 启动 IAM Identity Center 登录
/// - `POST /auth/idc/poll/:session_id` - 轮询 IAM Identity Center 登录
/// - `POST /auth/sso-token` - SSO 令牌导入
/// - `POST /auth/builderid/start` - 启动 Builder ID 登录
/// - `POST /auth/builderid/poll/:session_id` - 轮询 Builder ID 登录
///
/// ## API 密钥管理
/// - `GET /api-keys` - 获取所有 API 密钥
/// - `POST /api-keys` - 创建 API 密钥
/// - `GET /api-keys/:id` - 获取单个 API 密钥
/// - `PUT /api-keys/:id` - 更新 API 密钥
/// - `DELETE /api-keys/:id` - 删除 API 密钥
/// - `POST /api-keys/:id/reset-usage` - 重置使用量
///
/// ## 系统信息
/// - `GET/DELETE /system/logs`
/// - `GET /system/status`
/// - `GET /system/stats`
/// - `POST /system/stats/reset`
/// - `GET /system/version`
/// - `GET /system/machine-id`
///
/// 保留短路径：
/// - `GET /logs`
/// - `DELETE /logs`
/// - `GET /status`
/// - `GET /stats`
/// - `POST /stats/reset`
/// - `GET /version`
/// - `GET /generate-machine-id`
///
/// # 认证
/// 需要 Admin API 密钥认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    Router::new()
        // 凭据管理
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route(
            "/accounts",
            get(list_credential_alias_views).post(add_credential),
        )
        .route("/credentials/balances/cached", get(get_cached_balances))
        .route("/credentials/import", post(import_credentials))
        .route("/credentials/import/record", post(import_credential_record))
        .route("/credentials/export", post(export_credential_backup))
        .route("/export", post(export_credentials_snapshot))
        .route("/accounts/batch", post(batch_credentials))
        .route(
            "/accounts/models/refresh",
            post(refresh_all_credential_models),
        )
        .route(
            "/accounts/{id}",
            put(update_credential_alias).delete(delete_credential_alias),
        )
        .route("/accounts/{id}/refresh", post(refresh_credential_alias))
        .route("/accounts/{id}/test", post(test_credential_alias))
        .route("/accounts/{id}/full", get(get_credential_alias_full))
        .route(
            "/accounts/{id}/models",
            get(refresh_credential_alias_models),
        )
        .route(
            "/accounts/{id}/models/cached",
            get(get_cached_credential_alias_models),
        )
        .route(
            "/accounts/{id}/models/refresh",
            post(refresh_credential_models),
        )
        .route(
            "/accounts/{id}/overage",
            get(get_credential_alias_overage).post(set_credential_alias_overage),
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
        .route(
            "/credentials/{id}/overage",
            get(get_credential_overage).post(set_credential_overage),
        )
        .route("/credentials/{id}/proxy", post(set_credential_proxy))
        .route(
            "/credentials/{id}/proxy-by-region",
            post(set_credential_proxy_by_region),
        )
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
        .route(
            "/config/settings",
            get(get_access_settings).post(update_access_settings),
        )
        .route(
            "/config/common",
            get(get_common_config).post(update_common_config),
        )
        .route(
            "/settings",
            get(get_access_settings).post(update_access_settings),
        )
        .route("/common", get(get_common_config).post(update_common_config))
        .route(
            "/config/thinking",
            get(get_thinking_config).post(update_thinking_config),
        )
        .route(
            "/thinking",
            get(get_thinking_config).post(update_thinking_config),
        )
        .route(
            "/config/endpoint",
            get(get_endpoint_config).post(update_endpoint_config),
        )
        .route(
            "/endpoint",
            get(get_endpoint_config).post(update_endpoint_config),
        )
        .route(
            "/config/proxy",
            get(get_proxy_config).post(update_proxy_config),
        )
        .route(
            "/proxy",
            get(get_proxy_url_config).post(update_proxy_url_config),
        )
        .route("/proxies", get(list_proxies).post(add_proxy))
        .route("/proxies/import", post(import_proxies))
        .route("/proxies/auto-assign", post(auto_assign_proxies))
        .route("/proxies/{id}", put(update_proxy).delete(delete_proxy))
        .route("/proxies/{id}/test", post(test_proxy))
        .route(
            "/config/prompt-filter",
            get(get_prompt_filter_config).post(update_prompt_filter_config),
        )
        .route(
            "/prompt-filter",
            get(get_prompt_filter_config).post(update_prompt_filter_config),
        )
        .route(
            "/config/model-mappings",
            get(get_model_mappings).post(update_model_mappings),
        )
        .route(
            "/model-mappings",
            get(get_model_mappings).post(update_model_mappings),
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
        // API 密钥管理
        .route("/api-keys", get(get_api_keys).post(create_api_key))
        .route(
            "/api-keys/{id}",
            get(get_api_key).put(update_api_key).delete(delete_api_key),
        )
        .route("/api-keys/{id}/reset-usage", post(reset_api_key_usage))
        // 系统信息
        .route(
            "/system/logs",
            get(get_request_logs).delete(clear_request_logs),
        )
        .route("/system/status", get(get_system_status))
        .route("/system/stats", get(get_stats))
        .route("/system/stats/reset", post(reset_stats))
        .route("/system/version", get(get_version))
        .route("/system/machine-id", get(generate_machine_id))
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
