import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  SetConcurrencyRequest,
  AddCredentialRequest,
  AddCredentialResponse,
  ImportKiroGoCredentialRequest,
} from '@/types/api'

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

type ApiRecord = Record<string, unknown>

function asRecord(value: unknown): ApiRecord {
  return value && typeof value === 'object' ? value as ApiRecord : {}
}

function payloadRecord(value: unknown): ApiRecord {
  const raw = asRecord(value)
  for (const key of ['data', 'result', 'payload']) {
    const nested = asRecord(raw[key])
    if (Object.keys(nested).length > 0) return nested
  }
  return raw
}

function stringField(data: ApiRecord, ...keys: string[]): string {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'string') return value
  }
  return ''
}

function stringLikeField(data: ApiRecord, ...keys: string[]): string {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'string') return value
    if (typeof value === 'number') return String(value)
  }
  return ''
}

function numberField(data: ApiRecord, ...keys: string[]): number | undefined {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'number') return value
  }
  return undefined
}

function numberLikeField(data: ApiRecord, ...keys: string[]): number | undefined {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'number') return value
    if (typeof value === 'string' && value.trim() !== '') {
      const parsed = Number(value)
      if (Number.isFinite(parsed)) return parsed
    }
  }
  return undefined
}

function responseKeys(data: ApiRecord): string {
  return Object.keys(data).sort().join(', ') || '无字段'
}

// 请求拦截器添加 API Key
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>('/credentials')
  return {
    ...data,
    total: data?.total ?? 0,
    available: data?.available ?? 0,
    credentials: Array.isArray(data?.credentials) ? data.credentials : [],
  }
}

// 设置凭据禁用状态
export async function setCredentialDisabled(
  id: number,
  disabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/disabled`,
    { disabled } as SetDisabledRequest
  )
  return data
}

// 设置凭据优先级
export async function setCredentialPriority(
  id: number,
  priority: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/priority`,
    { priority } as SetPriorityRequest
  )
  return data
}

// 设置凭据并发上限（null = 跟随全局）
export async function setCredentialConcurrency(
  id: number,
  concurrency: number | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/concurrency`,
    { concurrency } as SetConcurrencyRequest
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset`)
  return data
}

// 强制刷新 Token
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/refresh`)
  return data
}

// 获取凭据余额；force=true 跳过后端缓存，强制走云端
export async function getCredentialBalance(
  id: number,
  force = false,
): Promise<BalanceResponse> {
  const url = force ? `/credentials/${id}/balance?force=1` : `/credentials/${id}/balance`
  const { data } = await api.get<BalanceResponse>(url)
  return data
}

// 获取凭据可用模型列表（30 分钟内存缓存；force=true 跳过缓存）
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

export async function getCredentialModels(
  id: number,
  options: { provider?: string; force?: boolean } = {},
): Promise<ListAvailableModelsResponse> {
  const params = new URLSearchParams()
  if (options.provider) params.set('provider', options.provider)
  if (options.force) params.set('force', '1')
  const qs = params.toString()
  const url = qs ? `/credentials/${id}/models?${qs}` : `/credentials/${id}/models`
  const { data } = await api.get<ListAvailableModelsResponse>(url)
  return data
}

// 切换上游 overage 开关
export async function setOverageStatus(
  id: number,
  enabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/overage`, {
    enabled,
  })
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>('/credentials', req)
  return data
}

