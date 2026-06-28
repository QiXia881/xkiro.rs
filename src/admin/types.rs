//! Admin API 类型定义

use serde::{Deserialize, Serialize};

// ============ 凭据状态 ============

/// 所有凭据状态响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsStatusResponse {
    /// 凭据总数
    pub total: usize,
    /// 可用凭据数量（未禁用）
    pub available: usize,
    /// 各凭据状态列表
    pub credentials: Vec<CredentialStatusItem>,
}

/// 单个凭据的状态信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatusItem {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级（数字越小优先级越高）
    pub priority: u32,
    /// Kiro-Go 兼容权重（0/1=普通，2+=更高份额）
    pub weight: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// Token 过期时间（RFC3339 格式）
    pub expires_at: Option<String>,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 端点名称（决定该凭据走哪套 Kiro API，已回退到默认端点）
    pub endpoint: String,
    /// 该凭据当前可用的并发许可数（available permits）
    pub available_permits: usize,
    /// 凭据级最大并发上限（已下发到 Semaphore 的总许可数）
    pub max_permits: usize,

    /// 凭据级并发上限配置（None=回退全局 per_credential_concurrency）
    pub concurrency: Option<u32>,
}

// ============ 操作请求 ============

/// 启用/禁用凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledRequest {
    /// 是否禁用
    pub disabled: bool,
}

/// 修改优先级请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPriorityRequest {
    /// 新优先级值
    pub priority: u32,
}

/// 修改单凭据并发上限请求
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConcurrencyRequest {
    /// 凭据级最大并发（>=1，None=回退全局 per_credential_concurrency）
    pub concurrency: Option<u32>,
}

/// 切换上游 overage 开关请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetOverageRequest {
    /// 是否开启远端超额（true=ENABLED / false=DISABLED）
    pub enabled: bool,
}

/// 修改 region 请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRegionRequest {
    /// 凭据级 Region（用于 Token 刷新），空字符串表示清除
    pub region: Option<String>,
    /// 凭据级 API Region（单独覆盖 API 请求），空字符串表示清除
    pub api_region: Option<String>,
}

