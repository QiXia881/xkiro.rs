import { useState } from 'react'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, AlertCircle, Loader2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { useCredentials, useAddCredential, useDeleteCredential } from '@/hooks/use-credentials'
import { getCredentialBalance, setCredentialDisabled } from '@/api/credentials'
import { extractErrorMessage, sha256Hex } from '@/lib/utils'

interface ImportJsonDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

interface CredentialInput {
  refreshToken?: string
  provider?: string
  clientId?: string
  clientSecret?: string
  region?: string
  authRegion?: string
  apiRegion?: string
  priority?: number
  concurrency?: number | null
  machineId?: string
  kiroApiKey?: string
  authMethod?: string
  endpoint?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
}

// 下拉可选 provider（仅 OAuth 凭据用于分类，api_key 无 provider 概念）
type ProviderChoice = 'Google' | 'Github' | 'Enterprise'
const PROVIDER_CHOICES: ProviderChoice[] = ['Google', 'Github', 'Enterprise']

// provider → 后端 authMethod。Google/Github = social；Enterprise/BuilderId = idc
function providerToAuthMethod(provider: string): 'social' | 'idc' {
  const p = provider.trim().toLowerCase()
  if (p === 'enterprise' || p === 'builderid' || p === 'builder-id' || p === 'idc') return 'idc'
  return 'social'
}

// 是否 api_key 凭据（api_key 跳过 provider 选择）
function isApiKeyCredential(cred: CredentialInput): boolean {
  return !!cred.kiroApiKey?.trim() || cred.authMethod === 'api_key'
}

// 是否需要用户手动选 provider：OAuth 凭据且既无 provider 字段、也无显式 authMethod
function needsProviderSelection(cred: CredentialInput): boolean {
  return !isApiKeyCredential(cred) && !cred.provider?.trim() && !cred.authMethod?.trim()
}

// 缺省 provider 猜测：带 clientId/clientSecret 视为 Enterprise，否则 Google
function guessProvider(cred: CredentialInput): ProviderChoice {
  return cred.clientId?.trim() && cred.clientSecret?.trim() ? 'Enterprise' : 'Google'
}

interface VerificationResult {
  index: number
  status: 'pending' | 'checking' | 'verifying' | 'verified' | 'duplicate' | 'failed'
  error?: string
  usage?: string
  email?: string
  credentialId?: number
  rollbackStatus?: 'success' | 'failed' | 'skipped'
  rollbackError?: string
}

interface TaskResult {
  outcome: 'success' | 'duplicate' | 'failed'
  rollbackStatus?: VerificationResult['rollbackStatus']
}

