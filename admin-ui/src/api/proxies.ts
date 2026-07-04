import { adminApi as api } from '@/api/client'
import type {
  ProxyAutoAssignRequest,
  ProxyAutoAssignResponse,
  ProxyImportRequest,
  ProxyImportResponse,
  ProxyListResponse,
  ProxyTestResponse,
  ProxyUpsertRequest,
  SetCredentialProxyByRegionResponse,
  SetCredentialProxyRequest,
  SuccessResponse,
} from '@/types/api'

export async function getProxies(): Promise<ProxyListResponse> {
  const { data } = await api.get<ProxyListResponse>('/proxies')
  return data
}

export async function addProxy(req: ProxyUpsertRequest): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>('/proxies', req)
  return data
}

export async function updateProxy(id: number, req: ProxyUpsertRequest): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>(`/proxies/${id}`, req)
  return data
}

export async function deleteProxy(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/proxies/${id}`)
  return data
}

export async function testProxy(id: number): Promise<ProxyTestResponse> {
  const { data } = await api.post<ProxyTestResponse>(`/proxies/${id}/test`)
  return data
}

export async function importProxies(req: ProxyImportRequest): Promise<ProxyImportResponse> {
  const { data } = await api.post<ProxyImportResponse>('/proxies/import', req)
  return data
}

export async function autoAssignProxies(
  req: ProxyAutoAssignRequest,
): Promise<ProxyAutoAssignResponse> {
  const { data } = await api.post<ProxyAutoAssignResponse>('/proxies/auto-assign', req)
  return data
}

export async function setCredentialProxy(
  id: number,
  proxyId: number | null,
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/proxy`,
    { proxyId } satisfies SetCredentialProxyRequest,
  )
  return data
}

export async function setCredentialProxyByRegion(
  id: number,
  region: string | null,
): Promise<SetCredentialProxyByRegionResponse> {
  const { data } = await api.post<SetCredentialProxyByRegionResponse>(
    `/credentials/${id}/proxy-by-region`,
    { region },
  )
  return data
}
