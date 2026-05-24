import { useEffect, useState } from 'react'
import { toast } from 'sonner'
import { RefreshCw, ChevronUp, ChevronDown, Wallet, Trash2, Loader2 } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
import { Progress } from '@/components/ui/progress'
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
import { getCredentialBalance } from '@/api/credentials'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
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

function formatResetDate(ts: number | null): string | null {
  if (!ts) return null
  const date = new Date(ts * 1000)
  return date.toLocaleDateString('zh-CN', { year: 'numeric', month: '2-digit', day: '2-digit' })
}

function daysUntil(ts: number | null): number | null {
  if (!ts) return null
  const diff = ts * 1000 - Date.now()
  if (diff <= 0) return 0
  return Math.ceil(diff / (1000 * 60 * 60 * 24))
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
      <div className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="w-3 h-3 animate-spin" /> 加载中...
      </div>
    )
  }

  if (!balance) {
    return (
      <div className="flex items-center justify-between">
        <span className="text-muted-foreground text-sm">剩余用量：</span>
        <span className="text-sm text-muted-foreground">未知</span>
      </div>
    )
  }

  const limit = balance.usageLimit
  const used = balance.currentUsage
  // 正式额度部分：未超额时按实际使用，超额时锁定 100%
  const baseUsed = Math.min(used, limit)
  const baseRemaining = Math.max(0, limit - used)
  const basePercent = limit > 0 ? Math.min(100, (baseUsed / limit) * 100) : 0
  // 超额部分：current_usage 超过 usage_limit 的差值
  const overUsed = Math.max(0, used - limit)
  const overCap = balance.overageCap || 0
  const overPercent = overCap > 0 ? Math.min(100, (overUsed / overCap) * 100) : 0

  const resetStr = formatResetDate(balance.nextResetAt)
  const daysLeft = daysUntil(balance.nextResetAt)
  const showOverage = balance.overageCapability === 'OVERAGE_CAPABLE' || overUsed > 0 || overCap > 0

  return (
    <div className="space-y-3">
      {/* 正式额度 */}
      <div className="space-y-1">
        <div className="flex items-center justify-between">
          <span className="text-muted-foreground text-sm">正式额度</span>
          <span className="font-medium text-sm">
            {baseRemaining.toFixed(2)} / {limit.toFixed(2)}
            <span className="text-xs text-muted-foreground ml-1">
              ({(100 - basePercent).toFixed(1)}% 剩余)
            </span>
          </span>
        </div>
        <Progress value={basePercent} className="h-2" />
        {resetStr && (
          <div className="text-xs text-muted-foreground">
            下次重置：{resetStr}
            {daysLeft != null && ` · ${daysLeft === 0 ? '今日' : `${daysLeft} 天后`}`}
          </div>
        )}
      </div>

      {/* 超额额度 */}
      {showOverage && (
        <div className="space-y-1">
          <div className="flex items-center justify-between">
            <span className="text-muted-foreground text-sm">超额额度</span>
            <span className="font-medium text-sm">
              {overUsed.toFixed(2)} / {overCap > 0 ? overCap.toFixed(2) : '—'}
              {overCap > 0 && (
                <span className="text-xs text-muted-foreground ml-1">
                  ({overPercent.toFixed(1)}% 已用)
                </span>
              )}
            </span>
          </div>
          {overCap > 0 ? (
            <Progress value={overPercent} className="h-2" />
          ) : (
            <div className="text-xs text-muted-foreground">订阅未提供超额上限</div>
          )}
          <div className="flex items-center justify-between text-xs pt-0.5">
            <span className="text-muted-foreground">
              {overageMutating
                ? '切换中...'
                : balance.overageCapability === 'OVERAGE_CAPABLE'
                  ? balance.overageStatus === 'ENABLED'
                    ? '订阅支持超额 · 已开启'
                    : '订阅支持超额 · 已关闭'
                  : balance.overageCapability === 'OVERAGE_INCAPABLE'
                    ? '订阅不支持超额'
                    : ''}
            </span>
            {balance.overageCapability === 'OVERAGE_CAPABLE' && (
              <div className="flex items-center gap-2">
                <span className="text-muted-foreground">超额开关</span>
                <Switch
                  checked={balance.overageStatus === 'ENABLED'}
                  disabled={overageMutating}
                  onCheckedChange={onToggleOverage}
                />
              </div>
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
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
  onBalanceChange,
}: CredentialCardProps) {
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
  // 与 runtime-stats 1.5s 轮询解耦，避免每次接口返回都把所有卡片掀一遍。
  const [, setTick] = useState(0)
  useEffect(() => {
    const timer = setInterval(() => setTick((t) => t + 1), 30_000)
    return () => clearInterval(timer)
  }, [])

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const setConcurrency = useSetCredentialConcurrency()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()
  const setOverage = useSetOverage()
  const overageMutating = setOverage.isPending

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

  // KAM 模式：乐观更新 → 调上游切换 → 成功后拉真值覆盖 → 失败回滚
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

  return (
    <>
      <Card>
        <CardHeader className="pb-2">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Checkbox
                checked={selected}
                onCheckedChange={onToggleSelect}
              />
              <CardTitle className="text-lg flex items-center gap-2">
                {credential.email || `凭据 #${credential.id}`}
                {credential.disabled && (
                  <Badge variant="destructive">已禁用</Badge>
                )}
                {credential.disabled && credential.disabledReason && (
                  <Badge variant="outline">{credential.disabledReason}</Badge>
                )}
                {credential.authMethod && (
                  <Badge variant="secondary">
                    {credential.authMethod === 'api_key' ? 'API Key' :
                     credential.authMethod === 'idc' ? 'IdC' :
                     credential.authMethod === 'social' ? 'Social' :
                     credential.authMethod}
                  </Badge>
                )}
                {credential.endpoint && (
                  <Badge variant="outline">{credential.endpoint}</Badge>
                )}
              </CardTitle>
            </div>
            <div className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground">启用</span>
              <Switch
                checked={!credential.disabled}
                onCheckedChange={handleToggleDisabled}
                disabled={setDisabled.isPending}
              />
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          {/* 信息网格 */}
          <div className="grid grid-cols-2 gap-4 text-sm">
            <div>
              <span className="text-muted-foreground">优先级：</span>
              {editingPriority ? (
                <div className="inline-flex items-center gap-1 ml-1">
                  <Input
                    type="number"
                    value={priorityValue}
                    onChange={(e) => setPriorityValue(e.target.value)}
                    className="w-16 h-7 text-sm"
                    min="0"
                  />
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={handlePriorityChange}
                    disabled={setPriority.isPending}
                  >
                    ✓
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={() => {
                      setEditingPriority(false)
                      setPriorityValue(String(credential.priority))
                    }}
                  >
                    ✕
                  </Button>
                </div>
              ) : (
                <span
                  className="font-medium cursor-pointer hover:underline ml-1"
                  onClick={() => setEditingPriority(true)}
                >
                  {credential.priority}
                  <span className="text-xs text-muted-foreground ml-1">(点击编辑)</span>
                </span>
              )}
            </div>
            <div>
              <span className="text-muted-foreground">独立并发：</span>
              {editingConcurrency ? (
                <div className="inline-flex items-center gap-1 ml-1">
                  <Input
                    type="number"
                    value={concurrencyValue}
                    onChange={(e) => setConcurrencyValue(e.target.value)}
                    className="w-20 h-7 text-sm"
                    min="1"
                    placeholder="留空回退"
                  />
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={handleConcurrencyChange}
                    disabled={setConcurrency.isPending}
                  >
                    ✓
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={() => {
                      setEditingConcurrency(false)
                      setConcurrencyValue(
                        credential.concurrency == null ? '' : String(credential.concurrency)
                      )
                    }}
                  >
                    ✕
                  </Button>
                </div>
              ) : (
                <span
                  className="font-medium cursor-pointer hover:underline ml-1"
                  onClick={() => setEditingConcurrency(true)}
                >
                  {credential.concurrency == null ? '全局回退' : credential.concurrency}
                  <span className="text-xs text-muted-foreground ml-1">(点击编辑)</span>
                </span>
              )}
            </div>
            <div>
              <span className="text-muted-foreground">失败次数：</span>
              <span className={credential.failureCount > 0 ? 'text-red-500 font-medium' : ''}>
                {credential.failureCount}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">刷新失败：</span>
              <span className={credential.refreshFailureCount > 0 ? 'text-red-500 font-medium' : ''}>
                {credential.refreshFailureCount}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">订阅等级：</span>
              <span className="font-medium">
                {loadingBalance ? (
                  <Loader2 className="inline w-3 h-3 animate-spin" />
                ) : balance?.subscriptionTitle || '未知'}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">成功次数：</span>
              <span className="font-medium">{credential.successCount}</span>
            </div>
            <div>
              <span className="text-muted-foreground">并发占用：</span>
              <span
                className={
                  credential.maxPermits > 0 &&
                  credential.maxPermits - credential.availablePermits >= credential.maxPermits
                    ? 'font-medium text-amber-500'
                    : 'font-medium'
                }
              >
                {credential.maxPermits - credential.availablePermits}/{credential.maxPermits}
              </span>
            </div>
            <div className="col-span-2">
              <span className="text-muted-foreground">最后调用：</span>
              <span className="font-medium">{formatLastUsed(credential.lastUsedAt)}</span>
            </div>
            {credential.maskedApiKey && (
              <div className="col-span-2">
                <span className="text-muted-foreground">API Key：</span>
                <span className="font-mono font-medium">{credential.maskedApiKey}</span>
              </div>
            )}
            <div className="col-span-2 space-y-2">
              <BalanceBlock
                balance={balance}
                loading={loadingBalance}
                overageMutating={overageMutating}
                onToggleOverage={handleToggleOverage}
              />
            </div>
            {credential.hasProxy && (
              <div className="col-span-2">
                <span className="text-muted-foreground">代理：</span>
                <span className="font-medium">{credential.proxyUrl}</span>
              </div>
            )}
            {credential.hasProfileArn && (
              <div className="col-span-2">
                <Badge variant="secondary">有 Profile ARN</Badge>
              </div>
            )}
          </div>

          {/* 操作按钮 */}
          <div className="flex flex-wrap gap-2 pt-2 border-t">
            <Button
              size="sm"
              variant="outline"
              onClick={handleReset}
              disabled={resetFailure.isPending || (credential.failureCount === 0 && credential.refreshFailureCount === 0)}
            >
              <RefreshCw className="h-4 w-4 mr-1" />
              重置失败
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={handleForceRefresh}
              disabled={forceRefresh.isPending || credential.disabled || credential.authMethod === 'api_key'}
              title={credential.authMethod === 'api_key' ? 'API Key 凭据无需刷新 Token' : credential.disabled ? '已禁用的凭据无法刷新 Token' : '强制刷新 Token'}
            >
              <RefreshCw className={`h-4 w-4 mr-1 ${forceRefresh.isPending ? 'animate-spin' : ''}`} />
              刷新 Token
            </Button>
            <Button
              size="sm"
              variant="outline"
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
            >
              <ChevronUp className="h-4 w-4 mr-1" />
              提高优先级
            </Button>
            <Button
              size="sm"
              variant="outline"
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
            >
              <ChevronDown className="h-4 w-4 mr-1" />
              降低优先级
            </Button>
            <Button
              size="sm"
              variant="default"
              onClick={() => onViewBalance(credential.id)}
            >
              <Wallet className="h-4 w-4 mr-1" />
              查看余额
            </Button>
            <Button
              size="sm"
              variant="destructive"
              onClick={() => setShowDeleteDialog(true)}
              disabled={!credential.disabled}
              title={!credential.disabled ? '需要先禁用凭据才能删除' : undefined}
            >
              <Trash2 className="h-4 w-4 mr-1" />
              删除
            </Button>
          </div>
        </CardContent>
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