export function ImportJsonDialog({ open, onOpenChange }: ImportJsonDialogProps) {
  const [jsonInput, setJsonInput] = useState('')
  const [importing, setImporting] = useState(false)
  const [batchSize, setBatchSize] = useState(5)
  const [progress, setProgress] = useState({ current: 0, total: 0 })
  const [currentProcessing, setCurrentProcessing] = useState<string>('')
  const [results, setResults] = useState<VerificationResult[]>([])

  // provider 选择弹窗：缺 provider 的 OAuth 凭据由用户下拉指定
  // pendingChoices: 每个待选凭据的 {index(原数组下标), label, provider(当前选值)}
  const [providerPrompt, setProviderPrompt] = useState<{
    credentials: CredentialInput[]
    choices: { index: number; label: string; provider: ProviderChoice }[]
  } | null>(null)

  const { data: existingCredentials } = useCredentials()
  const { mutateAsync: addCredential } = useAddCredential()
  const { mutateAsync: deleteCredential } = useDeleteCredential()

  const rollbackCredential = async (id: number): Promise<{ success: boolean; error?: string }> => {
    try {
      await setCredentialDisabled(id, true)
    } catch (error) {
      return {
        success: false,
        error: `禁用失败: ${extractErrorMessage(error)}`,
      }
    }

    try {
      await deleteCredential(id)
      return { success: true }
    } catch (error) {
      return {
        success: false,
        error: `删除失败: ${extractErrorMessage(error)}`,
      }
    }
  }

  const resetForm = () => {
    setJsonInput('')
    setProgress({ current: 0, total: 0 })
    setCurrentProcessing('')
    setResults([])
    setProviderPrompt(null)
  }

  // 入口：解析 JSON → 检测缺 provider 的 OAuth 凭据 → 需选则弹窗，否则直接导入
  const handleStartImport = async () => {
    // 先单独解析 JSON，给出精准的错误提示
    let credentials: CredentialInput[]
    try {
      const parsed = JSON.parse(jsonInput)
      credentials = Array.isArray(parsed) ? parsed : [parsed]
    } catch (error) {
      toast.error('JSON 格式错误: ' + extractErrorMessage(error))
      return
    }

    if (credentials.length === 0) {
      toast.error('没有可导入的凭据')
      return
    }

    // 筛出缺 provider（且无 authMethod）的 OAuth 凭据，交由用户下拉指定
    const missing = credentials
      .map((cred, index) => ({ cred, index }))
      .filter(({ cred }) => needsProviderSelection(cred))

    if (missing.length > 0) {
      setProviderPrompt({
        credentials,
        choices: missing.map(({ cred, index }) => ({
          index,
          label: cred.refreshToken
            ? `凭据 #${index + 1}（${cred.refreshToken.trim().slice(0, 12)}…）`
            : `凭据 #${index + 1}`,
          provider: guessProvider(cred),
        })),
      })
      return
    }

    await runImport(credentials)
  }

  // 用户在弹窗中确认所选 provider 后，把选择写回各凭据再继续导入
  const confirmProviderSelection = async () => {
    if (!providerPrompt || importing) return
    const { credentials, choices } = providerPrompt
    const resolved = credentials.map((cred, i) => {
      const choice = choices.find(c => c.index === i)
      return choice ? { ...cred, provider: choice.provider } : cred
    })
    setProviderPrompt(null)
    await runImport(resolved)
  }

  const runImport = async (credentials: CredentialInput[]) => {
    try {
      setImporting(true)
      setProgress({ current: 0, total: credentials.length })

      const initialResults: VerificationResult[] = credentials.map((_, i) => ({
        index: i + 1,
        status: 'pending',
      }))
      setResults(initialResults)

      const existingOauthHashes = new Set(
        existingCredentials?.credentials
          .map(c => c.refreshTokenHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )
      const existingApiKeyHashes = new Set(
        existingCredentials?.credentials
          .map(c => c.apiKeyHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )

      // Pre-compute all hashes in parallel
      const allHashes = await Promise.all(credentials.map(async (cred) => {
        const isApiKeyCred = !!(cred.kiroApiKey?.trim()) || cred.authMethod === 'api_key'
        if (isApiKeyCred) {
          const key = cred.kiroApiKey?.trim() || ''
          return key ? await sha256Hex(key) : null
        }
        const token = cred.refreshToken?.trim() || ''
        return token ? await sha256Hex(token) : null
      }))

      // Pre-dedup: mark same-list duplicates; first occurrence wins
      const seenOauth = new Set<string>()
      const seenApiKey = new Set<string>()
      const isDuplicateFlags = credentials.map((cred, i) => {
        const isApiKeyCred = !!(cred.kiroApiKey?.trim()) || cred.authMethod === 'api_key'
        const hash = allHashes[i]
        if (!hash) return false
        if (isApiKeyCred) {
          if (existingApiKeyHashes.has(hash) || seenApiKey.has(hash)) return true
          seenApiKey.add(hash)
        } else {
          if (existingOauthHashes.has(hash) || seenOauth.has(hash)) return true
          seenOauth.add(hash)
        }
        return false
      })

      const processCredential = async (i: number): Promise<TaskResult> => {
        const cred = credentials[i]
        const isApiKeyCred = !!(cred.kiroApiKey?.trim()) || cred.authMethod === 'api_key'
        const hash = allHashes[i]

        setResults(prev => {
          const updated = [...prev]
          updated[i] = { ...updated[i], status: 'checking' }
          return updated
        })

        if (isDuplicateFlags[i]) {
          const existingCred = isApiKeyCred
            ? existingCredentials?.credentials.find(c => c.apiKeyHash === hash)
            : existingCredentials?.credentials.find(c => c.refreshTokenHash === hash)
          setResults(prev => {
            const updated = [...prev]
            updated[i] = {
              ...updated[i],
              status: 'duplicate',
              error: '该凭据已存在',
              email: existingCred?.email || undefined,
            }
            return updated
          })
          setProgress(prev => ({ ...prev, current: prev.current + 1 }))
          return { outcome: 'duplicate' }
        }

        if (isApiKeyCred && !cred.kiroApiKey?.trim()) {
          setResults(prev => {
            const updated = [...prev]
            updated[i] = { ...updated[i], status: 'failed', error: '缺少 kiroApiKey' }
            return updated
          })
          setProgress(prev => ({ ...prev, current: prev.current + 1 }))
          return { outcome: 'failed' }
        }
        if (!isApiKeyCred && !cred.refreshToken?.trim()) {
          setResults(prev => {
            const updated = [...prev]
            updated[i] = { ...updated[i], status: 'failed', error: '缺少 refreshToken' }
            return updated
          })
          setProgress(prev => ({ ...prev, current: prev.current + 1 }))
          return { outcome: 'failed' }
        }

        setResults(prev => {
          const updated = [...prev]
          updated[i] = { ...updated[i], status: 'verifying' }
          return updated
        })

        let addedCredId: number | null = null
        try {
          if (isApiKeyCred) {
            const addedCred = await addCredential({
              authMethod: 'api_key',
              kiroApiKey: cred.kiroApiKey?.trim(),
              priority: cred.priority || 0,
              concurrency: cred.concurrency ?? null,
              authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
              apiRegion: cred.apiRegion?.trim() || undefined,
              machineId: cred.machineId?.trim() || undefined,
              endpoint: cred.endpoint?.trim() || undefined,
              proxyUrl: cred.proxyUrl?.trim() || undefined,
              proxyUsername: cred.proxyUsername?.trim() || undefined,
              proxyPassword: cred.proxyPassword?.trim() || undefined,
            })
            addedCredId = addedCred.credentialId

            let balance
            for (let attempt = 0; attempt < 3; attempt++) {
              await new Promise(resolve => setTimeout(resolve, attempt === 0 ? 500 : 1500))
              try {
                balance = await getCredentialBalance(addedCred.credentialId)
                break
              } catch (err) {
                if (attempt === 2) throw err
              }
            }

            setResults(prev => {
              const updated = [...prev]
              updated[i] = {
                ...updated[i],
                status: 'verified',
                usage: `${balance!.currentUsage}/${balance!.usageLimit}`,
                email: addedCred.email || undefined,
                credentialId: addedCred.credentialId,
              }
              return updated
            })
            setProgress(prev => ({ ...prev, current: prev.current + 1 }))
            return { outcome: 'success' }
          }

          // OAuth path
          const token = cred.refreshToken!.trim()
          const clientId = cred.clientId?.trim() || undefined
          const clientSecret = cred.clientSecret?.trim() || undefined
          const rawAuthMethod = cred.authMethod?.trim()
          if (rawAuthMethod) {
            const lower = rawAuthMethod.toLowerCase()
            if (!['social', 'idc', 'api_key'].includes(lower)) {
              throw new Error(`未知的 authMethod: ${rawAuthMethod}`)
            }
          }
          const authMethod = rawAuthMethod
            ? (rawAuthMethod.toLowerCase() === 'idc' ? 'idc' : 'social')
            : cred.provider?.trim()
              ? providerToAuthMethod(cred.provider)
              : clientId && clientSecret ? 'idc' : 'social'

          if (authMethod === 'idc' && (!clientId || !clientSecret)) {
            throw new Error('idc 模式需要同时提供 clientId 和 clientSecret')
          }

          const addedCred = await addCredential({
            refreshToken: token,
            authMethod,
            authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
            apiRegion: cred.apiRegion?.trim() || undefined,
            clientId,
            clientSecret,
            priority: cred.priority || 0,
            concurrency: cred.concurrency ?? null,
            machineId: cred.machineId?.trim() || undefined,
            endpoint: cred.endpoint?.trim() || undefined,
            proxyUrl: cred.proxyUrl?.trim() || undefined,
            proxyUsername: cred.proxyUsername?.trim() || undefined,
            proxyPassword: cred.proxyPassword?.trim() || undefined,
          })
          addedCredId = addedCred.credentialId

          let balance
          for (let attempt = 0; attempt < 3; attempt++) {
            await new Promise(resolve => setTimeout(resolve, attempt === 0 ? 500 : 1500))
            try {
              balance = await getCredentialBalance(addedCred.credentialId)
              break
            } catch (err) {
              if (attempt === 2) throw err
            }
          }

          setResults(prev => {
            const updated = [...prev]
            updated[i] = {
              ...updated[i],
              status: 'verified',
              usage: `${balance!.currentUsage}/${balance!.usageLimit}`,
              email: addedCred.email || undefined,
              credentialId: addedCred.credentialId,
            }
            return updated
          })
          setProgress(prev => ({ ...prev, current: prev.current + 1 }))
          return { outcome: 'success' }
        } catch (error) {
          let rollbackStatus: VerificationResult['rollbackStatus'] = 'skipped'
          let rollbackError: string | undefined
          if (addedCredId) {
            const rollback = await rollbackCredential(addedCredId)
            if (rollback.success) {
              rollbackStatus = 'success'
            } else {
              rollbackStatus = 'failed'
              rollbackError = rollback.error
            }
          }
          setResults(prev => {
            const updated = [...prev]
            updated[i] = {
              ...updated[i],
              status: 'failed',
              error: extractErrorMessage(error),
              email: undefined,
              rollbackStatus,
              rollbackError,
            }
            return updated
          })
          setProgress(prev => ({ ...prev, current: prev.current + 1 }))
          return { outcome: 'failed', rollbackStatus }
        }
      }

      let successCount = 0, duplicateCount = 0, failCount = 0
      let rollbackSuccessCount = 0, rollbackFailedCount = 0, rollbackSkippedCount = 0
      const totalBatches = Math.ceil(credentials.length / batchSize)

      for (let batchIdx = 0; batchIdx < totalBatches; batchIdx++) {
        const start = batchIdx * batchSize
        const batchIndices = Array.from(
          { length: Math.min(batchSize, credentials.length - start) },
          (_, k) => start + k
        )
        setCurrentProcessing(`第 ${batchIdx + 1} 批 / 共 ${totalBatches} 批`)
        const batchResults = await Promise.all(batchIndices.map(i => processCredential(i)))
        for (const r of batchResults) {
          if (r.outcome === 'success') {
            successCount++
          } else if (r.outcome === 'duplicate') {
            duplicateCount++
          } else {
            failCount++
            if (r.rollbackStatus === 'success') rollbackSuccessCount++
            else if (r.rollbackStatus === 'failed') rollbackFailedCount++
            else if (r.rollbackStatus === 'skipped') rollbackSkippedCount++
          }
        }
      }

      if (failCount === 0 && duplicateCount === 0) {
        toast.success(`成功导入并验活 ${successCount} 个凭据`)
      } else {
        const failureSummary = failCount > 0
          ? `，失败 ${failCount} 个（已排除 ${rollbackSuccessCount}，未排除 ${rollbackFailedCount}，无需排除 ${rollbackSkippedCount}）`
          : ''
        toast.info(`验活完成：成功 ${successCount} 个，重复 ${duplicateCount} 个${failureSummary}`)
        if (rollbackFailedCount > 0) {
          toast.warning(`有 ${rollbackFailedCount} 个失败凭据回滚未完成，请手动禁用并删除`)
        }
      }
    } catch (error) {
      toast.error('导入失败: ' + extractErrorMessage(error))
    } finally {
      setImporting(false)
    }
  }

  // 模板：一键填入对应 provider 的 JSON 骨架，方便用户照着改
  const insertTemplate = (kind: ProviderChoice | 'api_key') => {
    if (importing) return
    let tpl: Record<string, unknown>
    switch (kind) {
      case 'Google':
        tpl = { refreshToken: '', provider: 'Google' }
        break
      case 'Github':
        tpl = { refreshToken: '', provider: 'Github' }
        break
      case 'Enterprise':
        tpl = { refreshToken: '', clientId: '', clientSecret: '', provider: 'Enterprise' }
        break
      case 'api_key':
        tpl = { kiroApiKey: 'ksk_', authMethod: 'api_key' }
        break
    }
    setJsonInput(JSON.stringify([tpl], null, 2))
  }

  const getStatusIcon = (status: VerificationResult['status']) => {
    switch (status) {
      case 'pending':
        return <div className="w-5 h-5 rounded-full border-2 border-gray-300" />
      case 'checking':
      case 'verifying':
        return <Loader2 className="w-5 h-5 animate-spin text-blue-500" />
      case 'verified':
        return <CheckCircle2 className="w-5 h-5 text-green-500" />
      case 'duplicate':
        return <AlertCircle className="w-5 h-5 text-yellow-500" />
      case 'failed':
        return <XCircle className="w-5 h-5 text-red-500" />
    }
  }

  const getStatusText = (result: VerificationResult) => {
    switch (result.status) {
      case 'pending':
        return '等待中'
      case 'checking':
        return '检查重复...'
      case 'verifying':
        return '验活中...'
      case 'verified':
        return '验活成功'
      case 'duplicate':
        return '重复凭据'
      case 'failed':
        if (result.rollbackStatus === 'success') return '验活失败（已排除）'
        if (result.rollbackStatus === 'failed') return '验活失败（未排除）'
        return '验活失败（未创建）'
    }
  }

  return (
    <>
    <Dialog
      open={open}
      onOpenChange={(newOpen) => {
        // 关闭时清空表单（但不在导入过程中清空）
        if (!newOpen && !importing) {
          resetForm()
        }
        onOpenChange(newOpen)
      }}
    >
      <DialogContent className="sm:max-w-2xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>导入 JSON（添加凭据 + 自动验活）</DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 py-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">
              JSON 格式凭据
            </label>
            {results.length === 0 && (
              <div className="flex flex-wrap gap-2">
                <span className="text-xs text-muted-foreground self-center">模板：</span>
                <Button type="button" size="sm" variant="outline" className="h-7 text-xs"
                  disabled={importing} onClick={() => insertTemplate('Google')}>
                  Google
                </Button>
                <Button type="button" size="sm" variant="outline" className="h-7 text-xs"
                  disabled={importing} onClick={() => insertTemplate('Github')}>
                  Github
                </Button>
                <Button type="button" size="sm" variant="outline" className="h-7 text-xs"
                  disabled={importing} onClick={() => insertTemplate('Enterprise')}>
                  Enterprise
                </Button>
                <Button type="button" size="sm" variant="outline" className="h-7 text-xs"
                  disabled={importing} onClick={() => insertTemplate('api_key')}>
                  API Key
                </Button>
              </div>
            )}
            <div
              onDragOver={(e) => { e.preventDefault(); e.stopPropagation() }}
              onDrop={async (e) => {
                e.preventDefault(); e.stopPropagation()
                if (importing) return
                const file = e.dataTransfer.files?.[0]
                if (!file) return
                if (!/\.(json|txt)$/i.test(file.name) && file.type && !file.type.includes('json') && !file.type.includes('text')) {
                  toast.error('仅支持 .json / 文本文件')
                  return
                }
                try {
                  const text = await file.text()
                  setJsonInput(text)
                  toast.success(`已读取 ${file.name}`)
                } catch (err) {
                  toast.error('读取文件失败：' + extractErrorMessage(err))
                }
              }}
            >
              <textarea
                placeholder={'粘贴或拖入 JSON 文件（支持单个对象或数组）\n\nGoogle/Github: [{"refreshToken":"...","provider":"Google"}]\nEnterprise: [{"refreshToken":"...","clientId":"...","clientSecret":"...","provider":"Enterprise"}]\nAPI Key: [{"kiroApiKey":"ksk_xxx"}]\n\n未提供 provider 的 OAuth 凭据将弹窗让你选择来源\n可选字段: priority / concurrency / machineId / endpoint / proxyUrl / region'}
                value={jsonInput}
                onChange={(e) => setJsonInput(e.target.value)}
                disabled={importing}
                className="flex min-h-[200px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50 font-mono"
              />
            </div>
            <p className="text-xs text-muted-foreground">
              💡 拖入 .json 文件可直接读取；导入时自动验活，失败的凭据会被排除
            </p>
            {results.length === 0 && (
              <div className="flex items-center gap-2">
                <span className="text-sm text-muted-foreground">并发批大小：</span>
                <input
                  type="number"
                  min={1}
                  max={20}
                  value={batchSize}
                  onChange={(e) => setBatchSize(Math.max(1, Math.min(20, parseInt(e.target.value) || 5)))}
                  disabled={importing}
                  className="w-16 h-8 rounded-md border border-input bg-background px-2 text-center text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
                />
                <span className="text-xs text-muted-foreground">（同时处理 {batchSize} 个凭据）</span>
              </div>
            )}
          </div>

          {(importing || results.length > 0) && (
            <>
              {/* 进度条 */}
              <div className="space-y-2">
                <div className="flex justify-between text-sm">
                  <span>{importing ? '验活进度' : '验活完成'}</span>
                  <span>{progress.current} / {progress.total}</span>
                </div>
                <div className="w-full bg-secondary rounded-full h-2">
                  <div
                    className="bg-primary h-2 rounded-full transition-all"
                    style={{ width: `${progress.total > 0 ? Math.round((progress.current / progress.total) * 100) : 0}%` }}
                  />
                </div>
                {importing && currentProcessing && (
                  <div className="text-xs text-muted-foreground">
                    {currentProcessing}
                  </div>
                )}
              </div>

              {/* 统计 */}
              <div className="flex gap-4 text-sm">
                <span className="text-green-600 dark:text-green-400">
                  ✓ 成功: {results.filter(r => r.status === 'verified').length}
                </span>
                <span className="text-yellow-600 dark:text-yellow-400">
                  ⚠ 重复: {results.filter(r => r.status === 'duplicate').length}
                </span>
                <span className="text-red-600 dark:text-red-400">
                  ✗ 失败: {results.filter(r => r.status === 'failed').length}
                </span>
              </div>

              {/* 结果列表 */}
              <div className="border rounded-md divide-y max-h-[300px] overflow-y-auto">
                {results.map((result) => (
                  <div key={result.index} className="p-3">
                    <div className="flex items-start gap-3">
                      {getStatusIcon(result.status)}
                      <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-2">
                          <span className="text-sm font-medium">
                            {result.email || `凭据 #${result.index}`}
                          </span>
                          <span className="text-xs text-muted-foreground">
                            {getStatusText(result)}
                          </span>
                        </div>
                        {result.usage && (
                          <div className="text-xs text-muted-foreground mt-1">
                            用量: {result.usage}
                          </div>
                        )}
                        {result.error && (
                          <div className="text-xs text-red-600 dark:text-red-400 mt-1">
                            {result.error}
                          </div>
                        )}
                        {result.rollbackError && (
                          <div className="text-xs text-red-600 dark:text-red-400 mt-1">
                            回滚失败: {result.rollbackError}
                          </div>
                        )}
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={() => {
              onOpenChange(false)
              resetForm()
            }}
            disabled={importing}
          >
            {importing ? '验活中...' : results.length > 0 ? '关闭' : '取消'}
          </Button>
          {results.length === 0 && (
            <Button
              type="button"
              onClick={handleStartImport}
              disabled={importing || !jsonInput.trim()}
            >
              开始导入并验活
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>

      {/* provider 选择弹窗：缺 provider 的 OAuth 凭据由用户下拉指定 */}
      <Dialog
        open={!!providerPrompt}
        onOpenChange={(o) => { if (!o) setProviderPrompt(null) }}
      >
        <DialogContent className="sm:max-w-md max-h-[80vh] flex flex-col">
          <DialogHeader>
            <DialogTitle>选择凭据来源</DialogTitle>
          </DialogHeader>
          <div className="flex-1 overflow-y-auto space-y-3 py-2">
            <p className="text-xs text-muted-foreground">
              以下凭据未指定 provider，请为每个选择来源。Google / Github 归为 social，Enterprise 归为 idc。
            </p>
            {providerPrompt?.choices.map((choice) => (
              <div key={choice.index} className="flex items-center justify-between gap-3">
                <span className="text-sm truncate flex-1" title={choice.label}>
                  {choice.label}
                </span>
                <select
                  value={choice.provider}
                  onChange={(e) => {
                    const provider = e.target.value as ProviderChoice
                    setProviderPrompt(prev => prev && {
                      ...prev,
                      choices: prev.choices.map(c =>
                        c.index === choice.index ? { ...c, provider } : c
                      ),
                    })
                  }}
                  className="flex h-9 w-36 rounded-md border border-input bg-background px-3 py-1 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
                >
                  {PROVIDER_CHOICES.map(p => (
                    <option key={p} value={p}>{p}</option>
                  ))}
                </select>
              </div>
            ))}
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setProviderPrompt(null)}>
              取消
            </Button>
            <Button type="button" onClick={confirmProviderSelection}>
              确认并导入
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}
