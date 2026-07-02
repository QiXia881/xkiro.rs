export const CREDENTIAL_METADATA_FIELDS = [
  { key: 'groupId', wireKey: 'group_id', label: '分组 ID' },
  { key: 'tagLinks', wireKey: 'tag_links', label: '标签链接' },
  { key: 'hasUsageData', wireKey: 'has_usage_data', label: '用量数据' },
  { key: 'hasAvailableModelsCache', wireKey: 'has_available_models_cache', label: '模型缓存' },
  { key: 'sourceFailureCount', wireKey: 'source_failure_count', label: '来源失败次数' },
  { key: 'sourceLastFailureAt', wireKey: 'source_last_failure_at', label: '来源上次失败' },
  { key: 'sourceDisabledReason', wireKey: 'source_disabled_reason', label: '来源禁用原因' },
  { key: 'sourceSuccessCount', wireKey: 'source_success_count', label: '来源成功次数' },
  { key: 'region', wireKey: 'region', label: '区域' },
  { key: 'authRegion', wireKey: 'auth_region', label: '认证区域' },
  { key: 'apiRegion', wireKey: 'api_region', label: 'API 区域' },
  { key: 'machineId', wireKey: 'machine_id', label: '设备 ID' },
  { key: 'endpoint', wireKey: 'endpoint', label: '端点' },
  { key: 'startUrl', wireKey: 'start_url', label: '启动地址' },
  { key: 'clientIdHash', wireKey: 'client_id_hash', label: '客户端 ID 哈希' },
  { key: 'ssoSessionId', wireKey: 'sso_session_id', label: 'SSO 会话' },
  { key: 'tokenEndpoint', wireKey: 'token_endpoint', label: '令牌端点' },
  { key: 'issuerUrl', wireKey: 'issuer_url', label: '签发方地址' },
  { key: 'scopes', wireKey: 'scopes', label: '授权范围' },
] as const

export const CREDENTIAL_IDENTITY_FIELDS = [
  { key: 'email', label: '邮箱' },
  { key: 'nickname', label: '昵称' },
  { key: 'label', label: '标签' },
  { key: 'sourceAccountId', label: '来源 ID' },
  { key: 'status', label: '来源状态' },
  { key: 'addedAt', label: '添加时间' },
  { key: 'userId', label: '用户 ID' },
] as const

export const CREDENTIAL_AUTH_LABELS = {
  apiKey: 'API 密钥',
  microsoftEntra: 'Microsoft / Entra ID',
  awsBuilderId: 'AWS Builder ID',
  iamIdentityCenter: 'IAM Identity Center',
  github: 'GitHub',
  google: 'Google',
  kiroSso: 'Kiro SSO',
  social: '社交登录',
} as const

export const CREDENTIAL_SOURCE_FORMATS = {
  credentialBackup: 'xkiro.credentials.bundle',
  cachedCredential: 'compatible.cache-record',
  credentialSnapshot: 'external.account-export',
  flatCredential: 'flat.credentials',
  unknown: 'unknown',
} as const

export const CREDENTIAL_IMPORT_MODES = {
  skipExisting: 'skipExisting',
  mergeMissing: 'mergeMissing',
  replaceExisting: 'replaceExisting',
} as const

export const CREDENTIAL_IMPORT_ACTIONS = {
  added: 'added',
  skipped: 'skipped',
  merged: 'merged',
  replaced: 'replaced',
  invalid: 'invalid',
} as const

export type CredentialImportMode =
  typeof CREDENTIAL_IMPORT_MODES[keyof typeof CREDENTIAL_IMPORT_MODES]

export type CredentialImportAction =
  typeof CREDENTIAL_IMPORT_ACTIONS[keyof typeof CREDENTIAL_IMPORT_ACTIONS]