export async function importKiroGoCredential(
  req: ImportKiroGoCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post('/auth/credentials', req)
  const raw = asRecord(data)
  const account = asRecord(raw.account)
  const credentialId = numberField(account, 'id') ?? numberField(raw, 'credentialId', 'credential_id') ?? 0
  return {
    success: Boolean(raw.success ?? credentialId),
    message: stringField(raw, 'message') || `凭据添加成功，ID: ${credentialId}`,
    credentialId,
    email: stringField(account, 'email') || stringField(raw, 'email') || undefined,
  }
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

// 压缩配置类型
export interface CompressionConfig {
  maxRequestBodyBytes: number
}

// 获取压缩配置
export async function getCompressionConfig(): Promise<CompressionConfig> {
  const { data } = await api.get<CompressionConfig>('/config/compression')
  return data
}

// 更新压缩配置
export async function setCompressionConfig(config: CompressionConfig): Promise<CompressionConfig> {
  const { data } = await api.put<CompressionConfig>('/config/compression', config)
  return data
}

// 获取全局配置
export async function getGlobalConfig(): Promise<import('../types/api').GlobalConfigResponse> {
  const { data } = await api.get<import('../types/api').GlobalConfigResponse>('/config/global')
  return data
}

// 更新全局配置
export async function updateGlobalConfig(
  req: import('../types/api').UpdateGlobalConfigRequest,
): Promise<import('../types/api').GlobalConfigResponse> {
  const { data } = await api.put<import('../types/api').GlobalConfigResponse>('/config/global', req)
  return data
}

// 获取运行时状态（K/N + lastUsed，5s 高频轮询）
export async function getRuntimeStats(): Promise<import('../types/api').RuntimeStatsResponse> {
  const { data } = await api.get<import('../types/api').RuntimeStatsResponse>('/credentials/runtime-stats')
  return data
}

// 批量强制刷新 Token（服务端 Semaphore(8) 并发，前端一次往返）
export async function refreshBatch(
  ids: number[],
): Promise<import('../types/api').BatchRefreshResponse> {
  const { data } = await api.post<import('../types/api').BatchRefreshResponse>(
    '/credentials/refresh-batch',
    { ids } as import('../types/api').BatchRefreshRequest,
  )
  return data
}

// 批量查询余额（服务端 Semaphore(8) 并发，前端一次往返）
export async function refreshBalancesBatch(
  ids: number[],
): Promise<import('../types/api').BatchRefreshBalanceResponse> {
  const { data } = await api.post<import('../types/api').BatchRefreshBalanceResponse>(
    '/credentials/refresh-balances-batch',
    { ids } as import('../types/api').BatchRefreshRequest,
  )
  return data
}

// 按 ID 列表导出 token.json 兼容格式（可被批量导入直接吃回）
export interface ExportTokenJsonItem {
  provider: string
  refreshToken: string
  clientId?: string
  clientSecret?: string
  authMethod: string
  priority?: number
  region?: string
  apiRegion?: string
  machineId?: string
}

export async function exportTokenJson(ids: number[]): Promise<ExportTokenJsonItem[]> {
  const { data } = await api.post<ExportTokenJsonItem[]>(
    '/credentials/export-token-json',
    { ids },
  )
  return data
}

// KAM (kiro-account-manager) 兼容导出项
export interface ExportKamItem {
  id: string
  email?: string
  label: string
  status: string
  addedAt: string
  accessToken?: string
  refreshToken?: string
  expiresAt?: string
  provider?: string
  userId?: string
  authMethod?: string
  clientId?: string
  clientSecret?: string
  region?: string
  startUrl?: string
  profileArn?: string
  machineId?: string
  enabled: boolean
}

export async function exportKam(ids: number[]): Promise<ExportKamItem[]> {
  const { data } = await api.post<ExportKamItem[]>(
    '/credentials/export-kam',
    { ids },
  )
  return data
}

// 获取所有凭据缓存余额（首屏预填，避免手动查询）
export async function getCachedBalances(): Promise<import('../types/api').CachedBalancesResponse> {
  const { data } = await api.get<import('../types/api').CachedBalancesResponse>(
    '/credentials/balances/cached',
  )
  return {
    balances: Array.isArray(data?.balances) ? data.balances : [],
  }
}

// ============ 系统提示注入 ============

import type {
  SystemPromptResponse,
  UpdateSystemPromptRequest,
  UpsertUserPresetRequest,
} from '@/types/api'

export async function getSystemPrompt(): Promise<SystemPromptResponse> {
  const { data } = await api.get<SystemPromptResponse>('/config/system-prompt')
  return data
}

export async function updateSystemPrompt(
  req: UpdateSystemPromptRequest,
): Promise<SystemPromptResponse> {
  const { data } = await api.put<SystemPromptResponse>('/config/system-prompt', req)
  return data
}

export async function upsertUserPreset(
  req: UpsertUserPresetRequest,
): Promise<SystemPromptResponse> {
  const { data } = await api.post<SystemPromptResponse>('/config/user-presets', req)
  return data
}

export async function deleteUserPreset(id: string): Promise<SystemPromptResponse> {
  const { data } = await api.delete<SystemPromptResponse>(
    `/config/user-presets/${encodeURIComponent(id)}`,
  )
  return data
}

// ============ Social OAuth 登录 ============

import type {
  StartSocialLoginRequest,
  StartSocialLoginResponse,
  PollSocialLoginResponse,
  StartIdcLoginRequest,
  StartIdcLoginResponse,
  PollIdcLoginResponse,
  StartIamSsoLoginResponse,
  CompleteIamSsoLoginResponse,
  StartBuilderIdLoginRequest,
  StartBuilderIdLoginResponse,
  PollBuilderIdLoginResponse,
  StartKiroSsoLoginResponse,
  PollKiroSsoLoginResponse,
  CompleteKiroSsoLoginResponse,
} from '@/types/api'

export async function startSocialLogin(
  req: StartSocialLoginRequest,
): Promise<StartSocialLoginResponse> {
  const { data } = await api.post<StartSocialLoginResponse>('/auth/social/start', req)
  return data
}

export async function pollSocialLogin(
  sessionId: string,
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post<PollSocialLoginResponse>(
    `/auth/social/poll/${sessionId}`,
  )
  return data
}

export async function completeSocialLoginCallback(
  sessionId: string,
  callbackUrl: string,
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post<PollSocialLoginResponse>(
    `/auth/social/callback/${sessionId}`,
    { callbackUrl },
  )
  return data
}

export async function startIdcLogin(
  req: StartIdcLoginRequest,
): Promise<StartIdcLoginResponse> {
  const { data } = await api.post<StartIdcLoginResponse>('/auth/idc/start', req)
  return data
}

export async function pollIdcLogin(
  sessionId: string,
): Promise<PollIdcLoginResponse> {
  const { data } = await api.post<PollIdcLoginResponse>(
    `/auth/idc/poll/${sessionId}`,
  )
  return data
}

export async function startIamSsoLogin(
  req: StartIdcLoginRequest,
): Promise<StartIamSsoLoginResponse> {
  const { data } = await api.post<StartIamSsoLoginResponse>('/auth/iam-sso/start', req)
  const raw = asRecord(data)
  return {
    sessionId: stringField(raw, 'sessionId', 'session_id'),
    authorizeUrl: stringField(raw, 'authorizeUrl', 'authorize_url'),
    expiresIn: numberField(raw, 'expiresIn', 'expires_in') ?? 0,
  }
}

export async function completeIamSsoLogin(
  sessionId: string,
  callbackUrl: string,
): Promise<CompleteIamSsoLoginResponse> {
  const { data } = await api.post<CompleteIamSsoLoginResponse>('/auth/iam-sso/complete', {
    sessionId,
    callbackUrl,
  })
  const raw = asRecord(data)
  const account = asRecord(raw.account)
  const id = numberField(account, 'id')
  return {
    success: Boolean(raw.success),
    account: id === undefined ? undefined : {
      id,
      email: stringField(account, 'email') || undefined,
    },
  }
}

export async function startKiroSsoLogin(): Promise<StartKiroSsoLoginResponse> {
  const { data } = await api.post<StartKiroSsoLoginResponse>('/auth/kiro-sso/start', {})
  const raw = asRecord(data)
  return {
    sessionId: stringField(raw, 'sessionId', 'session_id'),
    signInUrl: stringField(raw, 'signInUrl', 'sign_in_url'),
    interval: numberField(raw, 'interval') ?? 2,
  }
}

export async function pollKiroSsoLogin(
  sessionId: string,
): Promise<PollKiroSsoLoginResponse> {
  const { data } = await api.post<PollKiroSsoLoginResponse>('/auth/kiro-sso/poll', { sessionId })
  return data
}

export async function completeKiroSsoLogin(
  sessionId: string,
  callbackUrl: string,
): Promise<CompleteKiroSsoLoginResponse> {
  const { data } = await api.post<CompleteKiroSsoLoginResponse>('/auth/kiro-sso/complete', {
    sessionId,
    callbackUrl,
  })
  const raw = asRecord(data)
  return {
    success: Boolean(raw.success),
    status: stringField(raw, 'status') as CompleteKiroSsoLoginResponse['status'],
    redirectUrl: stringField(raw, 'redirectUrl', 'redirect_url') || undefined,
    error: stringField(raw, 'error') || undefined,
  }
}

export async function cancelKiroSsoLogin(sessionId: string): Promise<{ success: boolean }> {
  const { data } = await api.post<{ success: boolean }>('/auth/kiro-sso/cancel', { sessionId })
  return data
}

// SSO Token 导入
export async function importSsoToken(
  tokens: string[],
  region?: string,
): Promise<{ imported: number; results: Array<{ token_index: number; credential_id?: number; email?: string; error?: string }> }> {
  const { data } = await api.post('/auth/sso-token', {
    token: tokens.join('\n'),
    region: region?.trim() || 'us-east-1',
  })
  const raw = asRecord(data)
  const results = Array.isArray(raw.results) ? raw.results as Array<Record<string, unknown>> : []
  return {
    imported: numberField(raw, 'successCount', 'success_count', 'imported') ?? 0,
    results: results.map((item, index) => ({
      token_index: numberField(item, 'tokenIndex', 'token_index', 'index') ?? index,
      credential_id: numberField(item, 'credentialId', 'credential_id'),
      email: stringField(item, 'email') || undefined,
      error: stringField(item, 'error') || undefined,
    })),
  }
}

// Builder ID 登录
export async function startBuilderIdLogin(
  req: StartBuilderIdLoginRequest,
): Promise<StartBuilderIdLoginResponse> {
  const { data } = await api.post<StartBuilderIdLoginResponse>('/auth/builderid/start', req)
  const raw = payloadRecord(data)
  const verificationUriComplete = stringField(raw, 'verificationUriComplete', 'verification_uri_complete')
  const verificationUri = verificationUriComplete || stringField(raw, 'verificationUri', 'verification_uri')
  if (!verificationUri) {
    throw new Error(`后端返回的 Builder ID 验证地址为空，响应字段: ${responseKeys(raw)}`)
  }
  return {
    sessionId: stringField(raw, 'sessionId', 'session_id'),
    userCode: stringField(raw, 'userCode', 'user_code'),
    verificationUri,
    verificationUriComplete: verificationUriComplete || undefined,
    pollInterval: numberField(raw, 'pollInterval', 'poll_interval', 'interval') ?? 5,
    expiresIn: numberField(raw, 'expiresIn', 'expires_in') ?? 0,
  }
}

export async function completeBuilderIdLogin(
  sessionId: string,
  callbackUrl: string,
): Promise<CompleteIamSsoLoginResponse> {
  const { data } = await api.post<CompleteIamSsoLoginResponse>('/auth/builderid/complete', {
    sessionId,
    callbackUrl,
  })
  const raw = asRecord(data)
  const account = asRecord(raw.account)
  const id = numberField(account, 'id')
  return {
    success: Boolean(raw.success),
    account: id === undefined ? undefined : {
      id,
      email: stringField(account, 'email') || undefined,
    },
  }
}

export async function pollBuilderIdLogin(sessionId: string): Promise<PollBuilderIdLoginResponse> {
  const { data } = await api.post<PollBuilderIdLoginResponse>('/auth/builderid/poll', { sessionId })
  const raw = asRecord(data)
  if (Boolean(raw.completed)) {
    const account = asRecord(raw.account)
    return {
      status: 'success',
      credentialId: numberField(account, 'id') ?? numberField(raw, 'credentialId', 'credential_id') ?? 0,
      email: stringField(account, 'email') || undefined,
    }
  }
  if (raw.success === false) {
    const status = stringField(raw, 'status')
    if (status === 'expired') return { status }
    return {
      status: 'error',
      message: stringField(raw, 'message', 'error') || '授权失败',
    }
  }
  const status = stringField(raw, 'status')
  if (status === 'success') {
    return {
      status,
      credentialId: numberField(raw, 'credentialId', 'credential_id') ?? 0,
    }
  }
  if (status === 'error') {
    return {
      status,
      message: stringField(raw, 'message', 'error') || '授权失败',
    }
  }
  if (status === 'expired') return { status }
  return {
    status: 'pending',
    pollInterval: numberField(raw, 'pollInterval', 'poll_interval', 'interval'),
  }
}

// 请求日志
export interface RequestLogEntry {
  time: string
  endpoint: string
  model: string
  credential_id: string
  status: 'success' | 'error'
  error?: string
  error_type?: string
  tokens?: number
  credits?: number
  duration_ms: number
}

function normalizeRequestLogEntry(value: unknown): RequestLogEntry {
  const raw = asRecord(value)
  const status = stringField(raw, 'status') === 'success' ? 'success' : 'error'
  return {
    time: stringLikeField(raw, 'time') || '-',
    endpoint: stringField(raw, 'endpoint') || '-',
    model: stringField(raw, 'model') || '-',
    credential_id: stringLikeField(raw, 'credential_id', 'credentialId') || '-',
    status,
    error: stringField(raw, 'error') || undefined,
    error_type: stringField(raw, 'error_type', 'errorType') || undefined,
    tokens: numberLikeField(raw, 'tokens'),
    credits: numberLikeField(raw, 'credits'),
    duration_ms: numberLikeField(raw, 'duration_ms', 'durationMs', 'duration') ?? 0,
  }
}

export async function getRequestLogs(): Promise<{ logs: RequestLogEntry[]; total: number; success: number; errors: number }> {
  const { data } = await api.get('/logs')
  const raw = payloadRecord(data)
  const logs = Array.isArray(raw.logs)
    ? raw.logs.map(normalizeRequestLogEntry)
    : []
  const success = numberLikeField(raw, 'success', 'successCount', 'success_count')
    ?? logs.filter(log => log.status === 'success').length
  const errors = numberLikeField(raw, 'errors', 'failed', 'failedCount', 'failed_count', 'errorCount', 'error_count')
    ?? logs.filter(log => log.status === 'error').length
  return {
    logs,
    total: numberLikeField(raw, 'total') ?? logs.length,
    success,
    errors,
  }
}

export async function clearRequestLogs(): Promise<{ cleared: number }> {
  const { data } = await api.delete('/logs')
  return data
}

// 系统状态
export async function getSystemStatus(): Promise<{
  status: string
  version: string
  uptime_seconds: number
  total_credentials: number
  available_credentials: number
  total_requests: number
  success_requests: number
  failed_requests: number
  total_tokens: number
  total_credits: number
}> {
  const { data } = await api.get('/status')
  return data
}

// 详细统计
export async function getStats(): Promise<{
  total_requests: number
  success_requests: number
  failed_requests: number
  total_tokens: number
  total_credits: number
  error_breakdown: Record<string, number>
}> {
  const { data } = await api.get('/stats')
  return data
}

// 版本信息
export async function getVersion(): Promise<{ version: string; name: string }> {
  const { data } = await api.get('/version')
  return data
}

// 重置统计
export async function resetStats(): Promise<{ message: string }> {
  const { data } = await api.post('/stats/reset')
  return data
}

// 生成 Machine ID
export async function generateMachineId(): Promise<{ machine_id: string }> {
  const { data } = await api.get('/generate-machine-id')
  return data
}

// 代理配置
export async function getProxyConfig(): Promise<{
  proxyUrl: string | null
  hasCredentials: boolean
}> {
  const { data } = await api.get('/proxy')
  return data
}

export async function updateProxyConfig(req: {
  proxyUrl: string | null
  proxyUsername: string | null
  proxyPassword: string | null
}): Promise<{ message: string }> {
  const { data } = await api.post('/proxy', req)
  return data
}

export interface XkiroSettings {
  apiKey?: string | null
  requireApiKey: boolean
  port: number
  host: string
  allowOverUsage: boolean
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

export async function getXkiroSettings(): Promise<XkiroSettings> {
  const { data } = await api.get('/settings')
  return data
}

export async function updateXkiroSettings(req: Partial<{
  apiKey: string
  requireApiKey: boolean
  password: string
  allowOverUsage: boolean
}>): Promise<{ success: boolean }> {
  const { data } = await api.post('/settings', req)
  return data
}

export async function getThinkingConfig(): Promise<ThinkingConfig> {
  const { data } = await api.get('/thinking')
  return data
}

export async function updateThinkingConfig(req: ThinkingConfig): Promise<{ success: boolean }> {
  const { data } = await api.post('/thinking', req)
  return data
}

export async function getEndpointConfig(): Promise<EndpointConfig> {
  const { data } = await api.get('/endpoint')
  return data
}

export async function updateEndpointConfig(req: EndpointConfig): Promise<{ success: boolean }> {
  const { data } = await api.post('/endpoint', req)
  return data
}

export async function getXkiroProxyConfig(): Promise<{ proxyURL: string }> {
  const { data } = await api.get('/proxy')
  return data
}

export async function updateXkiroProxyConfig(proxyURL: string): Promise<{ success: boolean }> {
  const { data } = await api.post('/proxy', { proxyURL })
  return data
}

export async function getPromptFilterConfig(): Promise<PromptFilterConfig> {
  const { data } = await api.get('/prompt-filter')
  return data
}

export async function updatePromptFilterConfig(req: PromptFilterConfig): Promise<{ success: boolean }> {
  const { data } = await api.post('/prompt-filter', req)
  return data
}
