//! Kiro OAuth 凭据数据模型
//!
//! 支持从 Kiro IDE 的凭据文件加载，使用社交登录认证方式
//! 支持单凭据和多凭据配置格式

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::http_client::ProxyConfig;
use crate::kiro::region::{
    DEFAULT_Q_TRANSPORT_REGION, is_known_bad_q_transport_region, normalize_q_transport_region,
    trimmed_region,
};
use crate::model::config::Config;

pub const KIRO_BUILDER_ID_START_URL: &str = "https://view.awsapps.com/start";

/// Kiro OAuth 凭据
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KiroCredentials {
    /// 凭据唯一标识符（自增 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,

    /// 访问令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,

    /// 刷新令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    /// Profile ARN
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,

    /// 过期时间 (RFC3339 格式)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// 认证方式 (social / idc / external_idp / api_key)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,

    /// 身份提供方
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// Kiro 用户 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    /// OIDC Client ID (IAM Identity Center 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// OIDC Client Secret (IAM Identity Center 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,

    /// External IdP OAuth2 token endpoint（external_idp 刷新需要）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,

    /// External IdP OIDC issuer URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,

    /// External IdP 授权 scopes（空格分隔）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,

    /// AWS SSO Start URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,

    /// 本地缓存使用的 clientIdHash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_hash: Option<String>,

    /// 本地缓存 idToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,

    /// 本地缓存 SSO session ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_session_id: Option<String>,

    /// 凭据优先级（数字越小优先级越高，默认为 0）
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: u32,

    /// 调度权重（0/1 等价于 1，2+ 表示加权轮询份额）
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub weight: u32,

    /// 凭据级最大并发数（可选）
    ///
    /// 未配置时回退到 `config.perCredentialConcurrency` 全局值。
    /// 取值至少为 1，0 视为非法（运行时由 set_credential_concurrency 校验）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,

    /// 凭据级区域配置（用于 OIDC 令牌刷新）
    /// 未配置时回退到 config.json 的全局 region
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// 凭据级认证区域（用于令牌刷新）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// 凭据级 API 区域（用于 API 请求）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    /// 凭据级机器 ID。缺失时加载/导入流程会生成账号级 UUID 并持久化。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,

    /// 用户邮箱（从 Anthropic API 获取）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// 导入来源 / 凭据元数据（透传保真）
    ///
    /// `#[serde(flatten)]` 保持这些键在顶层的 camelCase 线格式不变，
    /// 仅在 Rust 侧收拢到子结构，缩小主结构的逻辑表面。
    #[serde(flatten)]
    pub meta: CredentialSourceMetadata,

    /// 凭据级代理 URL（可选）
    /// 支持 http/https/socks5 协议
    /// 特殊值 "direct" 表示显式不使用代理（即使全局配置了代理）
    /// 未配置时回退到全局代理配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,

    /// 代理池引用 ID（可选）
    /// 设置后运行时从代理池回填真实代理，凭证文件只保存引用。
    #[serde(alias = "proxy_id", skip_serializing_if = "Option::is_none")]
    pub proxy_id: Option<u64>,

    /// 凭据是否被禁用（默认为 false）
    #[serde(default)]
    pub disabled: bool,

    /// API 密钥（headless 模式）
    /// 格式: ksk_xxxxxxxx
    /// 设置后直接作为 Bearer 令牌使用，无需 refreshToken
    #[serde(
        alias = "kiroApiKey",
        alias = "kiro_api_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key: Option<String>,

    /// 端点名称（可选）
    ///
    /// 决定该凭据走哪套 Kiro API。未配置时回退到 `config.defaultEndpoint`（默认 "ide"）。
    /// 端点名必须在启动时注册的端点 registry 中存在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// 导入来源 / 凭据元数据（透传保真）
///
/// 这些字段几乎只经序列化往返（与 kiro-account-manager 账号池格式互通）或供 admin-ui 展示，
/// 极少被核心调度逻辑读取。归组于此后经 `#[serde(flatten)]` 挂回 `KiroCredentials`，
/// 顶层 JSON 线格式保持不变（camelCase、None 跳过），仅收拢 Rust 侧字段表面。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSourceMetadata {
    /// 导入来源 ID（可能是非数字字符串 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_account_id: Option<String>,

    /// 导入来源用户自定义显示名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// 导入来源状态（active/banned/suspended 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// 导入来源添加时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,

    /// 历史 username/password 字段，仅为导入/导出保真保留
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    /// 订阅等级（KIRO PRO+ / KIRO FREE 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub subscription_title: Option<String>,

    /// 上游 overage 开关状态（保留 overageStatus 导入/导出字段）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub overage_status: Option<String>,

    /// 导入来源原始 usage API 响应
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_data: Option<serde_json::Value>,

    /// 导入来源分组 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,

    /// 导入来源标签关联数组
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_links: Option<serde_json::Value>,

    /// 导入来源可用模型缓存
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_models_cache: Option<serde_json::Value>,

    /// 导入来源失败次数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_count: Option<u32>,

    /// 导入来源最后失败时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_at: Option<String>,

    /// 导入来源禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,

    /// 导入来源成功次数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_count: Option<u64>,

    /// 凭据元数据：CSRF token 占位/历史字段
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csrf_token: Option<String>,

    /// 凭据元数据：显示昵称
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,

    /// 凭据元数据：封禁状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_status: Option<String>,

    /// 凭据元数据：封禁原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_reason: Option<String>,

    /// 凭据元数据：封禁时间戳
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ban_time: Option<i64>,

    /// 凭据元数据：订阅类型
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,

    /// 凭据元数据：订阅剩余天数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_remaining: Option<i64>,

    /// 凭据元数据：用量快照
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_current: Option<f64>,

    /// 凭据元数据：用量上限
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<f64>,

    /// 凭据元数据：用量百分比
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_percent: Option<f64>,

    /// 凭据元数据：下次重置日期
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset_date: Option<String>,

    /// 凭据元数据：最后刷新时间戳
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<i64>,

    /// 凭据元数据：试用用量快照
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_current: Option<f64>,

    /// 凭据元数据：试用用量上限
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_limit: Option<f64>,

    /// 凭据元数据：试用用量百分比
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_usage_percent: Option<f64>,

    /// 凭据元数据：试用状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_status: Option<String>,

    /// 凭据元数据：试用过期时间戳
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_expires_at: Option<i64>,

    /// 凭据元数据：overage 能力
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_capability: Option<String>,

    /// 凭据元数据：overage 上限
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_cap: Option<f64>,

    /// 凭据元数据：overage 单价
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_rate: Option<f64>,

    /// 凭据元数据：当前 overage 消耗
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_overages: Option<f64>,

    /// 凭据元数据：overage 检查时间戳
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_checked_at: Option<i64>,

    /// 凭据元数据：请求统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_count: Option<u64>,

    /// 凭据元数据：错误统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_count: Option<u64>,

    /// 凭据元数据：token 统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,

    /// 凭据元数据：credit 统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_credits: Option<f64>,

    /// 凭据元数据：最后使用时间戳
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,

    /// 外部导出视图元数据：创建时间戳
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,

    /// 外部导出视图元数据：标签数组
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<serde_json::Value>,

    /// allowOverage 导入提示，仅用于加载后归一到 overage_status，不再写回。
    #[serde(rename = "allowOverage", default, skip_serializing)]
    pub allow_overage_import: bool,
}

