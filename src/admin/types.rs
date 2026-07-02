//! Admin API 类型定义

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::models::AvailableModel;
use crate::model::config::CredentialMachineIdStrategy;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

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

/// `/accounts` 凭据别名视图返回的凭据项
#[derive(Debug)]
pub struct CredentialAliasViewItem {
    pub id: String,
    pub email: String,
    pub user_id: String,
    pub nickname: String,
    pub auth_method: String,
    pub provider: String,
    pub region: String,
    pub enabled: bool,
    pub ban_status: String,
    pub ban_reason: String,
    pub ban_time: i64,
    pub expires_at: i64,
    pub has_token: bool,
    pub machine_id: String,
    pub weight: u32,
    pub overage_status: String,
    pub overage_capability: String,
    pub overage_cap: f64,
    pub overage_rate: f64,
    pub current_overages: f64,
    pub overage_checked_at: i64,
    pub proxy_url: String,
    pub subscription_type: String,
    pub subscription_title: String,
    pub days_remaining: i64,
    pub usage_current: f64,
    pub usage_limit: f64,
    pub usage_percent: f64,
    pub next_reset_date: String,
    pub last_refresh: i64,
    pub trial_usage_current: f64,
    pub trial_usage_limit: f64,
    pub trial_usage_percent: f64,
    pub trial_status: String,
    pub trial_expires_at: i64,
    pub request_count: u64,
    pub error_count: u64,
    pub total_tokens: u64,
    pub total_credits: f64,
    pub last_used: i64,
}

impl Serialize for CredentialAliasViewItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("CredentialAliasViewItem", 41)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("email", &self.email)?;
        state.serialize_field("userId", &self.user_id)?;
        state.serialize_field("nickname", &self.nickname)?;
        state.serialize_field("authMethod", &self.auth_method)?;
        state.serialize_field("provider", &self.provider)?;
        state.serialize_field("region", &self.region)?;
        state.serialize_field("enabled", &self.enabled)?;
        state.serialize_field("banStatus", &self.ban_status)?;
        state.serialize_field("banReason", &self.ban_reason)?;
        state.serialize_field("banTime", &self.ban_time)?;
        state.serialize_field("expiresAt", &self.expires_at)?;
        state.serialize_field("hasToken", &self.has_token)?;
        state.serialize_field("machineId", &self.machine_id)?;
        state.serialize_field("weight", &self.weight)?;
        state.serialize_field("overageStatus", &self.overage_status)?;
        state.serialize_field("overageCapability", &self.overage_capability)?;
        state.serialize_field("overageCap", &self.overage_cap)?;
        state.serialize_field("overageRate", &self.overage_rate)?;
        state.serialize_field("currentOverages", &self.current_overages)?;
        state.serialize_field("overageCheckedAt", &self.overage_checked_at)?;
        state.serialize_field("proxyUrl", &self.proxy_url)?;
        state.serialize_field("proxyURL", &self.proxy_url)?;
        state.serialize_field("subscriptionType", &self.subscription_type)?;
        state.serialize_field("subscriptionTitle", &self.subscription_title)?;
        state.serialize_field("daysRemaining", &self.days_remaining)?;
        state.serialize_field("usageCurrent", &self.usage_current)?;
        state.serialize_field("usageLimit", &self.usage_limit)?;
        state.serialize_field("usagePercent", &self.usage_percent)?;
        state.serialize_field("nextResetDate", &self.next_reset_date)?;
        state.serialize_field("lastRefresh", &self.last_refresh)?;
        state.serialize_field("trialUsageCurrent", &self.trial_usage_current)?;
        state.serialize_field("trialUsageLimit", &self.trial_usage_limit)?;
        state.serialize_field("trialUsagePercent", &self.trial_usage_percent)?;
        state.serialize_field("trialStatus", &self.trial_status)?;
        state.serialize_field("trialExpiresAt", &self.trial_expires_at)?;
        state.serialize_field("requestCount", &self.request_count)?;
        state.serialize_field("errorCount", &self.error_count)?;
        state.serialize_field("totalTokens", &self.total_tokens)?;
        state.serialize_field("totalCredits", &self.total_credits)?;
        state.serialize_field("lastUsed", &self.last_used)?;
        state.end()
    }
}

#[derive(Debug, Clone)]
pub struct CredentialFullExportResponse {
    pub id: String,
    pub email: Option<String>,
    pub user_id: Option<String>,
    pub nickname: String,
    pub access_token: Option<String>,
    pub refresh_token: String,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub auth_method: String,
    pub provider: Option<String>,
    pub region: Option<String>,
    pub start_url: Option<String>,
    pub expires_at: Option<i64>,
    pub machine_id: Option<String>,
    pub weight: u32,
    pub profile_arn: Option<String>,
    pub token_endpoint: Option<String>,
    pub issuer_url: Option<String>,
    pub scopes: Option<String>,
    pub client_id_hash: Option<String>,
    pub id_token: Option<String>,
    pub sso_session_id: Option<String>,
    pub proxy_url: Option<String>,
    pub proxy_id: Option<u64>,
    pub overage_status: Option<String>,
    pub overage_capability: Option<String>,
    pub overage_cap: f64,
    pub overage_rate: f64,
    pub current_overages: f64,
    pub overage_checked_at: i64,
    pub enabled: bool,
    pub ban_status: Option<String>,
    pub ban_reason: Option<String>,
    pub ban_time: i64,
    pub subscription_type: Option<String>,
    pub subscription_title: Option<String>,
    pub days_remaining: i64,
    pub usage_current: f64,
    pub usage_limit: f64,
    pub usage_percent: f64,
    pub next_reset_date: Option<String>,
    pub last_refresh: i64,
    pub trial_usage_current: f64,
    pub trial_usage_limit: f64,
    pub trial_usage_percent: f64,
    pub trial_status: Option<String>,
    pub trial_expires_at: i64,
    pub request_count: u64,
    pub error_count: u64,
    pub total_tokens: u64,
    pub total_credits: f64,
    pub last_used: i64,
}

impl Serialize for CredentialFullExportResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("CredentialFullExportResponse", 53)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("email", &self.email)?;
        state.serialize_field("userId", &self.user_id)?;
        state.serialize_field("nickname", &self.nickname)?;
        state.serialize_field("accessToken", &self.access_token)?;
        state.serialize_field("refreshToken", &self.refresh_token)?;
        state.serialize_field("clientId", &self.client_id)?;
        state.serialize_field("clientSecret", &self.client_secret)?;
        state.serialize_field("authMethod", &self.auth_method)?;
        state.serialize_field("provider", &self.provider)?;
        state.serialize_field("region", &self.region)?;
        state.serialize_field("startUrl", &self.start_url)?;
        state.serialize_field("expiresAt", &self.expires_at)?;
        state.serialize_field("machineId", &self.machine_id)?;
        state.serialize_field("weight", &self.weight)?;
        state.serialize_field("profileArn", &self.profile_arn)?;
        state.serialize_field("tokenEndpoint", &self.token_endpoint)?;
        state.serialize_field("issuerUrl", &self.issuer_url)?;
        state.serialize_field("scopes", &self.scopes)?;
        state.serialize_field("clientIdHash", &self.client_id_hash)?;
        state.serialize_field("idToken", &self.id_token)?;
        state.serialize_field("ssoSessionId", &self.sso_session_id)?;
        state.serialize_field("proxyUrl", &self.proxy_url)?;
        state.serialize_field("proxyURL", &self.proxy_url)?;
        state.serialize_field("proxyId", &self.proxy_id)?;
        state.serialize_field("overageStatus", &self.overage_status)?;
        state.serialize_field("overageCapability", &self.overage_capability)?;
        state.serialize_field("overageCap", &self.overage_cap)?;
        state.serialize_field("overageRate", &self.overage_rate)?;
        state.serialize_field("currentOverages", &self.current_overages)?;
        state.serialize_field("overageCheckedAt", &self.overage_checked_at)?;
        state.serialize_field("enabled", &self.enabled)?;
        state.serialize_field("banStatus", &self.ban_status)?;
        state.serialize_field("banReason", &self.ban_reason)?;
        state.serialize_field("banTime", &self.ban_time)?;
        state.serialize_field("subscriptionType", &self.subscription_type)?;
        state.serialize_field("subscriptionTitle", &self.subscription_title)?;
        state.serialize_field("daysRemaining", &self.days_remaining)?;
        state.serialize_field("usageCurrent", &self.usage_current)?;
        state.serialize_field("usageLimit", &self.usage_limit)?;
        state.serialize_field("usagePercent", &self.usage_percent)?;
        state.serialize_field("nextResetDate", &self.next_reset_date)?;
        state.serialize_field("lastRefresh", &self.last_refresh)?;
        state.serialize_field("trialUsageCurrent", &self.trial_usage_current)?;
        state.serialize_field("trialUsageLimit", &self.trial_usage_limit)?;
        state.serialize_field("trialUsagePercent", &self.trial_usage_percent)?;
        state.serialize_field("trialStatus", &self.trial_status)?;
        state.serialize_field("trialExpiresAt", &self.trial_expires_at)?;
        state.serialize_field("requestCount", &self.request_count)?;
        state.serialize_field("errorCount", &self.error_count)?;
        state.serialize_field("totalTokens", &self.total_tokens)?;
        state.serialize_field("totalCredits", &self.total_credits)?;
        state.serialize_field("lastUsed", &self.last_used)?;
        state.end()
    }
}

