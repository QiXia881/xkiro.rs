import axios from 'axios'
import { storage } from '@/lib/storage'
import {
  CREDENTIAL_MATERIAL_FIELDS,
  type CredentialMaterialFlags,
  type OptionalCredentialMaterialFlags,
} from '@/lib/credential-material'
import {
  CREDENTIAL_IMPORT_ACTIONS,
  CREDENTIAL_METADATA_FIELDS,
  type CredentialImportAction,
  type CredentialImportMode,
  type CredentialMetadataKey,
} from '@/lib/credential-metadata'
import { REQUEST_LOG_STATUSES } from '@/types/api'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  SetConcurrencyRequest,
  GlobalConfigResponse,
  UpdateGlobalConfigRequest,
  CompressionConfig,
  RuntimeStatsResponse,
  BatchRefreshRequest,
  BatchRefreshResponse,
  BatchRefreshBalanceResponse,
  ListAvailableModelsResponse,
  AddCredentialRequest,
  AddCredentialResponse,
  ImportCredentialRecordRequest,
  SystemStatusResponse,
  StatsResponse,
  GenerateMachineIdResponse,
  RequestLogEntry,
  RequestLogsResponse,
  ClearRequestLogsResponse,
  CredentialBackup,
  CredentialImportItem,
  CredentialImportResponse,
  CachedBalancesResponse,
  AccessSettings,
  UpdateAccessSettingsRequest,
  CommonConfig,
  UpdateCommonConfigRequest,
  ThinkingConfig,
  EndpointConfig,
  PromptFilterConfig,
  ProxyConfig,
  UpdateProxyConfigRequest,
  SystemVersionResponse,
  OperationSuccessResponse,
  MessageResponse,
  SsoTokenImportResponse,
  CredentialLoginDetails,
  CredentialLoginDetailsEnvelope,
} from '@/types/api'

type NormalizedCredentialMetadataFields = Pick<CredentialLoginDetails, CredentialMetadataKey>
type NormalizedRequiredCredentialMetadataFields = NormalizedCredentialMetadataFields & {
  hasUsageData: boolean
  hasAvailableModelsCache: boolean
}
type NormalizedStatsFields = Pick<
  StatsResponse,
  | 'totalRequests'
  | 'successRequests'
  | 'failedRequests'
  | 'totalTokens'
  | 'totalCredits'
  | 'uptime'
  | 'credentialsTotal'
  | 'credentialsAvailable'
>

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

const ADMIN_API_ROUTES = {
  credentials: '/credentials',
  credential: (id: number) => `/credentials/${id}`,
  credentialDisabled: (id: number) => `/credentials/${id}/disabled`,
  credentialPriority: (id: number) => `/credentials/${id}/priority`,
  credentialConcurrency: (id: number) => `/credentials/${id}/concurrency`,
  credentialReset: (id: number) => `/credentials/${id}/reset`,
  credentialRefresh: (id: number) => `/credentials/${id}/refresh`,
  credentialBalance: (id: number, force = false) => `/credentials/${id}/balance${force ? '?force=1' : ''}`,
  credentialModels: (id: number, query = '') => `/credentials/${id}/models${query ? `?${query}` : ''}`,
  credentialOverage: (id: number) => `/credentials/${id}/overage`,
  credentialRuntimeStats: '/credentials/runtime-stats',
  credentialRefreshBatch: '/credentials/refresh-batch',
  credentialRefreshBalancesBatch: '/credentials/refresh-balances-batch',
  credentialCachedBalances: '/credentials/balances/cached',
  credentialImportRecord: '/credentials/import/record',
  credentialBackupExport: '/credentials/export',
  credentialImport: '/credentials/import',
  config: {
    compression: '/config/compression',
    global: '/config/global',
    proxy: '/config/proxy',
    accessSettings: '/config/settings',
    common: '/config/common',
    thinking: '/config/thinking',
    endpoint: '/config/endpoint',
    promptFilter: '/config/prompt-filter',
    systemPrompt: '/config/system-prompt',
    userPresets: '/config/user-presets',
    userPreset: (id: string) => `/config/user-presets/${encodeURIComponent(id)}`,
  },
  system: {
    logs: '/system/logs',
    status: '/system/status',
    stats: '/system/stats',
    version: '/system/version',
    resetStats: '/system/stats/reset',
    machineId: '/system/machine-id',
  },
  auth: {
    socialStart: '/auth/social/start',
    socialPoll: (sessionId: string) => `/auth/social/poll/${sessionId}`,
    socialCallback: (sessionId: string) => `/auth/social/callback/${sessionId}`,
    idcStart: '/auth/idc/start',
    idcPoll: (sessionId: string) => `/auth/idc/poll/${sessionId}`,
    iamSsoStart: '/auth/iam-sso/start',
    iamSsoComplete: '/auth/iam-sso/complete',
    kiroSsoStart: '/auth/kiro-sso/start',
    kiroSsoPoll: '/auth/kiro-sso/poll',
    kiroSsoComplete: '/auth/kiro-sso/complete',
    kiroSsoCancel: '/auth/kiro-sso/cancel',
    ssoToken: '/auth/sso-token',
    builderIdStart: '/auth/builderid/start',
    builderIdComplete: '/auth/builderid/complete',
    builderIdPoll: '/auth/builderid/poll',
  },
} as const

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

