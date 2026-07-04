import {
  CREDENTIAL_MATERIAL_FIELDS,
  type CredentialMaterialFlags,
  type OptionalCredentialMaterialFlags,
} from '@/lib/credential-material'
import {
  CREDENTIAL_METADATA_FIELDS,
  CREDENTIAL_IDENTITY_FIELDS,
  type CredentialMetadataKey,
  type CredentialIdentityKey,
} from '@/lib/credential-metadata'
import type { CredentialLoginDetails } from '@/types/api'

export type NormalizedCredentialMetadataFields = Pick<CredentialLoginDetails, CredentialMetadataKey>
export type NormalizedRequiredCredentialMetadataFields = NormalizedCredentialMetadataFields & {
  hasUsageData: boolean
  hasAvailableModelsCache: boolean
}
export type NormalizedCredentialIdentityFields = Pick<CredentialLoginDetails, CredentialIdentityKey>

export const ADMIN_API_ROUTES = {
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

export type ApiRecord = Record<string, unknown>

export function asRecord(value: unknown): ApiRecord {
  return value && typeof value === 'object' ? value as ApiRecord : {}
}

export function payloadRecord(value: unknown): ApiRecord {
  const raw = asRecord(value)
  for (const key of ['data', 'result', 'payload']) {
    const nested = asRecord(raw[key])
    if (Object.keys(nested).length > 0) return nested
  }
  return raw
}

export function stringField(data: ApiRecord, ...keys: string[]): string {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'string') return value
  }
  return ''
}

export function stringLikeField(data: ApiRecord, ...keys: string[]): string {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'string') return value
    if (typeof value === 'number') return String(value)
  }
  return ''
}

export function numberField(data: ApiRecord, ...keys: string[]): number | undefined {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'number') return value
  }
  return undefined
}

export function numberLikeField(data: ApiRecord, ...keys: string[]): number | undefined {
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

export function booleanField(data: ApiRecord, ...keys: string[]): boolean | undefined {
  for (const key of keys) {
    const value = data[key]
    if (typeof value === 'boolean') return value
  }
  return undefined
}

export function normalizeCredentialMaterialFlags(data: ApiRecord): OptionalCredentialMaterialFlags {
  const flags = {} as OptionalCredentialMaterialFlags
  for (const { key, wireKey } of CREDENTIAL_MATERIAL_FIELDS) {
    flags[key] = booleanField(data, key, wireKey)
  }
  return flags
}

export function normalizeRequiredCredentialMaterialFlags(data: ApiRecord): CredentialMaterialFlags {
  const flags = {} as CredentialMaterialFlags
  for (const { key, wireKey } of CREDENTIAL_MATERIAL_FIELDS) {
    flags[key] = booleanField(data, key, wireKey) ?? false
  }
  return flags
}

export function normalizeCredentialMetadataFields(
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

export function normalizeCredentialIdentityFields(
  data: ApiRecord,
): NormalizedCredentialIdentityFields {
  const identity: Record<string, unknown> = {}
  for (const { key, wireKey } of CREDENTIAL_IDENTITY_FIELDS) {
    identity[key] = stringField(data, key, wireKey) || undefined
  }
  return identity as NormalizedCredentialIdentityFields
}

export function normalizeRequiredCredentialMetadataFields(
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

export function valueField(data: ApiRecord, ...keys: string[]): unknown {
  for (const key of keys) {
    const value = data[key]
    if (value !== undefined && value !== null) return value
  }
  return undefined
}

export function responseKeys(data: ApiRecord): string {
  return Object.keys(data).sort().join(', ') || '无字段'
}

export function credentialDetailsRecord(data: ApiRecord): ApiRecord {
  for (const key of ['details', 'credential', 'account']) {
    const nested = asRecord(data[key])
    if (Object.keys(nested).length > 0) return nested
  }
  return {}
}

export function normalizeLoginDetails(
  value: unknown,
  fallbackId?: number,
): CredentialLoginDetails | undefined {
  const detail = asRecord(value)
  const id = numberField(detail, 'id') ?? fallbackId
  if (id === undefined) return undefined
  return {
    id,
    // authMethod/provider 是认证身份（经 formatCredentialAuthLabel 渲染），
    // 与 profile 身份字段语义不同，保留显式提取。
    authMethod: stringField(detail, 'authMethod', 'auth_method') || undefined,
    provider: stringField(detail, 'provider') || undefined,
    ...normalizeCredentialIdentityFields(detail),
    ...normalizeCredentialMetadataFields(detail),
    ...normalizeCredentialMaterialFlags(detail),
  }
}
