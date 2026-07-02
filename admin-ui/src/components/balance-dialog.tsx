import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Progress } from '@/components/ui/progress'
import { useCredentialBalance } from '@/hooks/use-credentials'
import {
  formatCredentialBalanceDate,
  formatCredentialBalanceNumber,
  formatCredentialOverageStatusLabel,
  getCredentialBalanceBaseUsage,
  getCredentialBalanceOverageUsage,
} from '@/lib/credential-balance'
import { parseError } from '@/lib/utils'

interface BalanceDialogProps {
  credentialId: number | null
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function BalanceDialog({ credentialId, open, onOpenChange }: BalanceDialogProps) {
  const { data: balance, isLoading, error } = useCredentialBalance(credentialId, true)
  const baseUsage = balance ? getCredentialBalanceBaseUsage(balance) : null
  const overageUsage = balance ? getCredentialBalanceOverageUsage(balance) : null

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            凭据 #{credentialId} 余额信息
          </DialogTitle>
        </DialogHeader>

        {isLoading && (
          <div className="flex items-center justify-center py-8">
            <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-primary"></div>
          </div>
        )}

        {error && (() => {
          const parsed = parseError(error)
          return (
            <div className="py-6 space-y-3">
              <div className="flex items-center justify-center gap-2 text-red-500">
                <svg className="h-5 w-5" viewBox="0 0 20 20" fill="currentColor">
                  <path fillRule="evenodd" d="M10 18a8 8 0 100-16 8 8 0 000 16zM8.707 7.293a1 1 0 00-1.414 1.414L8.586 10l-1.293 1.293a1 1 0 101.414 1.414L10 11.414l1.293 1.293a1 1 0 001.414-1.414L11.414 10l1.293-1.293a1 1 0 00-1.414-1.414L10 8.586 8.707 7.293z" clipRule="evenodd" />
                </svg>
                <span className="font-medium">{parsed.title}</span>
              </div>
              {parsed.detail && (
                <div className="text-sm text-muted-foreground text-center px-4">
                  {parsed.detail}
                </div>
              )}
            </div>
          )
        })()}

        {balance && baseUsage && overageUsage && (
          <div className="space-y-4">
            {/* 订阅类型 */}
            <div className="text-center">
              <span className="text-lg font-semibold">
                {balance.subscriptionTitle || '未知订阅类型'}
              </span>
            </div>

            {/* 正式额度进度 */}
            <div className="space-y-2">
              <div className="flex justify-between text-sm">
                <span>正式额度</span>
                <span className="text-muted-foreground">
                  {formatCredentialBalanceNumber(baseUsage.used)} / {formatCredentialBalanceNumber(baseUsage.limit)}
                </span>
              </div>
              <Progress value={baseUsage.percent} />
              <div className="text-center text-sm text-muted-foreground">
                剩余 ${formatCredentialBalanceNumber(baseUsage.remaining)} · 下次重置 {formatCredentialBalanceDate(balance.nextResetAt)}
              </div>
            </div>

            {/* 超额额度进度 */}
            {overageUsage.visible && (
              <div className="space-y-2 pt-2 border-t">
                <div className="flex justify-between text-sm">
                  <span>超额额度</span>
                  <span className="text-muted-foreground">
                    {formatCredentialBalanceNumber(overageUsage.remaining)} / {overageUsage.cap > 0 ? formatCredentialBalanceNumber(overageUsage.cap) : '—'}
                  </span>
                </div>
                {overageUsage.cap > 0 ? (
                  <Progress value={overageUsage.percent} />
                ) : (
                  <div className="text-xs text-muted-foreground">订阅未提供超额上限</div>
                )}
                <div className="text-center text-xs text-muted-foreground">
                  {formatCredentialOverageStatusLabel(balance, { remote: true })}
                </div>
              </div>
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}