function booleanField(data: ApiRecord, ...keys: string[]): boolean | undefined {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'boolean') return value
  }
  return undefined
}

function normalizeCredentialMaterialFlags(data: ApiRecord): OptionalCredentialMaterialFlags {
  const flags = {} as OptionalCredentialMaterialFlags
  for (const { key, wireKey } of CREDENTIAL_MATERIAL_FIELDS) {
    flags[key] = booleanField(data, key, wireKey)
  }
  return flags
}

function normalizeRequiredCredentialMaterialFlags(data: ApiRecord): CredentialMaterialFlags {
  const flags = {} as CredentialMaterialFlags
  for (const { key, wireKey } of CREDENTIAL_MATERIAL_FIELDS) {
    flags[key] = booleanField(data, key, wireKey) ?? false
  }
  return flags
}

function normalizeCredentialMetadataFields(
  data: ApiRecord,
  options: { numberLike?: boolean } = {},
): NormalizedCredentialMetadataFields {
  const metadata: Record<string, unknown> = {}
  const numericField = options.numberLike ? numberLikeField : numberField
  for (const { key, wireKey } of CREDENTIAL_METADATA_FIELDS) {
    switch (key) {
      case 'tagLinks':
        metadata[key] = valueField(data, key, wireKey)
        break
      case 'hasUsageData':
      case 'hasAvailableModelsCache':
        metadata[key] = booleanField(data, key, wireKey)
        break
      case 'sourceFailureCount':
      case 'sourceSuccessCount':
        metadata[key] = numericField(data, key, wireKey)
        break
      default:
        metadata[key] = stringField(data, key, wireKey) || undefined
        break
    }
  }
  return metadata as NormalizedCredentialMetadataFields
}

function normalizeRequiredCredentialMetadataFields(
  data: ApiRecord,
  options: { numberLike?: boolean } = {},
): NormalizedRequiredCredentialMetadataFields {
  const metadata = normalizeCredentialMetadataFields(data, options)
  return {
    ...metadata,
    hasUsageData: metadata.hasUsageData ?? false,
    hasAvailableModelsCache: metadata.hasAvailableModelsCache ?? false,
  }
}

function valueField(data: ApiRecord, ...keys: string[]): unknown {
  for (const key of keys) {
    const value = data[key]
    if (value !== undefined && value !== null) return value
  }
  return undefined
}

function responseKeys(data: ApiRecord): string {
  return Object.keys(data).sort().join(', ') || '无字段'
}

function credentialDetailsRecord(data: ApiRecord): ApiRecord {
  for (const key of ['details', 'credential', 'account']) {
    const nested = asRecord(data[key])
    if (Object.keys(nested).length > 0) return nested
  }
  return {}
}

