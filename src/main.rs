//! xkiro.rs - Kiro API 代理

// 抑制有意保留的未使用代码警告（admin 模块、协议功能等）
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]

mod admin;
mod admin_ui;
mod anthropic;
mod common;
mod http_client;
mod kiro;
mod model;
mod openai;
pub mod token;

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use clap::Parser;
use kiro::background_refresh::BackgroundRefreshConfig;
#[allow(unused_imports)]
use kiro::endpoint::CODEWHISPERER_ENDPOINT_NAME;
use kiro::endpoint::{CliEndpoint, CodewhispererEndpoint, IdeEndpoint, KiroEndpoint};
use kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use kiro::provider::KiroProvider;
use kiro::token_manager::MultiTokenManager;
use model::arg::{Args, Command};
use model::config::Config;
use parking_lot::RwLock;

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 子命令优先：init 走交互式向导直接退出，不进入服务启动流程
    if let Some(Command::Init { force }) = args.command {
        let config_path = args
            .config
            .unwrap_or_else(|| Config::default_config_path().to_string());
        if let Err(e) = model::init::run_init(std::path::Path::new(&config_path), force) {
            eprintln!("初始化失败: {:#}", e);
            std::process::exit(1);
        }
        return;
    }

    // social-helper 子命令：本机完成 OAuth 并回传远程 xkiro，独立于服务启动流程
    if let Some(Command::SocialHelper {
        server,
        session,
        provider,
        api_key,
        auth_endpoint,
        proxy,
    }) = args.command
    {
        let helper_args = kiro::auth::helper::HelperArgs {
            server,
            session,
            provider,
            api_key,
            auth_endpoint,
            proxy,
        };
        if let Err(e) = kiro::auth::helper::run(helper_args).await {
            eprintln!("Social helper 失败: {:#}", e);
            std::process::exit(1);
        }
        return;
    }

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 加载配置：文件不存在则进交互式向导（运行 init 流程后再加载）
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    if !std::path::Path::new(&config_path).exists() {
        eprintln!("未检测到配置文件 {}，进入初始化向导。", config_path);
        if let Err(e) = model::init::run_init(std::path::Path::new(&config_path), false) {
            eprintln!("初始化失败: {:#}", e);
            std::process::exit(1);
        }
    }
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 加载凭证（支持单对象或数组格式）
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());
    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        tracing::error!("加载凭证失败: {}", e);
        std::process::exit(1);
    });

    // 判断是否为多凭据格式（用于刷新后回写）
    let is_multiple_format = credentials_config.is_multiple();

    // 转换为按优先级排序的凭据列表
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 获取第一个凭据用于日志显示
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!("主凭证: {:?}", first_credentials);

    // 获取 API Key
    let api_key = config.api_key.clone().unwrap_or_else(|| {
        if config.require_api_key {
            tracing::error!("配置文件中未设置 apiKey");
            std::process::exit(1);
        }
        String::new()
    });
    let client_api_key_runtime = Arc::new(RwLock::new(api_key.clone()));
    let require_api_key_runtime =
        Arc::new(std::sync::atomic::AtomicBool::new(config.require_api_key));
    let admin_api_key_runtime = Arc::new(RwLock::new(
        config.admin_api_key.clone().unwrap_or_default(),
    ));
    let thinking_config = Arc::new(RwLock::new(anthropic::middleware::ThinkingRuntimeConfig {
        suffix: config.thinking_suffix.clone(),
        openai_format: config.openai_thinking_format.clone(),
        claude_format: config.claude_thinking_format.clone(),
    }));

    // 构建代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 构建端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
        let cli = CliEndpoint::new();
        endpoints.insert(cli.name().to_string(), Arc::new(cli));
        let cw = CodewhispererEndpoint::new();
        endpoints.insert(cw.name().to_string(), Arc::new(cw));
    }

    // 校验默认端点存在
    if !endpoints.contains_key(&config.default_endpoint) {
        tracing::error!("默认端点 \"{}\" 未注册", config.default_endpoint);
        std::process::exit(1);
    }

    // 校验所有凭据声明的端点都已注册
    for cred in &credentials_list {
        let name = cred.endpoint.as_deref().unwrap_or(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            tracing::error!(
                "凭据 id={:?} 指定了未知端点 \"{}\"（已注册: {:?}）",
                cred.id,
                name,
                endpoints.keys().collect::<Vec<_>>()
            );
            std::process::exit(1);
        }
    }

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.into()),
        is_multiple_format,
    )
    .unwrap_or_else(|e| {
        tracing::error!("创建 Token 管理器失败: {}", e);
        std::process::exit(1);
    });
    let token_manager = Arc::new(token_manager);

    // 启动后台 Token 刷新任务（默认配置：每 60s 检查一次，提前 15 分钟刷新）
    let _background_refresher =
        token_manager.start_background_refresh(BackgroundRefreshConfig::default());
    tracing::info!("后台 Token 刷新任务已启动");

    // 余额初始化由 AdminService::prefetch_balances_on_startup 统一负责：
    // 一次上游 getUsageLimits → 同时回填磁盘缓存（dashboard）+ 运行时缓存（路由决策）+ 低余额禁用

    let kiro_provider = KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
    );
    let kiro_provider = Arc::new(kiro_provider);

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
    });

    // tiktoken cl100k_base 精确计数开关（admin API 不暴露热改，需重启）
    token::set_precise_counting(config.precise_token_counting);

    let api_keys_cache_dir = token_manager.cache_dir();
    let api_keys_store_path = api_keys_cache_dir
        .as_ref()
        .map(|d| d.join("kiro_api_keys.json"));
    let api_keys_runtime = admin::AdminService::load_api_keys_runtime_with_legacy(
        api_keys_cache_dir.as_deref(),
        config.api_key.as_deref(),
        config.require_api_key,
    );

    // 共享压缩配置（admin API 可运行时修改）
    let compression_config = Arc::new(RwLock::new(config.compression.clone()));

    // 共享系统提示清洗配置（admin API 可运行时修改）
    let prompt_filter_config = Arc::new(RwLock::new(config.prompt_filter.clone()));

    // 共享系统提示注入运行时配置（admin API 可运行时修改）
    let prompt_runtime = crate::model::runtime::shared_from_config(&config);

    // Prompt Cache 运行时（共享引用，支持热更新）
    // Prompt Cache 运行时（共享引用，支持热更新）
    let prompt_cache_runtime =
        Arc::new(RwLock::new(anthropic::middleware::PromptCacheRuntime::new(
            config.prompt_cache_ttl_seconds,
            config.prompt_cache_accounting_enabled,
        )));

    // 构建 Anthropic API 路由（profile_arn 由首个凭据提供）
    let anthropic_app = anthropic::create_router_with_provider(
        &api_key,
        config.require_api_key,
        client_api_key_runtime.clone(),
        require_api_key_runtime.clone(),
        Some(kiro_provider.clone()),
        first_credentials.profile_arn.clone(),
        config.extract_thinking,
        compression_config.clone(),
        prompt_filter_config.clone(),
        prompt_runtime.clone(),
        prompt_cache_runtime.clone(),
        thinking_config.clone(),
        api_keys_runtime.clone(),
        api_keys_store_path,
        Some(crate::openai::responses_store::responses_store_dir(
            std::path::Path::new(&config_path),
        )),
    );

    // 构建 Admin API 路由（如果配置了非空的 admin_api_key）
    // 安全检查：空字符串被视为未配置，防止空 key 绕过认证
    let admin_key_valid = config
        .admin_api_key
        .as_ref()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);

    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app
        } else {
            let admin_service = admin::AdminService::new(
                token_manager.clone(),
                Some(kiro_provider.clone()),
                compression_config.clone(),
                client_api_key_runtime.clone(),
                require_api_key_runtime.clone(),
                admin_api_key_runtime.clone(),
                prompt_filter_config.clone(),
                thinking_config.clone(),
                prompt_cache_runtime.clone(),
                prompt_runtime.clone(),
                api_keys_runtime.clone(),
                endpoint_names.clone(),
            );
            let admin_state = admin::AdminState::new(
                admin_api_key_runtime,
                admin_service,
                compression_config.clone(),
            );

            // 注册 credit usage 观察者：metering 事件透传时同步更新 admin disk cache
            {
                let observer: std::sync::Arc<dyn crate::kiro::token_manager::CreditUsageObserver> =
                    admin_state.service.clone();
                token_manager.set_credit_observer(std::sync::Arc::downgrade(&observer));
            }

            // 启动时后台并行预取所有未禁用凭据余额，写入 disk-cache
            // 让前端首次访问 dashboard 时就能从 /balances/cached 直接拿到完整快照
            {
                let svc = admin_state.service.clone();
                tokio::spawn(async move { svc.prefetch_balances_on_startup().await });
            }
            // 启动周期性余额刷新：周期/并发/启停由 config.balance_refresh_* 控制（热更新）
            // 同步两层缓存（admin disk + token_manager 运行时），低余额自动禁用
            admin_state.service.clone().start_periodic_balance_refresh();

            let admin_app = admin::create_admin_router(admin_state);

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app.clone())
                .nest("/admin/api", admin_app)
                .nest("/admin", admin_ui_app)
        }
    } else {
        anthropic_app
    };

    // 记录启动时间（用于 health endpoint uptime 计算）
    let start_time = std::time::Instant::now();

    // 添加公共端点（无需 API Key 认证）：health、telemetry sink
    let app = app
        .route(
            "/health",
            get({
                let start_time = start_time;
                move || async move {
                    let uptime = start_time.elapsed().as_secs();
                    Json(serde_json::json!({
                        "status": "ok",
                        "version": env!("CARGO_PKG_VERSION"),
                        "uptime": uptime
                    }))
                }
            }),
        )
        .route(
            "/",
            get({
                let start_time = start_time;
                move || async move {
                    let uptime = start_time.elapsed().as_secs();
                    Json(serde_json::json!({
                        "status": "ok",
                        "version": env!("CARGO_PKG_VERSION"),
                        "uptime": uptime
                    }))
                }
            }),
        )
        // Claude Code 遥测端点：黑洞，返回 200 OK
        .route(
            "/api/event_logging/batch",
            post(|| async { (StatusCode::OK, Json(serde_json::json!({"status":"ok"}))) }),
        );

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    if config.require_api_key {
        tracing::info!("API Key: {}***", &api_key[..(api_key.len() / 2)]);
    } else {
        tracing::info!("API Key 认证已关闭");
    }
    tracing::info!("可用 API:");
    tracing::info!("  GET  /health");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    tracing::info!("  POST /v1/chat/completions");
    tracing::info!("  POST /v1/responses");
    tracing::info!("  POST /api/event_logging/batch");
    if admin_key_valid {
        tracing::info!("Admin API:");
        tracing::info!("  GET  /api/admin/credentials");
        tracing::info!("  POST /api/admin/credentials/:index/disabled");
        tracing::info!("  POST /api/admin/credentials/:index/priority");
        tracing::info!("  POST /api/admin/credentials/:index/reset");
        tracing::info!("  GET  /api/admin/credentials/:index/balance");
        tracing::info!("Admin UI:");
        tracing::info!("  GET  /admin");
    }

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