/// 单个凭据的状态信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatusItem {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级（数字越小优先级越高）
    pub priority: u32,
    /// 调度权重（0/1=普通，2+=更高份额）
    pub weight: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 令牌过期时间（RFC3339 格式）
    pub expires_at: Option<String>,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 身份提供方（Google / GitHub / BuilderId / AzureAD 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Kiro 用户 ID（导入来源展示元数据）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// 导入来源 ID（可能是非数字字符串 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_account_id: Option<String>,
    /// 用户自定义显示名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 导入来源状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 导入来源添加时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    /// 显示昵称
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// 导入来源分组 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// 导入来源标签关联数组
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_links: Option<serde_json::Value>,
    /// 导入来源原始 usage API 响应
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_data: Option<serde_json::Value>,
    /// 是否存在导入来源可用模型缓存
    pub has_available_models_cache: bool,
    /// 导入来源失败次数（区别于运行时 failure_count）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_failure_count: Option<u32>,
    /// 导入来源最后失败时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_last_failure_at: Option<String>,
    /// 导入来源禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_disabled_reason: Option<String>,
    /// 导入来源成功次数（区别于运行时 success_count）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_success_count: Option<u64>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// 是否保存了访问令牌（不返回明文）
    pub has_token: bool,
    /// 是否保存了刷新令牌（不返回明文）
    pub has_refresh_token: bool,
    /// 是否保存了 clientId（不返回明文）
    pub has_client_id: bool,
    /// 是否保存了 clientSecret（不返回明文）
    pub has_client_secret: bool,
    /// 是否保存了 ID 令牌（不返回明文）
    pub has_id_token: bool,
    /// 是否保存了 API 密钥（列表接口仅另行返回脱敏值）
    pub has_api_key: bool,
    /// 是否保存了代理认证信息（不返回明文）
    pub has_proxy_credentials: bool,
    /// 凭据级区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// 凭据级认证区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,
    /// 凭据级 API 区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,
    /// 凭据级机器 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// AWS SSO Start URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    /// 本地缓存 clientIdHash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_hash: Option<String>,
    /// 本地缓存 SSO session ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_session_id: Option<String>,
    /// External IdP 令牌端点
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// External IdP issuer URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    /// External IdP scopes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    /// 订阅类型
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    /// 订阅标题
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_title: Option<String>,
    /// 剩余天数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_remaining: Option<i64>,
    /// 远端 overage 开关状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<String>,
    /// 远端 overage 能力
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_capability: Option<String>,
    /// 远端 overage 上限
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_cap: Option<f64>,
    /// 远端 overage 单价
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_rate: Option<f64>,
    /// 当前 overage 消耗
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_overages: Option<f64>,
    /// overage 最近同步时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_checked_at: Option<i64>,
    /// 封禁状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_status: Option<String>,
    /// 封禁原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_reason: Option<String>,
    /// 封禁时间戳
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_time: Option<i64>,
    /// 用量快照
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_current: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<i64>,
    /// 试用用量快照
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_current: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_expires_at: Option<i64>,
    /// 持久化统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_credits: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<serde_json::Value>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// apiKey 的 SHA-256 哈希（仅 API 密钥凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// apiKey 的脱敏展示（仅 API 密钥凭据，用于前端显示）
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
    /// 绑定的代理池 ID（None = 未绑定代理池）
    pub proxy_id: Option<u64>,
    /// 令牌刷新连续失败次数
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

/// 切换上游超额开关请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetOverageRequest {
    /// 是否开启远端超额（true=ENABLED / false=DISABLED）
    pub enabled: bool,
}

/// 修改区域请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRegionRequest {
    /// 凭据级区域（用于令牌刷新），空字符串表示清除
    pub region: Option<String>,
    /// 凭据级 API 区域（单独覆盖 API 请求），空字符串表示清除
    pub api_region: Option<String>,
}