function normalizeLoginDetails(
  value: unknown,
  fallbackId?: number,
): CredentialLoginDetails | undefined {
  const detail = asRecord(value)
  const id = numberField(detail, 'id') ?? fallbackId
  if (id === undefined) return undefined
  return {
    id,
    email: stringField(detail, 'email') || undefined,
    authMethod: stringField(detail, 'authMethod', 'auth_method') || undefined,
    provider: stringField(detail, 'provider') || undefined,
    userId: stringField(detail, 'userId', 'user_id') || undefined,
    sourceAccountId: stringField(detail, 'sourceAccountId', 'source_account_id') || undefined,
    label: stringField(detail, 'label') || undefined,
    status: stringField(detail, 'status') || undefined,
    addedAt: stringField(detail, 'addedAt', 'added_at') || undefined,
    nickname: stringField(detail, 'nickname') || undefined,
    ...normalizeCredentialMetadataFields(detail),
    ...normalizeCredentialMaterialFlags(detail),
  }
}

function normalizeLoginDetailsEnvelope(raw: ApiRecord): CredentialLoginDetailsEnvelope {
  const details = normalizeLoginDetails(credentialDetailsRecord(raw))
  return {
    details,
  }
}

function normalizeLoginSuccessFields(raw: ApiRecord): {
  credentialId?: number
  authMethod?: string
  provider?: string
  details?: CredentialLoginDetails
} {
  const { details } = normalizeLoginDetailsEnvelope(raw)
  return {
    credentialId: details?.id ?? numberField(raw, 'credentialId', 'credential_id'),
    authMethod: (details?.authMethod ?? stringField(raw, 'authMethod', 'auth_method')) || undefined,
    provider: (details?.provider ?? stringField(raw, 'provider')) || undefined,
    details,
  }
}

function normalizeCompletedLoginDetailsEnvelope(raw: ApiRecord): CredentialLoginDetailsEnvelope & {
  success: boolean
} {
  return {
    success: Boolean(raw.success),
    ...normalizeLoginDetailsEnvelope(raw),
  }
}

function normalizeAddCredentialResponse(data: unknown): AddCredentialResponse {
  const raw = asRecord(data)
  const detailsRecord = credentialDetailsRecord(raw)
  const source = Object.keys(detailsRecord).length > 0 ? detailsRecord : raw
  const credentialId = numberField(source, 'id')
    ?? numberField(raw, 'credentialId', 'credential_id')
    ?? 0
  const details = normalizeLoginDetails(source, credentialId > 0 ? credentialId : undefined)
  const detailSource = details ? asRecord(details) : source
  return {
    success: Boolean(raw.success ?? credentialId),
    message: stringField(raw, 'message') || `凭据添加成功，ID: ${credentialId}`,
    credentialId: details?.id ?? credentialId,
    email: (details?.email ?? stringField(raw, 'email')) || undefined,
    authMethod: (details?.authMethod ?? stringField(source, 'authMethod', 'auth_method')) || undefined,
    provider: (details?.provider ?? stringField(source, 'provider')) || undefined,
    userId: (details?.userId ?? stringField(source, 'userId', 'user_id')) || undefined,
    sourceAccountId: (details?.sourceAccountId ?? stringField(source, 'sourceAccountId', 'source_account_id')) || undefined,
    label: (details?.label ?? stringField(source, 'label')) || undefined,
    status: (details?.status ?? stringField(source, 'status')) || undefined,
    addedAt: (details?.addedAt ?? stringField(source, 'addedAt', 'added_at')) || undefined,
    nickname: (details?.nickname ?? stringField(source, 'nickname')) || undefined,
    ...normalizeCredentialMetadataFields(detailSource),
    ...normalizeRequiredCredentialMaterialFlags(detailSource),
  }
}

// 请求拦截器添加 API 密钥
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>(ADMIN_API_ROUTES.credentials)
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
    ADMIN_API_ROUTES.credentialDisabled(id),
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
    ADMIN_API_ROUTES.credentialPriority(id),
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
    ADMIN_API_ROUTES.credentialConcurrency(id),
    { concurrency } as SetConcurrencyRequest
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(ADMIN_API_ROUTES.credentialReset(id))
  return data
}

// 强制刷新令牌
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(ADMIN_API_ROUTES.credentialRefresh(id))
  return data
}

// 获取凭据余额；force=true 跳过后端缓存，强制走云端
export async function getCredentialBalance(
  id: number,
  force = false,
): Promise<BalanceResponse> {
  const { data } = await api.get<BalanceResponse>(ADMIN_API_ROUTES.credentialBalance(id, force))
  return data
}