/// 修改 endpoint 请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetEndpointRequest {
    /// endpoint 名称，空字符串或 null 表示回退到 defaultEndpoint
    pub endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroGoUpdateAccountRequest {
    pub enabled: Option<bool>,
    pub weight: Option<u32>,
    #[serde(alias = "proxyURL")]
    pub proxy_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroGoImportCredentialsRequest {
    #[serde(alias = "accessToken")]
    pub access_token: Option<String>,
    #[serde(alias = "refreshToken")]
    pub refresh_token: String,
    #[serde(alias = "clientId")]
    pub client_id: Option<String>,
    #[serde(alias = "clientSecret")]
    pub client_secret: Option<String>,
    pub auth_method: Option<String>,
    pub provider: Option<String>,
    #[serde(alias = "userId")]
    pub user_id: Option<String>,
    pub region: Option<String>,
    #[serde(alias = "authRegion")]
    pub auth_region: Option<String>,
    #[serde(alias = "apiRegion")]
    pub api_region: Option<String>,
    #[serde(alias = "tokenEndpoint")]
    pub token_endpoint: Option<String>,
    #[serde(alias = "issuerUrl")]
    pub issuer_url: Option<String>,
    pub scopes: Option<String>,
    #[serde(alias = "startUrl")]
    pub start_url: Option<String>,
    #[serde(alias = "clientIdHash")]
    pub client_id_hash: Option<String>,
    #[serde(alias = "idToken")]
    pub id_token: Option<String>,
    #[serde(alias = "ssoSessionId")]
    pub sso_session_id: Option<String>,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub weight: u32,
    #[serde(default)]
    pub concurrency: Option<u32>,
    pub id: Option<serde_json::Value>,
    pub email: Option<String>,
    #[serde(alias = "profileArn")]
    pub profile_arn: Option<String>,
    #[serde(alias = "machineId")]
    pub machine_id: Option<String>,
    #[serde(alias = "proxyURL")]
    pub proxy_url: Option<String>,
    pub proxy_username: Option<String>,
    pub proxy_password: Option<String>,
    #[serde(alias = "overageStatus")]
    pub overage_status: Option<String>,
    pub endpoint: Option<String>,
    pub enabled: Option<bool>,
    pub disabled: Option<bool>,
}

/// 添加凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialRequest {
    /// 刷新令牌（OAuth 凭据必填，API Key 凭据不需要）
    #[serde(alias = "refreshToken")]
    pub refresh_token: Option<String>,

    /// 认证方式（可选，默认 social）
    #[serde(default = "default_auth_method")]
    pub auth_method: String,

    /// OIDC Client ID（IdC 认证需要）
    #[serde(alias = "clientId")]
    pub client_id: Option<String>,

    /// OIDC Client Secret（IdC 认证需要）
    #[serde(alias = "clientSecret")]
    pub client_secret: Option<String>,

    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,

    /// Kiro-Go 兼容权重（0/1=普通，2+=更高份额）
    #[serde(default)]
    pub weight: u32,

    /// 凭据级最大并发（>=1，None=回退全局 per_credential_concurrency）
    #[serde(default)]
    pub concurrency: Option<u32>,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    pub api_region: Option<String>,

    /// 凭据级 Machine ID（可选，64 位字符串）
    /// 未配置时回退到 config.json 的 machineId
    #[serde(alias = "machineId")]
    pub machine_id: Option<String>,

    /// 用户邮箱（可选，用于前端显示）
    pub email: Option<String>,

    /// 凭据级代理 URL（可选，特殊值 "direct" 表示不使用代理）
    #[serde(alias = "proxyURL")]
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    pub proxy_password: Option<String>,

    /// Kiro API Key（API Key 凭据必填，格式: ksk_xxxxxxxx）
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 端点名称（可选，未配置时使用 config.defaultEndpoint）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

fn default_auth_method() -> String {
    "social".to_string()
}

/// 添加凭据成功响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialResponse {
    pub success: bool,
    pub message: String,
    /// 新添加的凭据 ID
    pub credential_id: u64,
    /// 用户邮箱（如果获取成功）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

// ============ 余额查询 ============

/// 余额查询响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    /// 凭据 ID
    pub id: u64,
    /// 订阅类型
    pub subscription_title: Option<String>,
    /// 当前使用量
    pub current_usage: f64,
    /// 使用限额
    pub usage_limit: f64,
    /// 剩余额度
    pub remaining: f64,
    /// 使用百分比
    pub usage_percentage: f64,
    /// 下次重置时间（Unix 时间戳）
    pub next_reset_at: Option<f64>,
    /// 超额上限（订阅可超额时 > 0）
    #[serde(default)]
    pub overage_cap: f64,
    /// 超额资格 (OVERAGE_CAPABLE / OVERAGE_INCAPABLE / null)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_capability: Option<String>,
    /// 远端超额开关 (ENABLED / DISABLED / null)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<String>,
}

/// 缓存余额信息
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedBalanceItem {
    /// 凭据 ID
    pub id: u64,
    /// 当前使用量
    pub current_usage: f64,
    /// 使用限额（base + trial + bonus 聚合）
    pub usage_limit: f64,
    /// 剩余额度
    pub remaining: f64,
    /// 使用百分比
    pub usage_percentage: f64,
    /// 订阅类型
    pub subscription_title: Option<String>,
    /// 下次重置时间（Unix 时间戳）
    pub next_reset_at: Option<f64>,
    /// 超额上限
    #[serde(default)]
    pub overage_cap: f64,
    /// 超额资格
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_capability: Option<String>,
    /// 远端超额开关
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<String>,
    /// 缓存时间（Unix 毫秒时间戳）
    pub cached_at: u64,
    /// 缓存存活时间（秒），缓存过期时间 = cached_at + ttl_secs * 1000
    pub ttl_secs: u64,
}

/// 所有凭据的缓存余额响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedBalancesResponse {
    /// 各凭据的缓存余额列表
    pub balances: Vec<CachedBalanceItem>,
}

