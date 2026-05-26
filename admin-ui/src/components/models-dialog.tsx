import { useState } from 'react'
import { Loader2, RefreshCw, Star } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { useCredentialModels } from '@/hooks/use-credentials'
import { parseError } from '@/lib/utils'
import type { AvailableModel } from '@/api/credentials'

interface ModelsDialogProps {
  credentialId: number | null
  open: boolean
  onOpenChange: (open: boolean) => void
}

function formatTokens(value?: number): string {
  if (!value || value <= 0) return '—'
  if (value >= 1000) return `${(value / 1000).toFixed(value % 1000 === 0 ? 0 : 1)}k`
  return value.toString()
}

function ModelRow({ model }: { model: AvailableModel }) {
  const ctx = model.contextWindow ?? model.tokenLimits?.maxInputTokens
  const out = model.tokenLimits?.maxOutputTokens
  const cache = model.promptCaching?.supportsPromptCaching
  return (
    <div className="rounded-md border bg-card px-3 py-2.5 space-y-1.5">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5 flex-wrap">
            <span className="font-mono text-sm font-medium truncate">{model.modelId}</span>
            {model.isDefault && (
              <Badge variant="default" className="h-4 px-1 gap-0.5 text-2xs">
                <Star className="h-2.5 w-2.5" />
                默认
              </Badge>
            )}
            {model.provider && (
              <Badge variant="outline" className="h-4 px-1 text-2xs">
                {model.provider}
              </Badge>
            )}
          </div>
          {model.modelName && model.modelName !== model.modelId && (
            <div className="text-xs text-muted-foreground mt-0.5">{model.modelName}</div>
          )}
        </div>
        {model.rateMultiplier !== undefined && model.rateMultiplier !== null && (
          <div className="text-xs font-mono text-muted-foreground shrink-0 mt-0.5">
            ×{model.rateMultiplier}
            {model.rateUnit ? ` ${model.rateUnit}` : ''}
          </div>
        )}
      </div>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-0.5 text-2xs text-muted-foreground font-mono">
        <span>ctx {formatTokens(ctx)}</span>
        <span>out {formatTokens(out)}</span>
        {cache && <span className="text-emerald-600 dark:text-emerald-500">prompt-cache</span>}
        {model.supportedInputTypes && model.supportedInputTypes.length > 0 && (
          <span>in: {model.supportedInputTypes.join(',').toLowerCase()}</span>
        )}
        {model.capabilities && model.capabilities.length > 0 && (
          <span className="truncate">caps: {model.capabilities.join(',').toLowerCase()}</span>
        )}
      </div>
      {model.description && (
        <div className="text-xs text-muted-foreground line-clamp-2">{model.description}</div>
      )}
    </div>
  )
}

export function ModelsDialog({ credentialId, open, onOpenChange }: ModelsDialogProps) {
  const [forceTick, setForceTick] = useState(0)
  const force = forceTick > 0
  const query = useCredentialModels(credentialId, {
    force,
    enabled: open,
  })

  const handleRefresh = () => {
    setForceTick(t => t + 1)
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl max-h-[85vh] flex flex-col p-0 gap-0">
        <DialogHeader className="px-6 pt-5 pb-3 border-b shrink-0">
          <div className="flex items-center justify-between gap-2">
            <DialogTitle>凭据 #{credentialId} 可用模型</DialogTitle>
            <Button
              size="sm"
              variant="ghost"
              className="h-7 px-2 text-xs mr-6"
              onClick={handleRefresh}
              disabled={query.isFetching}
              title="跳过缓存重新拉取"
            >
              <RefreshCw className={`mr-1 h-3 w-3 ${query.isFetching ? 'animate-spin' : ''}`} />
              刷新
            </Button>
          </div>
        </DialogHeader>

        <div className="flex-1 min-h-0 overflow-y-auto px-6 py-4">
          {query.isLoading && (
            <div className="flex items-center justify-center py-12">
              <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
            </div>
          )}

          {query.error && (() => {
            const parsed = parseError(query.error)
            return (
              <div className="py-8 space-y-2 text-center">
                <div className="text-sm font-medium text-red-500">{parsed.title}</div>
                {parsed.detail && (
                  <div className="text-xs text-muted-foreground px-4">{parsed.detail}</div>
                )}
              </div>
            )
          })()}

          {query.data && (
            <div className="space-y-2">
              <div className="text-xs text-muted-foreground">
                共 {query.data.availableModels.length} 个模型
                {query.data.defaultModel && (
                  <>
                    {' · 默认: '}
                    <span className="font-mono">{query.data.defaultModel.modelId}</span>
                  </>
                )}
              </div>
              {query.data.availableModels.length === 0 ? (
                <div className="py-8 text-center text-sm text-muted-foreground">无可用模型</div>
              ) : (
                query.data.availableModels.map(m => <ModelRow key={m.modelId} model={m} />)
              )}
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}