export async function getCredentialModels(
  id: number,
  options: { provider?: string; force?: boolean } = {},
): Promise<ListAvailableModelsResponse> {
  const params = new URLSearchParams()
  if (options.provider) params.set('provider', options.provider)
  if (options.force) params.set('force', '1')
  const qs = params.toString()
  const { data } = await api.get<ListAvailableModelsResponse>(ADMIN_API_ROUTES.credentialModels(id, qs))
  return data
}

// 切换上游 overage 开关
export async function setOverageStatus(
  id: number,
  enabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(ADMIN_API_ROUTES.credentialOverage(id), {
    enabled,
  })
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>(ADMIN_API_ROUTES.credentials, req)
  return normalizeAddCredentialResponse(data)
}

export async function importCredentialRecord(
  req: ImportCredentialRecordRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.credentialImportRecord, req)
  return normalizeAddCredentialResponse(data)
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(ADMIN_API_ROUTES.credential(id))
  return data
}

// 获取压缩配置
export async function getCompressionConfig(): Promise<CompressionConfig> {
  const { data } = await api.get<CompressionConfig>(ADMIN_API_ROUTES.config.compression)
  return data
}

// 更新压缩配置
export async function setCompressionConfig(config: CompressionConfig): Promise<CompressionConfig> {
  const { data } = await api.put<CompressionConfig>(ADMIN_API_ROUTES.config.compression, config)
  return data
}

// 获取全局配置
export async function getGlobalConfig(): Promise<GlobalConfigResponse> {
  const { data } = await api.get<GlobalConfigResponse>(ADMIN_API_ROUTES.config.global)
  return data
}

// 更新全局配置
export async function updateGlobalConfig(
  req: UpdateGlobalConfigRequest,
): Promise<GlobalConfigResponse> {
  const { data } = await api.put<GlobalConfigResponse>(ADMIN_API_ROUTES.config.global, req)
  return data
}

// 获取运行时状态（K/N + lastUsed，5s 高频轮询）
export async function getRuntimeStats(): Promise<RuntimeStatsResponse> {
  const { data } = await api.get<RuntimeStatsResponse>(ADMIN_API_ROUTES.credentialRuntimeStats)
  return data
}

// 批量强制刷新令牌（服务端 Semaphore(8) 并发，前端一次往返）
export async function refreshBatch(
  ids: number[],
): Promise<BatchRefreshResponse> {
  const { data } = await api.post<BatchRefreshResponse>(
    ADMIN_API_ROUTES.credentialRefreshBatch,
    { ids } as BatchRefreshRequest,
  )
  return data
}

// 批量查询余额（服务端 Semaphore(8) 并发，前端一次往返）
export async function refreshBalancesBatch(
  ids: number[],
): Promise<BatchRefreshBalanceResponse> {
  const { data } = await api.post<BatchRefreshBalanceResponse>(
    ADMIN_API_ROUTES.credentialRefreshBalancesBatch,
    { ids } as BatchRefreshRequest,
  )
  return data
}

function normalizeCredentialImportAction(value: unknown): CredentialImportAction {
  const action = typeof value === 'string' ? value.trim().toLowerCase() : ''
  switch (action) {
    case CREDENTIAL_IMPORT_ACTIONS.added:
    case CREDENTIAL_IMPORT_ACTIONS.skipped:
    case CREDENTIAL_IMPORT_ACTIONS.merged:
    case CREDENTIAL_IMPORT_ACTIONS.replaced:
    case CREDENTIAL_IMPORT_ACTIONS.invalid:
      return action
    default:
      return CREDENTIAL_IMPORT_ACTIONS.invalid
  }
}

function normalizeStringArray(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) return undefined
  const strings = value.filter((item): item is string => typeof item === 'string')
  return strings.length > 0 ? strings : undefined
}

