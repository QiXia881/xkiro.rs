import type { CredentialMaterialFlags, OptionalCredentialMaterialFlags } from '@/lib/credential-material'
import {
  CREDENTIAL_SOURCE_FORMATS,
  type CredentialImportAction,
  type CredentialImportMetadataFields,
  CredentialStatusMetadataFields,
  OptionalCredentialMetadataFields,
} from '@/lib/credential-metadata'

export const REQUEST_LOG_STATUSES = {
  success: 'success',
  error: 'error',
} as const

export const REQUEST_LOG_ERROR_TYPES = {
  quota: 'quota',
  overage: 'overage',
  suspended: 'suspended',
  auth: 'auth',
  profile: 'profile',
  unknown: 'unknown',
} as const

export type RequestLogStatus = typeof REQUEST_LOG_STATUSES[keyof typeof REQUEST_LOG_STATUSES]
export type RequestLogErrorType = typeof REQUEST_LOG_ERROR_TYPES[keyof typeof REQUEST_LOG_ERROR_TYPES]

// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem extends CredentialMaterialFlags, CredentialStatusMetadataFields {
  id: number
  priority: number
  weight: number
  disabled: boolean
  failureCount: number
  expiresAt: string | null
  authMethod: string | null
  provider?: string
  userId?: string
  sourceAccountId?: string
  label?: string
  status?: string
  addedAt?: string
  nickname?: string
  subscriptionType?: string
  subscriptionTitle?: string
  daysRemaining?: number
  overageStatus?: string
  overageCapability?: string
  overageCap?: number
  overageRate?: number
  currentOverages?: number
  overageCheckedAt?: number
  banStatus?: string
  banReason?: string
  banTime?: number
  usageCurrent?: number
  usageLimit?: number
  usagePercent?: number
  nextResetDate?: string
  lastRefresh?: number
  trialUsageCurrent?: number
  trialUsageLimit?: number
  trialUsagePercent?: number
  trialStatus?: string
  trialExpiresAt?: number
  requestCount?: number
  errorCount?: number
  totalTokens?: number
  totalCredits?: number
  lastUsed?: number
  createdAt?: number
  tags?: unknown
  email?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  proxyId: number | null
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
  /** 调度亲和：true=session/API key 黏住同凭据；false=每条消息独立平摊 */
  sessionAffinityEnabled: boolean
  /** admin UI 隐私模式（邮箱脱敏展示） */
  privacyMode: boolean
  compression: CompressionConfigPayload
}

// 全局配置内嵌的压缩配置（与后端 CompressionConfigResponse 对齐）
export interface CompressionConfigPayload {
  maxRequestBodyBytes: number
}

export type CompressionConfig = CompressionConfigPayload

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

export interface AccessSettings {
  apiKey?: string | null
  requireApiKey: boolean
  port: number
  host: string
  allowOverUsage: boolean
}

export interface UpdateAccessSettingsRequest {
  apiKey?: string
  requireApiKey?: boolean
  password?: string
  allowOverUsage?: boolean
}

export type CredentialMachineIdStrategy = 'local' | 'random'

export interface CommonConfig {
  machineId: string
  credentialMachineIdStrategy: CredentialMachineIdStrategy
}

export interface UpdateCommonConfigRequest {
  credentialMachineIdStrategy?: CredentialMachineIdStrategy
}

export interface ThinkingConfig {
  suffix: string
  openaiFormat: 'reasoning_content' | 'thinking' | 'think'
  claudeFormat: 'reasoning_content' | 'thinking' | 'think'
}

export interface EndpointConfig {
  preferredEndpoint: 'auto' | 'kiro' | 'codewhisperer' | 'amazonq'
  endpointFallback: boolean
}

export interface PromptFilterRule {
  id: string
  name: string
  enabled: boolean
  type: 'regex' | 'lines-containing' | 'contains'
  match: string
  replace?: string
}

export interface PromptFilterConfig {
  filterClaudeCode: boolean
  filterEnvNoise: boolean
  filterStripBoundaries: boolean
  rules: PromptFilterRule[]
}

export interface ProxyConfig {
  proxyUrl: string | null
  hasCredentials: boolean
}

export interface UpdateProxyConfigRequest {
  proxyUrl: string | null
  proxyUsername: string | null
  proxyPassword: string | null
}

