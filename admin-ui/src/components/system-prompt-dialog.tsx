import { useEffect, useMemo, useState } from 'react'
import { Loader2, Plus, Trash2 } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import {
  useSystemPrompt,
  useUpdateSystemPrompt,
  useUpsertUserPreset,
  useDeleteUserPreset,
} from '@/hooks/use-system-prompt'
import type {
  PresetItem,
  SystemPromptPosition,
  UpsertUserPresetRequest,
} from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

interface SystemPromptDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

const ID_REGEX = /^[a-z0-9_-]{1,32}$/

export function SystemPromptDialog({ open, onOpenChange }: SystemPromptDialogProps) {
  const { data, isLoading } = useSystemPrompt(open)
  const update = useUpdateSystemPrompt()
  const upsert = useUpsertUserPreset()
  const remove = useDeleteUserPreset()

  const [enabled, setEnabled] = useState(false)
  const [position, setPosition] = useState<SystemPromptPosition>('append')
  const [customContent, setCustomContent] = useState('')
  const [enabledIds, setEnabledIds] = useState<Set<string>>(new Set())

  // user preset form
  const [presetId, setPresetId] = useState('')
  const [presetName, setPresetName] = useState('')
  const [presetDesc, setPresetDesc] = useState('')
  const [presetContent, setPresetContent] = useState('')
  const [editingId, setEditingId] = useState<string | null>(null)

  useEffect(() => {
    if (!data) return
    setEnabled(data.enabled)
    setPosition(data.position)
    setCustomContent(data.customContent ?? '')
    setEnabledIds(new Set(data.presets.filter(p => p.enabled).map(p => p.id)))
  }, [data])

  const builtinPresets = useMemo(
    () => (data?.presets ?? []).filter(p => p.source === 'builtin'),
    [data],
  )
  const userPresets = useMemo(
    () => (data?.presets ?? []).filter(p => p.source === 'user'),
    [data],
  )

  const togglePreset = (id: string) => {
    setEnabledIds(prev => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const handleSave = async () => {
    try {
      await update.mutateAsync({
        enabled,
        position,
        customContent,
        enabledPresets: Array.from(enabledIds),
      })
      toast.success('已保存，立即生效')
    } catch (e) {
      toast.error(`保存失败: ${extractErrorMessage(e)}`)
    }
  }

  const resetForm = () => {
    setPresetId('')
    setPresetName('')
    setPresetDesc('')
    setPresetContent('')
    setEditingId(null)
  }

  const startEdit = (p: PresetItem) => {
    setEditingId(p.id)
    setPresetId(p.id)
    setPresetName(p.name)
    setPresetDesc(p.description)
    setPresetContent(p.content ?? '')
  }

  const handleUpsert = async () => {
    const id = presetId.trim()
    if (!ID_REGEX.test(id)) {
      toast.error('id 仅允许 [a-z0-9_-]，长度 1-32')
      return
    }
    if (!presetName.trim()) {
      toast.error('name 不能为空')
      return
    }
    if (!presetContent.trim()) {
      toast.error('content 不能为空')
      return
    }
    const req: UpsertUserPresetRequest = {
      id,
      name: presetName.trim(),
      description: presetDesc,
      content: presetContent,
    }
    try {
      await upsert.mutateAsync(req)
      toast.success(editingId ? '已更新' : '已新增')
      resetForm()
    } catch (e) {
      toast.error(`保存失败: ${extractErrorMessage(e)}`)
    }
  }

  const handleDelete = async (id: string) => {
    if (!confirm(`删除用户预设 "${id}"？`)) return
    try {
      await remove.mutateAsync(id)
      toast.success('已删除')
      if (editingId === id) resetForm()
    } catch (e) {
      toast.error(`删除失败: ${extractErrorMessage(e)}`)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-3xl p-0 gap-0 h-[80vh] max-h-[720px] flex flex-col overflow-hidden">
        <DialogHeader className="px-6 pt-5 pb-4 border-b shrink-0">
          <DialogTitle>系统提示注入</DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto px-6 py-4 space-y-5">
          {isLoading || !data ? (
            <div className="flex items-center justify-center py-10 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin mr-2" /> 加载中
            </div>
          ) : (
            <>
              {/* --- Header card: enable + position --- */}
              <CardSection title="基本设置">
                <div className="flex items-center justify-between gap-4 py-2">
                  <div>
                    <div className="text-sm font-medium">启用注入</div>
                    <p className="text-xs text-muted-foreground mt-0.5">
                      关闭后所有 preset / 自定义内容都不生效
                    </p>
                  </div>
                  <Switch checked={enabled} onCheckedChange={setEnabled} />
                </div>
                <div className="flex items-center justify-between gap-4 py-2">
                  <div>
                    <div className="text-sm font-medium">注入位置</div>
                    <p className="text-xs text-muted-foreground mt-0.5">
                      prepend = 注入到 system 列表头部；append = 追加到尾部
                    </p>
                  </div>
                  <select
                    value={position}
                    onChange={e => setPosition(e.target.value as SystemPromptPosition)}
                    className="h-8 rounded-md border border-input bg-background px-3 text-sm"
                  >
                    <option value="append">append</option>
                    <option value="prepend">prepend</option>
                  </select>
                </div>
                <p className="mt-2 text-xs text-muted-foreground leading-relaxed">
                  最终注入文本顺序：内置预设 → 用户预设 → 自定义内容（按 \n\n 拼接）。
                  所有片段都启用且非空才会出现。
                </p>
              </CardSection>

              {/* --- Built-in Rules card --- */}
              <CardSection title="内置规则">
                {builtinPresets.length === 0 ? (
                  <p className="text-sm text-muted-foreground py-4">暂无内置预设</p>
                ) : (
                  <div className="space-y-2">
                    {builtinPresets.map(p => (
                      <div
                        key={p.id}
                        className="flex items-start justify-between gap-4 rounded-md border bg-card px-3 py-2"
                      >
                        <div className="flex-1 min-w-0">
                          <div className="flex items-center gap-2">
                            <span className="text-sm font-medium">{p.name}</span>
                            <span className="text-xs text-muted-foreground">{p.id}</span>
                          </div>
                          <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                            {p.description}
                          </p>
                        </div>
                        <Switch
                          checked={enabledIds.has(p.id)}
                          onCheckedChange={() => togglePreset(p.id)}
                        />
                      </div>
                    ))}
                  </div>
                )}
              </CardSection>

              {/* --- Custom Rules card --- */}
              <CardSection title="自定义规则">
                <div className="space-y-2">
                  {userPresets.length === 0 ? (
                    <p className="text-sm text-muted-foreground py-2">暂无用户预设</p>
                  ) : (
                    userPresets.map(p => (
                      <div
                        key={p.id}
                        className="flex items-start justify-between gap-3 rounded-md border bg-card px-3 py-2"
                      >
                        <div className="flex-1 min-w-0">
                          <div className="flex items-center gap-2">
                            <span className="text-sm font-medium">{p.name}</span>
                            <span className="text-xs text-muted-foreground">{p.id}</span>
                          </div>
                          {p.description && (
                            <p className="text-xs text-muted-foreground mt-1 leading-relaxed line-clamp-2">
                              {p.description}
                            </p>
                          )}
                        </div>
                        <div className="flex items-center gap-2 shrink-0">
                          <Switch
                            checked={enabledIds.has(p.id)}
                            onCheckedChange={() => togglePreset(p.id)}
                          />
                          <Button
                            variant="outline"
                            size="sm"
                            onClick={() => startEdit(p)}
                            className="h-7 px-2 text-xs"
                          >
                            编辑
                          </Button>
                          <Button
                            variant="ghost"
                            size="icon"
                            onClick={() => handleDelete(p.id)}
                            className="h-7 w-7 text-destructive hover:text-destructive"
                          >
                            <Trash2 className="h-3.5 w-3.5" />
                          </Button>
                        </div>
                      </div>
                    ))
                  )}
                </div>

                {/* inline add/edit form */}
                <div className="mt-3 space-y-2 rounded-md border bg-muted/20 p-3">
                  <div className="flex items-center justify-between">
                    <div className="text-xs font-medium text-muted-foreground uppercase">
                      {editingId ? `编辑 ${editingId}` : '新增预设'}
                    </div>
                    {editingId && (
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={resetForm}
                        className="h-6 px-2 text-xs"
                      >
                        取消编辑
                      </Button>
                    )}
                  </div>
                  <div className="grid grid-cols-2 gap-2">
                    <Input
                      placeholder="id (a-z0-9_-, 1-32)"
                      value={presetId}
                      onChange={e => setPresetId(e.target.value)}
                      disabled={editingId !== null}
                      className="h-8 text-sm"
                    />
                    <Input
                      placeholder="名称"
                      value={presetName}
                      onChange={e => setPresetName(e.target.value)}
                      className="h-8 text-sm"
                    />
                  </div>
                  <Input
                    placeholder="描述（可选）"
                    value={presetDesc}
                    onChange={e => setPresetDesc(e.target.value)}
                    className="h-8 text-sm"
                  />
                  <textarea
                    placeholder="prompt 正文"
                    value={presetContent}
                    onChange={e => setPresetContent(e.target.value)}
                    className="w-full min-h-[120px] rounded-md border border-input bg-background px-3 py-2 text-sm font-mono"
                  />
                  <div className="flex justify-end gap-2">
                    <Button
                      size="sm"
                      onClick={handleUpsert}
                      disabled={upsert.isPending}
                    >
                      {upsert.isPending ? (
                        <Loader2 className="h-4 w-4 animate-spin mr-2" />
                      ) : (
                        <Plus className="h-4 w-4 mr-1" />
                      )}
                      {editingId ? '更新' : '保存'}
                    </Button>
                  </div>
                </div>
              </CardSection>

              {/* --- Custom Content card --- */}
              <CardSection title="自定义内容">
                <p className="text-xs text-muted-foreground leading-relaxed mb-2">
                  填入额外文本，注入时拼到所有 preset 之后。留空则不注入此段。
                </p>
                <textarea
                  value={customContent}
                  onChange={e => setCustomContent(e.target.value)}
                  placeholder="任意 prompt 内容..."
                  className="w-full min-h-[180px] rounded-md border border-input bg-background px-3 py-2 text-sm font-mono"
                />
              </CardSection>
            </>
          )}
        </div>

        {/* Single bottom save button */}
        {data && (
          <div className="flex gap-2 px-6 py-3 border-t bg-muted/20 shrink-0">
            <Button
              onClick={handleSave}
              disabled={update.isPending}
              size="sm"
              className="flex-1"
            >
              {update.isPending && <Loader2 className="h-4 w-4 animate-spin mr-2" />}
              保存（立即生效）
            </Button>
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}

function CardSection({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border bg-card p-4 space-y-3">
      <div className="text-sm font-semibold">{title}</div>
      {children}
    </div>
  )
}