function normalizeCredentialImportItem(value: unknown, index: number): CredentialImportItem {
  const raw = asRecord(value)
  return {
    index: numberLikeField(raw, 'index') ?? index,
    action: normalizeCredentialImportAction(raw.action),
    sourceFormat: stringField(raw, 'sourceFormat', 'source_format') || 'unknown',
    fingerprint: stringLikeField(raw, 'fingerprint') || '(unknown)',
    credentialId: numberLikeField(raw, 'credentialId', 'credential_id'),
    reason: stringField(raw, 'reason') || undefined,
    authMethod: stringField(raw, 'authMethod', 'auth_method') || undefined,
    provider: stringField(raw, 'provider') || undefined,
    email: stringField(raw, 'email') || undefined,
    userId: stringField(raw, 'userId', 'user_id') || undefined,
    ...normalizeRequiredCredentialMetadataFields(raw, { numberLike: true }),
    willRefresh: booleanField(raw, 'willRefresh', 'will_refresh') ?? false,
    ...normalizeRequiredCredentialMaterialFlags(raw),
    warnings: normalizeStringArray(raw.warnings),
  }
}

function normalizeCredentialImportResponse(data: unknown): CredentialImportResponse {
  const raw = payloadRecord(data)
  const summary = asRecord(raw.summary)
  const items = Array.isArray(raw.items)
    ? raw.items.map(normalizeCredentialImportItem)
    : []
  return {
    summary: {
      parsed: numberLikeField(summary, 'parsed') ?? items.length,
      added: numberLikeField(summary, CREDENTIAL_IMPORT_ACTIONS.added) ?? items.filter(item => item.action === CREDENTIAL_IMPORT_ACTIONS.added).length,
      skipped: numberLikeField(summary, CREDENTIAL_IMPORT_ACTIONS.skipped) ?? items.filter(item => item.action === CREDENTIAL_IMPORT_ACTIONS.skipped).length,
      merged: numberLikeField(summary, CREDENTIAL_IMPORT_ACTIONS.merged) ?? items.filter(item => item.action === CREDENTIAL_IMPORT_ACTIONS.merged).length,
      replaced: numberLikeField(summary, CREDENTIAL_IMPORT_ACTIONS.replaced) ?? items.filter(item => item.action === CREDENTIAL_IMPORT_ACTIONS.replaced).length,
      invalid: numberLikeField(summary, CREDENTIAL_IMPORT_ACTIONS.invalid) ?? items.filter(item => item.action === CREDENTIAL_IMPORT_ACTIONS.invalid).length,
    },
    items,
  }
}

export async function exportCredentialBackup(ids: number[]): Promise<CredentialBackup> {
  const { data } = await api.post<CredentialBackup>(
    ADMIN_API_ROUTES.credentialBackupExport,
    { ids },
  )
  return data
}

export async function importCredentials(
  input: unknown,
  dryRun: boolean,
  mode: CredentialImportMode,
): Promise<CredentialImportResponse> {
  const { data } = await api.post(
    ADMIN_API_ROUTES.credentialImport,
    { dryRun, mode, input },
  )
  return normalizeCredentialImportResponse(data)
}

