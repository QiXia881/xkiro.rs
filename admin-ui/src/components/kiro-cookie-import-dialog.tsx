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
import { importKiroGoCredential } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface KiroCookieImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

export function KiroCookieImportDialog({ open, onOpenChange, onSuccess }: KiroCookieImportDialogProps) {
  const [cookie, setCookie] = useState('')
  const [provider, setProvider] = useState<'Google' | 'Github'>('Google')
  const [importing, setImporting] = useState(false)
  const [result, setResult] = useState<{ status: 'success' | 'error'; error?: string } | null>(null)

  const handleImport = async () => {
    if (!cookie.trim()) {
      toast.error('请输入 RefreshToken cookie')
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

      await importKiroGoCredential({
        refreshToken,
        authMethod: 'social',
        provider,
      })

      setResult({ status: 'success' })
      toast.success('导入成功！')
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
            Kiro Web Cookie 导入
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">
              RefreshToken Cookie
            </label>
            <textarea
              className="w-full h-24 p-3 text-sm border rounded-md resize-none font-mono"
              placeholder={"从浏览器 DevTools → Application → Cookies 中复制\nhttps://app.kiro.dev 的 RefreshToken 值"}
              value={cookie}
              onChange={e => setCookie(e.target.value)}
              disabled={importing}
            />
            <p className="text-xs text-muted-foreground">
              登录 https://app.kiro.dev 后，从浏览器 Cookie 中获取 RefreshToken
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
                Google
              </Button>
              <Button
                variant={provider === 'Github' ? 'default' : 'outline'}
                size="sm"
                onClick={() => setProvider('Github')}
                disabled={importing}
              >
                GitHub
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
