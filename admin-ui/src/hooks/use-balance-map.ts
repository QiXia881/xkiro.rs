import { useEffect, useState } from 'react'
import { getCachedBalances } from '@/api/credentials'
import type { BalanceResponse, CredentialStatusItem, RuntimeStatsItem } from '@/types/api'

/**
 * 凭据余额缓存管理。
 *
 * 集中维护 `balanceMap`（id -> 余额）及查询中的 `loadingBalanceIds`，
 * 并托管三个副作用：
 * - 裁剪：删除已不存在凭据的缓存，避免残留旧数据；
 * - 预填：首次挂载拉取后端磁盘缓存余额；
 * - 合入：把 runtime-stats（1s 轮询）里嵌的余额快照实时投影进来。
 *
 * dashboard 只需把凭据列表与 runtimeMap 传进来，
 * 批量 handler 继续通过返回的 setter 回填余额。
 */
export function useBalanceMap(
  credentials: CredentialStatusItem[],
  runtimeMap: Map<number, RuntimeStatsItem> | undefined,
) {
  const [balanceMap, setBalanceMap] = useState<Map<number, BalanceResponse>>(new Map())
  const [loadingBalanceIds, setLoadingBalanceIds] = useState<Set<number>>(new Set())

  // 只保留当前仍存在的凭据缓存，避免删除后残留旧数据
  useEffect(() => {
    if (credentials.length === 0) {
      setBalanceMap(new Map())
      setLoadingBalanceIds(new Set())
      return
    }

    const validIds = new Set(credentials.map(credential => credential.id))

    setBalanceMap(prev => {
      const next = new Map<number, BalanceResponse>()
      prev.forEach((value, id) => {
        if (validIds.has(id)) {
          next.set(id, value)
        }
      })
      return next.size === prev.size ? prev : next
    })

    setLoadingBalanceIds(prev => {
      if (prev.size === 0) {
        return prev
      }
      const next = new Set<number>()
      prev.forEach(id => {
        if (validIds.has(id)) {
          next.add(id)
        }
      })
      return next.size === prev.size ? prev : next
    })
  }, [credentials])

  // 首次挂载拉取后端缓存余额，预填到 balanceMap
  // 后端启动时会并行预取所有未禁用凭据的余额并写入磁盘缓存，
  // 这里直接复用，省掉用户进入页面后再手动点查询的步骤
  useEffect(() => {
    let cancelled = false
    getCachedBalances()
      .then(resp => {
        if (cancelled) return
        setBalanceMap(prev => {
          const next = new Map(prev)
          const cachedBalances = Array.isArray(resp.balances) ? resp.balances : []
          cachedBalances.forEach(item => {
            // 把 CachedBalanceItem 投影到 BalanceResponse 形状（字段一一对应）
            next.set(item.id, {
              id: item.id,
              subscriptionTitle: item.subscriptionTitle,
              subscriptionType: item.subscriptionType,
              currentUsage: item.currentUsage,
              usageLimit: item.usageLimit,
              remaining: item.remaining,
              usagePercentage: item.usagePercentage,
              nextResetAt: item.nextResetAt,
              overageCap: item.overageCap,
              overageCapability: item.overageCapability,
              overageStatus: item.overageStatus,
            })
          })
          return next
        })
      })
      .catch(() => {
        // 缓存接口失败不打扰用户，让 dashboard 走原本的手动查询路径
      })
    return () => {
      cancelled = true
    }
  }, [])

  // 把 runtime-stats（1s 轮询）里嵌的余额投影到 balanceMap，实现实时显示
  // 后端余额来自 5min disk cache + 周期后台刷新；前端只负责消费快照
  useEffect(() => {
    if (!runtimeMap || runtimeMap.size === 0) return
    setBalanceMap(prev => {
      let mutated = false
      const next = new Map(prev)
      runtimeMap.forEach((runtime, id) => {
        if (!runtime.balance) return
        const existing = prev.get(id)
        // 浅比较关键字段，避免无变化时触发卡片重渲染
        if (
          existing
          && existing.currentUsage === runtime.balance.currentUsage
          && existing.usageLimit === runtime.balance.usageLimit
          && existing.remaining === runtime.balance.remaining
          && existing.subscriptionType === runtime.balance.subscriptionType
          && existing.overageStatus === runtime.balance.overageStatus
          && existing.overageCap === runtime.balance.overageCap
        ) {
          return
        }
        next.set(id, {
          id,
          subscriptionTitle: runtime.balance.subscriptionTitle,
          subscriptionType: runtime.balance.subscriptionType,
          currentUsage: runtime.balance.currentUsage,
          usageLimit: runtime.balance.usageLimit,
          remaining: runtime.balance.remaining,
          usagePercentage: runtime.balance.usagePercentage,
          nextResetAt: runtime.balance.nextResetAt,
          overageCap: runtime.balance.overageCap,
          overageCapability: runtime.balance.overageCapability,
          overageStatus: runtime.balance.overageStatus,
        })
        mutated = true
      })
      return mutated ? next : prev
    })
  }, [runtimeMap])

  return { balanceMap, setBalanceMap, loadingBalanceIds, setLoadingBalanceIds }
}