// ============ 全局代理配置 ============

/// 全局代理配置响应
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfigResponse {
    pub proxy_url: Option<String>,
    pub has_credentials: bool,
}

/// 更新全局代理配置请求
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProxyConfigRequest {
    #[serde(alias = "proxyURL")]
    pub proxy_url: Option<String>,
    pub proxy_username: Option<String>,
    pub proxy_password: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsResponse {
    pub api_key: Option<String>,
    pub require_api_key: bool,
    pub port: u16,
    pub host: String,
    pub allow_over_usage: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettingsRequest {
    pub api_key: Option<String>,
    pub require_api_key: Option<bool>,
    pub password: Option<String>,
    pub allow_over_usage: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfigResponse {
    pub suffix: String,
    pub openai_format: String,
    pub claude_format: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateThinkingConfigRequest {
    pub suffix: String,
    pub openai_format: String,
    pub claude_format: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointConfigResponse {
    pub preferred_endpoint: String,
    pub endpoint_fallback: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEndpointConfigRequest {
    pub preferred_endpoint: String,
    pub endpoint_fallback: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFilterRuleDto {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub rule_type: String,
    #[serde(rename = "match")]
    pub match_pattern: String,
    #[serde(default)]
    pub replace: String,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFilterConfigResponse {
    pub filter_claude_code: bool,
    pub filter_env_noise: bool,
    pub filter_strip_boundaries: bool,
    pub rules: Vec<PromptFilterRuleDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePromptFilterConfigRequest {
    pub filter_claude_code: bool,
    pub filter_env_noise: bool,
    pub filter_strip_boundaries: bool,
    pub rules: Vec<PromptFilterRuleDto>,
}

#[derive(Debug, Serialize)]
pub struct KiroGoProxyConfigResponse {
    #[serde(rename = "proxyURL")]
    pub proxy_url: String,
}

// ============ 全局配置 ============

/// 全局配置响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalConfigResponse {
    /// AWS Region
    pub region: String,
    /// Prompt Cache TTL（秒）
    pub prompt_cache_ttl_seconds: u64,
    /// 是否启用本地 Prompt Cache usage 记账
    pub prompt_cache_accounting_enabled: bool,
    /// 默认端点名称（凭据未显式指定 endpoint 时使用）
    pub default_endpoint: String,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 单凭据最大并发数（per-credential semaphore，>=1）
    pub per_credential_concurrency: usize,
    /// 全局并发上限（global semaphore，0=不限）
    pub global_concurrency: usize,
    /// 凭据队列等待超时（秒），超时后返回 429 overloaded_error
    pub acquire_wait_timeout_secs: u64,
    /// 是否启用周期余额刷新
    pub balance_refresh_enabled: bool,
    /// 周期余额刷新间隔（秒，最小 180）
    pub balance_refresh_interval_secs: u64,
    /// 周期余额刷新并发上限（1..=10）
    pub balance_refresh_concurrency: usize,
    /// 是否启用 session 亲和（同会话黏住同凭据；关闭则每条消息独立平摊）
    pub session_affinity_enabled: bool,
    /// admin UI 隐私模式（邮箱脱敏展示）
    pub privacy_mode: bool,
    /// 压缩配置
    pub compression: CompressionConfigResponse,
}

/// 压缩配置响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompressionConfigResponse {
    pub max_request_body_bytes: usize,
}

/// 更新全局配置请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateGlobalConfigRequest {
    /// AWS Region（可选）
    pub region: Option<String>,
    /// Prompt Cache TTL（秒，可选，仅支持 300 或 3600）
    pub prompt_cache_ttl_seconds: Option<u64>,
    /// 是否启用本地 Prompt Cache usage 记账（可选）
    pub prompt_cache_accounting_enabled: Option<bool>,
    /// 默认端点名称（可选）
    pub default_endpoint: Option<String>,
    /// 是否开启非流式响应的 thinking 块提取（可选）
    pub extract_thinking: Option<bool>,
    /// 单凭据最大并发数（>=1，可选）
    pub per_credential_concurrency: Option<usize>,
    /// 全局并发上限（0=不限，可选）
    pub global_concurrency: Option<usize>,
    /// 凭据队列等待超时（秒，可选）
    pub acquire_wait_timeout_secs: Option<u64>,
    /// 是否启用周期余额刷新（可选）
    pub balance_refresh_enabled: Option<bool>,
    /// 周期余额刷新间隔（秒，可选；< 180 会被 clamp 到 180）
    pub balance_refresh_interval_secs: Option<u64>,
    /// 周期余额刷新并发上限（可选；clamp 到 1..=10）
    pub balance_refresh_concurrency: Option<usize>,
    /// 是否启用 session 亲和（可选）
    pub session_affinity_enabled: Option<bool>,
    /// admin UI 隐私模式（可选）
    pub privacy_mode: Option<bool>,
    /// 压缩配置（可选）
    pub compression: Option<UpdateCompressionConfigRequest>,
}

/// 更新压缩配置请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCompressionConfigRequest {
    pub max_request_body_bytes: Option<usize>,
}

// ============ 批量导入 token.json ============

/// 官方 token.json 格式（用于解析导入）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenJsonItem {
    pub provider: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub auth_method: Option<String>,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub weight: u32,
    pub region: Option<String>,
    pub api_region: Option<String>,
    pub machine_id: Option<String>,
}

/// 批量导入请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTokenJsonRequest {
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
    pub items: ImportItems,
}

fn default_dry_run() -> bool {
    true
}

/// 导入项（支持单个或数组）
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ImportItems {
    Single(TokenJsonItem),
    Multiple(Vec<TokenJsonItem>),
}

impl ImportItems {
    pub fn into_vec(self) -> Vec<TokenJsonItem> {
        match self {
            ImportItems::Single(item) => vec![item],
            ImportItems::Multiple(items) => items,
        }
    }
}

/// 批量导入响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTokenJsonResponse {
    pub summary: ImportSummary,
    pub items: Vec<ImportItemResult>,
}

/// 导入汇总
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub parsed: usize,
    pub added: usize,
    pub skipped: usize,
    pub invalid: usize,
}

/// 单项导入结果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportItemResult {
    pub index: usize,
    pub fingerprint: String,
    pub action: ImportAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<u64>,
}

