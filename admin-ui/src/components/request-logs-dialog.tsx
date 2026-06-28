import { useState, useEffect, useRef } from 'react'
import { toast } from 'sonner'
import { Trash2, RefreshCw, Clock, CheckCircle2, XCircle } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { getRequestLogs, clearRequestLogs, type RequestLogEntry } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface RequestLogsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function RequestLogsDialog({ open, onOpenChange }: RequestLogsDialogProps) {
  const [logs, setLogs] = useState<RequestLogEntry[]>([])
  const [stats, setStats] = useState({ total: 0, success: 0, errors: 0 })
  const [autoRefresh, setAutoRefresh] = useState(false)
  const [filter, setFilter] = useState<'all' | 'success' | 'error'>('all')
  const refreshTimerRef = useRef<ReturnType<typeof setInterval> | null>(null)

  useEffect(() => {
    if (open) loadLogs()
    return () => {
      if (refreshTimerRef.current) clearInterval(refreshTimerRef.current)
    }
  }, [open])

  useEffect(() => {
    if (autoRefresh && open) {
      refreshTimerRef.current = setInterval(loadLogs, 5000)
    } else {
      if (refreshTimerRef.current) {
        clearInterval(refreshTimerRef.current)
        refreshTimerRef.current = null
      }
    }
    return () => {
      if (refreshTimerRef.current) clearInterval(refreshTimerRef.current)
    }
  }, [autoRefresh, open])

  const loadLogs = async () => {
    try {
      const result = await getRequestLogs()
      setLogs(result.logs)
      setStats({ total: result.total, success: result.success, errors: result.errors })
    } catch (error) {
      setLogs([])
      setStats({ total: 0, success: 0, errors: 0 })
    }
  }

  const handleClear = async () => {
    try {
      await clearRequestLogs()
      setLogs([])
      setStats({ total: 0, success: 0, errors: 0 })
      toast.success('日志已清空')
    } catch (error) {
      toast.error(`清空失败: ${extractErrorMessage(error)}`)
    }
  }

  const filteredLogs = logs.filter(log => {
    if (filter === 'success') return log.status === 'success'
    if (filter === 'error') return log.status === 'error'
    return true
  })

  const getErrorBadge = (errorType?: string) => {
    if (!errorType) return null
    const variants: Record<string, { label: string; className: string }> = {
      quota: { label: 'Quota', className: 'bg-yellow-100 text-yellow-800 dark:bg-yellow-900/30 dark:text-yellow-400' },
      overage: { label: 'Overage', className: 'bg-orange-100 text-orange-800 dark:bg-orange-900/30 dark:text-orange-400' },
      suspended: { label: 'Suspended', className: 'bg-red-100 text-red-800 dark:bg-red-900/30 dark:text-red-400' },
      auth: { label: 'Auth', className: 'bg-purple-100 text-purple-800 dark:bg-purple-900/30 dark:text-purple-400' },
      profile: { label: 'Profile', className: 'bg-blue-100 text-blue-800 dark:bg-blue-900/30 dark:text-blue-400' },
      unknown: { label: 'Unknown', className: 'bg-gray-100 text-gray-800 dark:bg-gray-900/30 dark:text-gray-400' },
    }
    const variant = variants[errorType] || variants.unknown
    return (
      <Badge variant="outline" className={`text-xs ${variant.className}`}>
        {variant.label}
      </Badge>
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-4xl max-h-[80vh] flex flex-col">
        <DialogHeader className="shrink-0">
          <DialogTitle className="flex items-center justify-between">
            <span>请求日志</span>
            <div className="flex items-center gap-2">
              <Badge variant="outline" className="text-xs">
                总计: {stats.total}
              </Badge>
              <Badge variant="outline" className="text-xs text-green-600">
                成功: {stats.success}
              </Badge>
              <Badge variant="outline" className="text-xs text-red-600">
                失败: {stats.errors}
              </Badge>
            </div>
          </DialogTitle>
        </DialogHeader>

        <div className="flex items-center gap-2 shrink-0">
          <div className="flex gap-1">
            <Button
              variant={filter === 'all' ? 'default' : 'outline'}
              size="sm"
              onClick={() => setFilter('all')}
            >
              全部
            </Button>
            <Button
              variant={filter === 'success' ? 'default' : 'outline'}
              size="sm"
              onClick={() => setFilter('success')}
            >
              成功
            </Button>
            <Button
              variant={filter === 'error' ? 'default' : 'outline'}
              size="sm"
              onClick={() => setFilter('error')}
            >
              失败
            </Button>
          </div>
          <div className="flex-1" />
          <Button
            variant={autoRefresh ? 'default' : 'outline'}
            size="sm"
            onClick={() => setAutoRefresh(!autoRefresh)}
          >
            <RefreshCw className={`h-3.5 w-3.5 mr-1.5 ${autoRefresh ? 'animate-spin' : ''}`} />
            自动刷新
          </Button>
          <Button variant="outline" size="sm" onClick={loadLogs}>
            <RefreshCw className="h-3.5 w-3.5 mr-1.5" />
            刷新
          </Button>
          <Button variant="outline" size="sm" onClick={handleClear}>
            <Trash2 className="h-3.5 w-3.5 mr-1.5" />
            清空
          </Button>
        </div>

        <div className="flex-1 overflow-y-auto min-h-0">
          {filteredLogs.length === 0 ? (
            <div className="flex items-center justify-center h-32 text-muted-foreground text-sm">
              暂无日志
            </div>
          ) : (
            <div className="space-y-1">
              {filteredLogs.map((log, i) => (
                <div
                  key={i}
                  className={`p-3 rounded-md text-sm border ${
                    log.status === 'success'
                      ? 'bg-green-50/50 dark:bg-green-950/20 border-green-200 dark:border-green-800'
                      : 'bg-red-50/50 dark:bg-red-950/20 border-red-200 dark:border-red-800'
                  }`}
                >
                  <div className="flex items-center gap-3">
                    {log.status === 'success' ? (
                      <CheckCircle2 className="h-4 w-4 text-green-600 shrink-0" />
                    ) : (
                      <XCircle className="h-4 w-4 text-red-600 shrink-0" />
                    )}
                    <span className="text-xs text-muted-foreground flex items-center gap-1">
                      <Clock className="h-3 w-3" />
                      {log.time}
                    </span>
                    <Badge variant="outline" className="text-xs">
                      {log.endpoint}
                    </Badge>
                    <span className="font-mono text-xs">{log.model}</span>
                    <span className="text-xs text-muted-foreground">
                      #{log.credential_id}
                    </span>
                    {log.tokens && (
                      <span className="text-xs text-muted-foreground">
                        {log.tokens} tokens
                      </span>
                    )}
                    <span className="text-xs text-muted-foreground">
                      {log.duration_ms}ms
                    </span>
                    {getErrorBadge(log.error_type)}
                  </div>
                  {log.error && (
                    <p className="mt-1 text-xs text-red-600 pl-7">{log.error}</p>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}
