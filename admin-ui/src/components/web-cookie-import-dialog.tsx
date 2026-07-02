import { useState } from 'react'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, Loader2, Cookie } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { importCredentialRecord } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import { CREDENTIAL_AUTH_LABELS, formatCredentialAuthLabel } from '@/lib/credential-metadata'

interface WebCookieImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

export function WebCookieImportDialog({ open, onOpenChange, onSuccess }: WebCookieImportDialogProps) {
  const [cookie, setCookie] = useState('')
  const [provider, setProvider] = useState<'Google' | 'GitHub'>('Google')
  const [importing, setImporting] = useState(false)
  const [result, setResult] = useState<{ status: 'success' | 'error'; authLabel?: string; error?: string } | null>(null)

  const handleImport = async () => {
    if (!cookie.trim()) {
      toast.error('请输入刷新令牌 Cookie')
      return
    }

    setImporting(true)
    try {
      // 从 cookie 中提取 refreshToken
      // cookie 格式: RefreshToken=<value>
      let refreshToken = cookie.trim()
      if (refreshToken.includes('RefreshToken=')) {
        const match = refreshToken.match(/RefreshToken=([^;]+)/)
        if (match) {
          refreshToken = match[1]
        }
      }

      const added = await importCredentialRecord({
        refreshToken,
        authMethod: 'social',
        provider,
      })

      const authLabel = formatCredentialAuthLabel(added.provider, added.authMethod)
      setResult({ status: 'success', authLabel })
      toast.success(`导入成功，已添加${authLabel ? ` ${authLabel}` : ''} 凭据 #${added.credentialId}`)
      onSuccess()
    } catch (error) {
      setResult({ status: 'error', error: extractErrorMessage(error) })
      toast.error(`导入失败: ${extractErrorMessage(error)}`)
    } finally {
      setImporting(false)
    }
  }

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) {
      setCookie('')
      setProvider('Google')
      setResult(null)
    }
    onOpenChange(nextOpen)
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Cookie className="h-5 w-5" />
            浏览器 Cookie 导入
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">
              刷新令牌 Cookie
            </label>
            <textarea
              className="w-full h-24 p-3 text-sm border rounded-md resize-none font-mono"
              placeholder={"从浏览器 DevTools → Application → Cookies 中复制\nhttps://app.kiro.dev 的刷新令牌 Cookie 值"}
              value={cookie}
              onChange={e => setCookie(e.target.value)}
              disabled={importing}
            />
            <p className="text-xs text-muted-foreground">
              登录 https://app.kiro.dev 后，从浏览器 Cookie 中获取刷新令牌
            </p>
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium">登录方式</label>
            <div className="flex gap-2">
              <Button
                variant={provider === 'Google' ? 'default' : 'outline'}
                size="sm"
                onClick={() => setProvider('Google')}
                disabled={importing}
              >
                {CREDENTIAL_AUTH_LABELS.google}
              </Button>
              <Button
                variant={provider === 'GitHub' ? 'default' : 'outline'}
                size="sm"
                onClick={() => setProvider('GitHub')}
                disabled={importing}
              >
                {CREDENTIAL_AUTH_LABELS.github}
              </Button>
            </div>
          </div>

          {result && (
            <div
              className={`p-3 rounded-md text-sm ${
                result.status === 'success'
                  ? 'bg-green-50 dark:bg-green-950/30 border border-green-200 dark:border-green-800'
                  : 'bg-red-50 dark:bg-red-950/30 border border-red-200 dark:border-red-800'
              }`}
            >
              <div className="flex items-center gap-2">
                {result.status === 'success' ? (
                  <CheckCircle2 className="h-4 w-4 text-green-600" />
                ) : (
                  <XCircle className="h-4 w-4 text-red-600" />
                )}
                <span>{result.status === 'success' ? '导入成功！' : '导入失败'}</span>
              </div>
              {result.status === 'success' && result.authLabel && (
                <p className="mt-1 text-xs text-muted-foreground">{result.authLabel}</p>
              )}
              {result.error && (
                <p className="mt-1 text-xs text-red-600">{result.error}</p>
              )}
            </div>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => handleOpenChange(false)}>
            取消
          </Button>
          <Button onClick={handleImport} disabled={importing || !cookie.trim()}>
            {importing ? (
              <>
                <Loader2 className="h-4 w-4 mr-2 animate-spin" />
                导入中...
              </>
            ) : (
              '导入'
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
