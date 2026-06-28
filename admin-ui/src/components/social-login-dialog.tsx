import { useEffect, useRef, useState } from 'react'
import { CheckCircle, Copy, Loader2 } from 'lucide-react'
import { toast } from 'sonner'
import { completeSocialLoginCallback, startSocialLogin, pollSocialLogin } from '@/api/credentials'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import type { StartSocialLoginResponse } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'
import { storage } from '@/lib/storage'

interface SocialLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

type Step = 'form' | 'waiting' | 'done'
type SocialProvider = 'Github' | 'Google'

const POLL_INTERVAL_MS = 2000

export function SocialLoginDialog({ open, onOpenChange, onSuccess }: SocialLoginDialogProps) {
  const [step, setStep] = useState<Step>('form')
  const [provider, setProvider] = useState<SocialProvider>('Github')
  const [priority, setPriority] = useState('0')
  const [email, setEmail] = useState('')
  const [isStarting, setIsStarting] = useState(false)
  const [isSubmittingCallback, setIsSubmittingCallback] = useState(false)
  const [callbackUrl, setCallbackUrl] = useState('')
  const [session, setSession] = useState<StartSocialLoginResponse | null>(null)
  const [credentialId, setCredentialId] = useState<number | null>(null)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    return () => {
      if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
    }
  }, [])

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) {
      if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
      setStep('form')
      setProvider('Github')
      setPriority('0')
      setEmail('')
      setIsStarting(false)
      setIsSubmittingCallback(false)
      setCallbackUrl('')
      setSession(null)
      setCredentialId(null)
    }
    onOpenChange(nextOpen)
  }

  const schedulePoll = (sessionId: string) => {
    pollTimerRef.current = setTimeout(async () => {
      try {
        const result = await pollSocialLogin(sessionId)
        if (result.status === 'waiting') {
          schedulePoll(sessionId)
          return
        }
        if (result.status === 'success') {
          setCredentialId(result.credentialId)
          setStep('done')
          onSuccess()
          toast.success(`登录成功，已添加凭据 #${result.credentialId}`)
          return
        }
        if (result.status === 'error') {
          toast.error(`登录失败：${result.message}`)
          setStep('form')
          setSession(null)
          return
        }
        toast.error('登录会话已过期，请重新发起')
        setStep('form')
        setSession(null)
      } catch (error) {
        toast.error(`轮询失败：${extractErrorMessage(error)}`)
        schedulePoll(sessionId)
      }
    }, POLL_INTERVAL_MS)
  }

  const handleStart = async () => {
    setIsStarting(true)
    try {
      const nextSession = await startSocialLogin({
        provider,
        priority: parseInt(priority, 10) || 0,
        email: email.trim() || undefined,
        mode: 'manual',
      })
      setSession(nextSession)
      setStep('waiting')
      schedulePoll(nextSession.sessionId)
    } catch (error) {
      toast.error(`发起登录失败：${extractErrorMessage(error)}`)
    } finally {
      setIsStarting(false)
    }
  }

  const buildHelperCommand = (sessionId: string) => {
    const apiKey = storage.getApiKey() ?? '<ADMIN_API_KEY>'
    const server = window.location.origin
    return [
      'xkiro social-helper',
      `--server ${server}`,
      `--session ${sessionId}`,
      `--provider ${provider}`,
      `--api-key ${apiKey}`,
    ].join(' ')
  }

  const handleCopyCommand = (command: string) => {
    navigator.clipboard
      .writeText(command)
      .then(() => toast.success('已复制命令到剪贴板'))
      .catch(() => toast.error('复制失败，请手动选择复制'))
  }

  const handleCopyPortalUrl = (portalUrl: string) => {
    navigator.clipboard
      .writeText(portalUrl)
      .then(() => toast.success('登录链接已复制'))
      .catch(() => toast.error('复制失败，请手动选择复制'))
  }

  const handleSubmitCallback = async () => {
    if (!session?.sessionId) return
    const trimmedCallbackUrl = callbackUrl.trim()
    if (!trimmedCallbackUrl) {
      toast.error('请粘贴浏览器地址栏中的 localhost 回调 URL')
      return
    }

    setIsSubmittingCallback(true)
    try {
      const result = await completeSocialLoginCallback(session.sessionId, trimmedCallbackUrl)
      if (result.status === 'success') {
        if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
        setCredentialId(result.credentialId)
        setStep('done')
        setCallbackUrl('')
        onSuccess()
        toast.success(`登录成功，已添加凭据 #${result.credentialId}`)
        return
      }
      if (result.status === 'error') {
        toast.error(`登录失败：${result.message}`)
        return
      }
      if (result.status === 'expired') {
        if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
        toast.error('登录会话已过期，请重新发起')
        setStep('form')
        setSession(null)
        return
      }
      toast.info('回调已提交，继续等待登录完成')
    } catch (error) {
      toast.error(`提交回调失败：${extractErrorMessage(error)}`)
    } finally {
      setIsSubmittingCallback(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Kiro 账号登录</DialogTitle>
          <DialogDescription>
            复制登录地址到无痕窗口、隐私窗口或其他浏览器中访问；回调页面打不开时，将地址栏完整 URL 粘贴回来完成登录。
          </DialogDescription>
        </DialogHeader>

        {step === 'form' && (
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <div className="text-sm font-medium">登录提供方</div>
              <div className="grid grid-cols-2 gap-2">
                <Button
                  type="button"
                  variant={provider === 'Github' ? 'default' : 'outline'}
                  onClick={() => setProvider('Github')}
                >
                  GitHub
                </Button>
                <Button
                  type="button"
                  variant={provider === 'Google' ? 'default' : 'outline'}
                  onClick={() => setProvider('Google')}
                >
                  Google
                </Button>
              </div>
            </div>

            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-1.5">
                <label htmlFor="social-priority" className="text-sm font-medium">优先级</label>
                <Input
                  id="social-priority"
                  type="number"
                  min="0"
                  value={priority}
                  onChange={(event) => setPriority(event.target.value)}
                />
              </div>
              <div className="space-y-1.5">
                <label htmlFor="social-email" className="text-sm font-medium">邮箱（可选）</label>
                <Input
                  id="social-email"
                  placeholder="user@example.com"
                  value={email}
                  onChange={(event) => setEmail(event.target.value)}
                />
              </div>
            </div>
          </div>
        )}

        {step === 'waiting' && session && session.mode === 'helper' && (
          <div className="space-y-4 py-2">
            <div className="rounded-lg border bg-muted/50 p-4 space-y-3">
              <p className="text-sm text-muted-foreground">
                请在你的本机终端运行下面的命令。命令会输出 {provider} 登录地址，请复制到无痕窗口、隐私窗口或其他浏览器中访问；授权后会自动把凭据回传到此服务。
              </p>
              <div className="relative">
                <pre className="overflow-x-auto rounded-md bg-background border px-3 py-2 pr-10 text-xs font-mono whitespace-pre-wrap break-all">
                  {buildHelperCommand(session.sessionId)}
                </pre>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="absolute right-1 top-1 h-7 w-7"
                  onClick={() => handleCopyCommand(buildHelperCommand(session.sessionId))}
                  aria-label="复制命令"
                >
                  <Copy className="h-3.5 w-3.5" />
                </Button>
              </div>
              <p className="text-xs text-muted-foreground">
                命令中包含 Admin API Key，请勿分享给他人。需本机已安装与服务端同版本的 xkiro.rs。
              </p>
            </div>

            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              正在等待本机 helper 回传凭据…
            </div>
          </div>
        )}

        {step === 'waiting' && session && session.mode !== 'helper' && (
          <div className="space-y-4 py-2">
            <div className="rounded-lg border bg-muted/50 p-4 space-y-3">
              <p className="text-sm text-muted-foreground">
                请复制下面的链接到无痕窗口、隐私窗口或其他浏览器中完成 {provider} 登录。授权完成后，
                浏览器会跳到 `127.0.0.1` 回调地址；如果页面打不开，请复制地址栏完整 URL 粘贴到下方。
              </p>
              <div className="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-300">
                请务必复制到无痕窗口、隐私窗口或其他浏览器中访问。普通窗口会自动沿用
                上一个已登录的 {provider} 账号，导致无法添加新账号。
              </div>
              {session.portalUrl && (
                <div className="space-y-2">
                  <code className="block rounded-md border bg-background p-2 text-xs font-mono break-all">
                    {session.portalUrl}
                  </code>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => handleCopyPortalUrl(session.portalUrl!)}
                  >
                    <Copy className="mr-2 h-3.5 w-3.5" />
                    复制登录链接
                  </Button>
                </div>
              )}
            </div>

            <div className="space-y-1.5">
              <label htmlFor="social-callback-url" className="text-sm font-medium">回调 URL</label>
              <Input
                id="social-callback-url"
                placeholder="http://127.0.0.1:3128/oauth/callback?code=..."
                value={callbackUrl}
                onChange={(event) => setCallbackUrl(event.target.value)}
              />
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={handleSubmitCallback}
                disabled={isSubmittingCallback}
              >
                {isSubmittingCallback && <Loader2 className="mr-2 h-3.5 w-3.5 animate-spin" />}
                提交回调 URL
              </Button>
            </div>

            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              正在等待 OAuth 回调 URL…
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
            <Button variant="outline" onClick={() => handleOpenChange(false)}>
              关闭
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