// 获取所有凭据缓存余额（首屏预填，避免手动查询）
export async function getCachedBalances(): Promise<CachedBalancesResponse> {
  const { data } = await api.get<CachedBalancesResponse>(
    ADMIN_API_ROUTES.credentialCachedBalances,
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
  const { data } = await api.get<SystemPromptResponse>(ADMIN_API_ROUTES.config.systemPrompt)
  return data
}

export async function updateSystemPrompt(
  req: UpdateSystemPromptRequest,
): Promise<SystemPromptResponse> {
  const { data } = await api.put<SystemPromptResponse>(ADMIN_API_ROUTES.config.systemPrompt, req)
  return data
}

export async function upsertUserPreset(
  req: UpsertUserPresetRequest,
): Promise<SystemPromptResponse> {
  const { data } = await api.post<SystemPromptResponse>(ADMIN_API_ROUTES.config.userPresets, req)
  return data
}

export async function deleteUserPreset(id: string): Promise<SystemPromptResponse> {
  const { data } = await api.delete<SystemPromptResponse>(ADMIN_API_ROUTES.config.userPreset(id))
  return data
}

// ============ 社交 OAuth 登录 ============

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

function normalizeSocialLoginResponse(data: unknown): PollSocialLoginResponse {
  const raw = asRecord(data)
  const status = stringField(raw, 'status')
  if (status === 'success') {
    const login = normalizeLoginSuccessFields(raw)
    const credentialId = login.credentialId
    if (credentialId === undefined) {
      return {
        status: 'error',
        message: `登录成功但响应缺少 credentialId，响应字段: ${responseKeys(raw)}`,
      }
    }
    return {
      status,
      credentialId,
      authMethod: login.authMethod,
      provider: login.provider,
      details: login.details,
    }
  }
  if (status === 'error') {
    return {
      status,
      message: stringField(raw, 'message', 'error') || '登录失败',
    }
  }
  if (status === 'expired') return { status }
  return { status: 'waiting' }
}

export async function startSocialLogin(
  req: StartSocialLoginRequest,
): Promise<StartSocialLoginResponse> {
  const { data } = await api.post<StartSocialLoginResponse>(ADMIN_API_ROUTES.auth.socialStart, req)
  return data
}

export async function pollSocialLogin(
  sessionId: string,
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post(
    ADMIN_API_ROUTES.auth.socialPoll(sessionId),
  )
  return normalizeSocialLoginResponse(data)
}

export async function completeSocialLoginCallback(
  sessionId: string,
  callbackUrl: string,
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post(
    ADMIN_API_ROUTES.auth.socialCallback(sessionId),
    { callbackUrl },
  )
  return normalizeSocialLoginResponse(data)
}

export async function startIdcLogin(
  req: StartIdcLoginRequest,
): Promise<StartIdcLoginResponse> {
  const { data } = await api.post<StartIdcLoginResponse>(ADMIN_API_ROUTES.auth.idcStart, req)
  return data
}

export async function pollIdcLogin(
  sessionId: string,
): Promise<PollIdcLoginResponse> {
  const { data } = await api.post(
    ADMIN_API_ROUTES.auth.idcPoll(sessionId),
  )
  const raw = asRecord(data)
  if (stringField(raw, 'status') === 'success') {
    const login = normalizeLoginSuccessFields(raw)
    return {
      status: 'success',
      credentialId: login.credentialId ?? 0,
      authMethod: login.authMethod,
      provider: login.provider,
      details: login.details,
    }
  }
  return data as PollIdcLoginResponse
}

export async function startIamSsoLogin(
  req: StartIdcLoginRequest,
): Promise<StartIamSsoLoginResponse> {
  const { data } = await api.post<StartIamSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.iamSsoStart,
    req,
  )
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
  const { data } = await api.post<CompleteIamSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.iamSsoComplete,
    { sessionId, callbackUrl },
  )
  return normalizeCompletedLoginDetailsEnvelope(asRecord(data))
}

export async function startKiroSsoLogin(): Promise<StartKiroSsoLoginResponse> {
  const { data } = await api.post<StartKiroSsoLoginResponse>(ADMIN_API_ROUTES.auth.kiroSsoStart, {})
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
  const { data } = await api.post<PollKiroSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.kiroSsoPoll,
    { sessionId },
  )
  const raw = asRecord(data)
  const { details } = normalizeLoginDetailsEnvelope(raw)
  return {
    success: Boolean(raw.success),
    completed: Boolean(raw.completed),
    status: (stringField(raw, 'status') || undefined) as PollKiroSsoLoginResponse['status'],
    error: stringField(raw, 'error') || undefined,
    details,
  }
}

export async function completeKiroSsoLogin(
  sessionId: string,
  callbackUrl: string,
): Promise<CompleteKiroSsoLoginResponse> {
  const { data } = await api.post<CompleteKiroSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.kiroSsoComplete,
    { sessionId, callbackUrl },
  )
  const raw = asRecord(data)
  return {
    success: Boolean(raw.success),
    status: stringField(raw, 'status') as CompleteKiroSsoLoginResponse['status'],
    redirectUrl: stringField(raw, 'redirectUrl', 'redirect_url') || undefined,
    error: stringField(raw, 'error') || undefined,
  }
}

export async function cancelKiroSsoLogin(sessionId: string): Promise<OperationSuccessResponse> {
  const { data } = await api.post<OperationSuccessResponse>(
    ADMIN_API_ROUTES.auth.kiroSsoCancel,
    { sessionId },
  )
  return data
}

