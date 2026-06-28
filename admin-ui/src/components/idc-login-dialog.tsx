import { useState } from 'react'
import { toast } from 'sonner'
import { CheckCircle, Copy, Loader2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { completeIamSsoLogin, startIamSsoLogin } from '@/api/credentials'
import type { StartIamSsoLoginResponse } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

interface IdcLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

type Step = 'form' | 'waiting' | 'done'

export function IdcLoginDialog({ open, onOpenChange, onSuccess }: IdcLoginDialogProps) {
  const [step, setStep] = useState<Step>('form')
  const [region, setRegion] = useState('us-east-1')
  const [startUrl, setStartUrl] = useState('')
  const [callbackUrl, setCallbackUrl] = useState('')
  const [isStarting, setIsStarting] = useState(false)
  const [isCompleting, setIsCompleting] = useState(false)
  const [session, setSession] = useState<StartIamSsoLoginResponse | null>(null)
  const [credentialId, setCredentialId] = useState<number | null>(null)

  const reset = () => {
    setStep('form')
    setSession(null)
    setCallbackUrl('')
    setCredentialId(null)
    setIsStarting(false)
    setIsCompleting(false)
  }

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) reset()
    onOpenChange(nextOpen)
  }

  const handleStart = async () => {
    const trimmedRegion = region.trim()
    const trimmedStartUrl = startUrl.trim()
    if (!trimmedRegion) {
      toast.error('请填写 AWS Region')
      return
    }
    if (!trimmedStartUrl) {
      toast.error('请填写 IAM Identity Center 的 SSO Start URL')
      return
    }

    setIsStarting(true)
    try {
      const response = await startIamSsoLogin({
        region: trimmedRegion,
        startUrl: trimmedStartUrl,
      })
      const sessionId = response.sessionId.trim()
      const authorizeUrl = response.authorizeUrl.trim()
      if (!sessionId || !authorizeUrl) {
        toast.error('后端返回的 IAM SSO 授权链接为空，请检查服务日志')
        return
      }
      setSession({ ...response, sessionId, authorizeUrl })
      setStep('waiting')
    } catch (error) {
      toast.error('发起登录失败：' + extractErrorMessage(error))
    } finally {
      setIsStarting(false)
    }
  }

  const copyAuthorizeUrl = async () => {
    if (!session) return
    try {
      await navigator.clipboard.writeText(session.authorizeUrl)
      toast.success('授权链接已复制')
    } catch (error) {
      toast.error('复制失败：' + extractErrorMessage(error))
    }
  }

  const handleComplete = async () => {
    if (!session) return
    const trimmedCallbackUrl = callbackUrl.trim()
    if (!trimmedCallbackUrl) {
      toast.error('请粘贴浏览器跳转后的回调 URL')
      return
    }

    setIsCompleting(true)
    try {
      const response = await completeIamSsoLogin(session.sessionId, trimmedCallbackUrl)
      if (!response.success || !response.account?.id) {
        toast.error('授权完成失败')
        return
      }
      setCredentialId(response.account.id)
      setStep('done')
      onSuccess()
      toast.success(`登录成功，已添加凭据 #${response.account.id}`)
    } catch (error) {
      toast.error('完成登录失败：' + extractErrorMessage(error))
    } finally {
      setIsCompleting(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>IAM Identity Center 登录</DialogTitle>
          <DialogDescription>
            使用 AWS IAM Identity Center 授权码流程添加企业账号。
          </DialogDescription>
        </DialogHeader>

        {step === 'form' && (
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <label htmlFor="idc-region" className="text-sm font-medium">AWS Region</label>
              <Input
                id="idc-region"
                placeholder="us-east-1"
                value={region}
                onChange={(event) => setRegion(event.target.value)}
              />
            </div>
            <div className="space-y-1.5">
              <label htmlFor="idc-start-url" className="text-sm font-medium">
                SSO Start URL
              </label>
              <Input
                id="idc-start-url"
                placeholder="https://d-xxxxxxxxxx.awsapps.com/start"
                value={startUrl}
                onChange={(event) => setStartUrl(event.target.value)}
              />
            </div>
          </div>
        )}

        {step === 'waiting' && session && (
          <div className="space-y-4 py-2">
            <div className="space-y-3 rounded-lg border bg-muted/50 p-4">
              <p className="text-sm text-muted-foreground">
                复制授权链接到无痕窗口、隐私窗口或其他浏览器中访问。完成登录后，浏览器会跳转到 127.0.0.1 的回调地址；如果页面打不开，复制地址栏中的完整 URL 粘贴到下面。
              </p>
              <code className="block rounded-md border bg-background p-2 text-xs font-mono break-all">
                {session.authorizeUrl}
              </code>
              <Button type="button" variant="outline" size="sm" onClick={copyAuthorizeUrl}>
                <Copy className="mr-2 h-3.5 w-3.5" />
                复制授权链接
              </Button>
            </div>
            <div className="space-y-1.5">
              <label htmlFor="idc-callback-url" className="text-sm font-medium">回调 URL</label>
              <Input
                id="idc-callback-url"
                placeholder="http://127.0.0.1/oauth/callback?code=..."
                value={callbackUrl}
                onChange={(event) => setCallbackUrl(event.target.value)}
              />
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
          {step === 'form' && (
            <Button onClick={handleStart} disabled={isStarting}>
              {isStarting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              发起登录
            </Button>
          )}
          {step === 'waiting' && (
            <>
              <Button variant="outline" onClick={() => handleOpenChange(false)}>
                取消
              </Button>
              <Button onClick={handleComplete} disabled={isCompleting}>
                {isCompleting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                完成登录
              </Button>
            </>
          )}
          {step === 'done' && (
            <Button onClick={() => handleOpenChange(false)}>关闭</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
