import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  addProxy,
  autoAssignProxies,
  deleteProxy,
  getProxies,
  importProxies,
  setCredentialProxy,
  setCredentialProxyByRegion,
  testProxy,
  updateProxy,
} from '@/api/proxies'
import { usePageActive } from '@/hooks/use-page-active'
import type { ProxyAutoAssignRequest, ProxyImportRequest, ProxyUpsertRequest } from '@/types/api'

export function useProxies() {
  const pageActive = usePageActive()
  return useQuery({
    queryKey: ['proxies'],
    queryFn: getProxies,
    refetchInterval: pageActive ? 30000 : false,
    refetchIntervalInBackground: false,
  })
}

export function useAddProxy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: ProxyUpsertRequest) => addProxy(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxies'] })
    },
  })
}

export function useUpdateProxy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: ProxyUpsertRequest }) => updateProxy(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxies'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useDeleteProxy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteProxy(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxies'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useTestProxy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => testProxy(id),
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: ['proxies'] })
    },
  })
}

export function useImportProxies() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: ProxyImportRequest) => importProxies(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxies'] })
    },
  })
}

export function useAutoAssignProxies() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: ProxyAutoAssignRequest) => autoAssignProxies(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxies'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useSetCredentialProxy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, proxyId }: { id: number; proxyId: number | null }) =>
      setCredentialProxy(id, proxyId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxies'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useSetCredentialProxyByRegion() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, region }: { id: number; region: string | null }) =>
      setCredentialProxyByRegion(id, region),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxies'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}
