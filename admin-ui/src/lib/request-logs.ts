import {
  REQUEST_LOG_ERROR_TYPES,
  REQUEST_LOG_STATUSES,
  type RequestLogErrorType,
  type RequestLogStatus,
} from '@/types/api'

export const REQUEST_LOG_FILTERS = {
  all: 'all',
  success: REQUEST_LOG_STATUSES.success,
  error: REQUEST_LOG_STATUSES.error,
} as const

export type RequestLogFilter = typeof REQUEST_LOG_FILTERS[keyof typeof REQUEST_LOG_FILTERS]

interface RequestLogStatusSource {
  status: RequestLogStatus
}

export interface RequestLogErrorTypeDisplay {
  label: string
  className: string
}

export const REQUEST_LOG_ERROR_TYPE_DISPLAY: Record<RequestLogErrorType, RequestLogErrorTypeDisplay> = {
  [REQUEST_LOG_ERROR_TYPES.quota]: { label: '配额', className: 'bg-yellow-100 text-yellow-800 dark:bg-yellow-900/30 dark:text-yellow-400' },
  [REQUEST_LOG_ERROR_TYPES.overage]: { label: '超额', className: 'bg-orange-100 text-orange-800 dark:bg-orange-900/30 dark:text-orange-400' },
  [REQUEST_LOG_ERROR_TYPES.suspended]: { label: '已暂停', className: 'bg-red-100 text-red-800 dark:bg-red-900/30 dark:text-red-400' },
  [REQUEST_LOG_ERROR_TYPES.auth]: { label: '认证', className: 'bg-purple-100 text-purple-800 dark:bg-purple-900/30 dark:text-purple-400' },
  [REQUEST_LOG_ERROR_TYPES.profile]: { label: '配置档案', className: 'bg-blue-100 text-blue-800 dark:bg-blue-900/30 dark:text-blue-400' },
  [REQUEST_LOG_ERROR_TYPES.unknown]: { label: '未知', className: 'bg-gray-100 text-gray-800 dark:bg-gray-900/30 dark:text-gray-400' },
}

export function getRequestLogErrorTypeDisplay(
  errorType: string | null | undefined,
): RequestLogErrorTypeDisplay | null {
  if (!errorType) return null
  return REQUEST_LOG_ERROR_TYPE_DISPLAY[errorType as RequestLogErrorType]
    ?? REQUEST_LOG_ERROR_TYPE_DISPLAY[REQUEST_LOG_ERROR_TYPES.unknown]
}

export function requestLogMatchesFilter(log: RequestLogStatusSource, filter: RequestLogFilter): boolean {
  if (filter === REQUEST_LOG_FILTERS.success) return log.status === REQUEST_LOG_STATUSES.success
  if (filter === REQUEST_LOG_FILTERS.error) return log.status === REQUEST_LOG_STATUSES.error
  return true
}

export function getRequestLogRowClass(status: RequestLogStatus): string {
  return status === REQUEST_LOG_STATUSES.success
    ? 'bg-green-50/50 dark:bg-green-950/20 border-green-200 dark:border-green-800'
    : 'bg-red-50/50 dark:bg-red-950/20 border-red-200 dark:border-red-800'
}
