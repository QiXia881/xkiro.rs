import { useMemo, useState } from 'react'
import { Loader2, Pencil, Trash2, Wand2, Wifi, X } from 'lucide-react'
import { toast } from 'sonner'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  useAddProxy,
  useAutoAssignProxies,
  useDeleteProxy,
  useImportProxies,
  useProxies,
  useTestProxy,
  useUpdateProxy,
} from '@/hooks/use-proxies'
import { extractErrorMessage } from '@/lib/utils'
import type { ProxyItem, ProxyUpsertRequest } from '@/types/api'

interface ProxyPoolDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

interface ProxyFormState {
  url: string
  username: string
  password: string
  region: string
  maxConcurrency: string
  note: string
  disabled: boolean
}

const EMPTY_FORM: ProxyFormState = {
  url: '',
  username: '',
  password: '',
  region: '',
  maxConcurrency: '',
  note: '',
  disabled: false,
}

function cleanProxyUrl(url: string): string {
  try {
    const parsed = new URL(url)
    parsed.username = ''
    parsed.password = ''
    return parsed.toString().replace(/\/$/, '')
  } catch {
    return url.replace(/^([a-z][a-z0-9+.-]*:\/\/)[^/@]+@/i, '$1')
  }
}

function formToRequest(form: ProxyFormState): ProxyUpsertRequest {
  const req: ProxyUpsertRequest = {
    url: form.url.trim(),
    disabled: form.disabled,
  }
  const username = form.username.trim()
  const region = form.region.trim()
  const note = form.note.trim()
  const maxConcurrency = Number.parseInt(form.maxConcurrency.trim(), 10)
  if (username) req.username = username
  if (form.password) req.password = form.password
  if (region) req.region = region
  if (note) req.note = note
  if (Number.isFinite(maxConcurrency) && maxConcurrency > 0) {
    req.maxConcurrency = maxConcurrency
  }
  return req
}