export async function importSsoToken(
  tokens: string[],
  region?: string,
): Promise<SsoTokenImportResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.auth.ssoToken, {
    token: tokens.join('\n'),
    region: region?.trim() || 'us-east-1',
  })
  const raw = asRecord(data)
  const results = Array.isArray(raw.results) ? raw.results as Array<Record<string, unknown>> : []
  return {
    imported: numberField(raw, 'successCount', 'success_count', 'imported') ?? 0,
    results: results.map((item, index) => ({
      tokenIndex: numberField(item, 'tokenIndex', 'token_index', 'index') ?? index,
      credentialId: numberField(item, 'credentialId', 'credential_id'),
      email: stringField(item, 'email') || undefined,
      error: stringField(item, 'error') || undefined,
    })),
  }
}

// 设备授权登录
export async function startBuilderIdLogin(
  req: StartBuilderIdLoginRequest,
): Promise<StartBuilderIdLoginResponse> {
  const { data } = await api.post<StartBuilderIdLoginResponse>(
    ADMIN_API_ROUTES.auth.builderIdStart,
    req,
  )
  const raw = payloadRecord(data)
  const verificationUriComplete = stringField(raw, 'verificationUriComplete', 'verification_uri_complete')
  const verificationUri = verificationUriComplete || stringField(raw, 'verificationUri', 'verification_uri')
  if (!verificationUri) {
    throw new Error(`后端返回的设备授权验证地址为空，响应字段: ${responseKeys(raw)}`)
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
  const { data } = await api.post<CompleteIamSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.builderIdComplete,
    { sessionId, callbackUrl },
  )
  return normalizeCompletedLoginDetailsEnvelope(asRecord(data))
}

export async function pollBuilderIdLogin(sessionId: string): Promise<PollBuilderIdLoginResponse> {
  const { data } = await api.post<PollBuilderIdLoginResponse>(
    ADMIN_API_ROUTES.auth.builderIdPoll,
    { sessionId },
  )
  const raw = asRecord(data)
  if (Boolean(raw.completed)) {
    const login = normalizeLoginSuccessFields(raw)
    return {
      status: 'success',
      credentialId: login.credentialId ?? 0,
      email: login.details?.email,
      authMethod: login.details?.authMethod,
      provider: login.details?.provider,
      details: login.details,
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
    const login = normalizeLoginSuccessFields(raw)
    return {
      status,
      credentialId: login.credentialId ?? 0,
      email: (login.details?.email ?? stringField(raw, 'email')) || undefined,
      authMethod: login.authMethod,
      provider: login.provider,
      details: login.details,
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

function normalizeRequestLogEntry(value: unknown): RequestLogEntry {
  const raw = asRecord(value)
  const status = stringField(raw, 'status') === REQUEST_LOG_STATUSES.success
    ? REQUEST_LOG_STATUSES.success
    : REQUEST_LOG_STATUSES.error
  return {
    time: stringLikeField(raw, 'time') || '-',
    endpoint: stringField(raw, 'endpoint') || '-',
    model: stringField(raw, 'model') || '-',
    credentialId: stringLikeField(raw, 'credentialId', 'credential_id', 'accountId') || '-',
    status,
    error: stringField(raw, 'error') || undefined,
    errorType: stringField(raw, 'error_type', 'errorType') || undefined,
    tokens: numberLikeField(raw, 'tokens'),
    credits: numberLikeField(raw, 'credits'),
    durationMs: numberLikeField(raw, 'duration_ms', 'durationMs', 'duration') ?? 0,
  }
}

export async function getRequestLogs(): Promise<RequestLogsResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.logs)
  const raw = payloadRecord(data)
  const logs = Array.isArray(raw.logs)
    ? raw.logs.map(normalizeRequestLogEntry)
    : []
  const success = numberLikeField(raw, 'success', 'successCount', 'success_count')
    ?? logs.filter(log => log.status === REQUEST_LOG_STATUSES.success).length
  const errors = numberLikeField(raw, 'errors', 'failed', 'failedCount', 'failed_count', 'errorCount', 'error_count')
    ?? logs.filter(log => log.status === REQUEST_LOG_STATUSES.error).length
  return {
    logs,
    total: numberLikeField(raw, 'total') ?? logs.length,
    success,
    errors,
  }
}

export async function clearRequestLogs(): Promise<ClearRequestLogsResponse> {
  const { data } = await api.delete(ADMIN_API_ROUTES.system.logs)
  return data
}

function normalizeSystemStatus(data: unknown): SystemStatusResponse {
  const raw = payloadRecord(data)
  const stats = normalizeStatsFields(raw)
  return {
    status: stringField(raw, 'status') || 'unknown',
    version: stringField(raw, 'version') || '',
    ...stats,
  }
}

function normalizeStatsFields(raw: ApiRecord): NormalizedStatsFields {
  return {
    totalRequests: numberLikeField(raw, 'totalRequests', 'total_requests') ?? 0,
    successRequests: numberLikeField(raw, 'successRequests', 'success_requests') ?? 0,
    failedRequests: numberLikeField(raw, 'failedRequests', 'failed_requests') ?? 0,
    totalTokens: numberLikeField(raw, 'totalTokens', 'total_tokens') ?? 0,
    totalCredits: numberLikeField(raw, 'totalCredits', 'total_credits') ?? 0,
    uptime: numberLikeField(raw, 'uptime', 'uptime_seconds') ?? 0,
    credentialsTotal: numberLikeField(raw, 'credentialsTotal', 'credentials_total', 'total_credentials', 'accounts') ?? 0,
    credentialsAvailable: numberLikeField(raw, 'credentialsAvailable', 'credentials_available', 'available_credentials', 'available') ?? 0,
  }
}

function normalizeStats(data: unknown): StatsResponse {
  return normalizeStatsFields(payloadRecord(data))
}

// 系统状态
export async function getSystemStatus(): Promise<SystemStatusResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.status)
  return normalizeSystemStatus(data)
}