/// 修改端点请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetEndpointRequest {
    /// 端点名称，空字符串或 null 表示回退到 defaultEndpoint
    pub endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialAliasUpdateRequest {
    pub enabled: Option<bool>,
    pub nickname: Option<String>,
    #[serde(alias = "machine_id")]
    pub machine_id: Option<String>,
    pub weight: Option<u32>,
    #[serde(alias = "proxyURL")]
    pub proxy_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCredentialRecordRequest {
    #[serde(alias = "accessToken")]
    pub access_token: Option<String>,
    #[serde(alias = "refreshToken")]
    pub refresh_token: Option<String>,
    #[serde(alias = "api_key", alias = "kiroApiKey", alias = "kiro_api_key")]
    pub api_key: Option<String>,
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
    #[serde(alias = "source_account_id")]
    pub source_account_id: Option<String>,
    pub email: Option<String>,
    pub label: Option<String>,
    pub status: Option<String>,
    #[serde(alias = "added_at")]
    pub added_at: Option<String>,
    pub password: Option<String>,
    #[serde(alias = "profileArn")]
    pub profile_arn: Option<String>,
    #[serde(alias = "machineId")]
    pub machine_id: Option<String>,
    #[serde(alias = "usage_data")]
    pub usage_data: Option<serde_json::Value>,
    #[serde(alias = "group_id")]
    pub group_id: Option<String>,
    #[serde(alias = "tag_links")]
    pub tag_links: Option<serde_json::Value>,
    #[serde(alias = "available_models_cache")]
    pub available_models_cache: Option<serde_json::Value>,
    #[serde(alias = "failure_count")]
    pub failure_count: Option<u32>,
    #[serde(alias = "last_failure_at")]
    pub last_failure_at: Option<String>,
    #[serde(alias = "disabled_reason")]
    pub disabled_reason: Option<String>,
    #[serde(alias = "success_count")]
    pub success_count: Option<u64>,
    #[serde(alias = "csrf_token")]
    pub csrf_token: Option<String>,
    pub nickname: Option<String>,
    #[serde(alias = "ban_status")]
    pub ban_status: Option<String>,
    #[serde(alias = "ban_reason")]
    pub ban_reason: Option<String>,
    #[serde(alias = "ban_time")]
    pub ban_time: Option<i64>,
    #[serde(alias = "subscription_type")]
    pub subscription_type: Option<String>,
    #[serde(alias = "subscriptionTitle", alias = "subscription_title")]
    pub subscription_title: Option<String>,
    #[serde(alias = "days_remaining")]
    pub days_remaining: Option<i64>,
    #[serde(alias = "usage_current")]
    pub usage_current: Option<f64>,
    #[serde(alias = "usage_limit")]
    pub usage_limit: Option<f64>,
    #[serde(alias = "usage_percent")]
    pub usage_percent: Option<f64>,
    #[serde(alias = "next_reset_date")]
    pub next_reset_date: Option<String>,
    #[serde(alias = "last_refresh")]
    pub last_refresh: Option<i64>,
    #[serde(alias = "trial_usage_current")]
    pub trial_usage_current: Option<f64>,
    #[serde(alias = "trial_usage_limit")]
    pub trial_usage_limit: Option<f64>,
    #[serde(alias = "trial_usage_percent")]
    pub trial_usage_percent: Option<f64>,
    #[serde(alias = "trial_status")]
    pub trial_status: Option<String>,
    #[serde(alias = "trial_expires_at")]
    pub trial_expires_at: Option<i64>,
    #[serde(alias = "overage_capability")]
    pub overage_capability: Option<String>,
    #[serde(alias = "overage_cap")]
    pub overage_cap: Option<f64>,
    #[serde(alias = "overage_rate")]
    pub overage_rate: Option<f64>,
    #[serde(alias = "current_overages")]
    pub current_overages: Option<f64>,
    #[serde(alias = "overage_checked_at")]
    pub overage_checked_at: Option<i64>,
    #[serde(alias = "request_count")]
    pub request_count: Option<u64>,
    #[serde(alias = "error_count")]
    pub error_count: Option<u64>,
    #[serde(alias = "total_tokens")]
    pub total_tokens: Option<u64>,
    #[serde(alias = "total_credits")]
    pub total_credits: Option<f64>,
    #[serde(alias = "lastUsed", alias = "last_used_at")]
    pub last_used_at: Option<i64>,
    #[serde(alias = "created_at")]
    pub created_at: Option<i64>,
    pub tags: Option<serde_json::Value>,
    #[serde(alias = "proxyURL")]
    pub proxy_url: Option<String>,
    pub proxy_username: Option<String>,
    pub proxy_password: Option<String>,
    #[serde(default, alias = "proxyId")]
    pub proxy_id: Option<u64>,
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
    /// 刷新令牌（OAuth 凭据必填，API 密钥凭据不需要）
    #[serde(alias = "refreshToken")]
    pub refresh_token: Option<String>,

    /// 认证方式（可选，默认 social）
    #[serde(default = "default_auth_method")]
    pub auth_method: String,

    /// OIDC Client ID（IAM Identity Center 认证需要）
    #[serde(alias = "clientId")]
    pub client_id: Option<String>,

    /// OIDC Client Secret（IAM Identity Center 认证需要）
    #[serde(alias = "clientSecret")]
    pub client_secret: Option<String>,

    /// 身份提供方（Google / GitHub / BuilderId / AzureAD 等）
    pub provider: Option<String>,

    /// Kiro 用户 ID
    #[serde(alias = "userId")]
    pub user_id: Option<String>,

    /// External IdP OAuth2 token endpoint（external_idp 刷新需要）
    #[serde(alias = "tokenEndpoint")]
    pub token_endpoint: Option<String>,

    /// External IdP OIDC issuer URL
    #[serde(alias = "issuerUrl")]
    pub issuer_url: Option<String>,

    /// External IdP scopes（空格分隔）
    pub scopes: Option<String>,

    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,

    /// 调度权重（0/1=普通，2+=更高份额）
    #[serde(default)]
    pub weight: u32,

    /// 凭据级最大并发（>=1，None=回退全局 per_credential_concurrency）
    #[serde(default)]
    pub concurrency: Option<u32>,

    /// 凭据级区域配置（用于 OIDC 令牌刷新）
    /// 未配置时回退到 config.json 的全局 region
    pub region: Option<String>,

    /// 凭据级认证区域（用于令牌刷新）
    pub auth_region: Option<String>,

    /// 凭据级 API 区域（用于 API 请求）
    pub api_region: Option<String>,

    /// 凭据级机器 ID。缺失时后端会生成账号级 UUID 并持久化。
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

    /// 引用式绑定的代理池条目 ID（可选）
    #[serde(default, alias = "proxy_id")]
    pub proxy_id: Option<u64>,

    /// API 密钥（API 密钥凭据必填，格式: ksk_xxxxxxxx）
    /// 设置后直接作为 Bearer 令牌使用，无需 refreshToken
    #[serde(
        alias = "kiroApiKey",
        alias = "kiro_api_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key: Option<String>,

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
    /// 认证方式
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    /// 身份提供方
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Kiro 用户 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// 导入来源 ID（可能是非数字字符串 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_account_id: Option<String>,
    /// 用户自定义显示名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 导入来源状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 导入来源添加时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    /// 显示昵称
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// 导入来源分组 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// 导入来源标签关联数组
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_links: Option<serde_json::Value>,
    /// 是否存在导入来源原始 usage API 响应
    pub has_usage_data: bool,
    /// 是否存在导入来源可用模型缓存
    pub has_available_models_cache: bool,
    /// 导入来源失败次数（区别于运行时 failure_count）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_failure_count: Option<u32>,
    /// 导入来源最后失败时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_last_failure_at: Option<String>,
    /// 导入来源禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_disabled_reason: Option<String>,
    /// 导入来源成功次数（区别于运行时 success_count）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_success_count: Option<u64>,
    /// 是否保存了 Profile ARN
    pub has_profile_arn: bool,
    /// 是否保存了访问令牌
    pub has_token: bool,
    /// 是否保存了刷新令牌
    pub has_refresh_token: bool,
    /// 是否保存了 clientId
    pub has_client_id: bool,
    /// 是否保存了 clientSecret
    pub has_client_secret: bool,
    /// 是否保存了 ID 令牌
    pub has_id_token: bool,
    /// 是否保存了 API 密钥
    pub has_api_key: bool,
    /// 是否保存了代理认证信息
    pub has_proxy_credentials: bool,
    /// 凭据级区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// 凭据级认证区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,
    /// 凭据级 API 区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,
    /// 凭据级机器 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// AWS SSO Start URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    /// 本地缓存 clientIdHash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_hash: Option<String>,
    /// 本地缓存 SSO session ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_session_id: Option<String>,
    /// External IdP 令牌端点
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// External IdP issuer URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    /// External IdP scopes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    /// 端点名称
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl AddCredentialResponse {
    pub fn from_login_details(
        message: impl Into<String>,
        details: CredentialLoginDetailsResponse,
    ) -> Self {
        Self {
            success: true,
            message: message.into(),
            credential_id: details.id,
            email: details.email,
            auth_method: details.auth_method,
            provider: details.provider,
            user_id: details.user_id,
            source_account_id: details.source_account_id,
            label: details.label,
            status: details.status,
            added_at: details.added_at,
            nickname: details.nickname,
            group_id: details.group_id,
            tag_links: details.tag_links,
            has_usage_data: details.has_usage_data,
            has_available_models_cache: details.has_available_models_cache,
            source_failure_count: details.source_failure_count,
            source_last_failure_at: details.source_last_failure_at,
            source_disabled_reason: details.source_disabled_reason,
            source_success_count: details.source_success_count,
            has_profile_arn: details.has_profile_arn,
            has_token: details.has_token,
            has_refresh_token: details.has_refresh_token,
            has_client_id: details.has_client_id,
            has_client_secret: details.has_client_secret,
            has_id_token: details.has_id_token,
            has_api_key: details.has_api_key,
            has_proxy_credentials: details.has_proxy_credentials,
            region: details.region,
            auth_region: details.auth_region,
            api_region: details.api_region,
            machine_id: details.machine_id,
            start_url: details.start_url,
            client_id_hash: details.client_id_hash,
            sso_session_id: details.sso_session_id,
            token_endpoint: details.token_endpoint,
            issuer_url: details.issuer_url,
            scopes: details.scopes,
            endpoint: details.endpoint,
        }
    }
}

#[derive(Debug)]
pub struct ImportCredentialRecordResponse {
    response: AddCredentialResponse,
}

impl ImportCredentialRecordResponse {
    pub fn new(response: AddCredentialResponse) -> Self {
        Self { response }
    }
}

impl Serialize for ImportCredentialRecordResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut root = serde_json::to_value(&self.response)
            .map_err(<S::Error as serde::ser::Error>::custom)?;
        if let serde_json::Value::Object(root_object) = &mut root {
            let mut details = root_object.clone();
            details.insert(
                "id".to_string(),
                serde_json::Value::Number(self.response.credential_id.into()),
            );
            let details = serde_json::Value::Object(details);
            insert_details_account_alias(root_object, details);
        }
        root.serialize(serializer)
    }
}