export function ProxyPoolDialog({ open, onOpenChange }: ProxyPoolDialogProps) {
  const { data, isLoading } = useProxies()
  const addProxy = useAddProxy()
  const updateProxy = useUpdateProxy()
  const deleteProxy = useDeleteProxy()
  const testProxy = useTestProxy()
  const importProxies = useImportProxies()
  const autoAssignProxies = useAutoAssignProxies()
  const [form, setForm] = useState<ProxyFormState>(EMPTY_FORM)
  const [editingId, setEditingId] = useState<number | null>(null)
  const [testingId, setTestingId] = useState<number | null>(null)
  const [importText, setImportText] = useState('')
  const [importRegion, setImportRegion] = useState('')
  const [importMaxConcurrency, setImportMaxConcurrency] = useState('')

  const proxies = Array.isArray(data?.proxies) ? data.proxies : []
  const grouped = useMemo(() => {
    const map = new Map<string, ProxyItem[]>()
    for (const proxy of proxies) {
      const key = proxy.region?.trim() || '未分组'
      map.set(key, [...(map.get(key) ?? []), proxy])
    }
    return [...map.entries()].sort(([left], [right]) => {
      if (left === '未分组') return 1
      if (right === '未分组') return -1
      return left.localeCompare(right)
    })
  }, [proxies])

  const resetForm = () => {
    setForm(EMPTY_FORM)
    setEditingId(null)
  }

  const startEdit = (proxy: ProxyItem) => {
    setEditingId(proxy.id)
    setForm({
      url: proxy.url,
      username: proxy.username ?? '',
      password: '',
      region: proxy.region ?? '',
      maxConcurrency: proxy.maxConcurrency ? String(proxy.maxConcurrency) : '',
      note: proxy.note ?? '',
      disabled: proxy.disabled,
    })
  }

  const submitForm = () => {
    if (!form.url.trim()) {
      toast.error('代理 URL 不能为空')
      return
    }
    const req = formToRequest(form)
    if (editingId !== null) {
      updateProxy.mutate(
        { id: editingId, req },
        {
          onSuccess: response => {
            toast.success(response.message || '代理已更新')
            resetForm()
          },
          onError: error => toast.error(`更新代理失败: ${extractErrorMessage(error)}`),
        },
      )
      return
    }
    addProxy.mutate(req, {
      onSuccess: response => {
        toast.success(response.message || '代理已新增')
        resetForm()
      },
      onError: error => toast.error(`新增代理失败: ${extractErrorMessage(error)}`),
    })
  }

  const handleTest = (id: number) => {
    setTestingId(id)
    testProxy.mutate(id, {
      onSuccess: response => {
        if (response.ok) {
          toast.success(
            `代理可用，出口 ${response.exitIp ?? '未知'}${response.latencyMs ? `，${response.latencyMs}ms` : ''}`,
          )
        } else {
          toast.error(response.error || '代理不可用')
        }
      },
      onError: error => toast.error(`测试代理失败: ${extractErrorMessage(error)}`),
      onSettled: () => setTestingId(null),
    })
  }

  const handleImport = () => {
    const text = importText.trim()
    if (!text) {
      toast.error('请输入代理列表')
      return
    }
    const maxConcurrency = Number.parseInt(importMaxConcurrency.trim(), 10)
    importProxies.mutate(
      {
        text,
        region: importRegion.trim() || undefined,
        maxConcurrency: Number.isFinite(maxConcurrency) && maxConcurrency > 0 ? maxConcurrency : undefined,
      },
      {
        onSuccess: response => {
          toast.success(`导入完成：成功 ${response.added}，失败 ${response.failed}`)
          if (response.errors.length > 0) {
            toast.error(response.errors.slice(0, 5).join('\n'))
          }
          if (response.added > 0) setImportText('')
        },
        onError: error => toast.error(`导入代理失败: ${extractErrorMessage(error)}`),
      },
    )
  }

  const handleAutoAssign = () => {
    autoAssignProxies.mutate(
      { credentialIds: [], reassignBound: false },
      {
        onSuccess: response => {
          toast.success(`自动分配完成：绑定 ${response.assigned.length}，跳过 ${response.skipped.length}`)
        },
        onError: error => toast.error(`自动分配失败: ${extractErrorMessage(error)}`),
      },
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-5xl">
        <DialogHeader>
          <DialogTitle>代理池</DialogTitle>
          <DialogDescription>
            管理共享代理并把凭据绑定到代理池条目。凭据文件只保存 proxyId，运行时再回填真实代理。
          </DialogDescription>
        </DialogHeader>

        <div className="grid min-h-0 flex-1 grid-cols-1 gap-4 overflow-hidden lg:grid-cols-[360px_1fr]">
          <div className="space-y-4 overflow-y-auto pr-1">
            <div className="rounded-xl border bg-card p-4">
              <div className="mb-3 flex items-center justify-between">
                <div className="font-medium">{editingId === null ? '新增代理' : `编辑代理 #${editingId}`}</div>
                {editingId !== null && (
                  <Button variant="ghost" size="sm" onClick={resetForm}>
                    <X className="mr-1 h-3.5 w-3.5" />
                    取消
                  </Button>
                )}
              </div>
              <div className="space-y-3">
                <Input
                  value={form.url}
                  onChange={event => setForm({ ...form, url: event.target.value })}
                  placeholder="socks5://127.0.0.1:1080"
                />
                <div className="grid grid-cols-2 gap-2">
                  <Input
                    value={form.username}
                    onChange={event => setForm({ ...form, username: event.target.value })}
                    placeholder="用户名"
                  />
                  <Input
                    type="password"
                    value={form.password}
                    onChange={event => setForm({ ...form, password: event.target.value })}
                    placeholder={editingId === null ? '密码' : '留空则清空密码'}
                  />
                </div>
                <div className="grid grid-cols-2 gap-2">
                  <Input
                    value={form.region}
                    onChange={event => setForm({ ...form, region: event.target.value })}
                    placeholder="region，如 US:California"
                  />
                  <Input
                    value={form.maxConcurrency}
                    onChange={event => setForm({ ...form, maxConcurrency: event.target.value })}
                    placeholder="并发上限"
                  />
                </div>
                <Input
                  value={form.note}
                  onChange={event => setForm({ ...form, note: event.target.value })}
                  placeholder="备注"
                />
                <label className="flex items-center justify-between rounded-lg border px-3 py-2 text-sm">
                  禁用该代理
                  <Switch
                    checked={form.disabled}
                    onCheckedChange={checked => setForm({ ...form, disabled: checked })}
                  />
                </label>
                <Button
                  className="w-full"
                  onClick={submitForm}
                  disabled={addProxy.isPending || updateProxy.isPending}
                >
                  {(addProxy.isPending || updateProxy.isPending) && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                  {editingId === null ? '新增代理' : '保存代理'}
                </Button>
              </div>
            </div>

            <div className="rounded-xl border bg-card p-4">
              <div className="mb-3 font-medium">批量导入</div>
              <textarea
                value={importText}
                onChange={event => setImportText(event.target.value)}
                className="min-h-28 w-full resize-y rounded-md border bg-background px-3 py-2 text-sm outline-none focus:ring-2 focus:ring-ring"
                placeholder={'每行一个：\n1.2.3.4:1080:user:pass\nsocks5://1.2.3.4:1080,user,pass'}
              />
              <div className="mt-2 grid grid-cols-2 gap-2">
                <Input
                  value={importRegion}
                  onChange={event => setImportRegion(event.target.value)}
                  placeholder="统一 region"
                />
                <Input
                  value={importMaxConcurrency}
                  onChange={event => setImportMaxConcurrency(event.target.value)}
                  placeholder="统一并发"
                />
              </div>
              <Button
                variant="secondary"
                className="mt-3 w-full"
                onClick={handleImport}
                disabled={importProxies.isPending}
              >
                {importProxies.isPending && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                导入代理
              </Button>
            </div>
          </div>

          <div className="min-h-0 overflow-y-auto rounded-xl border">
            <div className="sticky top-0 z-10 flex items-center justify-between border-b bg-background/95 px-4 py-3 backdrop-blur">
              <div className="text-sm text-muted-foreground">
                共 <span className="font-medium text-foreground">{proxies.length}</span> 个代理
              </div>
              <Button variant="outline" size="sm" onClick={handleAutoAssign} disabled={autoAssignProxies.isPending}>
                {autoAssignProxies.isPending ? (
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                ) : (
                  <Wand2 className="mr-2 h-4 w-4" />
                )}
                自动分配
              </Button>
            </div>

            {isLoading ? (
              <div className="flex h-52 items-center justify-center text-muted-foreground">
                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                加载代理池
              </div>
            ) : proxies.length === 0 ? (
              <div className="flex h-52 items-center justify-center text-sm text-muted-foreground">
                暂无代理，先从左侧新增或批量导入
              </div>
            ) : (
              <div className="space-y-4 p-4">
                {grouped.map(([region, items]) => (
                  <section key={region} className="space-y-2">
                    <div className="flex items-center gap-2 text-sm font-medium">
                      {region}
                      <Badge variant="secondary">{items.length}</Badge>
                    </div>
                    <div className="space-y-2">
                      {items.map(proxy => (
                        <div key={proxy.id} className="rounded-lg border bg-card p-3">
                          <div className="flex items-start justify-between gap-3">
                            <div className="min-w-0">
                              <div className="flex items-center gap-2">
                                <span className="font-mono text-sm">#{proxy.id}</span>
                                {proxy.disabled && <Badge variant="secondary">禁用</Badge>}
                                {proxy.dead && <Badge variant="destructive">死亡</Badge>}
                                {!proxy.disabled && !proxy.dead && <Badge variant="default">可用</Badge>}
                              </div>
                              <div className="mt-1 truncate font-mono text-xs" title={proxy.url}>
                                {cleanProxyUrl(proxy.url)}
                              </div>
                              <div className="mt-2 flex flex-wrap gap-2 text-2xs text-muted-foreground">
                                <span>绑定 {proxy.boundCredentials}</span>
                                <span>并发 {proxy.availablePermits ?? '不限'}/{proxy.maxConcurrency ?? '不限'}</span>
                                {proxy.country && <span>国家 {proxy.country}</span>}
                                {proxy.consecutiveFailures > 0 && <span>失败 {proxy.consecutiveFailures}</span>}
                              </div>
                              {proxy.lastError && (
                                <div className="mt-1 truncate text-2xs text-destructive" title={proxy.lastError}>
                                  {proxy.lastError}
                                </div>
                              )}
                              {proxy.note && <div className="mt-1 text-2xs text-muted-foreground">{proxy.note}</div>}
                            </div>
                            <div className="flex shrink-0 gap-1">
                              <Button
                                variant="ghost"
                                size="icon"
                                className="h-8 w-8"
                                onClick={() => handleTest(proxy.id)}
                                title="测试代理"
                              >
                                {testingId === proxy.id ? (
                                  <Loader2 className="h-4 w-4 animate-spin" />
                                ) : (
                                  <Wifi className="h-4 w-4" />
                                )}
                              </Button>
                              <Button
                                variant="ghost"
                                size="icon"
                                className="h-8 w-8"
                                onClick={() => startEdit(proxy)}
                                title="编辑代理"
                              >
                                <Pencil className="h-4 w-4" />
                              </Button>
                              <Button
                                variant="ghost"
                                size="icon"
                                className="h-8 w-8 text-destructive"
                                onClick={() => {
                                  if (window.confirm(`删除代理 #${proxy.id}？关联凭据会自动解绑。`)) {
                                    deleteProxy.mutate(proxy.id, {
                                      onSuccess: response => toast.success(response.message || '代理已删除'),
                                      onError: error => toast.error(`删除代理失败: ${extractErrorMessage(error)}`),
                                    })
                                  }
                                }}
                                title="删除代理"
                              >
                                <Trash2 className="h-4 w-4" />
                              </Button>
                            </div>
                          </div>
                        </div>
                      ))}
                    </div>
                  </section>
                ))}
              </div>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}
