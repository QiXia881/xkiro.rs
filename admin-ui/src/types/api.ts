// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  weight: number
  disabled: boolean
  failureCount: number
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  endpoint: string
  /** 当前可用 permit 数（剩余配额） */
  availablePermits: number
  /** 该凭据的最大 permit 数（单凭据并发上限） */
  maxPermits: number
  /** 该凭据自定义并发上限（null = 跟随全局 perCredentialConcurrency） */
  concurrency: number | null
}

// 全局配置响应
export interface GlobalConfigResponse {
  region: string
  promptCacheTtlSeconds: number
  promptCacheAccountingEnabled: boolean
  defaultEndpoint: string
  extractThinking: boolean
  /** 单凭据最大并发数（>=1） */
  perCredentialConcurrency: number
  /** 全局并发上限（0=不限） */
  globalConcurrency: number
  /** 凭据队列等待超时（秒），超时返回 503 */
  acquireWaitTimeoutSecs: number
  /** 是否启用周期余额刷新 */
  balanceRefreshEnabled: boolean
  /** 周期余额刷新间隔（秒，最小 180） */
  balanceRefreshIntervalSecs: number
  /** 周期余额刷新并发上限（1..=10） */
  balanceRefreshConcurrency: number
  /** session 亲和：true=同会话黏住同凭据；false=每条消息独立平摊 */
  sessionAffinityEnabled: boolean
  /** admin UI 隐私模式（邮箱脱敏展示） */
  privacyMode: boolean
  compression: CompressionConfigPayload
}

// 全局配置内嵌的压缩配置（与后端 CompressionConfigResponse 对齐）
export interface CompressionConfigPayload {
  maxRequestBodyBytes: number
}

