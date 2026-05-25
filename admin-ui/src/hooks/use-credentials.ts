import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getCredentials,
  setCredentialDisabled,
  setCredentialPriority,
  setCredentialConcurrency,
  resetCredentialFailure,
  forceRefreshToken,
  getCredentialBalance,
  setOverageStatus,
  addCredential,
  deleteCredential,
} from '@/api/credentials'
import { usePageActive } from '@/hooks/use-page-active'
import type { AddCredentialRequest } from '@/types/api'

// 查询凭据列表
export function useCredentials() {
  const pageActive = usePageActive()
  return useQuery({
    queryKey: ['credentials'],
    queryFn: getCredentials,
    // 仅在用户停留前端时周期刷新；离开后停止
    refetchInterval: pageActive ? 30000 : false,
    refetchIntervalInBackground: false,
  })
}

// 查询凭据余额；force=true 跳过后端缓存
export function useCredentialBalance(id: number | null, force = false) {
  return useQuery({
    queryKey: ['credential-balance', id, force],
    queryFn: () => getCredentialBalance(id!, force),
    enabled: id !== null,
    retry: false, // 余额查询失败时不重试（避免重复请求被封禁的账号）
  })
}

// 设置禁用状态
export function useSetDisabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setCredentialDisabled(id, disabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置优先级
export function useSetPriority() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, priority }: { id: number; priority: number }) =>
      setCredentialPriority(id, priority),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置单凭据最大并发（null = 使用全局回退）
export function useSetCredentialConcurrency() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, concurrency }: { id: number; concurrency: number | null }) =>
      setCredentialConcurrency(id, concurrency),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置失败计数
export function useResetFailure() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetCredentialFailure(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 强制刷新 Token
export function useForceRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => forceRefreshToken(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 切换上游 overage 开关
export function useSetOverage() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, enabled }: { id: number; enabled: boolean }) =>
      setOverageStatus(id, enabled),
    onSuccess: (_data, vars) => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      queryClient.invalidateQueries({ queryKey: ['credential-balance', vars.id] })
    },
  })
}

// 添加新凭据
export function useAddCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: AddCredentialRequest) => addCredential(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 删除凭据
export function useDeleteCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteCredential(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}