/// 导入动作
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ImportAction {
    Added,
    Skipped,
    Invalid,
}

// ============ 导出 token.json ============

/// 导出请求（POST body）
///
/// `ids` 为空时报 400；调用方需指定要导出哪些凭据。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportTokenJsonRequest {
    pub ids: Vec<u64>,
}

/// 导出项（与 [`TokenJsonItem`] 字段对齐，可被批量导入直接吃回）
///
/// 仅写入有意义的字段：
/// - API Key 凭据无 refreshToken，不导出（导入端不支持，会被识别为 Invalid）
/// - `clientId` / `clientSecret`：仅 idc 凭据携带
/// - `region` / `apiRegion` / `machineId`：未配置则 None 序列化时省略
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportTokenJsonItem {
    pub provider: String,
    pub refresh_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub auth_method: String,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub priority: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub weight: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

// ============ 导出 KAM 兼容格式 ============

/// KAM 导出请求（POST body）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportKamRequest {
    pub ids: Vec<u64>,
}

/// KAM 兼容账号项（对齐 `kiro-account-manager/src-tauri/src/core/account.rs::Account`）
///
/// 字段命名 camelCase；`None` 字段省略以保持文件简洁。
/// `authMethod` 使用大写 `IdC` / 小写 `social`（KAM 约定）。
/// `provider` 取值：`Google` / `Github` / `BuilderId` / `Enterprise` / `Social`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportKamItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub label: String,
    pub status: String,
    pub added_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    pub enabled: bool,
}

// ============ 通用响应 ============

/// 操作成功响应
#[derive(Debug, Serialize)]
pub struct SuccessResponse {
    pub success: bool,
    pub message: String,
}

