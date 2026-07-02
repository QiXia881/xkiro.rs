import { useState } from 'react'
import { toast } from 'sonner'
import { AlertTriangle, CheckCircle2, FileJson, Loader2, XCircle } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { importCredentials } from '@/api/credentials'
import { getCredentialMaterialLabels as credentialMaterialLabels } from '@/lib/credential-material'
import {
  CREDENTIAL_IMPORT_ACTIONS,
  CREDENTIAL_IMPORT_MODES,
  CREDENTIAL_SOURCE_FORMATS,
  formatCredentialAuthLabel,
  formatCredentialImportActionLabel,
  formatCredentialImportModeLabel,
  formatCredentialSourceFormatLabel,
  getCredentialMetadataInlineLabels as credentialMetadataLabels,
  type CredentialImportMode,
} from '@/lib/credential-metadata'
import { extractErrorMessage } from '@/lib/utils'
import type {
  CredentialImportItem,
  CredentialImportResponse,
} from '@/types/api'

interface CredentialImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

function actionClass(action: CredentialImportItem['action']): string {
  switch (action) {
    case CREDENTIAL_IMPORT_ACTIONS.added:
    case CREDENTIAL_IMPORT_ACTIONS.merged:
    case CREDENTIAL_IMPORT_ACTIONS.replaced:
      return 'border-green-200 bg-green-50 text-green-900 dark:border-green-800 dark:bg-green-950/30 dark:text-green-100'
    case CREDENTIAL_IMPORT_ACTIONS.skipped:
      return 'border-amber-200 bg-amber-50 text-amber-900 dark:border-amber-800 dark:bg-amber-950/30 dark:text-amber-100'
    case CREDENTIAL_IMPORT_ACTIONS.invalid:
      return 'border-red-200 bg-red-50 text-red-900 dark:border-red-800 dark:bg-red-950/30 dark:text-red-100'
  }
}

function parseJsonInput(input: string): unknown {
  const trimmed = input.trim()
  if (!trimmed) throw new Error('请输入或选择凭据数据')
  return JSON.parse(trimmed)
}

