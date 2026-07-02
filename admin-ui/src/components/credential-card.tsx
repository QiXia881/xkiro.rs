import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import { RefreshCw, ChevronUp, ChevronDown, Wallet, Trash2, Loader2, RotateCcw, Boxes } from 'lucide-react'
import { Card } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { CredentialStatusItem, BalanceResponse } from '@/types/api'
import {
  useSetDisabled,
  useSetPriority,
  useSetCredentialConcurrency,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useSetOverage,
} from '@/hooks/use-credentials'
import { useProxies, useSetCredentialProxyByRegion } from '@/hooks/use-proxies'
import { getCredentialBalance } from '@/api/credentials'
import { usePrivacyMode } from '@/hooks/use-privacy-mode'
import {
  formatCredentialBalanceNumber,
  formatCredentialOverageStatusLabel,
  getCredentialBalanceBaseUsage,
  getCredentialBalanceOverageUsage,
} from '@/lib/credential-balance'
import { getCredentialMaterialRows } from '@/lib/credential-material'
import {
  compactCredentialMetadataValue,
  getCredentialIdentityRows,
  getCredentialMetadataRows,
  formatCredentialAuthLabel,
  maskEmail,
  stringifyCredentialMetadataValue,
} from '@/lib/credential-metadata'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  onViewModels: (id: number) => void
  selected: boolean
  onToggleSelect: () => void
  balance: BalanceResponse | null
  loadingBalance: boolean
  /** 父级 balanceMap 派发器，用于乐观更新/失败回滚/真值覆盖 */
  onBalanceChange: (id: number, next: BalanceResponse | null) => void
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return '从未使用'
  const date = new Date(lastUsedAt)
  const now = new Date()
  const diff = now.getTime() - date.getTime()
  if (diff < 0) return '刚刚'
  const seconds = Math.floor(diff / 1000)
  if (seconds < 60) return `${seconds} 秒前`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  return `${days} 天前`
}

interface BalanceBlockProps {
  balance: BalanceResponse | null
  loading: boolean
  overageMutating: boolean
  onToggleOverage: (next: boolean) => void
}

// 余额展示：正式额度进度条 + 超额进度条
// 正式额度按 usage_limit 计 100%；超额（current_usage > usage_limit）从超额池计算独立进度条
function BalanceBlock({ balance, loading, overageMutating, onToggleOverage }: BalanceBlockProps) {
  if (loading) {
    return (
      <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" /> 加载余额...
      </div>
    )
  }

  if (!balance) {
    return (
      <div className="flex items-center justify-between text-xs text-muted-foreground">
        <span>余额</span>
        <span>未查询</span>
      </div>
    )
  }

  const baseUsage = getCredentialBalanceBaseUsage(balance)
  const overageUsage = getCredentialBalanceOverageUsage(balance)
  const baseTone = baseUsage.percent >= 90 ? 'bg-destructive' : baseUsage.percent >= 70 ? 'bg-warning' : 'bg-foreground/80'

  return (
    <div className="space-y-3">
      {/* 正式额度 */}
      <div className="space-y-1.5">
        <div className="flex items-baseline justify-between gap-2">
          <span className="text-xs text-muted-foreground">正式额度</span>
          <span className="tabular text-xs font-medium">
            <span className="text-foreground">{formatCredentialBalanceNumber(baseUsage.remaining)}</span>
            <span className="text-muted-foreground"> / {formatCredentialBalanceNumber(baseUsage.limit)}</span>
          </span>
        </div>
        <div className="h-1.5 w-full overflow-hidden rounded-full bg-secondary">
          <div
            className={`h-full transition-all ${baseTone}`}
            style={{ width: `${baseUsage.percent}%` }}
          />
        </div>
        {baseUsage.resetAt && (
          <div className="text-2xs text-muted-foreground tabular">
            {baseUsage.resetAt} 重置
          </div>
        )}
      </div>

      {/* 超额额度 */}
      {overageUsage.visible && (
        <div className="space-y-1.5 rounded-md border border-dashed bg-muted/30 px-2.5 py-2">
          <div className="flex items-baseline justify-between gap-2">
            <span className="text-xs text-muted-foreground">超额额度</span>
            <span className="tabular text-xs font-medium">
              <span className="text-foreground">{formatCredentialBalanceNumber(overageUsage.remaining)}</span>
              <span className="text-muted-foreground"> / {overageUsage.cap > 0 ? formatCredentialBalanceNumber(overageUsage.cap) : '—'}</span>
            </span>
          </div>
          {overageUsage.cap > 0 ? (
            <div className="h-1.5 w-full overflow-hidden rounded-full bg-secondary">
              <div
                className="h-full bg-warning transition-all"
                style={{ width: `${overageUsage.percent}%` }}
              />
            </div>
          ) : (
            <div className="text-2xs text-muted-foreground">订阅未提供超额上限</div>
          )}
          <div className="flex items-center justify-between gap-2 pt-0.5">
            <span className="text-2xs text-muted-foreground">
              {formatCredentialOverageStatusLabel(balance, { mutating: overageMutating })}
            </span>
            {balance.overageCapability === 'OVERAGE_CAPABLE' && (
              <Switch
                checked={balance.overageStatus === 'ENABLED'}
                disabled={overageMutating}
                onCheckedChange={onToggleOverage}
                className="scale-75"
              />
            )}
          </div>
        </div>
      )}
    </div>
  )
}

