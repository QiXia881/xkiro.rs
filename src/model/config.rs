use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// 自定义系统提示词注入位置
///
/// - `Prepend`：插入到 system 数组最前（旧默认行为）
/// - `Append`：追加到 system 数组末尾（recency bias 权重最高，推荐用于 override）
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SystemPromptPosition {
    Prepend,
    Append,
}

impl Default for SystemPromptPosition {
    fn default() -> Self {
        Self::Append
    }
}

/// 用户自定义预设（与内置 `PRESETS` 并列，可在 Admin UI 中增删改）
///
/// id 必须全局唯一（含内置 id），仅允许 `[a-z0-9_-]`，长度 1-32。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserPreset {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub content: String,
}


/// 压缩配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompressionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub whitespace_compression: bool,
    #[serde(default = "default_thinking_strategy")]
    pub thinking_strategy: String,
    #[serde(default = "default_8000")]
    pub tool_result_max_chars: usize,
    #[serde(default = "default_80")]
    pub tool_result_head_lines: usize,
    #[serde(default = "default_40")]
    pub tool_result_tail_lines: usize,
    #[serde(default = "default_6000")]
    pub tool_use_input_max_chars: usize,
    #[serde(default = "default_4000")]
    pub tool_description_max_chars: usize,
    #[serde(default = "default_80_turns")]
    pub max_history_turns: usize,
    #[serde(default = "default_400k")]
    pub max_history_chars: usize,
    #[serde(default = "default_image_max_long_edge")]
    pub image_max_long_edge: u32,
    #[serde(default = "default_image_max_pixels_single")]
    pub image_max_pixels_single: u32,
    #[serde(default = "default_image_max_pixels_multi")]
    pub image_max_pixels_multi: u32,
    #[serde(default = "default_image_multi_threshold")]
    pub image_multi_threshold: usize,
    /// 图片压缩开关：关闭后透传原始图片（适用于上游已压缩的场景，如 TRAE 国际版）
    #[serde(default = "default_true")]
    pub image_compression_enabled: bool,
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            whitespace_compression: true,
            thinking_strategy: default_thinking_strategy(),
            tool_result_max_chars: default_8000(),
            tool_result_head_lines: default_80(),
            tool_result_tail_lines: default_40(),
            tool_use_input_max_chars: default_6000(),
            tool_description_max_chars: default_4000(),
            max_history_turns: default_80_turns(),
            max_history_chars: default_400k(),
            image_max_long_edge: default_image_max_long_edge(),
            image_max_pixels_single: default_image_max_pixels_single(),
            image_max_pixels_multi: default_image_max_pixels_multi(),
            image_multi_threshold: default_image_multi_threshold(),
            image_compression_enabled: true,
            max_request_body_bytes: default_max_request_body_bytes(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_thinking_strategy() -> String {
    "discard".to_string()
}
fn default_8000() -> usize {
    8000
}
fn default_80() -> usize {
    80
}
fn default_40() -> usize {
    40
}
fn default_6000() -> usize {
    6000
}
fn default_4000() -> usize {
    4000
}
fn default_80_turns() -> usize {
    80
}
fn default_400k() -> usize {
    400_000
}
fn default_image_max_long_edge() -> u32 {
    4000
}
fn default_image_max_pixels_single() -> u32 {
    4_000_000
}
fn default_image_max_pixels_multi() -> u32 {
    4_000_000
}
fn default_image_multi_threshold() -> usize {
    20
}
fn default_max_request_body_bytes() -> usize {
    4_718_592
}

/// 系统提示清洗配置
///
/// 镜像 KAM `gateway/prompt_filter.rs` 的 4 个开关 + 自定义规则。
/// 默认全 `false`、规则为空 —— 与未启用此模块时行为一致。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PromptFilterConfig {
    /// 检测 Claude Code 系统提示并整体替换为精简版
    #[serde(default)]
    pub filter_claude_code: bool,
    /// 删除 `--- SYSTEM PROMPT ---` / `--- END SYSTEM PROMPT ---` 边界标记
    #[serde(default)]
    pub filter_strip_boundaries: bool,
    /// 跳过 `# Environment` / `# auto memory` section 与单行噪音
    #[serde(default)]
    pub filter_env_noise: bool,
    /// 剥离 Claude Code 客户端注入的安全/沙箱限制段（content_safety、git_safety、
    /// safety_guardrails、executing_actions 等）。默认 false（opt-in）。
    #[serde(default)]
    pub filter_strip_restrictions: bool,
    /// 自定义过滤规则
    #[serde(default)]
    pub rules: Vec<PromptFilterRule>,
}

