import { useState } from 'react'
import { toast } from 'sonner'
import type { CredentialLoginDetails } from '@/types/api'

/**
 * 登录流程的成功态收尾（social / idc / builderid / kiro-sso 共用）。
 *
 * 四个登录对话框的 form/waiting 阶段各不相同（设备码轮询 / 手动回调 / PKCE / 托管），
 * 但成功收尾完全一致：记录 credentialId+authLabel+details、切到 done、弹同一条 toast、
 * 关闭时重置这些字段。此 hook 抽取这段共同逻辑（ER-8），form/waiting 仍由各对话框自持。
 */
export type LoginStep = 'form' | 'waiting' | 'done'

export interface LoginResult {
  step: LoginStep
  setStep: (step: LoginStep) => void
  credentialId: number | null
  credentialAuthLabel: string
  credentialDetails: CredentialLoginDetails | null
  /** 记录成功凭据、切到 done、触发 onSuccess、弹统一 toast。 */
  markSuccess: (id: number, authLabel: string, details: CredentialLoginDetails | null) => void
  /** 重置成功态字段并回到 form（关闭对话框时调用）。 */
  reset: () => void
}

export function useLoginResult(onSuccess: () => void): LoginResult {
  const [step, setStep] = useState<LoginStep>('form')
  const [credentialId, setCredentialId] = useState<number | null>(null)
  const [credentialAuthLabel, setCredentialAuthLabel] = useState('')
  const [credentialDetails, setCredentialDetails] = useState<CredentialLoginDetails | null>(null)

  const markSuccess = (
    id: number,
    authLabel: string,
    details: CredentialLoginDetails | null,
  ) => {
    setCredentialId(id)
    setCredentialAuthLabel(authLabel)
    setCredentialDetails(details)
    setStep('done')
    onSuccess()
    toast.success(`登录成功，已添加${authLabel ? ` ${authLabel}` : ''} 凭据 #${id}`)
  }

  const reset = () => {
    setStep('form')
    setCredentialId(null)
    setCredentialAuthLabel('')
    setCredentialDetails(null)
  }

  return {
    step,
    setStep,
    credentialId,
    credentialAuthLabel,
    credentialDetails,
    markSuccess,
    reset,
  }
}
