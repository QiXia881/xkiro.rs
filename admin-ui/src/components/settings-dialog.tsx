import { useState, useEffect } from 'react'
import { Loader2, RotateCcw, Zap, Wrench, MessageSquare, Image, Globe } from 'lucide-react'
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
  getCompressionConfig,
  getGlobalConfig,
  updateGlobalConfig,
  type CompressionConfig,
} from '@/api/credentials'
import type { GlobalConfigResponse, UpdateGlobalConfigRequest } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'
import { usePrivacyMode } from '@/hooks/use-privacy-mode'

interface SettingsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

const DEFAULT_CONFIG: CompressionConfig = {
  enabled: true,
  whitespaceCompression: true,
  thinkingStrategy: 'discard',
  toolResultMaxChars: 8000,
  toolResultHeadLines: 80,
  toolResultTailLines: 40,
  toolUseInputMaxChars: 6000,
  toolDescriptionMaxChars: 4000,
  maxHistoryTurns: 80,
  maxHistoryChars: 400000,
  imageMaxLongEdge: 4000,
  imageMaxPixelsSingle: 4000000,
  imageMaxPixelsMulti: 4000000,
  imageMultiThreshold: 20,
  imageCompressionEnabled: true,
  maxRequestBodyBytes: 4718592,
}

type TabId = 'general' | 'tool' | 'history' | 'image' | 'request'

const TABS: { id: TabId; label: string; icon: React.ReactNode }[] = [
  { id: 'general', label: '基础', icon: <Zap className="h-4 w-4" /> },
  { id: 'tool', label: 'Tool', icon: <Wrench className="h-4 w-4" /> },
  { id: 'history', label: '历史', icon: <MessageSquare className="h-4 w-4" /> },
  { id: 'image', label: '图片', icon: <Image className="h-4 w-4" /> },
  { id: 'request', label: '请求', icon: <Globe className="h-4 w-4" /> },
]

