import { Loader2 } from 'lucide-react'
import { Switch } from '@/components/ui/switch'
import type { BalanceResponse } from '@/types/api'
import {
  formatCredentialBalanceNumber,
  formatCredentialOverageStatusLabel,
  getCredentialBalanceBaseUsage,
  getCredentialBalanceOverageUsage,
} from '@/lib/credential-balance'

interface BalanceBlockProps {
  balance: BalanceResponse | null
  loading: boolean
  overageMutating: boolean
  onToggleOverage: (next: boolean) => void
}

// 余额展示：正式额度进度条 + 超额进度条
// 正式额度按 usage_limit 计 100%；超额（current_usage > usage_limit）从超额池计算独立进度条
export function BalanceBlock({ balance, loading, overageMutating, onToggleOverage }: BalanceBlockProps) {
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