export function CredentialCard({
  credential,
  onViewBalance,
  onViewModels,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
  onBalanceChange,
}: CredentialCardProps) {
  const { privacyMode } = usePrivacyMode()
  const displayEmail = credential.email ? maskEmail(credential.email, privacyMode) : null
  const displayTitle = credential.nickname || credential.label || displayEmail || `凭据 #${credential.id}`
  const tooltipContent = credential.email && displayTitle !== displayEmail ? displayEmail : null
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(String(credential.priority))
  const [editingConcurrency, setEditingConcurrency] = useState(false)
  const [concurrencyValue, setConcurrencyValue] = useState(
    credential.concurrency == null ? '' : String(credential.concurrency)
  )
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)

  // n4: 让「最后调用时间」相对时间动态刷新
  // formatLastUsed 内部 new Date() 取的是渲染时刻，所以只要触发组件重渲染就能滚动。
  // 这里用一个轻量 tick state，每 30s 触发一次 setState 强制重渲染；
  // 与 runtime-stats 1s 轮询解耦，避免每次接口返回都把所有卡片掀一遍。
  const [, setTick] = useState(0)
  useEffect(() => {
    const timer = setInterval(() => setTick((t) => t + 1), 30_000)
    return () => clearInterval(timer)
  }, [])

  // prop 变化时同步本地编辑 state（未在编辑中才覆盖，避免打断用户输入）
  useEffect(() => {
    if (!editingPriority) setPriorityValue(String(credential.priority))
  }, [credential.priority, editingPriority])

  useEffect(() => {
    if (!editingConcurrency) setConcurrencyValue(credential.concurrency == null ? '' : String(credential.concurrency))
  }, [credential.concurrency, editingConcurrency])

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const setConcurrency = useSetCredentialConcurrency()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()
  const setOverage = useSetOverage()
  const overageMutating = setOverage.isPending
  const { data: proxyData } = useProxies()
  const setProxyByRegion = useSetCredentialProxyByRegion()
  const boundProxy = proxyData?.proxies.find(proxy => proxy.id === credential.proxyId) ?? null
  const boundProxyRegion = boundProxy?.region ?? null
  const proxyRegions = useMemo(() => {
    const regions = new Set<string>()
    for (const proxy of proxyData?.proxies ?? []) {
      if (!proxy.disabled && !proxy.dead && proxy.region?.trim()) {
        regions.add(proxy.region.trim())
      }
    }
    return [...regions].sort((a, b) => a.localeCompare(b))
  }, [proxyData?.proxies])

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => {
          toast.success(res.message)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handleSetProxyRegion = (region: string | null) => {
    setProxyByRegion.mutate(
      { id: credential.id, region },
      {
        onSuccess: response => toast.success(response.message || '代理绑定已更新'),
        onError: error => toast.error(`代理绑定失败: ${error instanceof Error ? error.message : '未知错误'}`),
      },
    )
  }

  // 远端 overage：乐观更新 → 调上游切换 → 成功后拉真值覆盖 → 失败回滚
  // 父级 balanceMap 是单一真源，全部经 onBalanceChange 派发
  const handleToggleOverage = (next: boolean) => {
    if (!balance) return
    const prev = balance
    const optimistic: BalanceResponse = {
      ...balance,
      overageStatus: next ? 'ENABLED' : 'DISABLED',
    }
    onBalanceChange(credential.id, optimistic)

    setOverage.mutate(
      { id: credential.id, enabled: next },
      {
        onSuccess: async () => {
          toast.success(next ? '已开启超额' : '已关闭超额')
          // 拉真值覆盖：上游可能附带其它字段变化（cap/限额）
          try {
            const fresh = await getCredentialBalance(credential.id)
            onBalanceChange(credential.id, fresh)
          } catch {
            // 真值拉取失败保留乐观值，下次刷新会自然纠偏
          }
        },
        onError: (err) => {
          onBalanceChange(credential.id, prev)
          toast.error('切换超额开关失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handlePriorityChange = () => {
    const newPriority = parseInt(priorityValue, 10)
    if (isNaN(newPriority) || newPriority < 0) {
      toast.error('优先级必须是非负整数')
      return
    }
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingPriority(false)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handleConcurrencyChange = () => {
    const trimmed = concurrencyValue.trim()
    if (trimmed === '') {
      setConcurrency.mutate(
        { id: credential.id, concurrency: null },
        {
          onSuccess: (res) => {
            toast.success(res.message)
            setEditingConcurrency(false)
          },
          onError: (err) => {
            toast.error('操作失败: ' + (err as Error).message)
          },
        }
      )
      return
    }
    const n = parseInt(trimmed, 10)
    if (isNaN(n) || n < 1) {
      toast.error('独立并发必须为正整数或留空')
      return
    }
    setConcurrency.mutate(
      { id: credential.id, concurrency: n },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingConcurrency(false)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('操作失败: ' + (err as Error).message)
      },
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('刷新失败: ' + (err as Error).message)
      },
    })
  }

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error('请先禁用凭据再删除')
      setShowDeleteDialog(false)
      return
    }

    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
      },
      onError: (err) => {
        toast.error('删除失败: ' + (err as Error).message)
      },
    })
  }

  const authLabel = formatCredentialAuthLabel(credential.provider, credential.authMethod)
  const metadataRows = [
    ...getCredentialIdentityRows(credential, {
      keys: ['nickname', 'label', 'sourceAccountId', 'status', 'addedAt'],
    }),
    ...getCredentialMetadataRows(credential, { exclude: ['endpoint'] }),
    ...getCredentialMaterialRows(credential, { exclude: ['hasProfileArn', 'hasToken', 'hasRefreshToken'] }),
    { label: '封禁状态', value: credential.banStatus },
    { label: '封禁原因', value: credential.banReason },
    { label: '封禁时间', value: credential.banTime },
    { label: '请求数', value: credential.requestCount },
    { label: '错误数', value: credential.errorCount },
    { label: '总令牌数', value: credential.totalTokens },
    { label: '总额度', value: credential.totalCredits },
    { label: '上次使用', value: credential.lastUsed },
    { label: '创建时间', value: credential.createdAt },
    { label: '标签', value: credential.tags == null ? null : JSON.stringify(credential.tags) },
  ]
    .map((row) => ({
      label: row.label,
      raw: stringifyCredentialMetadataValue(row.value),
      value: compactCredentialMetadataValue(row.value),
    }))
    .filter((row) => row.value)

  const inFlight = Math.max(0, credential.maxPermits - credential.availablePermits)
  const usagePct = credential.maxPermits > 0
    ? Math.min(100, Math.round((inFlight / credential.maxPermits) * 100))
    : 0
  const usageBusy = credential.maxPermits > 0 && inFlight >= credential.maxPermits

  return (
    <>
      <Card
        className={`group relative flex h-full flex-col overflow-hidden transition-colors ${
          credential.disabled ? 'opacity-70' : 'hover:border-foreground/20'
        }`}
      >
        {/* 状态色条 */}
        <div
          className={`absolute inset-x-0 top-0 h-0.5 ${
            credential.disabled
              ? 'bg-destructive/60'
              : credential.failureCount > 0 || credential.refreshFailureCount > 0
                ? 'bg-warning/70'
                : 'bg-success/60'
          }`}
        />

        {/* === Header === */}
        <div className="flex items-start gap-2.5 px-4 pt-4 pb-3">
          <Checkbox
            checked={selected}
            onCheckedChange={onToggleSelect}
            className="mt-0.5"
          />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-1.5">
              {tooltipContent ? (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="truncate text-sm font-semibold tracking-tight cursor-default">
                      {displayTitle}
                    </span>
                  </TooltipTrigger>
                  <TooltipContent side="top" align="start">
                    {tooltipContent}
                  </TooltipContent>
                </Tooltip>
              ) : (
                <span className="truncate text-sm font-semibold tracking-tight">
                  {displayTitle}
                </span>
              )}
            </div>
            <div className="mt-1 flex flex-wrap items-center gap-1">
              <span className="text-2xs tabular text-muted-foreground">#{credential.id}</span>
              {authLabel && (
                <Badge variant="secondary" className="h-4 rounded px-1.5 text-2xs font-medium">
                  {authLabel}
                </Badge>
              )}
              {credential.endpoint && (
                <Badge variant="outline" className="h-4 rounded px-1.5 text-2xs font-medium">
                  {credential.endpoint}
                </Badge>
              )}
              {credential.hasProfileArn && (
                <Badge variant="outline" className="h-4 rounded px-1.5 text-2xs font-medium">
                  ARN
                </Badge>
              )}
              {credential.disabled && (
                <Badge variant="destructive" className="h-4 rounded px-1.5 text-2xs font-medium">
                  {credential.disabledReason || '已禁用'}
                </Badge>
              )}
            </div>
          </div>
          <Switch
            checked={!credential.disabled}
            onCheckedChange={handleToggleDisabled}
            disabled={setDisabled.isPending}
            className="mt-0.5"
          />
        </div>

        {/* === Balance === */}
        <div className="px-4 pb-3">
          <BalanceBlock
            balance={balance}
            loading={loadingBalance}
            overageMutating={overageMutating}
            onToggleOverage={handleToggleOverage}
          />
        </div>

        {/* === Stats === */}
        <div className="grid grid-cols-2 gap-x-4 gap-y-2.5 border-t bg-muted/20 px-4 py-3 text-xs">
          {/* 优先级（可编辑） */}
          <div className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground">优先级</span>
            {editingPriority ? (
              <div className="flex items-center gap-1">
                <Input
                  type="number"
                  value={priorityValue}
                  onChange={(e) => setPriorityValue(e.target.value)}
                  className="h-6 w-14 text-xs"
                  min="0"
                />
                <Button size="sm" variant="ghost" className="h-6 w-6 p-0" onClick={handlePriorityChange} disabled={setPriority.isPending}>✓</Button>
                <Button size="sm" variant="ghost" className="h-6 w-6 p-0" onClick={() => { setEditingPriority(false); setPriorityValue(String(credential.priority)) }}>✕</Button>
              </div>
            ) : (
              <button
                className="tabular font-medium hover:underline"
                onClick={() => setEditingPriority(true)}
                title="点击编辑"
              >
                {credential.priority}
              </button>
            )}
          </div>

          {/* 独立并发（可编辑） */}
          <div className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground">独立并发</span>
            {editingConcurrency ? (
              <div className="flex items-center gap-1">
                <Input
                  type="number"
                  value={concurrencyValue}
                  onChange={(e) => setConcurrencyValue(e.target.value)}
                  className="h-6 w-14 text-xs"
                  min="1"
                  placeholder="全局"
                />
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-6 px-1.5 text-2xs"
                  onClick={() => { setConcurrencyValue(''); }}
                  disabled={setConcurrency.isPending}
                  title="清空 = 恢复全局默认"
                >全局</Button>
                <Button size="sm" variant="ghost" className="h-6 w-6 p-0" onClick={handleConcurrencyChange} disabled={setConcurrency.isPending}>✓</Button>
                <Button size="sm" variant="ghost" className="h-6 w-6 p-0" onClick={() => { setEditingConcurrency(false); setConcurrencyValue(credential.concurrency == null ? '' : String(credential.concurrency)) }}>✕</Button>
              </div>
            ) : (
              <button
                className="tabular font-medium hover:underline"
                onClick={() => setEditingConcurrency(true)}
                title={credential.concurrency == null ? '当前跟随全局；点击编辑' : '已自定义；点击编辑'}
              >
                {credential.concurrency == null ? '跟随全局' : credential.concurrency}
              </button>
            )}
          </div>

          {/* 并发占用 */}
          <div className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground">并发占用</span>
            <span className={`tabular font-medium ${usageBusy ? 'text-warning' : ''}`}>
              {inFlight}/{credential.maxPermits}
              {credential.maxPermits > 0 && (
                <span className="ml-1 text-2xs text-muted-foreground">({usagePct}%)</span>
              )}
            </span>
          </div>

          {/* 订阅 */}
          <div className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground">订阅</span>
            <span className="truncate font-medium">
              {loadingBalance ? <Loader2 className="inline h-3 w-3 animate-spin" /> : balance?.subscriptionTitle || '—'}
            </span>
          </div>

          {/* 成功 / 失败 */}
          <div className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground">成功</span>
            <span className="tabular font-medium">{credential.successCount}</span>
          </div>
          <div className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground">失败</span>
            <span className={`tabular font-medium ${credential.failureCount > 0 ? 'text-destructive' : ''}`}>
              {credential.failureCount}
              {credential.refreshFailureCount > 0 && (
                <span className="ml-1 text-2xs text-muted-foreground">+{credential.refreshFailureCount} 刷新</span>
              )}
            </span>
          </div>

          {/* 最后调用 / API 密钥 */}
          <div className="col-span-2 flex items-center justify-between gap-2 pt-1.5 border-t border-border/50">
            <span className="text-muted-foreground">最后调用</span>
            <span className="tabular font-medium">{formatLastUsed(credential.lastUsedAt)}</span>
          </div>
          {credential.maskedApiKey && (
            <div className="col-span-2 flex items-center justify-between gap-2">
              <span className="text-muted-foreground">API 密钥</span>
              <span className="font-mono text-2xs">{credential.maskedApiKey}</span>
            </div>
          )}
          {credential.hasProxy && (credential.proxyUrl || boundProxy || credential.proxyId != null) && (
            <div className="col-span-2 flex items-center justify-between gap-2">
              <span className="text-muted-foreground">代理</span>
              <span
                className="truncate font-mono text-2xs"
                title={credential.proxyUrl || boundProxy?.url || `代理 #${credential.proxyId}`}
              >
                {credential.proxyUrl || (boundProxy ? `${boundProxy.region ?? '未分组'} · ${boundProxy.url}` : `代理 #${credential.proxyId}`)}
              </span>
            </div>
          )}
          <div className="col-span-2 flex items-center justify-between gap-2">
            <span className="text-muted-foreground">代理池</span>
            <select
              value={boundProxyRegion ?? ''}
              disabled={setProxyByRegion.isPending}
              onChange={event => handleSetProxyRegion(event.target.value ? event.target.value : null)}
              className="h-7 max-w-[62%] rounded-md border border-input bg-background px-2 text-2xs outline-none"
              title={boundProxy ? boundProxy.url : '未绑定代理池'}
            >
              <option value="">未绑定</option>
              {boundProxyRegion && !proxyRegions.includes(boundProxyRegion) && (
                <option value={boundProxyRegion}>{boundProxyRegion}</option>
              )}
              {proxyRegions.map(region => (
                <option key={region} value={region}>
                  {region}
                </option>
              ))}
            </select>
          </div>
          {boundProxy && (
            <div className="col-span-2 flex items-center justify-between gap-2">
              <span className="text-muted-foreground">池代理</span>
              <span className="truncate font-mono text-2xs" title={boundProxy.url}>
                {boundProxy.url}
              </span>
            </div>
          )}
          {metadataRows.length > 0 && (
            <div className="col-span-2 space-y-1.5 border-t border-border/50 pt-2">
              <div className="text-2xs font-medium text-muted-foreground">来源元数据</div>
              {metadataRows.map((row) => (
                <div key={row.label} className="flex items-center justify-between gap-2">
                  <span className="shrink-0 text-muted-foreground">{row.label}</span>
                  <span className="truncate font-mono text-2xs" title={row.raw || undefined}>
                    {row.value}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* === Footer 操作区 === */}
        <div className="mt-auto flex flex-wrap items-center gap-1 border-t bg-background px-3 py-2.5">
          <Button
            size="sm"
            variant="default"
            className="h-7 px-2.5 text-xs"
            onClick={() => onViewBalance(credential.id)}
          >
            <Wallet className="mr-1 h-3 w-3" />
            余额
          </Button>
          <Button
            size="sm"
            variant="ghost"
            className="h-7 px-2 text-xs"
            onClick={() => onViewModels(credential.id)}
            disabled={credential.authMethod === 'api_key'}
            title={credential.authMethod === 'api_key' ? 'API 密钥凭据不支持模型查询' : '查看可用模型'}
          >
            <Boxes className="mr-1 h-3 w-3" />
            模型
          </Button>
          <Button
            size="sm"
            variant="ghost"
            className="h-7 px-2 text-xs"
            onClick={handleForceRefresh}
            disabled={forceRefresh.isPending || credential.disabled || credential.authMethod === 'api_key'}
            title={credential.authMethod === 'api_key' ? 'API 密钥凭据无需刷新' : credential.disabled ? '已禁用' : '强制刷新令牌'}
          >
            <RefreshCw className={`mr-1 h-3 w-3 ${forceRefresh.isPending ? 'animate-spin' : ''}`} />
            刷新
          </Button>
          <Button
            size="sm"
            variant="ghost"
            className="h-7 px-2 text-xs"
            onClick={handleReset}
            disabled={resetFailure.isPending || (credential.failureCount === 0 && credential.refreshFailureCount === 0)}
          >
            <RotateCcw className="mr-1 h-3 w-3" />
            重置
          </Button>
          <div className="ml-auto flex items-center gap-0.5">
            <Button
              size="sm"
              variant="ghost"
              className="h-7 w-7 p-0"
              onClick={() => {
                const newPriority = Math.max(0, credential.priority - 1)
                setPriority.mutate(
                  { id: credential.id, priority: newPriority },
                  {
                    onSuccess: (res) => toast.success(res.message),
                    onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                  }
                )
              }}
              disabled={setPriority.isPending || credential.priority === 0}
              title="提高优先级"
            >
              <ChevronUp className="h-3.5 w-3.5" />
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="h-7 w-7 p-0"
              onClick={() => {
                const newPriority = credential.priority + 1
                setPriority.mutate(
                  { id: credential.id, priority: newPriority },
                  {
                    onSuccess: (res) => toast.success(res.message),
                    onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                  }
                )
              }}
              disabled={setPriority.isPending}
              title="降低优先级"
            >
              <ChevronDown className="h-3.5 w-3.5" />
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="h-7 w-7 p-0 text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={() => setShowDeleteDialog(true)}
              disabled={!credential.disabled}
              title={!credential.disabled ? '需要先禁用凭据才能删除' : '删除'}
            >
              <Trash2 className="h-3.5 w-3.5" />
            </Button>
          </div>
        </div>
      </Card>

      {/* 删除确认对话框 */}
      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除凭据 #{credential.id} 吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}