export function SettingsDialog({ open, onOpenChange }: SettingsDialogProps) {
  const [config, setConfig] = useState<CompressionConfig | null>(null)
  const [globalConfig, setGlobalConfig] = useState<GlobalConfigResponse | null>(null)
  const [loading, setLoading] = useState(false)
  const [saving, setSaving] = useState(false)
  const [activeTab, setActiveTab] = useState<TabId>('general')
  const { privacyMode, setPrivacyMode } = usePrivacyMode()

  useEffect(() => {
    if (open) loadConfig()
  }, [open])

  const loadConfig = async () => {
    setLoading(true)
    try {
      const [comp, global] = await Promise.all([getCompressionConfig(), getGlobalConfig()])
      setConfig(comp)
      setGlobalConfig(global)
    } catch (error) {
      toast.error(`加载配置失败: ${extractErrorMessage(error)}`)
    } finally {
      setLoading(false)
    }
  }

  const handleSave = async () => {
    if (!config || !globalConfig) return
    if (globalConfig.perCredentialConcurrency < 1) {
      toast.error('单凭据最大并发数不能小于 1')
      return
    }
    if (globalConfig.globalConcurrency < 0) {
      toast.error('全局并发上限不能为负数')
      return
    }
    if (globalConfig.acquireWaitTimeoutSecs < 1) {
      toast.error('排队等待超时不能小于 1 秒')
      return
    }
    if (globalConfig.balanceRefreshIntervalSecs < 180) {
      toast.error('余额刷新间隔不能小于 180 秒')
      return
    }
    if (
      globalConfig.balanceRefreshConcurrency < 1 ||
      globalConfig.balanceRefreshConcurrency > 10
    ) {
      toast.error('余额刷新并发必须在 1..=10')
      return
    }
    setSaving(true)
    try {
      const payload: UpdateGlobalConfigRequest = {
        perCredentialConcurrency: globalConfig.perCredentialConcurrency,
        globalConcurrency: globalConfig.globalConcurrency,
        acquireWaitTimeoutSecs: globalConfig.acquireWaitTimeoutSecs,
        balanceRefreshEnabled: globalConfig.balanceRefreshEnabled,
        balanceRefreshIntervalSecs: globalConfig.balanceRefreshIntervalSecs,
        balanceRefreshConcurrency: globalConfig.balanceRefreshConcurrency,
        sessionAffinityEnabled: globalConfig.sessionAffinityEnabled,
        privacyMode: globalConfig.privacyMode,
        compression: config,
      }
      const next = await updateGlobalConfig(payload)
      setGlobalConfig(next)
      setConfig(next.compression as CompressionConfig)
      toast.success('配置已保存，立即生效')
    } catch (error) {
      toast.error(`保存失败: ${extractErrorMessage(error)}`)
    } finally {
      setSaving(false)
    }
  }

  const handleReset = () => {
    setConfig({ ...DEFAULT_CONFIG })
    toast.info('已重置为默认值，点击保存生效')
  }

  const update = <K extends keyof CompressionConfig>(key: K, value: CompressionConfig[K]) => {
    setConfig(prev => prev ? { ...prev, [key]: value } : prev)
  }

  const updateGlobal = <K extends keyof GlobalConfigResponse>(key: K, value: GlobalConfigResponse[K]) => {
    setGlobalConfig(prev => prev ? { ...prev, [key]: value } : prev)
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-3xl p-0 gap-0 h-[80vh] max-h-[720px] flex flex-col overflow-hidden">
        <DialogHeader className="px-6 pt-5 pb-4 border-b shrink-0">
          <DialogTitle>设置</DialogTitle>
        </DialogHeader>

        <div className="flex flex-1 min-h-0 overflow-hidden">
          {/* 左侧导航 */}
          <nav className="w-40 shrink-0 border-r bg-muted/30 py-2 overflow-y-auto">
            {TABS.map(tab => (
              <button
                key={tab.id}
                onClick={() => setActiveTab(tab.id)}
                className={`w-full flex items-center gap-2 px-4 py-2 text-sm transition-colors ${
                  activeTab === tab.id
                    ? 'bg-background text-foreground font-medium border-r-2 border-primary'
                    : 'text-muted-foreground hover:text-foreground hover:bg-background/50'
                }`}
              >
                {tab.icon}
                {tab.label}
              </button>
            ))}
          </nav>

          {/* 右侧内容 */}
          <div className="flex-1 flex flex-col min-w-0 overflow-hidden">
            <div className="flex-1 overflow-y-auto px-6 py-4">
              {loading ? (
                <div className="flex items-center justify-center h-full">
                  <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
                </div>
              ) : config ? (
                <div className="space-y-4">
                  {activeTab === 'general' && (
                    <>
                      <ToggleRow label="启用压缩" desc="关闭后所有压缩逻辑跳过" checked={config.enabled} onChange={v => update('enabled', v)} />
                      <ToggleRow label="空白字符压缩" desc="合并连续空行和多余空格" checked={config.whitespaceCompression} onChange={v => update('whitespaceCompression', v)} />
                      <SelectRow
                        label="Thinking 处理"
                        desc="模型思考内容的处理方式"
                        value={config.thinkingStrategy}
                        options={[
                          { value: 'discard', label: '丢弃' },
                          { value: 'truncate', label: '截断' },
                          { value: 'keep', label: '保留' },
                        ]}
                        onChange={v => update('thinkingStrategy', v)}
                      />
                      {globalConfig && (
                        <>
                          <div className="pt-3 mt-2 border-t">
                            <div className="text-xs font-medium text-muted-foreground uppercase tracking-wider mb-1">并发限流</div>
                          </div>
                          <NumberRow
                            label="单凭据并发上限"
                            desc="同一凭据同时处理的最大请求数（>=1）"
                            value={globalConfig.perCredentialConcurrency}
                            onChange={v => updateGlobal('perCredentialConcurrency', v)}
                          />
                          <NumberRow
                            label="全局并发上限"
                            desc="所有凭据合计并发数，0 表示不限"
                            value={globalConfig.globalConcurrency}
                            onChange={v => updateGlobal('globalConcurrency', v)}
                          />
                          <NumberRow
                            label="排队超时（秒）"
                            desc="等待空闲凭据的最大秒数，超时返回 503"
                            value={globalConfig.acquireWaitTimeoutSecs}
                            onChange={v => updateGlobal('acquireWaitTimeoutSecs', v)}
                          />
                          <div className="pt-3 mt-2 border-t">
                            <div className="text-xs font-medium text-muted-foreground uppercase tracking-wider mb-1">余额刷新</div>
                          </div>
                          <ToggleRow
                            label="启用周期刷新"
                            desc="关闭后停止后台拉取，调度只用最后一次缓存（启动预取仍执行）"
                            checked={globalConfig.balanceRefreshEnabled}
                            onChange={v => updateGlobal('balanceRefreshEnabled', v)}
                          />
                          <NumberRow
                            label="刷新间隔（秒）"
                            desc="周期触发间隔，最小 180 秒"
                            value={globalConfig.balanceRefreshIntervalSecs}
                            onChange={v => updateGlobal('balanceRefreshIntervalSecs', v)}
                          />
                          <NumberRow
                            label="并发上限"
                            desc="同时刷新的凭据数（1..=10），超出按批顺序处理"
                            value={globalConfig.balanceRefreshConcurrency}
                            onChange={v => updateGlobal('balanceRefreshConcurrency', v)}
                          />
                          <div className="pt-3 mt-2 border-t">
                            <div className="text-xs font-medium text-muted-foreground uppercase tracking-wider mb-1">调度策略</div>
                          </div>
                          <ToggleRow
                            label="会话亲和（Session Affinity）"
                            desc="开启：同一会话黏住首次选中的凭据，整轮对话不切号 → 上游 prompt cache 命中率高，多号利用不均匀。关闭（默认）：每条消息独立走调度，按余额 / 并发占用平摊到所有可用号 → 多号同时跑，上游 cache 必失（客户端每次带完整 history，模型不会失忆），但更省 quota / 更快释放阻塞。"
                            checked={globalConfig.sessionAffinityEnabled}
                            onChange={v => updateGlobal('sessionAffinityEnabled', v)}
                          />
                          <div className="pt-3 mt-2 border-t">
                            <div className="text-xs font-medium text-muted-foreground uppercase tracking-wider mb-1">界面</div>
                          </div>
                          <ToggleRow
                            label="隐私模式"
                            desc="开启（默认）：邮箱在卡片标题等位置脱敏显示（us***45@***.com）。仅影响本地浏览器展示，不影响后端数据。"
                            checked={privacyMode}
                            onChange={setPrivacyMode}
                          />
                        </>
                      )}
                    </>
                  )}

                  {activeTab === 'tool' && (
                    <>
                      <NumberRow label="Result 最大字符" desc="超出后保留头尾截断中间" value={config.toolResultMaxChars} onChange={v => update('toolResultMaxChars', v)} />
                      <NumberRow label="保留头部行数" desc="截断时保留开头的行数" value={config.toolResultHeadLines} onChange={v => update('toolResultHeadLines', v)} />
                      <NumberRow label="保留尾部行数" desc="截断时保留末尾的行数" value={config.toolResultTailLines} onChange={v => update('toolResultTailLines', v)} />
                      <NumberRow label="Input 最大字符" desc="工具调用参数截断阈值" value={config.toolUseInputMaxChars} onChange={v => update('toolUseInputMaxChars', v)} />
                      <NumberRow label="Description 最大字符" desc="工具描述截断阈值" value={config.toolDescriptionMaxChars} onChange={v => update('toolDescriptionMaxChars', v)} />
                    </>
                  )}

                  {activeTab === 'history' && (
                    <>
                      <NumberRow label="最大保留轮数" desc="超出后丢弃最早的对话轮" value={config.maxHistoryTurns} onChange={v => update('maxHistoryTurns', v)} />
                      <NumberRow label="最大总字符数" desc="历史消息总字符上限" value={config.maxHistoryChars} onChange={v => update('maxHistoryChars', v)} />
                    </>
                  )}

                  {activeTab === 'image' && (
                    <>
                      <ToggleRow label="图片压缩" desc="关闭后透传原图（上游已压缩时建议关闭，如 TRAE 国际版）" checked={config.imageCompressionEnabled} onChange={v => update('imageCompressionEnabled', v)} />
                      <NumberRow label="最大长边 (px)" desc="超出后等比缩放" value={config.imageMaxLongEdge} onChange={v => update('imageMaxLongEdge', v)} />
                      <NumberRow label="单图最大像素" desc="单张图片总像素上限" value={config.imageMaxPixelsSingle} onChange={v => update('imageMaxPixelsSingle', v)} />
                      <NumberRow label="多图最大像素" desc="多图场景每张像素上限" value={config.imageMaxPixelsMulti} onChange={v => update('imageMaxPixelsMulti', v)} />
                      <NumberRow label="多图阈值" desc="图片数量超过此值启用多图限制" value={config.imageMultiThreshold} onChange={v => update('imageMultiThreshold', v)} />
                    </>
                  )}

                  {activeTab === 'request' && (
                    <>
                      <NumberRow label="请求体最大字节" desc="序列化后超出则触发额外压缩" value={config.maxRequestBodyBytes} onChange={v => update('maxRequestBodyBytes', v)} />
                    </>
                  )}
                </div>
              ) : (
                <p className="text-sm text-muted-foreground">加载失败，请关闭后重试</p>
              )}
            </div>

            {/* 底部操作栏 */}
            {config && !loading && (
              <div className="flex gap-2 px-6 py-3 border-t bg-muted/20">
                <Button onClick={handleSave} disabled={saving} size="sm" className="flex-1">
                  {saving && <Loader2 className="h-4 w-4 animate-spin mr-2" />}
                  保存
                </Button>
                <Button onClick={handleReset} variant="outline" size="sm">
                  <RotateCcw className="h-4 w-4 mr-1" />
                  重置
                </Button>
              </div>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function ToggleRow({ label, desc, checked, onChange }: { label: string; desc?: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <div className="flex items-center justify-between gap-4 py-2">
      <div>
        <div className="text-sm font-medium">{label}</div>
        {desc && <p className="text-xs text-muted-foreground mt-0.5">{desc}</p>}
      </div>
      <Switch checked={checked} onCheckedChange={onChange} />
    </div>
  )
}

function NumberRow({ label, desc, value, onChange }: { label: string; desc?: string; value: number; onChange: (v: number) => void }) {
  return (
    <div className="flex items-center justify-between gap-4 py-2">
      <div className="shrink-0">
        <div className="text-sm font-medium">{label}</div>
        {desc && <p className="text-xs text-muted-foreground mt-0.5">{desc}</p>}
      </div>
      <Input
        type="number"
        value={value}
        onChange={e => onChange(Number(e.target.value))}
        className="w-32 h-8 text-sm text-right"
      />
    </div>
  )
}

function SelectRow({ label, desc, value, options, onChange }: {
  label: string
  desc?: string
  value: string
  options: { value: string; label: string }[]
  onChange: (v: string) => void
}) {
  return (
    <div className="flex items-center justify-between gap-4 py-2">
      <div className="shrink-0">
        <div className="text-sm font-medium">{label}</div>
        {desc && <p className="text-xs text-muted-foreground mt-0.5">{desc}</p>}
      </div>
      <select
        value={value}
        onChange={e => onChange(e.target.value)}
        className="h-8 rounded-md border border-input bg-background px-3 text-sm"
      >
        {options.map(opt => <option key={opt.value} value={opt.value}>{opt.label}</option>)}
      </select>
    </div>
  )
}
