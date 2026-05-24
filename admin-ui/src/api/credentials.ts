import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  SetConcurrencyRequest,
  AddCredentialRequest,
  AddCredentialResponse,
} from '@/types/api'

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

// 请求拦截器添加 API Key
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>('/credentials')
  return data
}

// 设置凭据禁用状态
export async function setCredentialDisabled(
  id: number,
  disabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/disabled`,
    { disabled } as SetDisabledRequest
  )
  return data
}

// 设置凭据优先级
export async function setCredentialPriority(
  id: number,
  priority: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/priority`,
    { priority } as SetPriorityRequest
  )
  return data
}

// 设置凭据并发上限（null = 跟随全局）
export async function setCredentialConcurrency(
  id: number,
  concurrency: number | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/concurrency`,
    { concurrency } as SetConcurrencyRequest
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset`)
  return data
}

// 强制刷新 Token
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/refresh`)
  return data
}

// 获取凭据余额
export async function getCredentialBalance(id: number): Promise<BalanceResponse> {
  const { data } = await api.get<BalanceResponse>(`/credentials/${id}/balance`)
  return data
}

// 切换上游 overage 开关
export async function setOverageStatus(
  id: number,
  enabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/overage`, {
    enabled,
  })
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>('/credentials', req)
  return data
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

// 压缩配置类型
export interface CompressionConfig {
  enabled: boolean
  whitespaceCompression: boolean
  thinkingStrategy: string
  toolResultMaxChars: number
  toolResultHeadLines: number
  toolResultTailLines: number
  toolUseInputMaxChars: number
  toolDescriptionMaxChars: number
  maxHistoryTurns: number
  maxHistoryChars: number
  imageMaxLongEdge: number
  imageMaxPixelsSingle: number
  imageMaxPixelsMulti: number
  imageMultiThreshold: number
  maxRequestBodyBytes: number
}

// 获取压缩配置
export async function getCompressionConfig(): Promise<CompressionConfig> {
  const { data } = await api.get<CompressionConfig>('/config/compression')
  return data
}

// 更新压缩配置
export async function setCompressionConfig(config: CompressionConfig): Promise<CompressionConfig> {
  const { data } = await api.put<CompressionConfig>('/config/compression', config)
  return data
}

// 获取全局配置
export async function getGlobalConfig(): Promise<import('../types/api').GlobalConfigResponse> {
  const { data } = await api.get<import('../types/api').GlobalConfigResponse>('/config/global')
  return data
}

// 更新全局配置
export async function updateGlobalConfig(
  req: import('../types/api').UpdateGlobalConfigRequest,
): Promise<import('../types/api').GlobalConfigResponse> {
  const { data } = await api.put<import('../types/api').GlobalConfigResponse>('/config/global', req)
  return data
}

// 获取运行时状态（K/N + lastUsed，5s 高频轮询）
export async function getRuntimeStats(): Promise<import('../types/api').RuntimeStatsResponse> {
  const { data } = await api.get<import('../types/api').RuntimeStatsResponse>('/credentials/runtime-stats')
  return data
}

// 批量强制刷新 Token（服务端 Semaphore(8) 并发，前端一次往返）
export async function refreshBatch(
  ids: number[],
): Promise<import('../types/api').BatchRefreshResponse> {
  const { data } = await api.post<import('../types/api').BatchRefreshResponse>(
    '/credentials/refresh-batch',
    { ids } as import('../types/api').BatchRefreshRequest,
  )
  return data
}

// 批量查询余额（服务端 Semaphore(8) 并发，前端一次往返）
export async function refreshBalancesBatch(
  ids: number[],
): Promise<import('../types/api').BatchRefreshBalanceResponse> {
  const { data } = await api.post<import('../types/api').BatchRefreshBalanceResponse>(
    '/credentials/refresh-balances-batch',
    { ids } as import('../types/api').BatchRefreshRequest,
  )
  return data
}

// 获取所有凭据缓存余额（首屏预填，避免手动查询）
export async function getCachedBalances(): Promise<import('../types/api').CachedBalancesResponse> {
  const { data } = await api.get<import('../types/api').CachedBalancesResponse>(
    '/credentials/balances/cached',
  )
  return data
}