export type CredentialMetadataKey = typeof CREDENTIAL_METADATA_FIELDS[number]['key']
export type CredentialIdentityKey = typeof CREDENTIAL_IDENTITY_FIELDS[number]['key']
export interface OptionalCredentialMetadataFields extends Partial<Record<CredentialMetadataKey, unknown>> {
  groupId?: string
  tagLinks?: unknown
  hasUsageData?: boolean
  hasAvailableModelsCache?: boolean
  sourceFailureCount?: number
  sourceLastFailureAt?: string
  sourceDisabledReason?: string
  sourceSuccessCount?: number
  region?: string
  authRegion?: string
  apiRegion?: string
  machineId?: string
  endpoint?: string
  startUrl?: string
  clientIdHash?: string
  ssoSessionId?: string
  tokenEndpoint?: string
  issuerUrl?: string
  scopes?: string
}

export interface CredentialIdentityFields extends Partial<Record<CredentialIdentityKey, unknown>> {
  email?: string
  nickname?: string
  label?: string
  sourceAccountId?: string
  status?: string
  addedAt?: string
  userId?: string
}

export interface RequiredCredentialMetadataFlags {
  hasUsageData: boolean
  hasAvailableModelsCache: boolean
}

export type CredentialImportMetadataFields =
  Omit<OptionalCredentialMetadataFields, keyof RequiredCredentialMetadataFlags>
  & RequiredCredentialMetadataFlags

export type CredentialStatusMetadataFields =
  Omit<OptionalCredentialMetadataFields, 'endpoint' | 'hasUsageData' | 'hasAvailableModelsCache'>
  & {
    usageData?: unknown
    hasAvailableModelsCache: boolean
  }

export type CredentialMetadataSource = Partial<Record<CredentialMetadataKey, unknown>> & {
  hasUsageData?: boolean | null
  hasAvailableModelsCache?: boolean | null
  usageData?: unknown
}

export interface CredentialMetadataRow {
  label: string
  value: unknown
}

interface CredentialMetadataOptions {
  exclude?: readonly CredentialMetadataKey[]
}

interface CredentialIdentityOptions {
  keys?: readonly CredentialIdentityKey[]
}

function metadataFieldValue(source: CredentialMetadataSource, key: CredentialMetadataKey): unknown {
  if (key === 'hasUsageData') {
    if (source.hasUsageData) return '已保存'
    return source.usageData
  }
  if (key === 'hasAvailableModelsCache') {
    return source.hasAvailableModelsCache ? '已保存' : null
  }
  return source[key]
}

function hasMetadataValue(value: unknown): boolean {
  if (value === null || value === undefined) return false
  if (typeof value === 'string') return value.trim() !== ''
  return true
}

export function stringifyCredentialMetadataValue(value: unknown): string | null {
  if (!hasMetadataValue(value)) return null
  const text = (typeof value === 'object' ? JSON.stringify(value) : String(value)).trim()
  return text || null
}

export function compactCredentialMetadataValue(
  value: unknown,
  maxLength = 96,
  headLength = 44,
  tailLength = 28,
): string | null {
  const text = stringifyCredentialMetadataValue(value)
  if (!text) return null
  return text.length > maxLength
    ? `${text.slice(0, headLength)}...${text.slice(-tailLength)}`
    : text
}

export function getCredentialMetadataRows(
  source: CredentialMetadataSource | null | undefined,
  options: CredentialMetadataOptions = {},
): CredentialMetadataRow[] {
  if (!source) return []
  const excluded = new Set(options.exclude ?? [])
  return CREDENTIAL_METADATA_FIELDS.flatMap(({ key, label }) => {
    if (excluded.has(key)) return []
    const value = metadataFieldValue(source, key)
    return hasMetadataValue(value) ? [{ label, value }] : []
  })
}

export function getCredentialIdentityRows(
  source: CredentialIdentityFields | null | undefined,
  options: CredentialIdentityOptions = {},
): CredentialMetadataRow[] {
  if (!source) return []
  const keys = options.keys ? new Set(options.keys) : null
  return CREDENTIAL_IDENTITY_FIELDS.flatMap(({ key, label }) => {
    if (keys && !keys.has(key)) return []
    const value = source[key]
    return hasMetadataValue(value) ? [{ label, value }] : []
  })
}

export function maskEmail(email: string | null | undefined, privacyMode: boolean): string {
  if (!email) return email ?? ''
  if (!privacyMode) return email
  const [local, domain] = email.split('@')
  if (!domain) return email

  const maskedLocal = local.length <= 2
    ? '*'.repeat(local.length)
    : local.length <= 4
      ? `${local[0]}***`
      : `${local.slice(0, 2)}***${local.slice(-2)}`

  const domainParts = domain.split('.')
  const tld = domainParts[domainParts.length - 1]
  return `${maskedLocal}@***.${tld}`
}

