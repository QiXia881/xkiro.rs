import { useEffect, useRef, useState } from 'react'
import { toast } from 'sonner'
import { CheckCircle, Copy, Loader2, Shield } from 'lucide-react'
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
import { pollBuilderIdLogin, startBuilderIdLogin } from '@/api/credentials'
import type { StartBuilderIdLoginResponse } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

interface BuilderIdLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

type Step = 'form' | 'waiting' | 'done'

export function BuilderIdLoginDialog({ open, onOpenChange, onSuccess }: BuilderIdLoginDialogProps) {
  const [step, setStep] = useState<Step>('form')
  const [region, setRegion] = useState('us-east-1')
  const [isStarting, setIsStarting] = useState(false)
  const [session, setSession] = useState<StartBuilderIdLoginResponse | null>(null)
  const [credentialId, setCredentialId] = useState<number | null>(null)
  const pollingRef = useRef(false)

  useEffect(() => {
    if (step !== 'waiting' || !session) return

    const poll = async () => {
      if (pollingRef.current) return
      pollingRef.current = true
      try {
        const result = await pollBuilderIdLogin(session.sessionId)
        if (result.status === 'success') {
          setCredentialId(result.credentialId)
          setStep('done')
          onSuccess()
          toast.success(`登录成功，已添加凭据 #${result.credentialId}`)
        } else if (result.status === 'expired') {
          toast.error('Builder ID 授权已过期，请重新开始')
          setStep('form')
          setSession(null)
        } else if (result.status === 'error') {
          toast.error(`Builder ID 授权失败: ${result.message}`)
          setStep('form')
          setSession(null)
        } else if (result.pollInterval && result.pollInterval !== session.pollInterval) {
          setSession({ ...session, pollInterval: result.pollInterval })
        }
      } catch (error) {
        toast.error(`轮询授权失败: ${extractErrorMessage(error)}`)
      } finally {
        pollingRef.current = false
      }
    }

    void poll()
    const intervalMs = Math.max(session.pollInterval || 5, 1) * 1000
    const timer = window.setInterval(poll, intervalMs)
    return () => window.clearInterval(timer)
  }, [step, session, onSuccess])

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) {
      setStep('form')
      setSession(null)
      setCredentialId(null)
      setIsStarting(false)
      pollingRef.current = false
    }
    onOpenChange(nextOpen)
  }

  const handleStart = async () => {
    setIsStarting(true)
    try {
      const result = await startBuilderIdLogin({
        region: region || undefined,
      })
      const sessionId = result.sessionId.trim()
      const verificationUri = result.verificationUri.trim()
      if (!sessionId || !verificationUri) {
        toast.error('后端返回的 Builder ID 验证地址为空，请检查服务日志')
        return
      }
      setSession({ ...result, sessionId, verificationUri })
      setStep('waiting')
    } catch (error) {
      toast.error(`启动失败: ${extractErrorMessage(error)}`)
    } finally {
      setIsStarting(false)
    }
  }

  const copyToClipboard = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text)
      toast.success('已复制到剪贴板')
    } catch (error) {
      toast.error(`复制失败: ${extractErrorMessage(error)}`)
    }
  }

  const verificationUri = session?.verificationUri || ''

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Shield className="h-5 w-5" />
            AWS Builder ID 登录
          </DialogTitle>
          <DialogDescription>
            使用 AWS Builder ID 设备验证码流程添加凭据
          </DialogDescription>
        </DialogHeader>

        {step === 'form' && (
          <div className="space-y-4 py-4">
            <div className="space-y-2">
              <label className="text-sm font-medium">Region</label>
              <Input
                value={region}
                onChange={e => setRegion(e.target.value)}
                placeholder="us-east-1"
                disabled={isStarting}
              />
            </div>
          </div>
        )}

        {step === 'waiting' && session && (
          <div className="space-y-4 py-4">
            <div className="p-4 bg-muted rounded-lg space-y-3">
              <p className="text-sm font-medium">请在浏览器中输入验证码完成授权：</p>
              <div className="rounded-md border bg-background p-4 text-center">
                <p className="text-xs text-muted-foreground">验证码</p>
                <p className="mt-1 text-3xl font-semibold tracking-widest">{session.userCode}</p>
              </div>
              <div className="space-y-1.5">
                <p className="text-xs text-muted-foreground">验证地址</p>
                <div className="flex items-center gap-2">
                  <code className="flex-1 p-2 bg-background rounded text-xs font-mono break-all">
                    {verificationUri}
                  </code>
                  <Button
                    variant="outline"
                    size="icon"
                    onClick={() => copyToClipboard(verificationUri)}
                    disabled={!verificationUri}
                  >
                    <Copy className="h-4 w-4" />
                  </Button>
                </div>
              </div>
              <p className="text-xs text-muted-foreground">
                请复制验证地址到无痕窗口、隐私窗口或其他浏览器中访问。
              </p>
            </div>
            <div className="flex items-center justify-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              等待授权完成，系统会自动轮询登录结果
            </div>
          </div>
        )}

        {step === 'done' && credentialId && (
          <div className="space-y-4 py-4">
            <div className="flex items-center gap-3 p-4 bg-green-50 dark:bg-green-950/30 rounded-lg border border-green-200 dark:border-green-800">
              <CheckCircle className="h-5 w-5 text-green-600 shrink-0" />
              <div>
                <p className="font-medium text-green-800 dark:text-green-200">登录成功！</p>
                <p className="text-sm text-green-600 dark:text-green-400">
                  凭据 ID: {credentialId}
                </p>
              </div>
            </div>
          </div>
        )}

        <DialogFooter>
          {step === 'form' && (
            <>
              <Button variant="outline" onClick={() => handleOpenChange(false)}>
                取消
              </Button>
              <Button onClick={handleStart} disabled={isStarting}>
                {isStarting ? (
                  <>
                    <Loader2 className="h-4 w-4 mr-2 animate-spin" />
                    启动中...
                  </>
                ) : (
                  '开始授权'
                )}
              </Button>
            </>
          )}
          {step === 'waiting' && (
            <Button variant="outline" onClick={() => handleOpenChange(false)}>
              取消
            </Button>
          )}
          {step === 'done' && (
            <Button onClick={() => handleOpenChange(false)}>完成</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
