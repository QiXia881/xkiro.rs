import { useState } from 'react'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, Loader2, Key } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { importSsoToken } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { SsoTokenImportResult } from '@/types/api'

interface SsoTokenImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

export function SsoTokenImportDialog({ open, onOpenChange, onSuccess }: SsoTokenImportDialogProps) {
  const [tokens, setTokens] = useState('')
  const [region, setRegion] = useState('us-east-1')
  const [importing, setImporting] = useState(false)
  const [results, setResults] = useState<SsoTokenImportResult[]>([])
  const [step, setStep] = useState<'input' | 'results'>('input')

  const handleImport = async () => {
    const tokenList = tokens
      .split('\n')
      .map(t => t.trim())
      .filter(t => t.length > 0)

    if (tokenList.length === 0) {
      toast.error('请输入至少一个 SSO 令牌')
      return
    }

    setImporting(true)
    try {
      const result = await importSsoToken(tokenList, region)
      setResults(result.results)
      setStep('results')
      if (result.imported > 0) {
        toast.success(`成功导入 ${result.imported} 个凭据`)
        onSuccess()
      }
    } catch (error) {
      toast.error(`导入失败: ${extractErrorMessage(error)}`)
    } finally {
      setImporting(false)
    }
  }

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) {
      setTokens('')
      setRegion('us-east-1')
      setResults([])
      setStep('input')
    }
    onOpenChange(nextOpen)
  }

  const successCount = results.filter(r => r.credentialId).length
  const failCount = results.filter(r => r.error).length

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Key className="h-5 w-5" />
            SSO 令牌导入
          </DialogTitle>
        </DialogHeader>

        {step === 'input' ? (
          <div className="space-y-4 py-4">
            <div className="space-y-2">
              <label className="text-sm font-medium">
                SSO 令牌（每行一个，支持批量导入）
              </label>
              <textarea
                className="w-full h-32 p-3 text-sm border rounded-md resize-none font-mono"
                placeholder={"粘贴 x-amz-sso_authn cookie 值...\n每行一个令牌，支持批量导入"}
                value={tokens}
                onChange={e => setTokens(e.target.value)}
                disabled={importing}
              />
              <p className="text-xs text-muted-foreground">
                从浏览器 DevTools → Application → Cookies 中复制 x-amz-sso_authn 值
              </p>
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">区域</label>
              <Input
                value={region}
                onChange={e => setRegion(e.target.value)}
                placeholder="us-east-1"
                disabled={importing}
              />
            </div>
          </div>
        ) : (
          <div className="space-y-4 py-4">
            <div className="flex gap-4 text-sm">
              <span className="text-green-600">成功: {successCount}</span>
              {failCount > 0 && <span className="text-red-600">失败: {failCount}</span>}
            </div>
            <div className="max-h-60 overflow-y-auto space-y-2">
              {results.map((r, i) => (
                <div
                  key={i}
                  className={`p-3 rounded-md text-sm ${
                    r.credentialId
                      ? 'bg-green-50 dark:bg-green-950/30 border border-green-200 dark:border-green-800'
                      : 'bg-red-50 dark:bg-red-950/30 border border-red-200 dark:border-red-800'
                  }`}
                >
                  <div className="flex items-center gap-2">
                    {r.credentialId ? (
                      <CheckCircle2 className="h-4 w-4 text-green-600 shrink-0" />
                    ) : (
                      <XCircle className="h-4 w-4 text-red-600 shrink-0" />
                    )}
                    <span className="font-medium">
                      令牌 #{r.tokenIndex + 1}
                      {r.email && ` - ${r.email}`}
                    </span>
                  </div>
                  {r.error && (
                    <p className="mt-1 text-xs text-red-600">{r.error}</p>
                  )}
                  {r.credentialId && (
                    <p className="mt-1 text-xs text-green-600">
                      凭据 ID: {r.credentialId}
                    </p>
                  )}
                </div>
              ))}
            </div>
          </div>
        )}

        <DialogFooter>
          {step === 'input' ? (
            <>
              <Button variant="outline" onClick={() => handleOpenChange(false)}>
                取消
              </Button>
              <Button onClick={handleImport} disabled={importing || !tokens.trim()}>
                {importing ? (
                  <>
                    <Loader2 className="h-4 w-4 mr-2 animate-spin" />
                    导入中...
                  </>
                ) : (
                  '导入'
                )}
              </Button>
            </>
          ) : (
            <Button onClick={() => handleOpenChange(false)}>关闭</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
