import type { BalanceResponse } from '@/types/api'

export interface CredentialBalanceBaseUsage {
  used: number
  limit: number
  remaining: number
  percent: number
  resetAt: string | null
}

export interface CredentialBalanceOverageUsage {
  visible: boolean
  used: number
  cap: number
  remaining: number
  percent: number
}

export function formatCredentialBalanceNumber(value: number): string {
  return value.toLocaleString('zh-CN', {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })
}

export function formatCredentialBalanceDate(
  timestamp: number | null | undefined,
  fallback = '未知',
): string {
  if (!timestamp) return fallback
  return new Date(timestamp * 1000).toLocaleString('zh-CN')
}

export function getCredentialBalanceBaseUsage(balance: BalanceResponse): CredentialBalanceBaseUsage {
  const limit = balance.usageLimit
  const used = Math.min(balance.currentUsage, limit)
  const percent = limit > 0 ? Math.min(100, (used / limit) * 100) : 0

  return {
    used,
    limit,
    remaining: Math.max(0, balance.remaining),
    percent,
    resetAt: balance.nextResetAt ? formatCredentialBalanceDate(balance.nextResetAt) : null,
  }
}

export function getCredentialBalanceOverageUsage(balance: BalanceResponse): CredentialBalanceOverageUsage {
  const used = Math.max(0, balance.currentUsage - balance.usageLimit)
  const cap = balance.overageCap || 0
  const subscriptionType = balance.subscriptionType?.trim().toUpperCase()
  const canShowOverage = subscriptionType ? subscriptionType !== 'FREE' : true
  const visible = canShowOverage && (
    balance.overageCapability === 'OVERAGE_CAPABLE'
    || used > 0
    || cap > 0
  )

  return {
    visible,
    used,
    cap,
    remaining: Math.max(0, cap - used),
    percent: cap > 0 ? Math.min(100, (used / cap) * 100) : 0,
  }
}

export function formatCredentialOverageStatusLabel(
  balance: BalanceResponse,
  options: { mutating?: boolean; remote?: boolean } = {},
): string {
  if (options.mutating) return '切换中...'

  if (!options.remote && balance.overageCapability === 'OVERAGE_CAPABLE') {
    return balance.overageStatus === 'ENABLED' ? '已开启' : '已关闭'
  }

  if (balance.overageStatus === 'ENABLED') {
    return options.remote ? '远端开关：已启用' : '已开启'
  }
  if (balance.overageStatus === 'DISABLED') {
    return options.remote ? '远端开关：已禁用' : '已关闭'
  }
  if (balance.overageCapability === 'OVERAGE_INCAPABLE') {
    return options.remote ? '订阅不支持超额' : '订阅不支持'
  }
  return ''
}
