import { CheckCircle } from 'lucide-react'
import { CredentialLoginSummary } from '@/components/credential-login-summary'
import type { CredentialLoginDetails } from '@/types/api'

interface LoginSuccessViewProps {
  credentialId: number | null
  authLabel: string
  details: CredentialLoginDetails | null
}

/**
 * 登录成功态视图（social / idc / builderid / kiro-sso 共用的 done 阶段，ER-8）。
 * 绿色对勾 + "登录成功" + 凭据摘要，四个对话框此段完全一致。
 */
export function LoginSuccessView({ credentialId, authLabel, details }: LoginSuccessViewProps) {
  return (
    <div className="flex flex-col items-center gap-3 py-4">
      <CheckCircle className="h-10 w-10 text-green-500" />
      <p className="text-sm font-medium">登录成功</p>
      <CredentialLoginSummary
        credentialId={credentialId}
        authLabel={authLabel}
        details={details}
      />
    </div>
  )
}
