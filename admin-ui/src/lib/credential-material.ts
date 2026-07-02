export const CREDENTIAL_MATERIAL_FIELDS = [
  { key: 'hasProfileArn', wireKey: 'has_profile_arn', label: 'Profile ARN' },
  { key: 'hasToken', wireKey: 'has_token', label: '访问令牌' },
  { key: 'hasRefreshToken', wireKey: 'has_refresh_token', label: '刷新令牌' },
  { key: 'hasClientId', wireKey: 'has_client_id', label: '客户端 ID' },
  { key: 'hasClientSecret', wireKey: 'has_client_secret', label: '客户端密钥' },
  { key: 'hasIdToken', wireKey: 'has_id_token', label: 'ID 令牌' },
  { key: 'hasApiKey', wireKey: 'has_api_key', label: 'API 密钥' },
  { key: 'hasProxyCredentials', wireKey: 'has_proxy_credentials', label: '代理认证' },
] as const

export type CredentialMaterialField = typeof CREDENTIAL_MATERIAL_FIELDS[number]
export type CredentialMaterialKey = CredentialMaterialField['key']
export type CredentialMaterialFlags = Record<CredentialMaterialKey, boolean>
export type OptionalCredentialMaterialFlags = Partial<Record<CredentialMaterialKey, boolean>>
export type CredentialMaterialSource = Partial<Record<CredentialMaterialKey, boolean | null | undefined>>

export interface CredentialMaterialRow {
  label: string
  value: string | null
}

interface CredentialMaterialOptions {
  exclude?: readonly CredentialMaterialKey[]
}

export function formatCredentialMaterialPresence(value: boolean | null | undefined): string | null {
  return value ? '已保存' : null
}

export function getCredentialMaterialLabels(
  source: CredentialMaterialSource | null | undefined,
  options: CredentialMaterialOptions = {},
): string[] {
  if (!source) return []
  const excluded = new Set(options.exclude ?? [])
  return CREDENTIAL_MATERIAL_FIELDS
    .filter(({ key }) => !excluded.has(key))
    .filter(({ key }) => Boolean(source[key]))
    .map(({ label }) => label)
}

export function getCredentialMaterialRows(
  source: CredentialMaterialSource | null | undefined,
  options: CredentialMaterialOptions = {},
): CredentialMaterialRow[] {
  if (!source) return []
  const excluded = new Set(options.exclude ?? [])
  return CREDENTIAL_MATERIAL_FIELDS
    .filter(({ key }) => !excluded.has(key))
    .map(({ key, label }) => ({
      label,
      value: formatCredentialMaterialPresence(source[key]),
    }))
}