/// 判断是否为零（用于跳过序列化）
fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn canonicalize_auth_method_value(value: &str) -> &str {
    let value = value.trim();
    if value.eq_ignore_ascii_case("social")
        || value.eq_ignore_ascii_case("google")
        || value.eq_ignore_ascii_case("github")
    {
        "social"
    } else if value.eq_ignore_ascii_case("idc")
        || value.eq_ignore_ascii_case("builderid")
        || value.eq_ignore_ascii_case("builder-id")
        || value.eq_ignore_ascii_case("builder_id")
        || value.eq_ignore_ascii_case("iam")
        || value.eq_ignore_ascii_case("enterprise")
        || value.eq_ignore_ascii_case("identitycenter")
        || value.eq_ignore_ascii_case("identity-center")
        || value.eq_ignore_ascii_case("identity_center")
        || value.eq_ignore_ascii_case("aws-sso")
        || value.eq_ignore_ascii_case("aws_sso")
    {
        "idc"
    } else if value.eq_ignore_ascii_case("external-idp")
        || value.eq_ignore_ascii_case("external_idp")
        || value.eq_ignore_ascii_case("externalidp")
        || value.eq_ignore_ascii_case("azuread")
        || value.eq_ignore_ascii_case("azure")
        || value.eq_ignore_ascii_case("entra")
        || value.eq_ignore_ascii_case("entra-id")
        || value.eq_ignore_ascii_case("entra_id")
        || value.eq_ignore_ascii_case("microsoft")
        || value.eq_ignore_ascii_case("m365")
        || value.eq_ignore_ascii_case("office365")
        || value.eq_ignore_ascii_case("external")
    {
        "external_idp"
    } else if value.eq_ignore_ascii_case("api_key")
        || value.eq_ignore_ascii_case("apikey")
        || value.eq_ignore_ascii_case("api-key")
    {
        "api_key"
    } else {
        value
    }
}

/// 凭据配置（支持单对象或数组格式）
///
/// 自动识别配置文件格式：
/// - 单对象格式（单凭据配置输入）
/// - 数组格式（多凭据配置）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CredentialsConfig {
    /// 单个凭据配置输入
    Single(KiroCredentials),
    /// 多凭据数组
    Multiple(Vec<KiroCredentials>),
}

impl CredentialsConfig {
    /// 从文件加载凭据配置
    ///
    /// - 如果文件不存在，返回空数组
    /// - 如果文件内容为空，返回空数组
    /// - 支持单对象或数组格式
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();

        // 文件不存在时返回空数组
        if !path.exists() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let content = fs::read_to_string(path)?;

        // 文件为空时返回空数组
        if content.trim().is_empty() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// 转换为按优先级排序的凭据列表
    pub fn into_sorted_credentials(self) -> Vec<KiroCredentials> {
        match self {
            CredentialsConfig::Single(mut cred) => {
                cred.canonicalize_auth_method();
                vec![cred]
            }
            CredentialsConfig::Multiple(mut creds) => {
                // 按优先级排序（数字越小优先级越高）
                creds.sort_by_key(|c| c.priority);
                for cred in &mut creds {
                    cred.canonicalize_auth_method();
                }
                creds
            }
        }
    }

    /// 判断是否为多凭据格式（数组格式）
    pub fn is_multiple(&self) -> bool {
        matches!(self, CredentialsConfig::Multiple(_))
    }
}

impl KiroCredentials {
    /// 特殊值：显式不使用代理
    pub const PROXY_DIRECT: &'static str = "direct";

    pub(crate) fn canonical_auth_method_name(value: &str) -> &str {
        canonicalize_auth_method_value(value)
    }

    pub fn canonical_auth_method(&self) -> Option<&str> {
        self.auth_method
            .as_deref()
            .map(canonicalize_auth_method_value)
    }

