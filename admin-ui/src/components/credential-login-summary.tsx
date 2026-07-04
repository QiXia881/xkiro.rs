import type { CredentialLoginDetails } from '@/types/api'
import { getCredentialMaterialLabels as materialLabels } from '@/lib/credential-material'
import {
  compactCredentialMetadataValue,
  CREDENTIAL_SUMMARY_IDENTITY_KEYS,
  getCredentialIdentityRows,
  getCredentialMetadataRows,
} from '@/lib/credential-metadata'

interface CredentialLoginSummaryProps {
  credentialId: number | null
  authLabel: string
  details?: CredentialLoginDetails | null
}

export function CredentialLoginSummary({
  credentialId,
  authLabel,
  details,
}: CredentialLoginSummaryProps) {
  const rows = [
    { label: '凭据 ID', value: credentialId ?? details?.id },
    { label: '认证', value: authLabel },
    ...getCredentialIdentityRows(details, {
      keys: CREDENTIAL_SUMMARY_IDENTITY_KEYS,
    }),
    ...getCredentialMetadataRows(details),
    { label: '材料', value: materialLabels(details, { exclude: ['hasToken', 'hasRefreshToken'] }).join(', ') },
  ]
    .map((row) => ({ ...row, value: compactCredentialMetadataValue(row.value, 88, 40, 24) }))
    .filter((row) => row.value)

  if (rows.length === 0) return null

  return (
    <div className="w-full rounded-lg border bg-muted/40 p-3 text-left">
      <div className="grid gap-1.5 text-xs">
        {rows.map((row) => (
          <div key={row.label} className="grid grid-cols-[88px_1fr] gap-2">
            <span className="text-muted-foreground">{row.label}</span>
            <span className="break-all font-mono">{row.value}</span>
          </div>
        ))}
      </div>
    </div>
  )
}