/// 自定义提示过滤规则
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFilterRule {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// "regex" | "lines-containing" | "contains"
    pub rule_type: String,
    /// 匹配模式（正则或子串）
    pub match_pattern: String,
    /// 替换内容（仅 regex 类型使用）
    #[serde(default)]
    pub replace: String,
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 压缩配置
    #[serde(default)]
    pub compression: CompressionConfig,

    /// 系统提示清洗配置（默认全关，向后兼容）
    #[serde(default)]
    pub prompt_filter: PromptFilterConfig,

    /// 系统提示注入：自由文本补充内容（None 表示无）
    ///
    /// 与 `enabled_presets` / `user_presets` 拼接后由 `system_prompt_position` 指定位置注入
    /// 到 system role。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,

    /// 系统提示注入总开关（默认 false：关闭注入）
    #[serde(default)]
    pub system_prompt_enabled: bool,

    /// 启用的预设 id 列表（混合内置 + 用户自定义）
    #[serde(default)]
    pub enabled_presets: Vec<String>,

    /// 用户自定义预设清单（与内置 `PRESETS` 并列）
    #[serde(default)]
    pub user_presets: Vec<UserPreset>,

    /// 系统提示拼接结果在 system role 中的插入位置（默认 Append）
    #[serde(default)]
    pub system_prompt_position: SystemPromptPosition,

    /// Prompt Cache TTL（秒），默认 300 秒
    #[serde(default = "default_prompt_cache_ttl_seconds")]
    pub prompt_cache_ttl_seconds: u64,

    /// 是否启用本地 Prompt Cache usage 记账，默认 true
    #[serde(default = "default_true")]
    pub prompt_cache_accounting_enabled: bool,

    /// 单凭据最大并发请求数（默认 1）
    ///
    /// 同一凭据在任意时刻最多同时承载多少个上游 API 调用。设为 1 表示
    /// 严格串行，避免 Kiro 上游对单账号的速率/并发风险；增大后允许同账号
    /// 并行，但上游限流命中概率上升。
    #[serde(default = "default_per_credential_concurrency")]
    pub per_credential_concurrency: usize,

    /// 全局最大并发请求数（默认 0，表示不限）
    ///
    /// 跨所有凭据的总并发上限。0 = 不启用全局限流；> 0 时优先在全局闸门排队，
    /// 通过后再到对应凭据的 per-credential 闸门排队。
    #[serde(default = "default_global_concurrency")]
    pub global_concurrency: usize,

    /// 排队等待 permit 的超时秒数（默认 60）
    ///
    /// 请求进入凭据队列后若在此时间内仍未获得 permit，将以 429 overloaded_error
    /// 返回，避免客户端无限等待。
    #[serde(default = "default_acquire_wait_timeout_secs")]
    pub acquire_wait_timeout_secs: u64,

    /// 是否启用周期余额/订阅刷新（默认 true）
    ///
    /// 关闭后停止后台周期拉取，调度仅用最后一次缓存值；启动预取不受影响。
    #[serde(default = "default_true")]
    pub balance_refresh_enabled: bool,

    /// 是否启用 tiktoken cl100k_base 精确 token 计数
    ///
    /// 默认关闭，使用启发式 (字符比例 + 分段校准) — 与 Claude 真实计数偏差较小
    /// 且零成本。开启后用 OpenAI BPE，CJK 文本结果会显著高于启发式；适合
    /// 接英文为主的 OpenAI-flavored 客户端，或对 token 上限要严格 lower bound。
    #[serde(default)]
    pub precise_token_counting: bool,

    /// 周期余额刷新间隔（秒，默认 300，最小 180）
    ///
    /// 反序列化与 setter 都会 clamp 到 >=180，避免对上游过频。
    #[serde(default = "default_balance_refresh_interval_secs")]
    pub balance_refresh_interval_secs: u64,

    /// 周期余额刷新并发上限（默认 10，范围 1..=10）
    ///
    /// 凭据数 > 此值时分批，上一批完成才进入下一批；每批内 Semaphore 限并发。
    #[serde(default = "default_balance_refresh_concurrency")]
    pub balance_refresh_concurrency: usize,

    /// 是否启用 session 亲和（同会话黏住同凭据，提升上游 prompt cache 命中率）
    ///
    /// false（默认）：每条消息独立走 rank 调度，多号天然平摊；上游 prompt cache 必失，
    /// 但客户端每次都带完整 history，模型不会"失忆"。
    /// true：同 session_id 黏住首条选中的凭据，长会话内不切号；只在该号 sema 满 / 禁用 / 模型不支持时回退 rank。
    #[serde(default)]
    pub session_affinity_enabled: bool,

    /// admin UI 隐私模式（邮箱等敏感字段脱敏显示）
    ///
    /// 仅前端展示开关，后端只做存储 / 跨浏览器同步。默认开启。
    #[serde(default = "default_true")]
    pub privacy_mode: bool,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "0.11.107".to_string()
}