    pub(crate) fn is_external_idp_provider_alias(value: &str) -> bool {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "external_idp"
                | "external-idp"
                | "externalidp"
                | "azuread"
                | "azure"
                | "azure ad"
                | "entra"
                | "entra-id"
                | "entra_id"
                | "entra id"
                | "microsoft"
                | "microsoft 365"
                | "m365"
                | "office365"
                | "external"
        )
    }

    pub(crate) fn provider_implies_external_idp(provider: Option<&str>) -> bool {
        provider
            .map(Self::is_external_idp_provider_alias)
            .unwrap_or(false)
    }

    pub(crate) fn normalize_provider_for_auth_method(
        provider: Option<String>,
        auth_method: &str,
    ) -> Option<String> {
        let provider = provider.map(|value| value.trim().to_string());
        match canonicalize_auth_method_value(auth_method) {
            "external_idp" => Some(
                provider
                    .as_deref()
                    .filter(|value| !Self::is_external_idp_provider_alias(value))
                    .map(str::to_string)
                    .unwrap_or_else(|| "AzureAD".to_string()),
            ),
            "social" => provider.map(|value| match value.to_ascii_lowercase().as_str() {
                "github" => "GitHub".to_string(),
                "google" => "Google".to_string(),
                _ => value,
            }),
            "idc" => provider.map(|value| match value.to_ascii_lowercase().as_str() {
                "builderid" | "builder-id" | "builder id" => "BuilderId".to_string(),
                "enterprise" => "Enterprise".to_string(),
                _ => value,
            }),
            _ => provider,
        }
    }

    pub fn is_valid_profile_arn(value: &str) -> bool {
        let value = value.trim();
        let parts: Vec<&str> = value.splitn(4, ':').collect();
        parts.len() >= 3
            && parts[0] == "arn"
            && parts[2] == "codewhisperer"
            && value.contains(":profile/")
    }

    pub fn clean_profile_arn(value: Option<String>) -> Option<String> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|arn| Self::is_valid_profile_arn(arn))
            .map(str::to_string)
    }

    pub fn normalize_profile_arn(&mut self) -> bool {
        let original = self.profile_arn.take();
        let cleaned = Self::clean_profile_arn(original.clone());
        let changed = original != cleaned;
        self.profile_arn = cleaned;
        changed
    }

    pub fn profile_arn_region_from_value(value: &str) -> Option<&str> {
        let parts: Vec<&str> = value.splitn(6, ':').collect();
        if parts.len() < 6 || parts[0] != "arn" || parts[2] != "codewhisperer" {
            return None;
        }
        let region = parts[3].trim();
        (!region.is_empty()).then_some(region)
    }

    /// 获取默认凭据文件路径
    pub fn default_credentials_path() -> &'static str {
        "credentials.json"
    }

    /// 获取有效的认证区域（用于令牌刷新）
    /// 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    pub fn effective_auth_region<'a>(&'a self, config: &'a Config) -> &'a str {
        self.auth_region
            .as_deref()
            .or(self.region.as_deref())
            .unwrap_or(config.effective_auth_region())
    }

    /// 获取有效的 API 区域（用于 API 请求）
    /// 优先级：凭据.api_region > config.api_region > config.region
    pub fn effective_api_region<'a>(&'a self, config: &'a Config) -> &'a str {
        let region = self
            .configured_api_region(config)
            .or_else(|| trimmed_region(&config.region))
            .unwrap_or(DEFAULT_Q_TRANSPORT_REGION);
        normalize_q_transport_region(region)
    }

    pub fn configured_api_region<'a>(&'a self, config: &'a Config) -> Option<&'a str> {
        self.api_region
            .as_deref()
            .and_then(trimmed_region)
            .or_else(|| config.api_region.as_deref().and_then(trimmed_region))
    }

    pub fn profile_arn_region(&self) -> Option<&str> {
        let profile_arn = self.profile_arn_trimmed()?;
        Self::profile_arn_region_from_value(profile_arn)
    }

    pub fn management_profile_arn(&self) -> Option<&str> {
        self.profile_arn_trimmed()
    }

    /// 获取 Kiro/Q data-plane region。
    ///
    /// 优先使用 profileArn 内的 region，因为 auth/OIDC region 可能与真实 profile region 不同。
    pub fn effective_kiro_api_region<'a>(&'a self, config: &'a Config) -> &'a str {
        match self.profile_arn_region() {
            Some(region) if is_known_bad_q_transport_region(region) => self
                .configured_api_region(config)
                .map(normalize_q_transport_region)
                .unwrap_or(DEFAULT_Q_TRANSPORT_REGION),
            Some(region) => region,
            None => self.effective_api_region(config),
        }
    }

    /// 获取有效的代理配置
    /// 优先级：凭据代理 > 全局代理 > 无代理
    /// 特殊值 "direct" 表示显式不使用代理（即使全局配置了代理）
    pub fn effective_proxy(&self, global_proxy: Option<&ProxyConfig>) -> Option<ProxyConfig> {
        match self.proxy_url.as_deref() {
            Some(url) if url.eq_ignore_ascii_case(Self::PROXY_DIRECT) => None,
            Some(url) => {
                let mut proxy = ProxyConfig::new(url);
                if let (Some(username), Some(password)) =
                    (&self.proxy_username, &self.proxy_password)
                {
                    proxy = proxy.with_auth(username, password);
                }
                Some(proxy)
            }
            None => global_proxy.cloned(),
        }
    }

    pub fn canonicalize_auth_method(&mut self) {
        let auth_method = match &self.auth_method {
            Some(m) => m,
            None => return,
        };

        let canonical = canonicalize_auth_method_value(auth_method);
        if canonical != auth_method {
            self.auth_method = Some(canonical.to_string());
        }
    }

    pub fn apply_allow_overage_import_hint(&mut self) -> bool {
        if !self.meta.allow_overage_import {
            return false;
        }
        if self
            .meta
            .overage_status
            .as_deref()
            .map(str::trim)
            .filter(|status| !status.is_empty())
            .is_none()
        {
            self.meta.overage_status = Some("ENABLED".to_string());
        }
        self.meta.allow_overage_import = false;
        true
    }

    /// 检查凭据是否支持 Opus 模型
    ///
    /// Free 订阅不支持 Opus 模型，需要 PRO 或更高等级订阅
    pub fn supports_opus(&self) -> bool {
        match &self.meta.subscription_title {
            Some(title) => {
                let title_upper = title.to_uppercase();
                // 如果包含 FREE，则不支持 Opus
                !title_upper.contains("FREE")
            }
            // 如果还没有获取订阅信息，暂时允许（首次使用时会获取）
            None => true,
        }
    }

    /// 解析当前凭据生效的 endpoint 名称
    ///
    /// 优先级：凭据级 `endpoint` 字段 > 入参 `default_endpoint`（来自 `Config::default_endpoint`） > 兜底 "ide"
    ///
    /// 与调度层 endpoint 解析保持一致，避免同一凭据在刷新与请求阶段走不同端点。
    pub fn effective_endpoint_name<'a>(&'a self, default_endpoint: Option<&'a str>) -> &'a str {
        self.endpoint
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(default_endpoint)
            .unwrap_or("ide")
    }

    pub fn profile_arn_trimmed(&self) -> Option<&str> {
        self.profile_arn
            .as_deref()
            .map(str::trim)
            .filter(|arn| Self::is_valid_profile_arn(arn))
    }

    /// 检查是否为 API 密钥凭据
    ///
    /// API 密钥凭据直接使用 apiKey 作为 Bearer 令牌，无需 refreshToken
    pub fn is_api_key_credential(&self) -> bool {
        self.api_key.is_some()
            || self
                .auth_method
                .as_deref()
                .map(|m| canonicalize_auth_method_value(m).eq_ignore_ascii_case("api_key"))
                .unwrap_or(false)
    }

    pub fn is_aws_sso_oidc_credential(&self) -> bool {
        self.auth_method
            .as_deref()
            .map(|m| canonicalize_auth_method_value(m).eq_ignore_ascii_case("idc"))
            .unwrap_or(false)
            || (self.client_id.is_some() && self.client_secret.is_some())
    }

    pub fn is_external_idp_credential(&self) -> bool {
        self.auth_method
            .as_deref()
            .map(|m| canonicalize_auth_method_value(m).eq_ignore_ascii_case("external_idp"))
            .unwrap_or(false)
    }

    pub fn is_enterprise_idc_credential(&self) -> bool {
        self.is_aws_sso_oidc_credential()
            && self
                .provider
                .as_deref()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("Enterprise"))
    }
}