// 更新全局配置请求（所有字段可选）
export interface UpdateGlobalConfigRequest {
  region?: string
  promptCacheTtlSeconds?: number
  promptCacheAccountingEnabled?: boolean
  defaultEndpoint?: string
  extractThinking?: boolean
  perCredentialConcurrency?: number
  globalConcurrency?: number
  acquireWaitTimeoutSecs?: number
  balanceRefreshEnabled?: boolean
  balanceRefreshIntervalSecs?: number
  balanceRefreshConcurrency?: number
  sessionAffinityEnabled?: boolean
  privacyMode?: boolean
  compression?: Partial<CompressionConfigPayload>
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  /** 超额上限（订阅可超额时 > 0） */
  overageCap: number
  /** 超额资格 OVERAGE_CAPABLE / OVERAGE_INCAPABLE */
  overageCapability?: string | null
  /** 远端开关 ENABLED / DISABLED */
  overageStatus?: string | null
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 设置单凭据并发上限请求
export interface SetConcurrencyRequest {
  /** null 表示跟随全局 perCredentialConcurrency */
  concurrency: number | null
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'external_idp' | 'api_key'
  clientId?: string
  clientSecret?: string
  priority?: number
  weight?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
  /** 自定义并发上限（null/省略 = 跟随全局） */
  concurrency?: number | null
}

export interface ImportKiroGoCredentialRequest {
  accessToken?: string
  refreshToken: string
  clientId?: string
  clientSecret?: string
  authMethod?: string
  provider?: string
  region?: string
  authRegion?: string
  apiRegion?: string
  tokenEndpoint?: string
  issuerUrl?: string
  scopes?: string
  startUrl?: string
  clientIdHash?: string
  idToken?: string
  ssoSessionId?: string
  priority?: number
  weight?: number
  concurrency?: number | null
  id?: string | number
  email?: string
  profileArn?: string
  userId?: string | null
  machineId?: string
  proxyURL?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  overageStatus?: string
  endpoint?: string
  enabled?: boolean
  disabled?: boolean
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

// ============ 运行时状态轻量端点（高频轮询）============

/** 单个凭据的运行时状态（仅内存快照字段，5s 轮询） */
export interface RuntimeStatsItem {
  id: number
  lastUsedAt: string | null
  availablePermits: number
  maxPermits: number
  disabled: boolean
  /** 余额快照（来自 disk cache + 后台周期刷新）；未命中则 undefined */
  balance?: RuntimeBalanceSnapshot
}

/** runtime-stats 内嵌的余额快照（字段子集对齐 BalanceResponse） */
export interface RuntimeBalanceSnapshot {
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  overageCap: number
  overageCapability: string | null
  overageStatus: string | null
}

/** 运行时状态响应 */
export interface RuntimeStatsResponse {
  credentials: RuntimeStatsItem[]
}

// ============ 批量刷新 Token 端点 ============

/** 批量刷新请求 */
export interface BatchRefreshRequest {
  ids: number[]
}

/** 单个凭据的刷新结果 */
export interface BatchRefreshResultItem {
  id: number
  success: boolean
  /** 失败原因（success=true 时不存在） */
  error?: string
}

/** 批量刷新响应 */
export interface BatchRefreshResponse {
  results: BatchRefreshResultItem[]
  successCount: number
  failureCount: number
}

/** 批量刷新余额单项结果（success=true 时 balance 存在；success=false 时 error 存在） */
export interface BatchRefreshBalanceResultItem {
  id: number
  success: boolean
  balance?: BalanceResponse
  error?: string
}

/** 批量刷新余额响应 */
export interface BatchRefreshBalanceResponse {
  results: BatchRefreshBalanceResultItem[]
  successCount: number
  failureCount: number
}

// ============ 缓存余额端点 ============

/** 单个凭据的缓存余额条目 */
export interface CachedBalanceItem {
  id: number
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  subscriptionTitle: string | null
  nextResetAt: number | null
  overageCap: number
  overageCapability?: string | null
  overageStatus?: string | null
  /** 缓存时间（Unix 毫秒） */
  cachedAt: number
  /** TTL 秒，cachedAt + ttlSecs*1000 后过期 */
  ttlSecs: number
}

/** GET /credentials/balances/cached 响应 */
export interface CachedBalancesResponse {
  balances: CachedBalanceItem[]
}

// ============ 系统提示注入 ============

/** Preset 来源 */
export type PresetSource = 'builtin' | 'user'

/** 系统提示注入位置 */
export type SystemPromptPosition = 'prepend' | 'append'

/** 单条 preset（builtin 不含 content；user 含完整 content） */
export interface PresetItem {
  id: string
  name: string
  description: string
  source: PresetSource
  enabled: boolean
  content?: string
}

/** GET /config/system-prompt 响应 */
export interface SystemPromptResponse {
  enabled: boolean
  position: SystemPromptPosition
  customContent: string | null
  presets: PresetItem[]
}

/** PUT /config/system-prompt 请求（所有字段可选） */
export interface UpdateSystemPromptRequest {
  enabled?: boolean
  position?: SystemPromptPosition
  /** "" 表示清空；省略表示不变；非空表示覆盖 */
  customContent?: string
  /** 全量替换启用列表 */
  enabledPresets?: string[]
}

/** POST /config/user-presets 请求 */
export interface UpsertUserPresetRequest {
  id: string
  name: string
  description?: string
  content: string
}

// ============ Social OAuth 登录 ============

export interface StartSocialLoginRequest {
  priority?: number
  email?: string
  proxyUrl?: string
  authEndpoint?: string
  provider: 'Google' | 'Github'
  mode?: 'manual' | 'helper'
}

export interface StartSocialLoginResponse {
  sessionId: string
  mode: 'manual' | 'helper'
  portalUrl?: string
  expiresAt: string
}

export type PollSocialLoginResponse =
  | { status: 'waiting' }
  | { status: 'success'; credentialId: number }
  | { status: 'expired' }
  | { status: 'error'; message: string }

export interface StartIdcLoginRequest {
  region: string
  startUrl?: string
  priority?: number
  email?: string
  proxyUrl?: string
}

export interface StartIdcLoginResponse {
  sessionId: string
  userCode: string
  verificationUri: string
  verificationUriComplete?: string
  expiresAt: string
  pollInterval: number
}

export interface StartIamSsoLoginResponse {
  sessionId: string
  authorizeUrl: string
  expiresIn: number
}

export interface CompleteIamSsoLoginResponse {
  success: boolean
  account?: {
    id: number
    email?: string
  }
}

export type PollIdcLoginResponse =
  | { status: 'pending' }
  | { status: 'success'; credentialId: number }
  | { status: 'expired' }

export interface StartBuilderIdLoginRequest {
  region?: string
  priority?: number
  email?: string
}

export interface StartBuilderIdLoginResponse {
  sessionId: string
  userCode: string
  verificationUri: string
  verificationUriComplete?: string
  pollInterval: number
  expiresIn: number
}

export type PollBuilderIdLoginResponse =
  | { status: 'pending'; pollInterval?: number }
  | { status: 'success'; credentialId: number; email?: string }
  | { status: 'expired' }
  | { status: 'error'; message: string }

export interface StartKiroSsoLoginResponse {
  sessionId: string
  signInUrl: string
  interval: number
}

export interface PollKiroSsoLoginResponse {
  success: boolean
  completed: boolean
  status?: 'pending'
  error?: string
  account?: {
    id: number
    email?: string
    authMethod?: string
  }
}

export interface CompleteKiroSsoLoginResponse {
  success: boolean
  status: 'pending' | 'redirect' | 'submitted' | 'expired' | 'error'
  redirectUrl?: string
  error?: string
}