impl SuccessResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }
}

/// 错误响应
#[derive(Debug, Serialize)]
pub struct AdminErrorResponse {
    pub error: AdminError,
}

#[derive(Debug, Serialize)]
pub struct AdminError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl AdminErrorResponse {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: AdminError {
                error_type: error_type.into(),
                message: message.into(),
            },
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }

    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid or missing admin API key")
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn api_error(message: impl Into<String>) -> Self {
        Self::new("api_error", message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
}

// ============ 运行时状态轻量端点（高频轮询）============

/// 单个凭据的运行时状态（仅内存快照字段，不含静态元数据）
///
/// 用于 `GET /credentials/runtime-stats` 高频轮询（5s），
/// 与 `CredentialStatusItem` 互补：后者全量字段、低频；前者最小集合、高频。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatsItem {
    pub id: u64,
    pub last_used_at: Option<String>,
    pub available_permits: usize,
    pub max_permits: usize,
    pub disabled: bool,
    /// 余额快照（来自 5min disk cache + 后台周期刷新）；缓存未命中时为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<RuntimeBalanceSnapshot>,
}

/// runtime-stats 内嵌的余额快照
///
/// 字段与 [`CachedBalanceItem`] 中"非 TTL 元数据"部分一一对应；前端可直接投影到
/// `BalanceResponse` 复用现有渲染逻辑。1.5s 轮询批量带回，避免单条强制刷新 RTT。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeBalanceSnapshot {
    pub subscription_title: Option<String>,
    pub current_usage: f64,
    pub usage_limit: f64,
    pub remaining: f64,
    pub usage_percentage: f64,
    pub next_reset_at: Option<f64>,
    pub overage_cap: f64,
    pub overage_capability: Option<String>,
    pub overage_status: Option<String>,
}

/// 运行时状态响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatsResponse {
    pub credentials: Vec<RuntimeStatsItem>,
}

// ============ 批量刷新 Token 端点 ============

/// 批量刷新请求
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRefreshRequest {
    pub ids: Vec<u64>,
}

/// 单个凭据的刷新结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRefreshResultItem {
    pub id: u64,
    pub success: bool,
    /// 失败原因（success=true 时为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 批量刷新响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRefreshResponse {
    pub results: Vec<BatchRefreshResultItem>,
    pub success_count: usize,
    pub failure_count: usize,
}

// ============ 批量刷新余额端点 ============

/// 单个凭据的余额刷新结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRefreshBalanceResultItem {
    pub id: u64,
    pub success: bool,
    /// 成功时的余额信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<BalanceResponse>,
    /// 失败原因（success=true 时为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 批量刷新余额响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRefreshBalanceResponse {
    pub results: Vec<BatchRefreshBalanceResultItem>,
    pub success_count: usize,
    pub failure_count: usize,
}

// ============ 系统提示注入 ============

/// 单条 preset（含来源标记）用于前端展示
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetItem {
    pub id: String,
    pub name: String,
    pub description: String,
    /// "builtin" | "user"
    pub source: String,
    pub enabled: bool,
    /// 仅 user 来源时返回完整 content；builtin 不返回（前端不需要展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// 系统提示注入配置响应
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemPromptResponse {
    pub enabled: bool,
    pub position: String,
    pub custom_content: Option<String>,
    pub presets: Vec<PresetItem>,
}

/// 更新系统提示注入配置请求（所有字段可选；提供哪个就更新哪个）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSystemPromptRequest {
    pub enabled: Option<bool>,
    /// "prepend" | "append"
    pub position: Option<String>,
    /// 设为 Some(""):清空；None:不变；Some(non-empty):覆盖
    pub custom_content: Option<String>,
    /// 全量替换 enabled_presets id 列表
    pub enabled_presets: Option<Vec<String>>,
}

/// 创建/更新用户预设请求
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertUserPresetRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub content: String,
}

