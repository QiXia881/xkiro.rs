import { useState } from 'react'
import { toast } from 'sonner'
import { CheckCircle2, Loader2, FolderOpen, Upload } from 'lucide-react'
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

interface LocalCacheImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

type LocalProvider = 'BuilderId' | 'Enterprise' | 'Google' | 'GitHub'

const LOCAL_PROVIDER_OPTIONS: Array<{ value: LocalProvider; label: string }> = [
  { value: 'BuilderId', label: CREDENTIAL_AUTH_LABELS.awsBuilderId },
  { value: 'Enterprise', label: CREDENTIAL_AUTH_LABELS.iamIdentityCenter },
  { value: 'Google', label: CREDENTIAL_AUTH_LABELS.google },
  { value: 'GitHub', label: CREDENTIAL_AUTH_LABELS.github },
]

function readJsonObject(raw: string, label: string): Record<string, unknown> {
  try {
    const value = JSON.parse(raw)
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
      throw new Error(`${label} 必须是 JSON object`)
    }
    return value as Record<string, unknown>
  } catch (error) {
    throw new Error(`${label} 解析失败: ${extractErrorMessage(error)}`)
  }
}

function stringField(data: Record<string, unknown>, key: string): string {
  const value = data[key]
  return typeof value === 'string' ? value.trim() : ''
}