export function getCredentialMetadataInlineLabels(
  source: CredentialMetadataSource | null | undefined,
  options: CredentialMetadataOptions = {},
): string[] {
  return getCredentialMetadataRows(source, options).flatMap(({ label, value }) => {
    const text = stringifyCredentialMetadataValue(value)
    return text ? [`${label}: ${text}`] : []
  })
}

export function formatCredentialAuthLabel(
  provider: string | null | undefined,
  authMethod: string | null | undefined,
): string {
  const cleanProvider = provider?.trim()
  const cleanAuthMethod = authMethod?.trim()
  const normalizedProvider = cleanProvider?.toLowerCase()
  const normalizedAuthMethod = cleanAuthMethod?.toLowerCase()

  if (normalizedAuthMethod === 'api_key' || normalizedAuthMethod === 'apikey' || normalizedAuthMethod === 'api-key') {
    return CREDENTIAL_AUTH_LABELS.apiKey
  }
  if (normalizedProvider === 'azuread' || normalizedProvider === 'azure ad' || normalizedProvider === 'microsoft') {
    return CREDENTIAL_AUTH_LABELS.microsoftEntra
  }
  if (normalizedProvider === 'builderid' || normalizedProvider === 'builder-id' || normalizedProvider === 'builder id') {
    return CREDENTIAL_AUTH_LABELS.awsBuilderId
  }
  if (normalizedProvider === 'enterprise') return CREDENTIAL_AUTH_LABELS.iamIdentityCenter
  if (normalizedProvider === 'github') return CREDENTIAL_AUTH_LABELS.github
  if (normalizedProvider === 'google') return CREDENTIAL_AUTH_LABELS.google
  if (normalizedProvider === 'kiro sso') return CREDENTIAL_AUTH_LABELS.kiroSso
  if (cleanProvider) return cleanProvider
  if (normalizedAuthMethod === 'external_idp' || normalizedAuthMethod === 'external-idp') return CREDENTIAL_AUTH_LABELS.microsoftEntra
  if (normalizedAuthMethod === 'idc') return CREDENTIAL_AUTH_LABELS.iamIdentityCenter
  if (normalizedAuthMethod === 'social') return CREDENTIAL_AUTH_LABELS.social
  return cleanProvider || cleanAuthMethod || ''
}

export function formatCredentialSourceFormatLabel(sourceFormat: string): string {
  switch (sourceFormat) {
    case CREDENTIAL_SOURCE_FORMATS.credentialBackup:
      return 'xkiro.rs 完整备份'
    case CREDENTIAL_SOURCE_FORMATS.cachedCredential:
      return '缓存凭据'
    case CREDENTIAL_SOURCE_FORMATS.credentialSnapshot:
      return '凭据快照'
    case CREDENTIAL_SOURCE_FORMATS.flatCredential:
      return '扁平凭据'
    case CREDENTIAL_SOURCE_FORMATS.unknown:
      return '未知来源'
    default:
      return '自定义来源'
  }
}

export function formatCredentialImportModeLabel(mode: CredentialImportMode): string {
  switch (mode) {
    case CREDENTIAL_IMPORT_MODES.skipExisting:
      return '跳过已存在'
    case CREDENTIAL_IMPORT_MODES.mergeMissing:
      return '合并缺失字段'
    case CREDENTIAL_IMPORT_MODES.replaceExisting:
      return '替换已存在'
  }
}

export function formatCredentialImportActionLabel(
  action: CredentialImportAction,
): string {
  switch (action) {
    case CREDENTIAL_IMPORT_ACTIONS.added:
      return '新增'
    case CREDENTIAL_IMPORT_ACTIONS.skipped:
      return '跳过'
    case CREDENTIAL_IMPORT_ACTIONS.merged:
      return '合并'
    case CREDENTIAL_IMPORT_ACTIONS.replaced:
      return '替换'
    case CREDENTIAL_IMPORT_ACTIONS.invalid:
      return '无效'
  }
}