// ============ Social OAuth 登录 ============

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartSocialLoginRequest {
    #[serde(default)]
    pub priority: u32,
    pub email: Option<String>,
    pub proxy_url: Option<String>,
    pub auth_endpoint: Option<String>,
    pub provider: String,
    /// "manual"（默认，手动粘贴回调）或 "helper"（本机 helper 回传）
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartSocialLoginResponse {
    pub session_id: String,
    /// 实际生效的登录模式："manual" | "helper"
    pub mode: String,
    /// manual 模式返回：需要复制到浏览器的登录页 URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal_url: Option<String>,
    pub expires_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteSocialCallbackRequest {
    pub callback_url: String,
}

/// helper 模式回传：本机 helper 完成 OAuth 后把最终 token 投递到服务端
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteSocialLoginRequest {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub profile_arn: Option<String>,
    pub expires_at: Option<String>,
    pub expires_in: Option<i64>,
    pub machine_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum PollSocialLoginResponse {
    #[serde(rename = "waiting")]
    Waiting,
    #[serde(rename = "success")]
    Success { credential_id: u64 },
    #[serde(rename = "expired")]
    Expired,
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIdcLoginRequest {
    pub region: String,
    pub start_url: Option<String>,
    #[serde(default)]
    pub priority: u32,
    pub email: Option<String>,
    pub proxy_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIdcLoginResponse {
    pub session_id: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    pub expires_at: String,
    pub poll_interval: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteIamSsoLoginRequest {
    pub session_id: String,
    pub callback_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIamSsoLoginResponse {
    pub session_id: String,
    pub authorize_url: String,
    pub expires_in: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum PollIdcLoginResponse {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "success")]
    Success { credential_id: u64 },
    #[serde(rename = "expired")]
    Expired,
}

// ============ 请求日志和统计 ============

/// 请求日志响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogsResponse {
    pub logs: Vec<super::stats::RequestLog>,
    pub total: usize,
}

/// 系统状态响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatusResponse {
    pub status: String,
    pub version: String,
    pub uptime: u64,
    pub total_requests: i64,
    pub success_requests: i64,
    pub failed_requests: i64,
    pub total_tokens: i64,
    pub total_credits: f64,
    pub credentials_total: usize,
    pub credentials_available: usize,
}

/// 详细统计响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsResponse {
    pub total_requests: i64,
    pub success_requests: i64,
    pub failed_requests: i64,
    pub total_tokens: i64,
    pub total_credits: f64,
    pub uptime: u64,
    pub credentials_total: usize,
    pub credentials_available: usize,
}

/// 版本信息响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionResponse {
    pub version: String,
    pub name: String,
}

/// 生成 Machine ID 响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateMachineIdResponse {
    pub machine_id: String,
}

// ============ 批量操作 ============

/// 批量操作请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchOperationRequest {
    /// 要操作的凭据 ID 列表
    pub ids: Vec<u64>,
    /// 操作类型："enable" / "disable" / "refresh"
    pub action: String,
}

/// 批量操作结果项
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchOperationResultItem {
    pub id: u64,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 批量操作响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchOperationResponse {
    pub results: Vec<BatchOperationResultItem>,
    pub success_count: usize,
    pub failure_count: usize,
}

// ============ 凭据连通性测试 ============

/// 凭据连通性测试响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialTestResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ============ SSO Token 导入 ============

/// SSO Token 导入请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSsoTokenRequest {
    /// SSO Bearer Token（支持批量：多个 token 用换行分隔）
    #[serde(alias = "bearerToken")]
    pub token: String,
    /// AWS Region（可选，默认 us-east-1）
    #[serde(default = "default_sso_region")]
    pub region: String,
    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,
    /// 用户邮箱（可选）
    pub email: Option<String>,
    /// 代理 URL（可选）
    pub proxy_url: Option<String>,
}

fn default_sso_region() -> String {
    "us-east-1".to_string()
}

/// SSO Token 导入结果项
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoTokenImportResultItem {
    pub index: usize,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// SSO Token 导入响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSsoTokenResponse {
    pub results: Vec<SsoTokenImportResultItem>,
    pub success_count: usize,
    pub failure_count: usize,
}