fn default_system_version() -> String {
    const SYSTEM_VERSIONS: &[&str] = &["darwin#24.6.0", "win32#10.0.22631"];
    SYSTEM_VERSIONS[fastrand::usize(..SYSTEM_VERSIONS.len())].to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_extract_thinking() -> bool {
    true
}

fn default_prompt_cache_ttl_seconds() -> u64 {
    300
}

fn default_per_credential_concurrency() -> usize {
    1
}

fn default_global_concurrency() -> usize {
    0
}

fn default_acquire_wait_timeout_secs() -> u64 {
    60
}

/// 余额刷新最小间隔（秒），低于此值会被 clamp
pub const MIN_BALANCE_REFRESH_INTERVAL_SECS: u64 = 180;
/// 余额刷新并发硬上限
pub const MAX_BALANCE_REFRESH_CONCURRENCY: usize = 10;

fn default_balance_refresh_interval_secs() -> u64 {
    300
}

fn default_balance_refresh_concurrency() -> usize {
    MAX_BALANCE_REFRESH_CONCURRENCY
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            extract_thinking: default_extract_thinking(),
            default_endpoint: default_endpoint(),
            endpoints: HashMap::new(),
            compression: CompressionConfig::default(),
            prompt_filter: PromptFilterConfig::default(),
            system_prompt: None,
            system_prompt_enabled: false,
            enabled_presets: Vec::new(),
            user_presets: Vec::new(),
            system_prompt_position: SystemPromptPosition::default(),
            prompt_cache_ttl_seconds: default_prompt_cache_ttl_seconds(),
            prompt_cache_accounting_enabled: default_true(),
            per_credential_concurrency: default_per_credential_concurrency(),
            global_concurrency: default_global_concurrency(),
            acquire_wait_timeout_secs: default_acquire_wait_timeout_secs(),
            balance_refresh_enabled: true,
            balance_refresh_interval_secs: default_balance_refresh_interval_secs(),
            balance_refresh_concurrency: default_balance_refresh_concurrency(),
            session_affinity_enabled: false,
            privacy_mode: true,
            precise_token_counting: false,
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());
        config.clamp_balance_refresh();
        Ok(config)
    }

    /// 把余额刷新参数 clamp 到合法范围
    /// (interval >= MIN_BALANCE_REFRESH_INTERVAL_SECS, concurrency in 1..=MAX)
    pub fn clamp_balance_refresh(&mut self) {
        if self.balance_refresh_interval_secs < MIN_BALANCE_REFRESH_INTERVAL_SECS {
            self.balance_refresh_interval_secs = MIN_BALANCE_REFRESH_INTERVAL_SECS;
        }
        if self.balance_refresh_concurrency == 0 {
            self.balance_refresh_concurrency = 1;
        } else if self.balance_refresh_concurrency > MAX_BALANCE_REFRESH_CONCURRENCY {
            self.balance_refresh_concurrency = MAX_BALANCE_REFRESH_CONCURRENCY;
        }
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}
