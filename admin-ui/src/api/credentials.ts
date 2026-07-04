import { adminApi as api } from '@/api/client'
import {
  ADMIN_API_ROUTES,
  asRecord,
  payloadRecord,
  stringField,
  stringLikeField,
  numberField,
  numberLikeField,
  booleanField,
  credentialDetailsRecord,
  normalizeLoginDetails,
  normalizeCredentialIdentityFields,
  normalizeCredentialMetadataFields,
  normalizeRequiredCredentialMetadataFields,
  normalizeRequiredCredentialMaterialFlags,
} from './_normalizers'
import {
  CREDENTIAL_IMPORT_ACTIONS,
  type CredentialImportAction,
  type CredentialImportMode,
} from '@/lib/credential-metadata'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  SetConcurrencyRequest,
  RuntimeStatsResponse,
  BatchRefreshRequest,
  BatchRefreshResponse,
  BatchRefreshBalanceResponse,
  ListAvailableModelsResponse,
  AddCredentialRequest,
  AddCredentialResponse,
  ImportCredentialRecordRequest,
  CredentialBackup,
  CredentialImportItem,
  CredentialImportResponse,
  CachedBalancesResponse,
} from '@/types/api'

export * from './config'
export * from './system'
export * from './auth'

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
    // profile 身份字段走统一循环；detailSource 在 details 存在时即其规范化结果，
    // 不存在时回退到 source，与逐字段 `details?.X ?? stringField(source,...)` 等价。
    ...normalizeCredentialIdentityFields(detailSource),
    // email 有 raw 兜底（区别于其他字段的 source 兜底），单列覆盖 spread。
    email: (details?.email ?? stringField(raw, 'email')) || undefined,
    // authMethod/provider 是认证身份（非 profile 7 字段），保留显式提取。
    authMethod: (details?.authMethod ?? stringField(source, 'authMethod', 'auth_method')) || undefined,
    provider: (details?.provider ?? stringField(source, 'provider')) || undefined,
    ...normalizeCredentialMetadataFields(detailSource),
    ...normalizeRequiredCredentialMaterialFlags(detailSource),
  }
}

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
