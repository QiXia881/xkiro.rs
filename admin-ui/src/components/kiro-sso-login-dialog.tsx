import { useEffect, useRef, useState } from 'react'
import { CheckCircle, Copy, Loader2 } from 'lucide-react'
import { toast } from 'sonner'
import {
  cancelKiroSsoLogin,
  completeKiroSsoLogin,
  pollKiroSsoLogin,
  startKiroSsoLogin,
} from '@/api/credentials'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { StartKiroSsoLoginResponse } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

interface KiroSsoLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

type Step = 'intro' | 'waiting' | 'done'

export function KiroSsoLoginDialog({ open, onOpenChange, onSuccess }: KiroSsoLoginDialogProps) {
  const [step, setStep] = useState<Step>('intro')
  const [session, setSession] = useState<StartKiroSsoLoginResponse | null>(null)
  const [credentialId, setCredentialId] = useState<number | null>(null)
  const [starting, setStarting] = useState(false)
  const [submittingCallback, setSubmittingCallback] = useState(false)
  const [callbackUrl, setCallbackUrl] = useState('')
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    return () => {
      if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
    }
  }, [])

  const reset = async (shouldCancel: boolean) => {
    if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
    if (shouldCancel && session?.sessionId) {
      cancelKiroSsoLogin(session.sessionId).catch(() => {})
    }
    setStep('intro')
    setSession(null)
    setCredentialId(null)
    setStarting(false)
    setSubmittingCallback(false)
    setCallbackUrl('')
  }

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) reset(step === 'waiting')
    onOpenChange(nextOpen)
  }

  const schedulePoll = (sessionId: string, intervalSeconds: number) => {
    pollTimerRef.current = setTimeout(async () => {
      try {
        const result = await pollKiroSsoLogin(sessionId)
        if (result.success && !result.completed) {
          schedulePoll(sessionId, intervalSeconds)
          return
        }
        if (result.success && result.completed && result.account) {
          setCredentialId(result.account.id)
          setStep('done')
          onSuccess()
          toast.success(`登录成功，已添加凭据 #${result.account.id}`)
          return
        }
        toast.error(`登录失败：${result.error ?? '未知错误'}`)
        setStep('intro')
        setSession(null)
      } catch (error) {
        toast.error(`轮询失败：${extractErrorMessage(error)}`)
        schedulePoll(sessionId, intervalSeconds)
      }
    }, intervalSeconds * 1000)
  }

  const handleStart = async () => {
    setStarting(true)
    try {
      const next = await startKiroSsoLogin()
      if (!next.sessionId || !next.signInUrl) {
        toast.error('后端返回的 Enterprise SSO 登录链接为空，请检查服务日志')
        return
      }
      setSession(next)
      setStep('waiting')
      schedulePoll(next.sessionId, next.interval || 2)
    } catch (error) {
      toast.error(`发起登录失败：${extractErrorMessage(error)}`)
    } finally {
      setStarting(false)
    }
  }

  const copyUrl = async () => {
    if (!session?.signInUrl) return
    try {
      await navigator.clipboard.writeText(session.signInUrl)
      toast.success('登录链接已复制')
    } catch (error) {
      toast.error(`复制失败：${extractErrorMessage(error)}`)
    }
  }

  const handleSubmitCallback = async () => {
    if (!session?.sessionId) return
    const trimmedCallbackUrl = callbackUrl.trim()
    if (!trimmedCallbackUrl) {
      toast.error('请粘贴浏览器地址栏中的 localhost 回调 URL')
      return
    }

    setSubmittingCallback(true)
    try {
      const result = await completeKiroSsoLogin(session.sessionId, trimmedCallbackUrl)
      if (result.status === 'redirect' && !result.redirectUrl) {
        toast.error('后端返回的下一步登录链接为空，请检查服务日志')
        return
      }
      if (result.status === 'redirect' && result.redirectUrl) {
        setSession({ ...session, signInUrl: result.redirectUrl })
        setCallbackUrl('')
        toast.success('已生成 Microsoft 登录链接，请复制到无痕窗口或其他浏览器完成登录')
        return
      }
      if (result.status === 'submitted') {
        setCallbackUrl('')
        toast.success('回调已提交，正在创建凭据')
        return
      }
      if (!result.success) {
        toast.error(`回调提交失败：${result.error ?? '未知错误'}`)
        return
      }
      toast.info('回调已接收，继续等待登录完成')
    } catch (error) {
      toast.error(`回调提交失败：${extractErrorMessage(error)}`)
    } finally {
      setSubmittingCallback(false)
    }
  }

  const loginLinkLabel = session?.signInUrl.includes('microsoftonline.')
    ? 'Microsoft / Entra ID 登录地址'
    : 'Kiro Enterprise SSO 入口地址'

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Enterprise SSO - Microsoft 365</DialogTitle>
          <DialogDescription>
            使用 Kiro 托管登录页添加 Microsoft 365 / Entra ID（Azure AD）租户账号。
          </DialogDescription>
        </DialogHeader>

        {step === 'intro' && (
          <div className="space-y-3 py-2 text-sm text-muted-foreground">
            <p>启动后会生成 Kiro Enterprise SSO 入口地址，不会自动打开浏览器标签页。</p>
            <p>请复制地址到无痕窗口、隐私窗口或其他浏览器中访问；遇到 localhost 回调无法访问时，将地址栏完整 URL 粘贴回来继续。</p>
          </div>
        )}

        {step === 'waiting' && session && (
          <div className="space-y-4 py-2">
            <div className="rounded-lg border bg-muted/50 p-4 space-y-3">
              <p className="text-sm text-muted-foreground">
                复制当前地址到无痕窗口、隐私窗口或其他浏览器中访问。远程管理时，遇到 localhost 回调页面无法访问，请复制地址栏完整 URL 粘贴到下方。
              </p>
              <div className="space-y-1.5">
                <p className="text-xs text-muted-foreground">{loginLinkLabel}</p>
                <code className="block rounded-md border bg-background p-2 text-xs font-mono break-all">
                  {session.signInUrl}
                </code>
              </div>
              <Button type="button" variant="outline" size="sm" onClick={copyUrl}>
                <Copy className="mr-2 h-3.5 w-3.5" />
                复制登录链接
              </Button>
            </div>
            <div className="space-y-1.5">
              <label htmlFor="kiro-sso-callback-url" className="text-sm font-medium">回调 URL</label>
              <Input
                id="kiro-sso-callback-url"
                placeholder="http://localhost:3128/signin/callback?... 或 /oauth/callback?code=..."
                value={callbackUrl}
                onChange={(event) => setCallbackUrl(event.target.value)}
              />
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={handleSubmitCallback}
                disabled={submittingCallback}
              >
                {submittingCallback && <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" />}
                提交回调 URL
              </Button>
            </div>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              正在等待 Kiro Enterprise SSO 回调...
            </div>
          </div>
        )}

        {step === 'done' && (
          <div className="flex flex-col items-center gap-3 py-4">
            <CheckCircle className="h-10 w-10 text-green-500" />
            <p className="text-sm font-medium">登录成功</p>
            <p className="text-xs text-muted-foreground">凭据 #{credentialId} 已添加并启用</p>
          </div>
        )}

        <DialogFooter>
          {step === 'intro' && (
            <Button onClick={handleStart} disabled={starting}>
              {starting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              发起登录
            </Button>
          )}
          {step === 'waiting' && (
            <Button variant="outline" onClick={() => handleOpenChange(false)}>
              取消
            </Button>
          )}
          {step === 'done' && (
            <Button onClick={() => handleOpenChange(false)}>关闭</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