// ============ Builder ID 登录 ============

/// Builder ID 登录开始请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartBuilderIdLoginRequest {
    /// AWS Region（可选，默认 us-east-1）
    #[serde(default = "default_sso_region")]
    pub region: String,
    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,
    /// 用户邮箱（可选）
    pub email: Option<String>,
    /// 代理 URL（可选）
    pub proxy_url: Option<String>,
}

/// Builder ID 登录开始响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartBuilderIdLoginResponse {
    pub session_id: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    pub poll_interval: i64,
    pub expires_in: i64,
}

/// Builder ID 轮询响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum PollBuilderIdLoginResponse {
    #[serde(rename = "pending")]
    Pending { interval: i64 },
    #[serde(rename = "success")]
    Success {
        credential_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
    #[serde(rename = "expired")]
    Expired,
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollBuilderIdLoginRequest {
    pub session_id: String,
}

// ============ Kiro hosted SSO（Microsoft 365 / Entra ID）============

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartKiroSsoLoginRequest {
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollKiroSsoLoginRequest {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteKiroSsoLoginRequest {
    pub session_id: String,
    pub callback_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartKiroSsoLoginResponse {
    pub session_id: String,
    pub sign_in_url: String,
    pub interval: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroSsoAccountResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollKiroSsoLoginResponse {
    pub success: bool,
    pub completed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<KiroSsoAccountResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteKiroSsoLoginResponse {
    pub success: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ============ API Key 管理 ============

/// API Key 条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyEntry {
    /// 唯一 ID
    pub id: String,
    /// 名称（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Key 值（列表/获取时脱敏）
    pub key: String,
    /// 是否启用
    pub enabled: bool,
    /// 创建时间（Unix 秒时间戳）
    pub created_at: i64,
    /// 最后使用时间（Unix 秒时间戳）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    /// Token 使用限制（0=无限制）
    #[serde(default)]
    pub token_limit: i64,
    /// Credit 使用限制（0=无限制）
    #[serde(default)]
    pub credit_limit: f64,
    /// 已使用 Token 数
    #[serde(default)]
    pub tokens_used: i64,
    /// 已使用 Credits
    #[serde(default)]
    pub credits_used: f64,
    /// 请求次数
    #[serde(default)]
    pub requests_count: i64,
}

/// API Key 输出视图（不暴露明文 key）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyView {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub key_masked: String,
    pub enabled: bool,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    #[serde(default)]
    pub token_limit: i64,
    #[serde(default)]
    pub credit_limit: f64,
    #[serde(default)]
    pub tokens_used: i64,
    #[serde(default)]
    pub credits_used: f64,
    #[serde(default)]
    pub requests_count: i64,
}

/// 创建 API Key 请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateApiKeyRequest {
    /// 名称（可选）
    pub name: Option<String>,
    /// 自定义 Key（可选；不传则自动生成）
    pub key: Option<String>,
    /// 是否启用（默认 true）
    pub enabled: Option<bool>,
    /// Token 使用限制（0=无限制，默认 0）
    #[serde(default)]
    pub token_limit: i64,
    /// Credit 使用限制（0=无限制，默认 0）
    #[serde(default)]
    pub credit_limit: f64,
}

/// 更新 API Key 请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateApiKeyRequest {
    /// 名称
    pub name: Option<Option<String>>,
    /// Key 值
    pub key: Option<String>,
    /// 是否启用
    pub enabled: Option<bool>,
    /// Token 使用限制
    pub token_limit: Option<i64>,
    /// Credit 使用限制
    pub credit_limit: Option<f64>,
}

/// API Key 列表响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyListResponse {
    pub api_keys: Vec<ApiKeyView>,
}

/// API Key 创建响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateApiKeyResponse {
    pub success: bool,
    pub id: String,
    pub key: String,
    pub api_key: ApiKeyView,
}

/// API Key 更新/重置响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyMutationResponse {
    pub success: bool,
    pub api_key: ApiKeyView,
}