export function LocalCacheImportDialog({ open, onOpenChange, onSuccess }: LocalCacheImportDialogProps) {
  const [provider, setProvider] = useState<LocalProvider>('BuilderId')
  const [cacheJson, setCacheJson] = useState('')
  const [clientJson, setClientJson] = useState('')
  const [importing, setImporting] = useState(false)
  const [result, setResult] = useState<{ credentialId: number; email?: string; authLabel?: string } | null>(null)

  const isSocial = provider === 'Google' || provider === 'GitHub'

  const reset = () => {
    setProvider('BuilderId')
    setCacheJson('')
    setClientJson('')
    setImporting(false)
    setResult(null)
  }

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) reset()
    onOpenChange(nextOpen)
  }

  const loadFile = async (
    event: React.ChangeEvent<HTMLInputElement>,
    setter: (value: string) => void,
  ) => {
    const file = event.target.files?.[0]
    if (!file) return
    setter(await file.text())
    event.target.value = ''
  }

  const handleImport = async () => {
    if (!cacheJson.trim()) {
      toast.error('请粘贴或上传本地缓存 JSON')
      return
    }

    setImporting(true)
    try {
      const cacheData = readJsonObject(cacheJson, '本地缓存 JSON')
      const refreshToken = stringField(cacheData, 'refreshToken')
      if (!refreshToken) {
        throw new Error('本地缓存 JSON 缺少 refreshToken 字段')
      }

      let clientData: Record<string, unknown> | null = null
      if (!isSocial) {
        if (!clientJson.trim()) {
          throw new Error(`${CREDENTIAL_AUTH_LABELS.awsBuilderId} / ${CREDENTIAL_AUTH_LABELS.iamIdentityCenter} 需要客户端注册 JSON`)
        }
        clientData = readJsonObject(clientJson, '客户端注册 JSON')
        if (!stringField(clientData, 'clientId') || !stringField(clientData, 'clientSecret')) {
          throw new Error('客户端注册 JSON 缺少 clientId 或 clientSecret')
        }
      }

      const added = await importCredentialRecord({
        refreshToken,
        accessToken: stringField(cacheData, 'accessToken') || undefined,
        clientId: clientData ? stringField(clientData, 'clientId') : undefined,
        clientSecret: clientData ? stringField(clientData, 'clientSecret') : undefined,
        region: stringField(cacheData, 'region') || undefined,
        authRegion: stringField(cacheData, 'authRegion') || undefined,
        apiRegion: stringField(cacheData, 'apiRegion') || undefined,
        startUrl:
          stringField(cacheData, 'startUrl')
          || (clientData ? stringField(clientData, 'startUrl') : '')
          || undefined,
        clientIdHash:
          stringField(cacheData, 'clientIdHash')
          || (clientData ? stringField(clientData, 'clientIdHash') : '')
          || undefined,
        idToken: stringField(cacheData, 'idToken') || undefined,
        ssoSessionId: stringField(cacheData, 'ssoSessionId') || undefined,
        authMethod: clientData ? 'idc' : 'social',
        provider,
        profileArn: stringField(cacheData, 'profileArn') || undefined,
        machineId: stringField(cacheData, 'machineId') || undefined,
      })

      const authLabel = formatCredentialAuthLabel(added.provider, added.authMethod)
      setResult({ credentialId: added.credentialId, email: added.email, authLabel })
      toast.success(`导入成功，已添加${authLabel ? ` ${authLabel}` : ''} 凭据 #${added.credentialId}`)
      onSuccess()
    } catch (error) {
      toast.error(`导入失败: ${extractErrorMessage(error)}`)
    } finally {
      setImporting(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <FolderOpen className="h-5 w-5" />
            本地缓存导入
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="rounded-lg border bg-muted/50 p-3 text-sm text-muted-foreground">
            <p className="font-medium text-foreground">本地缓存位置</p>
            <p>Windows: <code>%USERPROFILE%\.aws\sso\cache\</code></p>
            <p>macOS / Linux: <code>~/.aws/sso/cache/</code></p>
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium">登录通道</label>
            <select
              className="w-full rounded-md border bg-background px-3 py-2 text-sm"
              value={provider}
              onChange={(event) => setProvider(event.target.value as LocalProvider)}
              disabled={importing}
            >
              {LOCAL_PROVIDER_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>{option.label}</option>
              ))}
            </select>
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium">本地缓存 JSON</label>
            <textarea
              className="h-28 w-full resize-none rounded-md border bg-background p-3 font-mono text-sm"
              placeholder='{"refreshToken":"...","accessToken":"...","region":"us-east-1"}'
              value={cacheJson}
              onChange={(event) => setCacheJson(event.target.value)}
              disabled={importing}
            />
            <label className="inline-flex">
              <span className="inline-flex cursor-pointer items-center rounded-md border px-3 py-1.5 text-sm hover:bg-muted">
                <Upload className="mr-2 h-3.5 w-3.5" />
                上传令牌 JSON
              </span>
              <input
                type="file"
                accept=".json"
                className="hidden"
                onChange={(event) => void loadFile(event, setCacheJson)}
              />
            </label>
          </div>

          {!isSocial && (
            <div className="space-y-2">
              <label className="text-sm font-medium">客户端注册 JSON</label>
              <textarea
                className="h-28 w-full resize-none rounded-md border bg-background p-3 font-mono text-sm"
                placeholder='{"clientId":"...","clientSecret":"..."}'
                value={clientJson}
                onChange={(event) => setClientJson(event.target.value)}
                disabled={importing}
              />
              <label className="inline-flex">
                <span className="inline-flex cursor-pointer items-center rounded-md border px-3 py-1.5 text-sm hover:bg-muted">
                  <Upload className="mr-2 h-3.5 w-3.5" />
                  上传客户端注册 JSON
                </span>
                <input
                  type="file"
                  accept=".json"
                  className="hidden"
                  onChange={(event) => void loadFile(event, setClientJson)}
                />
              </label>
            </div>
          )}

          {result && (
            <div className="rounded-md border border-green-200 bg-green-50 p-3 text-sm text-green-800 dark:border-green-800 dark:bg-green-950/30 dark:text-green-200">
              <div className="flex items-center gap-2">
                <CheckCircle2 className="h-4 w-4" />
                <span>导入成功，凭据 #{result.credentialId}</span>
              </div>
              {result.email && <p className="mt-1 text-xs">{result.email}</p>}
              {result.authLabel && <p className="mt-1 text-xs">{result.authLabel}</p>}
            </div>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => handleOpenChange(false)}>
            取消
          </Button>
          <Button onClick={handleImport} disabled={importing || !cacheJson.trim()}>
            {importing && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
            导入
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