// ============ 余额查询 ============

/// 余额查询响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    /// 凭据 ID
    pub id: u64,
    /// 订阅标题
    pub subscription_title: Option<String>,
    /// 规范化订阅类型（FREE / PRO / PRO_PLUS / POWER / ENTERPRISE / TEAMS）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
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
    /// 订阅标题
    pub subscription_title: Option<String>,
    /// 规范化订阅类型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyItem {
    pub id: u64,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    pub max_concurrency: Option<u32>,
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub dead: bool,
    pub consecutive_failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_checked: Option<String>,
    pub available_permits: Option<usize>,
    pub bound_credentials: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyListResponse {
    pub proxies: Vec<ProxyItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyUpsertRequest {
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyImportRequest {
    pub text: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub max_concurrency: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyImportResponse {
    pub added: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyAutoAssignRequest {
    #[serde(default)]
    pub credential_ids: Vec<u64>,
    #[serde(default)]
    pub reassign_bound: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyAutoAssignResponse {
    pub assigned: Vec<(u64, u64)>,
    pub skipped: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCredentialProxyRequest {
    #[serde(default, alias = "proxy_id")]
    pub proxy_id: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCredentialProxyByRegionRequest {
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessSettingsResponse {
    pub api_key: Option<String>,
    pub require_api_key: bool,
    pub port: u16,
    pub host: String,
    pub allow_over_usage: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAccessSettingsRequest {
    pub api_key: Option<String>,
    pub require_api_key: Option<bool>,
    pub password: Option<String>,
    pub allow_over_usage: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommonConfigResponse {
    pub machine_id: String,
    pub credential_machine_id_strategy: CredentialMachineIdStrategy,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCommonConfigRequest {
    pub credential_machine_id_strategy: Option<CredentialMachineIdStrategy>,
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
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default)]
    pub openai_format: Option<String>,
    #[serde(default)]
    pub claude_format: Option<String>,
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
    #[serde(default)]
    pub preferred_endpoint: Option<String>,
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
    #[serde(default)]
    pub filter_claude_code: Option<bool>,
    #[serde(default)]
    pub filter_env_noise: Option<bool>,
    #[serde(default)]
    pub filter_strip_boundaries: Option<bool>,
    #[serde(default)]
    pub rules: Option<Vec<PromptFilterRuleDto>>,
}

#[derive(Debug)]
pub struct ProxyUrlConfigResponse {
    pub proxy_url: String,
}

impl Serialize for ProxyUrlConfigResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ProxyUrlConfigResponse", 2)?;
        state.serialize_field("proxyUrl", &self.proxy_url)?;
        state.serialize_field("proxyURL", &self.proxy_url)?;
        state.end()
    }
}

// ============ 全局配置 ============

/// 全局配置响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalConfigResponse {
    /// AWS 区域
    pub region: String,
    /// Prompt Cache TTL（秒）
    pub prompt_cache_ttl_seconds: u64,
    /// 是否启用本地 Prompt Cache usage 记账
    pub prompt_cache_accounting_enabled: bool,
    /// Prompt Cache cache_read 输入 token 占比上限
    pub prompt_cache_max_ratio: f64,
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
    /// 是否启用调度亲和（session/API key 黏住同凭据；关闭则每条消息独立平摊）
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
    /// AWS 区域（可选）
    pub region: Option<String>,
    /// Prompt Cache TTL（秒，可选，仅支持 300 或 3600）
    pub prompt_cache_ttl_seconds: Option<u64>,
    /// 是否启用本地 Prompt Cache usage 记账（可选）
    pub prompt_cache_accounting_enabled: Option<bool>,
    /// Prompt Cache cache_read 输入 token 占比上限（0.0 < ratio <= 1.0）
    pub prompt_cache_max_ratio: Option<f64>,
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
    /// 是否启用调度亲和（可选）
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

// ============ 缓存凭据记录导入 ============

/// 缓存凭据记录格式（用于统一导入入口的自动识别）
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialCacheItem {
    pub provider: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub profile_arn: Option<String>,
    pub expires_at: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub auth_method: Option<String>,
    pub user_id: Option<String>,
    pub token_endpoint: Option<String>,
    pub issuer_url: Option<String>,
    pub scopes: Option<String>,
    pub start_url: Option<String>,
    pub client_id_hash: Option<String>,
    pub id_token: Option<String>,
    pub sso_session_id: Option<String>,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub weight: u32,
    #[serde(default)]
    pub concurrency: Option<u32>,
    pub region: Option<String>,
    pub auth_region: Option<String>,
    pub api_region: Option<String>,
    pub machine_id: Option<String>,
    pub email: Option<String>,
    #[serde(alias = "proxyURL")]
    pub proxy_url: Option<String>,
    #[serde(default, alias = "proxyId")]
    pub proxy_id: Option<u64>,
    pub overage_status: Option<String>,
    pub endpoint: Option<String>,
}

fn default_dry_run() -> bool {
    true
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

// ============ xkiro.rs 完整备份导入/导出 ============

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportCredentialBackupRequest {
    pub ids: Vec<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSnapshotExportRequest {
    #[serde(default)]
    pub ids: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialExportMaterial {
    pub access_token: String,
    pub csrf_token: String,
    pub refresh_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialSnapshotExportSubscription {
    #[serde(rename = "type")]
    pub subscription_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSnapshotExportUsage {
    pub current: f64,
    pub limit: f64,
    pub percent_used: f64,
    pub last_updated: i64,
}

#[derive(Debug, Clone)]
pub struct CredentialSnapshotExportItem {
    pub id: String,
    pub email: String,
    pub nickname: String,
    pub provider: String,
    pub user_id: Option<String>,
    pub machine_id: Option<String>,
    pub credentials: CredentialExportMaterial,
    pub subscription: CredentialSnapshotExportSubscription,
    pub usage: CredentialSnapshotExportUsage,
    pub tags: Vec<String>,
    pub status: String,
    pub created_at: i64,
    pub last_used_at: i64,
}

impl Serialize for CredentialSnapshotExportItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let len = 11
            + usize::from(!self.nickname.is_empty())
            + usize::from(self.user_id.is_some())
            + usize::from(self.machine_id.is_some());
        let mut state = serializer.serialize_struct("CredentialSnapshotExportItem", len)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("email", &self.email)?;
        if !self.nickname.is_empty() {
            state.serialize_field("nickname", &self.nickname)?;
        }
        state.serialize_field("provider", &self.provider)?;
        state.serialize_field("idp", &self.provider)?;
        if let Some(user_id) = &self.user_id {
            state.serialize_field("userId", user_id)?;
        }
        if let Some(machine_id) = &self.machine_id {
            state.serialize_field("machineId", machine_id)?;
        }
        state.serialize_field("credentials", &self.credentials)?;
        state.serialize_field("subscription", &self.subscription)?;
        state.serialize_field("usage", &self.usage)?;
        state.serialize_field("tags", &self.tags)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("createdAt", &self.created_at)?;
        state.serialize_field("lastUsedAt", &self.last_used_at)?;
        state.end()
    }
}

#[derive(Debug, Clone)]
pub struct CredentialSnapshotExportData {
    pub version: String,
    pub exported_at: i64,
    pub credentials: Vec<CredentialSnapshotExportItem>,
    pub groups: Vec<serde_json::Value>,
    pub tags: Vec<serde_json::Value>,
}

impl Serialize for CredentialSnapshotExportData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("CredentialSnapshotExportData", 6)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("exportedAt", &self.exported_at)?;
        state.serialize_field("credentials", &self.credentials)?;
        state.serialize_field("accounts", &self.credentials)?;
        state.serialize_field("groups", &self.groups)?;
        state.serialize_field("tags", &self.tags)?;
        state.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialBackupSource {
    pub app: String,
    pub schema: String,
    pub credential_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialBackupEntry {
    pub credential: KiroCredentials,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialBackup {
    pub format: String,
    pub version: u32,
    pub exported_at: String,
    pub source: CredentialBackupSource,
    pub credentials: Vec<CredentialBackupEntry>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CredentialImportMode {
    SkipExisting,
    MergeMissing,
    ReplaceExisting,
}

impl Default for CredentialImportMode {
    fn default() -> Self {
        Self::SkipExisting
    }
}

#[derive(Debug)]
pub struct ImportCredentialsRequest {
    pub dry_run: bool,
    pub mode: CredentialImportMode,
    pub input: serde_json::Value,
}

impl<'de> Deserialize<'de> for ImportCredentialsRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawImportCredentialsRequest {
            #[serde(default = "default_dry_run")]
            dry_run: bool,
            #[serde(default)]
            mode: CredentialImportMode,
            #[serde(default)]
            input: Option<serde_json::Value>,
            #[serde(default)]
            bundle: Option<serde_json::Value>,
        }

        let raw = RawImportCredentialsRequest::deserialize(deserializer)?;
        let input = raw
            .input
            .or(raw.bundle)
            .ok_or_else(|| serde::de::Error::missing_field("input"))?;

        Ok(Self {
            dry_run: raw.dry_run,
            mode: raw.mode,
            input,
        })
    }
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CredentialImportAction {
    Added,
    Skipped,
    Merged,
    Replaced,
    Invalid,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialImportSummary {
    pub parsed: usize,
    pub added: usize,
    pub skipped: usize,
    pub merged: usize,
    pub replaced: usize,
    pub invalid: usize,
}

impl CredentialImportSummary {
    pub fn single_invalid() -> Self {
        Self {
            parsed: 0,
            added: 0,
            skipped: 0,
            merged: 0,
            replaced: 0,
            invalid: 1,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialImportItem {
    pub index: usize,
    pub action: CredentialImportAction,
    pub source_format: String,
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_links: Option<serde_json::Value>,
    pub has_usage_data: bool,
    pub has_available_models_cache: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_failure_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_last_failure_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_disabled_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_success_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub will_refresh: bool,
    pub has_profile_arn: bool,
    pub has_token: bool,
    pub has_refresh_token: bool,
    pub has_client_id: bool,
    pub has_client_secret: bool,
    pub has_id_token: bool,
    pub has_api_key: bool,
    pub has_proxy_credentials: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl CredentialImportItem {
    pub fn invalid(
        index: usize,
        source_format: impl Into<String>,
        fingerprint: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            index,
            action: CredentialImportAction::Invalid,
            source_format: source_format.into(),
            fingerprint: fingerprint.into(),
            credential_id: None,
            reason: Some(reason.into()),
            auth_method: None,
            provider: None,
            email: None,
            user_id: None,
            machine_id: None,
            group_id: None,
            tag_links: None,
            has_usage_data: false,
            has_available_models_cache: false,
            source_failure_count: None,
            source_last_failure_at: None,
            source_disabled_reason: None,
            source_success_count: None,
            region: None,
            auth_region: None,
            api_region: None,
            start_url: None,
            client_id_hash: None,
            sso_session_id: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            endpoint: None,
            will_refresh: false,
            has_profile_arn: false,
            has_token: false,
            has_refresh_token: false,
            has_client_id: false,
            has_client_secret: false,
            has_id_token: false,
            has_api_key: false,
            has_proxy_credentials: false,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCredentialsResponse {
    pub summary: CredentialImportSummary,
    pub items: Vec<CredentialImportItem>,
}

impl ImportCredentialsResponse {
    pub fn invalid_import(reason: impl Into<String>) -> Self {
        Self {
            summary: CredentialImportSummary::single_invalid(),
            items: vec![CredentialImportItem::invalid(
                0, "unknown", "(input)", reason,
            )],
        }
    }
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

/// 模型缓存刷新响应（保留 count 字段）
#[derive(Debug, Serialize)]
pub struct RefreshCredentialModelsResponse {
    pub success: bool,
    pub message: String,
    pub count: usize,
}

impl RefreshCredentialModelsResponse {
    pub fn new(message: impl Into<String>, count: usize) -> Self {
        Self {
            success: true,
            message: message.into(),
            count,
        }
    }
}

/// 全量模型缓存刷新响应（保留 refreshed/failed 字段）
#[derive(Debug, Serialize)]
pub struct RefreshAllCredentialModelsResponse {
    pub success: bool,
    pub message: String,
    pub refreshed: usize,
    pub failed: usize,
}

impl RefreshAllCredentialModelsResponse {
    pub fn new(message: impl Into<String>, refreshed: usize, failed: usize) -> Self {
        Self {
            success: true,
            message: message.into(),
            refreshed,
            failed,
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
    pub subscription_type: Option<String>,
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

// ============ 批量刷新令牌端点 ============

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

// ============ 社交 OAuth 登录 ============

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

/// helper 模式回传：本机 helper 完成 OAuth 后把最终令牌投递到服务端
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

#[derive(Debug)]
pub enum PollSocialLoginResponse {
    Waiting,
    Success {
        credential_id: u64,
        auth_method: Option<String>,
        provider: Option<String>,
        details: Option<CredentialLoginDetailsResponse>,
    },
    Expired,
    Error {
        message: String,
    },
}

impl PollSocialLoginResponse {
    pub fn success(credential_id: u64, details: CredentialLoginDetailsResponse) -> Self {
        let auth_method = details.auth_method.clone();
        let provider = details.provider.clone();
        Self::Success {
            credential_id,
            auth_method,
            provider,
            details: Some(details),
        }
    }
}

impl Serialize for PollSocialLoginResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Waiting => {
                let mut state = serializer.serialize_struct("PollSocialLoginResponse", 1)?;
                state.serialize_field("status", "waiting")?;
                state.end()
            }
            Self::Success {
                credential_id,
                auth_method,
                provider,
                details,
            } => {
                let mut len = 2;
                len += login_metadata_field_count(
                    auth_method.as_deref(),
                    provider.as_deref(),
                    details.is_some(),
                );
                let mut state = serializer.serialize_struct("PollSocialLoginResponse", len)?;
                state.serialize_field("status", "success")?;
                state.serialize_field("credentialId", credential_id)?;
                serialize_login_metadata_fields(
                    &mut state,
                    auth_method.as_deref(),
                    provider.as_deref(),
                    details.as_ref(),
                )?;
                state.end()
            }
            Self::Expired => {
                let mut state = serializer.serialize_struct("PollSocialLoginResponse", 1)?;
                state.serialize_field("status", "expired")?;
                state.end()
            }
            Self::Error { message } => {
                let mut state = serializer.serialize_struct("PollSocialLoginResponse", 2)?;
                state.serialize_field("status", "error")?;
                state.serialize_field("message", message)?;
                state.end()
            }
        }
    }
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

#[derive(Debug)]
pub struct CompleteIamSsoLoginResponse {
    pub success: bool,
    pub details: CredentialLoginDetailsResponse,
}

impl CompleteIamSsoLoginResponse {
    pub fn success(details: CredentialLoginDetailsResponse) -> Self {
        Self {
            success: true,
            details,
        }
    }
}

impl Serialize for CompleteIamSsoLoginResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("CompleteIamSsoLoginResponse", 3)?;
        state.serialize_field("success", &self.success)?;
        serialize_details_account_alias(&mut state, &self.details)?;
        state.end()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIamSsoLoginResponse {
    pub session_id: String,
    pub authorize_url: String,
    pub expires_in: i64,
}

#[derive(Debug)]
pub enum PollIdcLoginResponse {
    Pending,
    Success {
        credential_id: u64,
        auth_method: Option<String>,
        provider: Option<String>,
        details: Option<CredentialLoginDetailsResponse>,
    },
    Expired,
}

impl PollIdcLoginResponse {
    pub fn success(credential_id: u64, details: CredentialLoginDetailsResponse) -> Self {
        let auth_method = details.auth_method.clone();
        let provider = details.provider.clone();
        Self::Success {
            credential_id,
            auth_method,
            provider,
            details: Some(details),
        }
    }
}

impl Serialize for PollIdcLoginResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Pending => {
                let mut state = serializer.serialize_struct("PollIdcLoginResponse", 1)?;
                state.serialize_field("status", "pending")?;
                state.end()
            }
            Self::Success {
                credential_id,
                auth_method,
                provider,
                details,
            } => {
                let mut len = 2;
                len += login_metadata_field_count(
                    auth_method.as_deref(),
                    provider.as_deref(),
                    details.is_some(),
                );
                let mut state = serializer.serialize_struct("PollIdcLoginResponse", len)?;
                state.serialize_field("status", "success")?;
                state.serialize_field("credentialId", credential_id)?;
                serialize_login_metadata_fields(
                    &mut state,
                    auth_method.as_deref(),
                    provider.as_deref(),
                    details.as_ref(),
                )?;
                state.end()
            }
            Self::Expired => {
                let mut state = serializer.serialize_struct("PollIdcLoginResponse", 1)?;
                state.serialize_field("status", "expired")?;
                state.end()
            }
        }
    }
}

// ============ 请求日志和统计 ============

/// 请求日志响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogsResponse {
    pub logs: Vec<super::stats::RequestLog>,
    pub total: usize,
    pub success: usize,
    pub errors: usize,
}

/// 系统状态响应
#[derive(Debug)]
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

impl SystemStatusResponse {
    pub fn new(
        status: impl Into<String>,
        version: impl Into<String>,
        uptime: u64,
        total_requests: i64,
        success_requests: i64,
        failed_requests: i64,
        total_tokens: i64,
        total_credits: f64,
        credentials_total: usize,
        credentials_available: usize,
    ) -> Self {
        Self {
            status: status.into(),
            version: version.into(),
            uptime,
            total_requests,
            success_requests,
            failed_requests,
            total_tokens,
            total_credits,
            credentials_total,
            credentials_available,
        }
    }
}

impl Serialize for SystemStatusResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("SystemStatusResponse", 12)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("accounts", &self.credentials_total)?;
        state.serialize_field("available", &self.credentials_available)?;
        state.serialize_field("uptime", &self.uptime)?;
        state.serialize_field("totalRequests", &self.total_requests)?;
        state.serialize_field("successRequests", &self.success_requests)?;
        state.serialize_field("failedRequests", &self.failed_requests)?;
        state.serialize_field("totalTokens", &self.total_tokens)?;
        state.serialize_field("totalCredits", &self.total_credits)?;
        state.serialize_field("credentialsTotal", &self.credentials_total)?;
        state.serialize_field("credentialsAvailable", &self.credentials_available)?;
        state.end()
    }
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

/// 生成机器 ID 响应
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialBatchRequest {
    pub ids: Vec<serde_json::Value>,
    pub action: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialBatchResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refreshed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CredentialRefreshInfo {
    pub email: String,
    pub user_id: String,
    pub subscription_type: String,
    pub subscription_title: String,
    pub days_remaining: i64,
    pub usage_current: f64,
    pub usage_limit: f64,
    pub usage_percent: f64,
    pub next_reset_date: String,
    pub last_refresh: i64,
    pub trial_usage_current: f64,
    pub trial_usage_limit: f64,
    pub trial_usage_percent: f64,
    pub trial_status: String,
    pub trial_expires_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialRefreshResponse {
    pub success: bool,
    pub info: CredentialRefreshInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialModelsResponse {
    pub success: bool,
    pub models: Vec<AvailableModel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CachedCredentialModelsResponse {
    pub success: bool,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialProbeRequest {
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialProbeResponse {
    pub success: bool,
    pub reply: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialOverageResponse {
    pub success: bool,
    pub overage_status: Option<String>,
    pub overage_capability: Option<String>,
    pub subscription_title: Option<String>,
    pub overage_cap: f64,
    pub overage_rate: f64,
    pub current_overages: f64,
    pub overage_checked_at: i64,
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

// ============ SSO 令牌导入 ============

/// SSO 令牌导入请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSsoTokenRequest {
    /// SSO Bearer 令牌（支持批量：多个令牌用换行分隔）
    #[serde(alias = "bearerToken")]
    pub token: String,
    /// AWS 区域（可选，默认 us-east-1）
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

/// SSO 令牌导入结果项
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

/// SSO 令牌导入响应
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
    /// AWS 区域（可选，默认 us-east-1）
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
#[derive(Debug)]
pub enum PollBuilderIdLoginResponse {
    Pending {
        interval: i64,
    },
    Success {
        credential_id: u64,
        email: Option<String>,
        auth_method: Option<String>,
        provider: Option<String>,
        details: Option<CredentialLoginDetailsResponse>,
    },
    Expired,
    Error {
        message: String,
    },
}

impl PollBuilderIdLoginResponse {
    pub fn success(credential_id: u64, details: CredentialLoginDetailsResponse) -> Self {
        let email = details.email.clone();
        let auth_method = details.auth_method.clone();
        let provider = details.provider.clone();
        Self::Success {
            credential_id,
            email,
            auth_method,
            provider,
            details: Some(details),
        }
    }
}

impl Serialize for PollBuilderIdLoginResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Pending { interval } => {
                let mut state = serializer.serialize_struct("PollBuilderIdLoginResponse", 2)?;
                state.serialize_field("status", "pending")?;
                state.serialize_field("interval", interval)?;
                state.end()
            }
            Self::Success {
                credential_id,
                email,
                auth_method,
                provider,
                details,
            } => {
                let mut len = 2;
                len += usize::from(email.is_some());
                len += login_metadata_field_count(
                    auth_method.as_deref(),
                    provider.as_deref(),
                    details.is_some(),
                );
                let mut state = serializer.serialize_struct("PollBuilderIdLoginResponse", len)?;
                state.serialize_field("status", "success")?;
                state.serialize_field("credentialId", credential_id)?;
                if let Some(email) = email {
                    state.serialize_field("email", email)?;
                }
                serialize_login_metadata_fields(
                    &mut state,
                    auth_method.as_deref(),
                    provider.as_deref(),
                    details.as_ref(),
                )?;
                state.end()
            }
            Self::Expired => {
                let mut state = serializer.serialize_struct("PollBuilderIdLoginResponse", 1)?;
                state.serialize_field("status", "expired")?;
                state.end()
            }
            Self::Error { message } => {
                let mut state = serializer.serialize_struct("PollBuilderIdLoginResponse", 2)?;
                state.serialize_field("status", "error")?;
                state.serialize_field("message", message)?;
                state.end()
            }
        }
    }
}

#[derive(Debug)]
pub struct PollBuilderIdLoginByBodyResponse {
    response: PollBuilderIdLoginResponse,
}

impl PollBuilderIdLoginByBodyResponse {
    pub fn new(response: PollBuilderIdLoginResponse) -> Self {
        Self { response }
    }
}

impl Serialize for PollBuilderIdLoginByBodyResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.response {
            PollBuilderIdLoginResponse::Pending { interval } => {
                let mut state =
                    serializer.serialize_struct("PollBuilderIdLoginByBodyResponse", 4)?;
                state.serialize_field("success", &true)?;
                state.serialize_field("completed", &false)?;
                state.serialize_field("status", "pending")?;
                state.serialize_field("interval", interval)?;
                state.end()
            }
            PollBuilderIdLoginResponse::Success {
                credential_id,
                email,
                auth_method,
                provider,
                details,
            } => {
                let details_json = details
                    .as_ref()
                    .and_then(|details| serde_json::to_value(details).ok())
                    .unwrap_or_else(|| {
                        serde_json::json!({
                            "id": credential_id,
                            "email": email,
                            "authMethod": auth_method,
                            "provider": provider,
                        })
                    });
                let mut state =
                    serializer.serialize_struct("PollBuilderIdLoginByBodyResponse", 4)?;
                state.serialize_field("success", &true)?;
                state.serialize_field("completed", &true)?;
                serialize_details_account_alias(&mut state, &details_json)?;
                state.end()
            }
            PollBuilderIdLoginResponse::Expired => {
                let mut state =
                    serializer.serialize_struct("PollBuilderIdLoginByBodyResponse", 4)?;
                state.serialize_field("success", &false)?;
                state.serialize_field("completed", &false)?;
                state.serialize_field("status", "expired")?;
                state.serialize_field("error", "expired")?;
                state.end()
            }
            PollBuilderIdLoginResponse::Error { message } => {
                let mut state =
                    serializer.serialize_struct("PollBuilderIdLoginByBodyResponse", 3)?;
                state.serialize_field("success", &false)?;
                state.serialize_field("completed", &false)?;
                state.serialize_field("error", message)?;
                state.end()
            }
        }
    }
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialLoginDetailsResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_links: Option<serde_json::Value>,
    pub has_usage_data: bool,
    pub has_available_models_cache: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_failure_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_last_failure_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_disabled_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_success_count: Option<u64>,
    pub has_profile_arn: bool,
    pub has_token: bool,
    pub has_refresh_token: bool,
    pub has_client_id: bool,
    pub has_client_secret: bool,
    pub has_id_token: bool,
    pub has_api_key: bool,
    pub has_proxy_credentials: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

fn details_account_alias_field_count(has_details: bool) -> usize {
    usize::from(has_details) * 2
}

fn login_metadata_field_count(
    auth_method: Option<&str>,
    provider: Option<&str>,
    has_details: bool,
) -> usize {
    usize::from(auth_method.is_some())
        + usize::from(provider.is_some())
        + details_account_alias_field_count(has_details)
}

fn serialize_details_account_alias<S, T>(state: &mut S, details: &T) -> Result<(), S::Error>
where
    S: SerializeStruct,
    T: Serialize + ?Sized,
{
    state.serialize_field("details", details)?;
    state.serialize_field("account", details)
}

fn insert_details_account_alias(
    object: &mut serde_json::Map<String, serde_json::Value>,
    details: serde_json::Value,
) {
    object.insert("details".to_string(), details.clone());
    object.insert("account".to_string(), details);
}

fn serialize_login_metadata_fields<S>(
    state: &mut S,
    auth_method: Option<&str>,
    provider: Option<&str>,
    details: Option<&CredentialLoginDetailsResponse>,
) -> Result<(), S::Error>
where
    S: SerializeStruct,
{
    if let Some(auth_method) = auth_method {
        state.serialize_field("authMethod", auth_method)?;
    }
    if let Some(provider) = provider {
        state.serialize_field("provider", provider)?;
    }
    if let Some(details) = details {
        serialize_details_account_alias(state, details)?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct PollKiroSsoLoginResponse {
    pub success: bool,
    pub completed: bool,
    pub status: Option<String>,
    pub error: Option<String>,
    pub details: Option<CredentialLoginDetailsResponse>,
}

impl PollKiroSsoLoginResponse {
    pub fn success(details: CredentialLoginDetailsResponse) -> Self {
        Self {
            success: true,
            completed: true,
            status: None,
            error: None,
            details: Some(details),
        }
    }
}

impl Serialize for PollKiroSsoLoginResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut len = 2;
        len += usize::from(self.status.is_some());
        len += usize::from(self.error.is_some());
        len += details_account_alias_field_count(self.details.is_some());

        let mut state = serializer.serialize_struct("PollKiroSsoLoginResponse", len)?;
        state.serialize_field("success", &self.success)?;
        state.serialize_field("completed", &self.completed)?;
        if let Some(status) = &self.status {
            state.serialize_field("status", status)?;
        }
        if let Some(error) = &self.error {
            state.serialize_field("error", error)?;
        }
        if let Some(details) = &self.details {
            serialize_details_account_alias(&mut state, details)?;
        }
        state.end()
    }
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

// ============ API 密钥管理 ============

/// API 密钥条目
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
    /// 是否由单 apiKey 配置迁移而来
    #[serde(default)]
    pub migrated: bool,
    /// 创建时间（Unix 秒时间戳）
    pub created_at: i64,
    /// 最后使用时间（Unix 秒时间戳）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    /// 令牌使用限制（0=无限制）
    #[serde(default)]
    pub token_limit: i64,
    /// 额度使用限制（0=无限制）
    #[serde(default)]
    pub credit_limit: f64,
    /// 已使用令牌数
    #[serde(default)]
    pub tokens_used: i64,
    /// 已使用额度
    #[serde(default)]
    pub credits_used: f64,
    /// 请求次数
    #[serde(default)]
    pub requests_count: i64,
}

/// API 密钥输出视图（不暴露明文 key）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyView {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub key_masked: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub migrated: bool,
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

/// 创建 API 密钥请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateApiKeyRequest {
    /// 名称（可选）
    pub name: Option<String>,
    /// 自定义 Key（可选；不传则自动生成）
    pub key: Option<String>,
    /// 是否启用（默认 true）
    pub enabled: Option<bool>,
    /// 令牌使用限制（0=无限制，默认 0）
    #[serde(default)]
    pub token_limit: i64,
    /// 额度使用限制（0=无限制，默认 0）
    #[serde(default)]
    pub credit_limit: f64,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 更新 API 密钥请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateApiKeyRequest {
    /// 名称
    pub name: Option<Option<String>>,
    /// Key 值
    pub key: Option<String>,
    /// 是否启用
    pub enabled: Option<bool>,
    /// 令牌使用限制
    pub token_limit: Option<i64>,
    /// 额度使用限制
    pub credit_limit: Option<f64>,
}

/// API 密钥列表响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyListResponse {
    pub api_keys: Vec<ApiKeyView>,
}

/// API 密钥创建响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateApiKeyResponse {
    pub success: bool,
    pub id: String,
    pub key: String,
    pub api_key: ApiKeyView,
}

/// API 密钥更新/重置响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyMutationResponse {
    pub success: bool,
    pub api_key: ApiKeyView,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login_details_fixture() -> CredentialLoginDetailsResponse {
        CredentialLoginDetailsResponse {
            id: 7,
            email: Some("user@example.com".to_string()),
            auth_method: Some("social".to_string()),
            provider: Some("GitHub".to_string()),
            user_id: None,
            source_account_id: None,
            label: None,
            status: None,
            added_at: None,
            nickname: None,
            group_id: None,
            tag_links: None,
            has_usage_data: false,
            has_available_models_cache: false,
            source_failure_count: None,
            source_last_failure_at: None,
            source_disabled_reason: None,
            source_success_count: None,
            has_profile_arn: false,
            has_token: true,
            has_refresh_token: true,
            has_client_id: false,
            has_client_secret: false,
            has_id_token: false,
            has_api_key: false,
            has_proxy_credentials: false,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            start_url: None,
            client_id_hash: None,
            sso_session_id: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            endpoint: None,
        }
    }

    #[test]
    fn import_credentials_request_accepts_canonical_input_field() {
        let request: ImportCredentialsRequest = serde_json::from_value(serde_json::json!({
            "dryRun": true,
            "mode": "mergeMissing",
            "input": { "credentials": [] }
        }))
        .unwrap();

        assert!(request.dry_run);
        assert_eq!(request.mode, CredentialImportMode::MergeMissing);
        assert_eq!(request.input, serde_json::json!({ "credentials": [] }));
    }

    #[test]
    fn import_credentials_request_keeps_bundle_wire_field_as_input_alias() {
        let request: ImportCredentialsRequest = serde_json::from_value(serde_json::json!({
            "dryRun": true,
            "mode": "mergeMissing",
            "bundle": { "credentials": [] }
        }))
        .unwrap();

        assert!(request.dry_run);
        assert_eq!(request.mode, CredentialImportMode::MergeMissing);
        assert_eq!(request.input, serde_json::json!({ "credentials": [] }));
    }

    #[test]
    fn import_credentials_request_prefers_input_over_bundle_alias() {
        let request: ImportCredentialsRequest = serde_json::from_value(serde_json::json!({
            "input": { "credentials": [{ "email": "canonical@example.com" }] },
            "bundle": { "credentials": [{ "email": "legacy@example.com" }] }
        }))
        .unwrap();

        assert_eq!(
            request.input,
            serde_json::json!({ "credentials": [{ "email": "canonical@example.com" }] })
        );
    }

    #[test]
    fn social_poll_success_uses_camel_case_credential_id() {
        let value = serde_json::to_value(PollSocialLoginResponse::Success {
            credential_id: 42,
            auth_method: Some("social".to_string()),
            provider: Some("GitHub".to_string()),
            details: None,
        })
        .unwrap();

        assert_eq!(value["status"], "success");
        assert_eq!(value["credentialId"], 42);
        assert_eq!(value["authMethod"], "social");
        assert!(value.get("credential_id").is_none());
    }

    #[test]
    fn add_credential_response_can_derive_from_login_details() {
        let mut details = login_details_fixture();
        details.group_id = Some("group-1".to_string());
        details.has_available_models_cache = true;
        details.region = Some("us-east-1".to_string());

        let value =
            serde_json::to_value(AddCredentialResponse::from_login_details("ok", details)).unwrap();

        assert_eq!(value["success"], true);
        assert_eq!(value["message"], "ok");
        assert_eq!(value["credentialId"], 7);
        assert_eq!(value["email"], "user@example.com");
        assert_eq!(value["groupId"], "group-1");
        assert_eq!(value["hasAvailableModelsCache"], true);
        assert_eq!(value["hasRefreshToken"], true);
        assert_eq!(value["region"], "us-east-1");
    }

    #[test]
    fn import_credential_record_response_keeps_details_and_account_wire_fields() {
        let mut details = login_details_fixture();
        details.group_id = Some("group-1".to_string());
        details.region = Some("us-east-1".to_string());

        let response = AddCredentialResponse::from_login_details("ok", details);
        let value = serde_json::to_value(ImportCredentialRecordResponse::new(response)).unwrap();

        assert_eq!(value["success"], true);
        assert_eq!(value["credentialId"], 7);
        assert_eq!(value["email"], "user@example.com");
        assert_eq!(value["details"], value["account"]);
        assert_eq!(value["details"]["id"], 7);
        assert_eq!(value["details"]["credentialId"], 7);
        assert_eq!(value["details"]["groupId"], "group-1");
        assert_eq!(value["details"]["region"], "us-east-1");
    }

    #[test]
    fn login_success_details_keep_details_and_account_wire_fields() {
        let details = login_details_fixture();
        let value = serde_json::to_value(PollSocialLoginResponse::success(42, details)).unwrap();

        assert_eq!(value["details"]["id"], 7);
        assert_eq!(value["details"]["email"], "user@example.com");
        assert_eq!(value["account"]["id"], 7);
        assert_eq!(value["account"]["email"], "user@example.com");
    }

    #[test]
    fn complete_iam_sso_response_keeps_details_and_account_wire_fields() {
        let details = login_details_fixture();
        let value = serde_json::to_value(CompleteIamSsoLoginResponse::success(details)).unwrap();

        assert_eq!(value["success"], true);
        assert_eq!(value["details"]["id"], 7);
        assert_eq!(value["account"]["id"], 7);
    }

    #[test]
    fn login_poll_success_variants_keep_account_wire_field() {
        let idc = serde_json::to_value(PollIdcLoginResponse::success(42, login_details_fixture()))
            .unwrap();
        let builder = serde_json::to_value(PollBuilderIdLoginResponse::success(
            42,
            login_details_fixture(),
        ))
        .unwrap();
        let hosted =
            serde_json::to_value(PollKiroSsoLoginResponse::success(login_details_fixture()))
                .unwrap();

        assert_eq!(idc["account"]["id"], 7);
        assert_eq!(builder["account"]["id"], 7);
        assert_eq!(hosted["account"]["id"], 7);
    }

    #[test]
    fn builder_id_body_poll_response_keeps_browser_wire_shape() {
        let pending = serde_json::to_value(PollBuilderIdLoginByBodyResponse::new(
            PollBuilderIdLoginResponse::Pending { interval: 3 },
        ))
        .unwrap();
        assert_eq!(pending["success"], true);
        assert_eq!(pending["completed"], false);
        assert_eq!(pending["status"], "pending");
        assert_eq!(pending["interval"], 3);

        let success = serde_json::to_value(PollBuilderIdLoginByBodyResponse::new(
            PollBuilderIdLoginResponse::success(42, login_details_fixture()),
        ))
        .unwrap();
        assert_eq!(success["success"], true);
        assert_eq!(success["completed"], true);
        assert!(success.get("status").is_none());
        assert!(success.get("credentialId").is_none());
        assert_eq!(success["details"], success["account"]);
        assert_eq!(success["details"]["id"], 7);

        let expired = serde_json::to_value(PollBuilderIdLoginByBodyResponse::new(
            PollBuilderIdLoginResponse::Expired,
        ))
        .unwrap();
        assert_eq!(expired["success"], false);
        assert_eq!(expired["completed"], false);
        assert_eq!(expired["status"], "expired");
        assert_eq!(expired["error"], "expired");

        let error = serde_json::to_value(PollBuilderIdLoginByBodyResponse::new(
            PollBuilderIdLoginResponse::Error {
                message: "denied".to_string(),
            },
        ))
        .unwrap();
        assert_eq!(error["success"], false);
        assert_eq!(error["completed"], false);
        assert!(error.get("status").is_none());
        assert_eq!(error["error"], "denied");
    }

    #[test]
    fn model_refresh_responses_keep_existing_summary_fields() {
        let single = serde_json::to_value(RefreshCredentialModelsResponse::new("ok", 7)).unwrap();
        assert_eq!(single["success"], true);
        assert_eq!(single["message"], "ok");
        assert_eq!(single["count"], 7);

        let all =
            serde_json::to_value(RefreshAllCredentialModelsResponse::new("ok", 11, 2)).unwrap();
        assert_eq!(all["success"], true);
        assert_eq!(all["message"], "ok");
        assert_eq!(all["refreshed"], 11);
        assert_eq!(all["failed"], 2);
    }

    #[test]
    fn system_status_exposes_credential_counts_and_account_fields() {
        let value = serde_json::to_value(SystemStatusResponse::new(
            "ok", "test", 9, 11, 7, 4, 123, 1.5, 3, 2,
        ))
        .unwrap();

        assert_eq!(value["accounts"], 3);
        assert_eq!(value["available"], 2);
        assert_eq!(value["credentialsTotal"], 3);
        assert_eq!(value["credentialsAvailable"], 2);
    }
}