export interface ProxyItem {
  id: number
  url: string
  username?: string | null
  region?: string | null
  country?: string | null
  maxConcurrency?: number | null
  disabled: boolean
  note?: string | null
  dead: boolean
  consecutiveFailures: number
  lastError?: string | null
  lastChecked?: string | null
  availablePermits?: number | null
  boundCredentials: number
}

export interface ProxyListResponse {
  proxies: ProxyItem[]
}

export interface ProxyUpsertRequest {
  url: string
  username?: string
  password?: string
  region?: string
  maxConcurrency?: number
  disabled?: boolean
  note?: string
}

export interface ProxyImportRequest {
  text: string
  region?: string
  maxConcurrency?: number
}

export interface ProxyImportResponse {
  added: number
  failed: number
  errors: string[]
}

export interface ProxyTestResponse {
  ok: boolean
  exitIp?: string
  latencyMs?: number
  error?: string
}

export interface ProxyAutoAssignRequest {
  credentialIds: number[]
  reassignBound: boolean
}

export interface ProxyAutoAssignResponse {
  assigned: [number, number][]
  skipped: number[]
}

export interface SetCredentialProxyRequest {
  proxyId: number | null
}

export interface SetCredentialProxyByRegionResponse {
  message: string
  proxyId: number | null
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  subscriptionType?: string | null
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

export interface AvailableModel {
  modelId: string
  modelName?: string
  description?: string
  provider?: string
  capabilities?: string[]
  contextWindow?: number
  isDefault?: boolean
  rateMultiplier?: number
  rateUnit?: string
  promptCaching?: {
    maximumCacheCheckpointsPerRequest?: number
    minimumTokensPerCacheCheckpoint?: number
    supportsPromptCaching?: boolean
  }
  supportedInputTypes?: string[]
  tokenLimits?: {
    maxInputTokens?: number
    maxOutputTokens?: number
  }
}

export interface ListAvailableModelsResponse {
  availableModels: AvailableModel[]
  nextToken?: string | null
  defaultModel?: AvailableModel | null
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

export interface OperationSuccessResponse {
  success: boolean
}

export interface MessageResponse {
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
  provider?: string
  userId?: string
  tokenEndpoint?: string
  issuerUrl?: string
  scopes?: string
  priority?: number
  weight?: number
  region?: string
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  proxyId?: number | null
  apiKey?: string
  endpoint?: string
  /** 自定义并发上限（null/省略 = 跟随全局） */
  concurrency?: number | null
}

export interface ImportCredentialRecordRequest {
  accessToken?: string
  refreshToken?: string
  apiKey?: string
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
  sourceAccountId?: string
  email?: string
  label?: string
  status?: string
  addedAt?: string
  password?: string
  profileArn?: string
  userId?: string | null
  machineId?: string
  usageData?: unknown
  groupId?: string
  tagLinks?: unknown
  availableModelsCache?: unknown
  failureCount?: number
  lastFailureAt?: string
  disabledReason?: string
  successCount?: number
  csrfToken?: string
  nickname?: string
  banStatus?: string
  banReason?: string
  banTime?: number
  subscriptionType?: string
  subscriptionTitle?: string
  daysRemaining?: number
  usageCurrent?: number
  usageLimit?: number
  usagePercent?: number
  nextResetDate?: string
  lastRefresh?: number
  trialUsageCurrent?: number
  trialUsageLimit?: number
  trialUsagePercent?: number
  trialStatus?: string
  trialExpiresAt?: number
  overageCapability?: string
  overageCap?: number
  overageRate?: number
  currentOverages?: number
  overageCheckedAt?: number
  requestCount?: number
  errorCount?: number
  totalTokens?: number
  totalCredits?: number
  lastUsedAt?: number
  lastUsed?: number
  createdAt?: number
  tags?: unknown
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  proxyId?: number | null
  overageStatus?: string
  endpoint?: string
  enabled?: boolean
  disabled?: boolean
}

// 添加凭据响应
export interface AddCredentialResponse extends CredentialMaterialFlags, OptionalCredentialMetadataFields {
  success: boolean
  message: string
  credentialId: number
  email?: string
  authMethod?: string
  provider?: string
  userId?: string
  sourceAccountId?: string
  label?: string
  status?: string
  addedAt?: string
  nickname?: string
}

export interface CredentialLoginDetails extends OptionalCredentialMaterialFlags, OptionalCredentialMetadataFields {
  id: number
  email?: string
  authMethod?: string
  provider?: string
  userId?: string
  sourceAccountId?: string
  label?: string
  status?: string
  addedAt?: string
  nickname?: string
}

export interface CredentialLoginDetailsEnvelope {
  details?: CredentialLoginDetails
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
  subscriptionType?: string | null
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

// ============ 系统状态和统计端点 ============

export interface SystemStatusResponse {
  status: string
  version: string
  uptime: number
  totalRequests: number
  successRequests: number
  failedRequests: number
  totalTokens: number
  totalCredits: number
  credentialsTotal: number
  credentialsAvailable: number
}

export interface StatsResponse {
  totalRequests: number
  successRequests: number
  failedRequests: number
  totalTokens: number
  totalCredits: number
  uptime: number
  credentialsTotal: number
  credentialsAvailable: number
}

export interface GenerateMachineIdResponse {
  machineId: string
}

export interface SystemVersionResponse {
  version: string
  name: string
}

export interface RequestLogEntry {
  time: string
  endpoint: string
  model: string
  credentialId: string
  status: RequestLogStatus
  error?: string
  errorType?: string
  tokens?: number
  credits?: number
  durationMs: number
}

export interface RequestLogsResponse {
  logs: RequestLogEntry[]
  total: number
  success: number
  errors: number
}

export interface ClearRequestLogsResponse {
  cleared: number
}

// ============ 批量刷新令牌端点 ============

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
  subscriptionType?: string | null
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

export interface CredentialBackup {
  format: typeof CREDENTIAL_SOURCE_FORMATS.credentialBackup
  version: number
  exportedAt: string
  source: {
    app: string
    schema: string
    credentialCount: number
  }
  credentials: Array<{
    credential: Record<string, unknown>
  }>
}

export interface CredentialImportItem extends CredentialMaterialFlags, CredentialImportMetadataFields {
  index: number
  action: CredentialImportAction
  sourceFormat: string
  fingerprint: string
  credentialId?: number
  reason?: string
  authMethod?: string
  provider?: string
  email?: string
  userId?: string
  willRefresh: boolean
  warnings?: string[]
}

export interface CredentialImportResponse {
  summary: {
    parsed: number
    added: number
    skipped: number
    merged: number
    replaced: number
    invalid: number
  }
  items: CredentialImportItem[]
}

export interface SsoTokenImportResult {
  tokenIndex: number
  credentialId?: number
  email?: string
  error?: string
}

export interface SsoTokenImportResponse {
  imported: number
  results: SsoTokenImportResult[]
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

// ============ 社交 OAuth 登录 ============

export interface StartSocialLoginRequest {
  priority?: number
  email?: string
  proxyUrl?: string
  authEndpoint?: string
  provider: 'Google' | 'GitHub'
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
  | ({
      status: 'success'
      credentialId: number
      authMethod?: string
      provider?: string
    } & CredentialLoginDetailsEnvelope)
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

export interface CompleteIamSsoLoginResponse extends CredentialLoginDetailsEnvelope {
  success: boolean
}

export type PollIdcLoginResponse =
  | { status: 'pending' }
  | ({
      status: 'success'
      credentialId: number
      authMethod?: string
      provider?: string
    } & CredentialLoginDetailsEnvelope)
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
  | ({
      status: 'success'
      credentialId: number
      email?: string
      authMethod?: string
      provider?: string
    } & CredentialLoginDetailsEnvelope)
  | { status: 'expired' }
  | { status: 'error'; message: string }

export interface StartKiroSsoLoginResponse {
  sessionId: string
  signInUrl: string
  interval: number
}

export interface PollKiroSsoLoginResponse extends CredentialLoginDetailsEnvelope {
  success: boolean
  completed: boolean
  status?: 'pending'
  error?: string
}

export interface CompleteKiroSsoLoginResponse {
  success: boolean
  status: 'pending' | 'redirect' | 'submitted' | 'expired' | 'error'
  redirectUrl?: string
  error?: string
}
