import { adminApi as api } from '@/api/client'
import { REQUEST_LOG_STATUSES } from '@/types/api'
import {
  ADMIN_API_ROUTES,
  type ApiRecord,
  asRecord,
  payloadRecord,
  stringField,
  stringLikeField,
  numberLikeField,
} from './_normalizers'
import type {
  SystemStatusResponse,
  StatsResponse,
  GenerateMachineIdResponse,
  RequestLogEntry,
  RequestLogsResponse,
  ClearRequestLogsResponse,
  SystemVersionResponse,
  MessageResponse,
} from '@/types/api'

type NormalizedStatsFields = Pick<
  StatsResponse,
  | 'totalRequests'
  | 'successRequests'
  | 'failedRequests'
  | 'totalTokens'
  | 'totalCredits'
  | 'uptime'
  | 'credentialsTotal'
  | 'credentialsAvailable'
>

function normalizeRequestLogEntry(value: unknown): RequestLogEntry {
  const raw = asRecord(value)
  const status = stringField(raw, 'status') === REQUEST_LOG_STATUSES.success
    ? REQUEST_LOG_STATUSES.success
    : REQUEST_LOG_STATUSES.error
  return {
    time: stringLikeField(raw, 'time') || '-',
    endpoint: stringField(raw, 'endpoint') || '-',
    model: stringField(raw, 'model') || '-',
    credentialId: stringLikeField(raw, 'credentialId', 'credential_id', 'accountId') || '-',
    status,
    error: stringField(raw, 'error') || undefined,
    errorType: stringField(raw, 'error_type', 'errorType') || undefined,
    tokens: numberLikeField(raw, 'tokens'),
    credits: numberLikeField(raw, 'credits'),
    durationMs: numberLikeField(raw, 'duration_ms', 'durationMs', 'duration') ?? 0,
  }
}

export async function getRequestLogs(): Promise<RequestLogsResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.logs)
  const raw = payloadRecord(data)
  const logs = Array.isArray(raw.logs)
    ? raw.logs.map(normalizeRequestLogEntry)
    : []
  const success = numberLikeField(raw, 'success', 'successCount', 'success_count')
    ?? logs.filter(log => log.status === REQUEST_LOG_STATUSES.success).length
  const errors = numberLikeField(raw, 'errors', 'failed', 'failedCount', 'failed_count', 'errorCount', 'error_count')
    ?? logs.filter(log => log.status === REQUEST_LOG_STATUSES.error).length
  return {
    logs,
    total: numberLikeField(raw, 'total') ?? logs.length,
    success,
    errors,
  }
}

export async function clearRequestLogs(): Promise<ClearRequestLogsResponse> {
  const { data } = await api.delete(ADMIN_API_ROUTES.system.logs)
  return data
}

function normalizeSystemStatus(data: unknown): SystemStatusResponse {
  const raw = payloadRecord(data)
  const stats = normalizeStatsFields(raw)
  return {
    status: stringField(raw, 'status') || 'unknown',
    version: stringField(raw, 'version') || '',
    ...stats,
  }
}

function normalizeStatsFields(raw: ApiRecord): NormalizedStatsFields {
  return {
    totalRequests: numberLikeField(raw, 'totalRequests', 'total_requests') ?? 0,
    successRequests: numberLikeField(raw, 'successRequests', 'success_requests') ?? 0,
    failedRequests: numberLikeField(raw, 'failedRequests', 'failed_requests') ?? 0,
    totalTokens: numberLikeField(raw, 'totalTokens', 'total_tokens') ?? 0,
    totalCredits: numberLikeField(raw, 'totalCredits', 'total_credits') ?? 0,
    uptime: numberLikeField(raw, 'uptime', 'uptime_seconds') ?? 0,
    credentialsTotal: numberLikeField(raw, 'credentialsTotal', 'credentials_total', 'total_credentials', 'accounts') ?? 0,
    credentialsAvailable: numberLikeField(raw, 'credentialsAvailable', 'credentials_available', 'available_credentials', 'available') ?? 0,
  }
}

function normalizeStats(data: unknown): StatsResponse {
  return normalizeStatsFields(payloadRecord(data))
}

// 系统状态
export async function getSystemStatus(): Promise<SystemStatusResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.status)
  return normalizeSystemStatus(data)
}

// 详细统计
export async function getStats(): Promise<StatsResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.stats)
  return normalizeStats(data)
}

// 版本信息
export async function getVersion(): Promise<SystemVersionResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.version)
  return data
}

// 重置统计
export async function resetStats(): Promise<MessageResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.system.resetStats)
  return data
}

// 生成机器 ID
export async function generateMachineId(): Promise<GenerateMachineIdResponse> {
  const { data } = await api.get(ADMIN_API_ROUTES.system.machineId)
  const raw = payloadRecord(data)
  return {
    machineId: stringField(raw, 'machineId', 'machine_id'),
  }
}