export function CredentialImportDialog({
  open,
  onOpenChange,
  onSuccess,
}: CredentialImportDialogProps) {
  const [jsonInput, setJsonInput] = useState('')
  const [mode, setMode] = useState<CredentialImportMode>(CREDENTIAL_IMPORT_MODES.skipExisting)
  const [running, setRunning] = useState(false)
  const [preview, setPreview] = useState<CredentialImportResponse | null>(null)

  const reset = () => {
    setJsonInput('')
    setMode(CREDENTIAL_IMPORT_MODES.skipExisting)
    setPreview(null)
    setRunning(false)
  }

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) reset()
    onOpenChange(nextOpen)
  }

  const handleFile = async (file: File | undefined) => {
    if (!file) return
    try {
      setJsonInput(await file.text())
      setPreview(null)
    } catch (error) {
      toast.error(`读取文件失败：${extractErrorMessage(error)}`)
    }
  }

  const runImport = async (dryRun: boolean) => {
    if (!dryRun && mode === CREDENTIAL_IMPORT_MODES.replaceExisting) {
      const confirmed = window.confirm(
        'replaceExisting 会覆盖已有凭据的令牌、设备 ID、区域、代理和 SSO 缓存字段。确认继续？'
      )
      if (!confirmed) return
    }
    setRunning(true)
    try {
      const input = parseJsonInput(jsonInput)
      const response = await importCredentials(input, dryRun, mode)
      setPreview(response)
      if (dryRun) {
        toast.success(`预览完成：解析 ${response.summary.parsed} 项`)
        return
      }
      const changed = response.summary.added + response.summary.merged + response.summary.replaced
      if (changed > 0) {
        toast.success(`导入完成：变更 ${changed} 项，跳过 ${response.summary.skipped} 项`)
        onSuccess()
      } else if (response.summary.invalid > 0) {
        toast.warning(`导入完成：${response.summary.invalid} 项失败`)
      } else {
        toast.info('没有需要导入的凭据')
      }
    } catch (error) {
      toast.error(`导入失败：${extractErrorMessage(error)}`)
    } finally {
      setRunning(false)
    }
  }

  const summary = preview?.summary

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <FileJson className="h-5 w-5" />
            凭据导入
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="rounded-lg border border-amber-200 bg-amber-50 p-3 text-sm text-amber-900 dark:border-amber-800 dark:bg-amber-950/30 dark:text-amber-100">
            <div className="flex items-start gap-2">
              <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
              <p>
                自动识别缓存凭据、xkiro.rs 完整备份和扁平凭据。完整备份可能包含刷新令牌、
                访问令牌、API 密钥、代理凭据和设备 ID；只导入可信来源。
              </p>
            </div>
          </div>

          <div className="grid gap-3 sm:grid-cols-[1fr_180px]">
            <div className="space-y-2">
              <label className="text-sm font-medium">导入模式</label>
              <select
                className="h-9 w-full rounded-md border bg-background px-3 text-sm"
                value={mode}
                onChange={event => setMode(event.target.value as CredentialImportMode)}
                disabled={running}
              >
                <option value={CREDENTIAL_IMPORT_MODES.skipExisting}>
                  {formatCredentialImportModeLabel(CREDENTIAL_IMPORT_MODES.skipExisting)}
                </option>
                <option value={CREDENTIAL_IMPORT_MODES.mergeMissing}>
                  {formatCredentialImportModeLabel(CREDENTIAL_IMPORT_MODES.mergeMissing)}
                </option>
                <option value={CREDENTIAL_IMPORT_MODES.replaceExisting}>
                  {formatCredentialImportModeLabel(CREDENTIAL_IMPORT_MODES.replaceExisting)}
                </option>
              </select>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">选择文件</label>
              <input
                type="file"
                accept="application/json,.json"
                className="block h-9 w-full text-xs file:mr-3 file:h-9 file:rounded-md file:border-0 file:bg-secondary file:px-3 file:text-sm file:text-secondary-foreground"
                disabled={running}
                onChange={event => void handleFile(event.target.files?.[0])}
              />
            </div>
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium">凭据数据</label>
            <textarea
              className="h-52 w-full resize-none rounded-md border bg-background p-3 font-mono text-xs"
              value={jsonInput}
              onChange={event => {
                setJsonInput(event.target.value)
                setPreview(null)
              }}
              disabled={running}
              placeholder={`粘贴缓存凭据、{"format":"${CREDENTIAL_SOURCE_FORMATS.credentialBackup}", ...} 或扁平凭据`}
            />
          </div>

          {summary && (
            <div className="rounded-lg border p-3 text-sm">
              <div className="mb-2 flex flex-wrap gap-3">
                <span>解析: {summary.parsed}</span>
                <span className="text-green-600">
                  {formatCredentialImportActionLabel(CREDENTIAL_IMPORT_ACTIONS.added)}: {summary.added}
                </span>
                <span className="text-green-600">
                  {formatCredentialImportActionLabel(CREDENTIAL_IMPORT_ACTIONS.merged)}: {summary.merged}
                </span>
                <span className="text-green-600">
                  {formatCredentialImportActionLabel(CREDENTIAL_IMPORT_ACTIONS.replaced)}: {summary.replaced}
                </span>
                <span className="text-amber-600">
                  {formatCredentialImportActionLabel(CREDENTIAL_IMPORT_ACTIONS.skipped)}: {summary.skipped}
                </span>
                <span className="text-red-600">
                  {formatCredentialImportActionLabel(CREDENTIAL_IMPORT_ACTIONS.invalid)}: {summary.invalid}
                </span>
              </div>
              <div className="max-h-56 space-y-2 overflow-y-auto">
                {preview.items.map(item => (
                  <div key={item.index} className={`rounded-md border p-2 ${actionClass(item.action)}`}>
                    <div className="flex items-center gap-2">
                      {item.action === CREDENTIAL_IMPORT_ACTIONS.invalid ? (
                        <XCircle className="h-4 w-4 shrink-0" />
                      ) : (
                        <CheckCircle2 className="h-4 w-4 shrink-0" />
                      )}
                      <span className="font-medium">
                        #{item.index + 1} {formatCredentialImportActionLabel(item.action)}
                      </span>
                      <span className="text-xs opacity-80">{item.fingerprint}</span>
                    </div>
                    <div className="mt-1 text-xs opacity-80">
                      {formatCredentialSourceFormatLabel(item.sourceFormat)} / {formatCredentialAuthLabel(item.provider, item.authMethod) || '未知认证'}
                      {item.email && ` / ${item.email}`}
                      {item.machineId && ` / 设备 ID: ${item.machineId}`}
                      {` / ${item.willRefresh ? '会刷新令牌' : '不刷新令牌'}`}
                    </div>
                    {credentialMaterialLabels(item, { exclude: ['hasToken', 'hasRefreshToken'] }).length > 0 && (
                      <div className="mt-1 text-xs opacity-80">
                        材料: {credentialMaterialLabels(item, { exclude: ['hasToken', 'hasRefreshToken'] }).join(', ')}
                      </div>
                    )}
                    {credentialMetadataLabels(item, { exclude: ['machineId'] }).length > 0 && (
                      <div className="mt-1 text-xs opacity-80 break-all">
                        元数据: {credentialMetadataLabels(item, { exclude: ['machineId'] }).join(' / ')}
                      </div>
                    )}
                    {item.reason && <div className="mt-1 text-xs">{item.reason}</div>}
                    {item.warnings?.map((warning, index) => (
                      <div key={index} className="mt-1 text-xs">
                        警告: {warning}
                      </div>
                    ))}
                  </div>
                ))}
              </div>
            </div>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => handleOpenChange(false)} disabled={running}>
            关闭
          </Button>
          <Button variant="outline" onClick={() => void runImport(true)} disabled={running || !jsonInput.trim()}>
            {running ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : null}
            预览
          </Button>
          <Button onClick={() => void runImport(false)} disabled={running || !jsonInput.trim()}>
            {running ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : null}
            执行导入
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