// 详细统计
export async function getStats(): Promise<StatsResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.stats)
  return normalizeStats(data)
}

// 版本信息
export async function getVersion(): Promise<SystemVersionResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.version)
  return data
}

// 重置统计
export async function resetStats(): Promise<MessageResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.system.resetStats)
  return data
}

// 生成机器 ID
export async function generateMachineId(): Promise<GenerateMachineIdResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.machineId)
  const raw = payloadRecord(data)
  return {
    machineId: stringField(raw, 'machineId', 'machine_id'),
  }
}

// 代理配置
export async function getProxyConfig(): Promise<ProxyConfig> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.proxy)
  return data
}

export async function updateProxyConfig(req: UpdateProxyConfigRequest): Promise<MessageResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.proxy, req)
  return data
}

export async function getAccessSettings(): Promise<AccessSettings> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.accessSettings)
  return data
}

export async function updateAccessSettings(req: UpdateAccessSettingsRequest): Promise<OperationSuccessResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.accessSettings, req)
  return data
}

export async function getCommonConfig(): Promise<CommonConfig> {
  const { data } = await api.get<CommonConfig>(ADMIN_API_ROUTES.config.common)
  const raw = asRecord(data)
  const strategy = stringField(raw, 'credentialMachineIdStrategy', 'credential_machine_id_strategy')
  return {
    machineId: stringField(raw, 'machineId', 'machine_id'),
    credentialMachineIdStrategy: strategy === 'local' ? 'local' : 'random',
  }
}

export async function updateCommonConfig(req: UpdateCommonConfigRequest): Promise<CommonConfig> {
  const { data } = await api.post<CommonConfig>(ADMIN_API_ROUTES.config.common, req)
  const raw = asRecord(data)
  const strategy = stringField(raw, 'credentialMachineIdStrategy', 'credential_machine_id_strategy')
  return {
    machineId: stringField(raw, 'machineId', 'machine_id'),
    credentialMachineIdStrategy: strategy === 'local' ? 'local' : 'random',
  }
}

export async function getThinkingConfig(): Promise<ThinkingConfig> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.thinking)
  return data
}

export async function updateThinkingConfig(req: ThinkingConfig): Promise<OperationSuccessResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.thinking, req)
  return data
}

export async function getEndpointConfig(): Promise<EndpointConfig> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.endpoint)
  return data
}

export async function updateEndpointConfig(req: EndpointConfig): Promise<OperationSuccessResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.endpoint, req)
  return data
}

export async function getPromptFilterConfig(): Promise<PromptFilterConfig> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.promptFilter)
  return data
}

export async function updatePromptFilterConfig(req: PromptFilterConfig): Promise<OperationSuccessResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.promptFilter, req)
  return data
}