#[cfg(test)]
impl KiroCredentials {
    fn from_json(json_string: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_string)
    }

    fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;

    #[allow(clippy::field_reassign_with_default)]
    fn config_with_regions(region: Option<&str>, api_region: Option<&str>) -> Config {
        let mut config = Config::default();
        if let Some(region) = region {
            config.region = region.to_string();
        }
        config.api_region = api_region.map(str::to_string);
        config
    }

    #[test]
    fn test_from_json() {
        let json = r#"{
            "accessToken": "test_token",
            "refreshToken": "test_refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2024-01-01T00:00:00Z",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2024-01-01T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("social".to_string()));
    }

    #[test]
    fn test_from_json_with_unknown_keys() {
        let json = r#"{
            "accessToken": "test_token",
            "unknownField": "should be ignored"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
    }

    #[test]
    fn test_api_key_serializes_canonical_and_reads_api_key_alias() {
        let alias_json = r#"{
            "authMethod": "api_key",
            "kiroApiKey": "ksk_alias"
        }"#;

        let creds = KiroCredentials::from_json(alias_json).unwrap();
        assert_eq!(creds.api_key.as_deref(), Some("ksk_alias"));

        let value = serde_json::to_value(&creds).unwrap();
        assert_eq!(
            value.get("apiKey").and_then(serde_json::Value::as_str),
            Some("ksk_alias")
        );
        assert!(value.get("kiroApiKey").is_none());
    }

    #[test]
    fn test_reference_credential_metadata_roundtrip() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "authMethod": "idc",
            "provider": "Enterprise",
            "userId": "user-1",
            "startUrl": "https://d-123.awsapps.com/start",
            "clientIdHash": "hash-1",
            "idToken": "id-token-1",
            "ssoSessionId": "session-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.provider.as_deref(), Some("Enterprise"));
        assert_eq!(creds.user_id.as_deref(), Some("user-1"));
        assert_eq!(
            creds.start_url.as_deref(),
            Some("https://d-123.awsapps.com/start")
        );
        assert_eq!(creds.client_id_hash.as_deref(), Some("hash-1"));
        assert_eq!(creds.id_token.as_deref(), Some("id-token-1"));
        assert_eq!(creds.sso_session_id.as_deref(), Some("session-1"));

        let output = creds.to_pretty_json().unwrap();
        assert!(output.contains("clientIdHash"));
        assert!(output.contains("ssoSessionId"));
    }

    // AC-1：归组后 meta 字段必须保持顶层 flat camelCase 线格式，且双次往返 byte 一致。
    // 覆盖大 u64/i64（超 2^53）+ f64 + 字符串，防止 flatten 归组回归。
    #[test]
    fn meta_fields_stay_flat_and_roundtrip_byte_identical() {
        let wire = r#"{"refreshToken":"r","subscriptionType":"PRO_PLUS","subscriptionTitle":"KIRO PRO+","overageStatus":"ENABLED","usagePercent":42.5,"usageCurrent":10.0,"usageLimit":100.0,"totalTokens":9007199254740993,"lastUsedAt":1700000000123456789,"banTime":1700000000,"errorCount":3,"requestCount":7,"nickname":"nick","status":"active","sourceAccountId":"acct-1"}"#;

        let creds = KiroCredentials::from_json(wire).unwrap();
        // 归组字段落到 meta 子结构
        assert_eq!(creds.meta.subscription_type.as_deref(), Some("PRO_PLUS"));
        assert_eq!(creds.meta.total_tokens, Some(9_007_199_254_740_993));
        assert_eq!(creds.meta.last_used_at, Some(1_700_000_000_123_456_789));
        assert_eq!(creds.meta.usage_percent, Some(42.5));

        // 顶层键仍是 flat camelCase（flatten 未引入嵌套 "meta" 对象）
        let value = serde_json::to_value(&creds).unwrap();
        let obj = value.as_object().unwrap();
        assert!(obj.get("meta").is_none(), "flatten 不得产生嵌套 meta 键");
        assert_eq!(
            obj.get("subscriptionType").and_then(|v| v.as_str()),
            Some("PRO_PLUS")
        );
        assert_eq!(
            obj.get("totalTokens").and_then(|v| v.as_u64()),
            Some(9_007_199_254_740_993)
        );
        assert_eq!(
            obj.get("lastUsedAt").and_then(|v| v.as_i64()),
            Some(1_700_000_000_123_456_789)
        );

        // 双次往返 byte 一致
        let first = serde_json::to_string(&creds).unwrap();
        let reparsed = KiroCredentials::from_json(&first).unwrap();
        let second = serde_json::to_string(&reparsed).unwrap();
        assert_eq!(first, second, "meta 归组后往返必须 byte 一致");
    }

    #[test]
    fn test_to_json() {
        let creds = KiroCredentials {
            id: None,
            access_token: Some("token".to_string()),
            refresh_token: None,
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            provider: None,
            user_id: None,
            client_id: None,
            client_secret: None,
            priority: 0,
            weight: 0,
            concurrency: None,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            proxy_id: None,
            disabled: false,
            api_key: None,
            endpoint: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            start_url: None,
            client_id_hash: None,
            id_token: None,
            sso_session_id: None,
            ..Default::default()
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("accessToken"));
        assert!(json.contains("authMethod"));
        assert!(!json.contains("refreshToken"));
        // priority 为 0 时不序列化
        assert!(!json.contains("priority"));
    }

    #[test]
    fn test_default_credentials_path() {
        assert_eq!(
            KiroCredentials::default_credentials_path(),
            "credentials.json"
        );
    }

    #[test]
    fn test_priority_default() {
        let json = r#"{"refreshToken": "test"}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 0);
    }

    #[test]
    fn test_priority_explicit() {
        let json = r#"{"refreshToken": "test", "priority": 5}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 5);
    }

    #[test]
    fn test_credentials_config_single() {
        let json = r#"{"refreshToken": "test", "expiresAt": "2025-12-31T00:00:00Z"}"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Single(_)));
    }

    #[test]
    fn test_credentials_config_multiple() {
        let json = r#"[
            {"refreshToken": "test1", "priority": 1},
            {"refreshToken": "test2", "priority": 0}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Multiple(_)));
        assert_eq!(config.into_sorted_credentials().len(), 2);
    }

    #[test]
    fn test_credentials_config_priority_sorting() {
        let json = r#"[
            {"refreshToken": "t1", "priority": 2},
            {"refreshToken": "t2", "priority": 0},
            {"refreshToken": "t3", "priority": 1}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        // 验证按优先级排序
        assert_eq!(list[0].refresh_token, Some("t2".to_string())); // priority 0
        assert_eq!(list[1].refresh_token, Some("t3".to_string())); // priority 1
        assert_eq!(list[2].refresh_token, Some("t1".to_string())); // priority 2
    }

    #[test]
    fn allow_overage_import_hint_applies_to_overage_status() {
        let mut creds =
            KiroCredentials::from_json(r#"{"refreshToken":"test","allowOverage":true}"#).unwrap();

        assert!(creds.apply_allow_overage_import_hint());
        assert_eq!(creds.meta.overage_status.as_deref(), Some("ENABLED"));
        assert!(!creds.meta.allow_overage_import);
        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("overageStatus"));
        assert!(!json.contains("allowOverage"));

        let mut preset = KiroCredentials::from_json(
            r#"{"refreshToken":"test","allowOverage":true,"overageStatus":"DISABLED"}"#,
        )
        .unwrap();
        assert!(preset.apply_allow_overage_import_hint());
        assert_eq!(preset.meta.overage_status.as_deref(), Some("DISABLED"));
        assert!(!preset.meta.allow_overage_import);
    }

    // ============ 区域字段测试 ============

    #[test]
    fn test_region_field_parsing() {
        // 测试解析包含 region 字段的 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, Some("us-east-1".to_string()));
    }

    #[test]
    fn test_region_field_missing_uses_default_none() {
        // 缺省 region 的单凭据配置输入应正常解析为 None。
        let json = r#"{
            "refreshToken": "test_refresh",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, None);
    }

    #[test]
    fn test_region_field_serialization() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            provider: None,
            user_id: None,
            client_id: None,
            client_secret: None,
            priority: 0,
            weight: 0,
            concurrency: None,
            region: Some("eu-west-1".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            proxy_id: None,
            disabled: false,
            api_key: None,
            endpoint: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            start_url: None,
            client_id_hash: None,
            id_token: None,
            sso_session_id: None,
            ..Default::default()
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("region"));
        assert!(json.contains("eu-west-1"));
    }

    #[test]
    fn test_region_field_none_not_serialized() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            provider: None,
            user_id: None,
            client_id: None,
            client_secret: None,
            priority: 0,
            weight: 0,
            concurrency: None,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            proxy_id: None,
            disabled: false,
            api_key: None,
            endpoint: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            start_url: None,
            client_id_hash: None,
            id_token: None,
            sso_session_id: None,
            ..Default::default()
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("region"));
    }

    // ============ MachineId 字段测试 ============

    #[test]
    fn test_machine_id_field_parsing() {
        let machine_id = "a".repeat(64);
        let json = format!(
            r#"{{
                "refreshToken": "test_refresh",
                "machineId": "{machine_id}"
            }}"#
        );

        let creds = KiroCredentials::from_json(&json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.machine_id, Some(machine_id));
    }

    #[test]
    fn test_machine_id_field_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = Some("b".repeat(64));

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("machineId"));
    }

    #[test]
    fn test_machine_id_field_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("machineId"));
    }

    #[test]
    fn test_multiple_credentials_with_different_regions() {
        // 测试多凭据场景下不同凭据使用各自的 region
        let json = r#"[
            {"refreshToken": "t1", "region": "us-east-1"},
            {"refreshToken": "t2", "region": "eu-west-1"},
            {"refreshToken": "t3"}
        ]"#;

        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        assert_eq!(list[0].region, Some("us-east-1".to_string()));
        assert_eq!(list[1].region, Some("eu-west-1".to_string()));
        assert_eq!(list[2].region, None);
    }

    #[test]
    fn test_region_field_with_all_fields() {
        // 测试包含所有字段的完整 JSON
        let json = r#"{
            "id": 1,
            "accessToken": "access",
            "refreshToken": "refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2025-12-31T00:00:00Z",
            "authMethod": "idc",
            "clientId": "client123",
            "clientSecret": "secret456",
            "priority": 5,
            "region": "ap-northeast-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.id, Some(1));
        assert_eq!(creds.access_token, Some("access".to_string()));
        assert_eq!(creds.refresh_token, Some("refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2025-12-31T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("idc".to_string()));
        assert_eq!(creds.client_id, Some("client123".to_string()));
        assert_eq!(creds.client_secret, Some("secret456".to_string()));
        assert_eq!(creds.priority, 5);
        assert_eq!(creds.region, Some("ap-northeast-1".to_string()));
    }

    #[test]
    fn test_profile_arn_trimmed_ignores_blank_cached_value() {
        let mut creds = KiroCredentials::default();
        creds.profile_arn = Some(" arn:aws:codewhisperer:profile/test ".to_string());
        assert_eq!(
            creds.profile_arn_trimmed(),
            Some("arn:aws:codewhisperer:profile/test")
        );

        creds.profile_arn = Some("   ".to_string());
        assert_eq!(creds.profile_arn_trimmed(), None);
    }

    #[test]
    fn test_profile_arn_trimmed_ignores_non_arn_values() {
        let mut creds = KiroCredentials::default();
        creds.profile_arn = Some("e3438419-4424-4e57-8990-ef76bd749a44".to_string());

        assert_eq!(creds.profile_arn_trimmed(), None);
    }

    #[test]
    fn test_clean_profile_arn_trims_and_rejects_non_arn_values() {
        assert_eq!(
            KiroCredentials::clean_profile_arn(Some(
                " arn:aws:codewhisperer:profile/test ".to_string()
            ))
            .as_deref(),
            Some("arn:aws:codewhisperer:profile/test")
        );
        assert_eq!(
            KiroCredentials::clean_profile_arn(Some(
                "e3438419-4424-4e57-8990-ef76bd749a44".to_string()
            )),
            None
        );
        assert_eq!(
            KiroCredentials::clean_profile_arn(Some(
                "arn:aws:iam::123:profile/not-kiro".to_string()
            )),
            None
        );
    }

    #[test]
    fn test_normalize_profile_arn_removes_microsoft_uuid_profile_id() {
        let mut creds = KiroCredentials {
            profile_arn: Some("e3438419-4424-4e57-8990-ef76bd749a44".to_string()),
            ..Default::default()
        };

        assert!(creds.normalize_profile_arn());
        assert_eq!(creds.profile_arn, None);
    }

    #[test]
    fn test_management_profile_arn_only_uses_cached_valid_arn() {
        let mut builder = KiroCredentials {
            auth_method: Some("idc".to_string()),
            provider: Some("BuilderId".to_string()),
            ..Default::default()
        };
        assert_eq!(builder.management_profile_arn(), None);

        builder.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:123:profile/custom".to_string());
        assert_eq!(
            builder.management_profile_arn(),
            Some("arn:aws:codewhisperer:eu-central-1:123:profile/custom")
        );

        let social = KiroCredentials {
            auth_method: Some("social".to_string()),
            provider: Some("GitHub".to_string()),
            ..Default::default()
        };
        assert_eq!(social.management_profile_arn(), None);
    }

    #[test]
    fn test_management_profile_arn_keeps_enterprise_cached_arn() {
        let enterprise = KiroCredentials {
            auth_method: Some("idc".to_string()),
            provider: Some("Enterprise".to_string()),
            profile_arn: Some("arn:aws:codewhisperer:us-east-1:123:profile/ignored".to_string()),
            ..Default::default()
        };

        assert_eq!(
            enterprise.management_profile_arn(),
            Some("arn:aws:codewhisperer:us-east-1:123:profile/ignored")
        );
    }

    #[test]
    fn test_profile_arn_region_parses_codewhisperer_arn() {
        let mut creds = KiroCredentials::default();
        creds.profile_arn =
            Some(" arn:aws:codewhisperer:eu-central-1:123456789012:profile/test ".to_string());

        assert_eq!(creds.profile_arn_region(), Some("eu-central-1"));
    }

    #[test]
    fn test_effective_kiro_api_region_prefers_profile_arn_region() {
        let mut config = Config::default();
        config.region = "us-east-1".to_string();
        config.api_region = Some("us-west-2".to_string());

        let mut creds = KiroCredentials::default();
        creds.api_region = Some("ap-southeast-1".to_string());
        creds.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:123456789012:profile/test".to_string());

        assert_eq!(creds.effective_api_region(&config), "ap-southeast-1");
        assert_eq!(creds.effective_kiro_api_region(&config), "eu-central-1");
    }

    #[test]
    fn test_effective_kiro_api_region_falls_back_to_existing_api_region_rule() {
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());

        assert_eq!(creds.effective_kiro_api_region(&config), "config-region");
    }

    #[test]
    fn test_effective_kiro_api_region_repairs_known_bad_profile_with_credential_override() {
        let config = config_with_regions(None, Some("eu-central-1"));
        let creds = KiroCredentials {
            api_region: Some(" us-west-2 ".to_string()),
            profile_arn: Some(
                "arn:aws:codewhisperer:eu-north-1:123456789012:profile/test".to_string(),
            ),
            ..Default::default()
        };

        assert_eq!(creds.effective_kiro_api_region(&config), "us-west-2");
    }

    #[test]
    fn test_effective_kiro_api_region_repairs_known_bad_profile_with_config_override() {
        let config = config_with_regions(None, Some(" eu-central-1 "));
        let creds = KiroCredentials {
            profile_arn: Some(
                "arn:aws:codewhisperer:EU-NORTH-1:123456789012:profile/test".to_string(),
            ),
            ..Default::default()
        };

        assert_eq!(creds.effective_kiro_api_region(&config), "eu-central-1");
    }

    #[test]
    fn test_effective_kiro_api_region_defaults_known_bad_profile_to_us_east_1() {
        let config = config_with_regions(Some("us-west-2"), None);
        let creds = KiroCredentials {
            profile_arn: Some(
                "arn:aws:codewhisperer:eu-north-1:123456789012:profile/test".to_string(),
            ),
            ..Default::default()
        };

        assert_eq!(
            creds.effective_kiro_api_region(&config),
            DEFAULT_Q_TRANSPORT_REGION
        );
    }

    #[test]
    fn test_region_roundtrip() {
        // 测试序列化和反序列化的往返一致性
        let original = KiroCredentials {
            id: Some(42),
            access_token: Some("token".to_string()),
            refresh_token: Some("refresh".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            provider: None,
            user_id: None,
            client_id: None,
            client_secret: None,
            priority: 3,
            weight: 0,
            concurrency: None,
            region: Some("us-west-2".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: Some("c".repeat(64)),
            email: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            proxy_id: None,
            disabled: false,
            api_key: None,
            endpoint: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            start_url: None,
            client_id_hash: None,
            id_token: None,
            sso_session_id: None,
            ..Default::default()
        };

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.access_token, original.access_token);
        assert_eq!(parsed.refresh_token, original.refresh_token);
        assert_eq!(parsed.priority, original.priority);
        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.machine_id, original.machine_id);
    }

    // ============ auth_region / api_region 字段测试 ============

    #[test]
    fn test_auth_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "authRegion": "eu-central-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.auth_region, Some("eu-central-1".to_string()));
        assert_eq!(creds.api_region, None);
    }

    #[test]
    fn test_api_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "apiRegion": "ap-southeast-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.api_region, Some("ap-southeast-1".to_string()));
        assert_eq!(creds.auth_region, None);
    }

    #[test]
    fn test_auth_api_region_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = Some("eu-west-1".to_string());
        creds.api_region = Some("us-west-2".to_string());

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("authRegion"));
        assert!(json.contains("eu-west-1"));
        assert!(json.contains("apiRegion"));
        assert!(json.contains("us-west-2"));
    }

    #[test]
    fn test_auth_api_region_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = None;
        creds.api_region = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("authRegion"));
        assert!(!json.contains("apiRegion"));
    }

    #[test]
    fn test_auth_api_region_roundtrip() {
        let mut original = KiroCredentials::default();
        original.refresh_token = Some("refresh".to_string());
        original.region = Some("us-east-1".to_string());
        original.auth_region = Some("eu-west-1".to_string());
        original.api_region = Some("ap-northeast-1".to_string());

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.auth_region, original.auth_region);
        assert_eq!(parsed.api_region, original.api_region);
    }

    #[test]
    fn test_missing_auth_api_region_defaults_to_none() {
        // 缺省 authRegion/apiRegion 的单凭据配置输入应正常解析为 None。
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.region, Some("us-east-1".to_string()));
        assert_eq!(creds.auth_region, None);
        assert_eq!(creds.api_region, None);
    }

    // ============ effective_auth_region / effective_api_region 优先级测试 ============

    #[test]
    fn test_effective_auth_region_credential_auth_region_highest() {
        // 凭据.auth_region > 凭据.region > config.auth_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());
        creds.auth_region = Some("cred-auth-region".to_string());

        assert_eq!(creds.effective_auth_region(&config), "cred-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_credential_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());
        // auth_region 未设置

        assert_eq!(creds.effective_auth_region(&config), "cred-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_auth_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let creds = KiroCredentials::default();
        // auth_region 和 region 均未设置

        assert_eq!(creds.effective_auth_region(&config), "config-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        // config.auth_region 未设置

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_auth_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_credential_api_region_highest() {
        // 凭据.api_region > config.api_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.api_region = Some("cred-api-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "cred-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_api_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_skips_blank_overrides() {
        let config = config_with_regions(Some(" config-region "), Some("  "));
        let creds = KiroCredentials {
            api_region: Some("\t".to_string()),
            ..Default::default()
        };

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_normalizes_known_bad_region() {
        let config = config_with_regions(Some("config-region"), None);
        let creds = KiroCredentials {
            api_region: Some(" EU-NORTH-1 ".to_string()),
            ..Default::default()
        };

        assert_eq!(
            creds.effective_api_region(&config),
            DEFAULT_Q_TRANSPORT_REGION
        );
    }

    #[test]
    fn test_effective_api_region_ignores_credential_region() {
        // 凭据.region 不参与 api_region 的回退链
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut creds = KiroCredentials::default();
        creds.auth_region = Some("auth-only".to_string());
        creds.api_region = Some("api-only".to_string());

        assert_eq!(creds.effective_auth_region(&config), "auth-only");
        assert_eq!(creds.effective_api_region(&config), "api-only");
    }

    // ============ 凭据级代理优先级测试 ============

    #[test]
    fn test_effective_proxy_credential_overrides_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("socks5://cred:1080".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, Some(ProxyConfig::new("socks5://cred:1080")));
    }

    #[test]
    fn test_effective_proxy_credential_with_auth() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("http://proxy:3128".to_string());
        creds.proxy_username = Some("user".to_string());
        creds.proxy_password = Some("pass".to_string());

        let result = creds.effective_proxy(Some(&global));
        let expected = ProxyConfig::new("http://proxy:3128").with_auth("user", "pass");
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_effective_proxy_direct_bypasses_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("direct".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_direct_case_insensitive() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("DIRECT".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_fallback_to_global() {
        let global = ProxyConfig::new("http://global:8080");
        let creds = KiroCredentials::default();

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, Some(ProxyConfig::new("http://global:8080")));
    }

    #[test]
    fn test_effective_proxy_none_when_no_proxy() {
        let creds = KiroCredentials::default();
        let result = creds.effective_proxy(None);
        assert_eq!(result, None);
    }

    #[test]
    fn test_external_idp_credential_detection_is_case_insensitive() {
        let mut creds = KiroCredentials::default();
        creds.auth_method = Some("EXTERNAL_IDP".to_string());
        assert!(creds.is_external_idp_credential());

        creds.auth_method = Some("external-idp".to_string());
        assert!(creds.is_external_idp_credential());

        creds.auth_method = Some("Microsoft".to_string());
        creds.canonicalize_auth_method();
        assert_eq!(creds.auth_method.as_deref(), Some("external_idp"));
        assert!(creds.is_external_idp_credential());

        creds.auth_method = Some("AzureAD".to_string());
        assert!(creds.is_external_idp_credential());

        creds.auth_method = Some("idc".to_string());
        assert!(!creds.is_external_idp_credential());
    }

    #[test]
    fn test_auth_method_aliases_canonicalize_in_core_model() {
        let cases = [
            ("Social", "social"),
            ("GitHub", "social"),
            ("Google", "social"),
            ("IdC", "idc"),
            ("builderid", "idc"),
            ("builder-id", "idc"),
            ("builder_id", "idc"),
            ("iam", "idc"),
            ("enterprise", "idc"),
            ("external-idp", "external_idp"),
            ("Microsoft", "external_idp"),
            ("AzureAD", "external_idp"),
            ("api-key", "api_key"),
            ("apikey", "api_key"),
        ];

        for (input, expected) in cases {
            let mut creds = KiroCredentials {
                auth_method: Some(input.to_string()),
                ..Default::default()
            };
            creds.canonicalize_auth_method();
            assert_eq!(creds.auth_method.as_deref(), Some(expected), "{input}");
        }
    }

    #[test]
    fn test_external_idp_provider_aliases_normalize_in_core_model() {
        for provider in [
            None,
            Some("Microsoft"),
            Some("Azure AD"),
            Some("entra id"),
            Some("external-idp"),
        ] {
            assert_eq!(
                KiroCredentials::normalize_provider_for_auth_method(
                    provider.map(str::to_string),
                    "external_idp",
                )
                .as_deref(),
                Some("AzureAD"),
                "{provider:?}",
            );
        }

        assert_eq!(
            KiroCredentials::normalize_provider_for_auth_method(
                Some("CustomIdP".to_string()),
                "external_idp",
            )
            .as_deref(),
            Some("CustomIdP"),
        );
        assert_eq!(
            KiroCredentials::normalize_provider_for_auth_method(
                Some("Microsoft".to_string()),
                "social",
            )
            .as_deref(),
            Some("Microsoft"),
        );
        assert_eq!(
            KiroCredentials::normalize_provider_for_auth_method(
                Some("Github".to_string()),
                "social",
            )
            .as_deref(),
            Some("GitHub"),
        );
        assert_eq!(
            KiroCredentials::normalize_provider_for_auth_method(
                Some("builder-id".to_string()),
                "idc",
            )
            .as_deref(),
            Some("BuilderId"),
        );
        assert!(KiroCredentials::provider_implies_external_idp(Some(
            "Microsoft 365"
        )));
        assert!(!KiroCredentials::provider_implies_external_idp(Some(
            "BuilderId"
        )));
    }

    #[test]
    fn test_aws_sso_oidc_detection_uses_canonical_auth_method_aliases() {
        for auth_method in ["IdC", "builderid", "builder-id", "iam", "enterprise"] {
            let creds = KiroCredentials {
                auth_method: Some(auth_method.to_string()),
                ..Default::default()
            };
            assert!(creds.is_aws_sso_oidc_credential(), "{auth_method}");
        }

        let creds = KiroCredentials {
            auth_method: Some("social".to_string()),
            client_id: Some("client".to_string()),
            client_secret: Some("secret".to_string()),
            ..Default::default()
        };
        assert!(creds.is_aws_sso_oidc_credential());
    }
}
