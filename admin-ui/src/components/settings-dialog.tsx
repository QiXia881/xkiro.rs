import { useEffect, useState, type ReactNode } from 'react'
import { Loader2, Plus, Save, Settings2, Trash2 } from 'lucide-react'
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
  getCommonConfig,
  getEndpointConfig,
  getProxyConfig,
  getAccessSettings,
  getPromptFilterConfig,
  getThinkingConfig,
  getModelMappings,
  getGlobalConfig,
  updateCommonConfig,
  updateEndpointConfig,
  updateProxyConfig,
  updateAccessSettings,
  updatePromptFilterConfig,
  updateThinkingConfig,
  updateModelMappings,
  updateGlobalConfig,
} from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import type {
  AccessSettings,
  CommonConfig,
  EndpointConfig,
  PromptFilterConfig,
  PromptFilterRule,
  ThinkingConfig,
  ModelMappingRule,
} from '@/types/api'

interface SettingsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

type SettingsTab = 'common' | 'access' | 'thinking' | 'endpoint' | 'proxy' | 'prompt-filter' | 'model-mappings' | 'context'

const TABS: { id: SettingsTab; label: string }[] = [
  { id: 'common', label: '常用' },
  { id: 'access', label: '访问控制' },
  { id: 'thinking', label: 'Thinking' },
  { id: 'endpoint', label: '端点' },
  { id: 'proxy', label: '代理' },
  { id: 'prompt-filter', label: 'Prompt Filter' },
  { id: 'model-mappings', label: '模型映射' },
  { id: 'context', label: '上下文/压缩触发' },
]

const DEFAULT_THINKING: ThinkingConfig = {
  suffix: '-thinking',
  openaiFormat: 'reasoning_content',
  claudeFormat: 'thinking',
}

export function SettingsDialog({ open, onOpenChange }: SettingsDialogProps) {
  const [activeTab, setActiveTab] = useState<SettingsTab>('common')
  const [loading, setLoading] = useState(false)
  const [saving, setSaving] = useState(false)
  const [settings, setSettings] = useState<AccessSettings | null>(null)
  const [common, setCommon] = useState<CommonConfig>({
    machineId: '',
    credentialMachineIdStrategy: 'random',
  })
  const [thinking, setThinking] = useState<ThinkingConfig>(DEFAULT_THINKING)
  const [endpoint, setEndpoint] = useState<EndpointConfig>({
    preferredEndpoint: 'auto',
    endpointFallback: true,
  })
  const [proxyType, setProxyType] = useState<'none' | 'http' | 'https' | 'socks5' | 'socks5h'>('none')
  const [proxyHost, setProxyHost] = useState('')
  const [proxyPort, setProxyPort] = useState('')
  const [proxyUsername, setProxyUsername] = useState('')
  const [proxyPassword, setProxyPassword] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [promptFilter, setPromptFilter] = useState<PromptFilterConfig>({
    filterClaudeCode: false,
    filterEnvNoise: false,
    filterStripBoundaries: false,
    rules: [],
  })
  const [modelMappings, setModelMappings] = useState<ModelMappingRule[]>([])
  const [contextWindowOverride, setContextWindowOverride] = useState('0')
  const [contextUsageMultiplier, setContextUsageMultiplier] = useState('1')

  useEffect(() => {
    if (open) loadSettings()
  }, [open])

  const loadSettings = async () => {
    setLoading(true)
    try {
      const [nextSettings, nextCommon, nextThinking, nextEndpoint, nextProxy, nextPromptFilter, nextModelMappings, nextGlobal] =
        await Promise.all([
          getAccessSettings(),
          getCommonConfig(),
          getThinkingConfig(),
          getEndpointConfig(),
          getProxyConfig(),
          getPromptFilterConfig(),
          getModelMappings(),
          getGlobalConfig(),
        ])
      setSettings(nextSettings)
      setCommon(nextCommon)
      setThinking(nextThinking)
      setEndpoint(nextEndpoint)
      setPromptFilter(nextPromptFilter)
      setModelMappings(nextModelMappings.rules ?? [])
      setContextWindowOverride(String(nextGlobal.contextWindowOverride ?? 0))
      setContextUsageMultiplier(String(nextGlobal.contextUsageMultiplier ?? 1))
      parseProxyURL(nextProxy.proxyUrl || '')
    } catch (error) {
      toast.error(`加载设置失败: ${extractErrorMessage(error)}`)
    } finally {
      setLoading(false)
    }
  }

  const handleSave = async () => {
    if (!settings) return

    const overrideNum = Number(contextWindowOverride.trim() || '0')
    if (!Number.isFinite(overrideNum) || overrideNum < 0) {
      toast.error('上下文窗口覆盖值必须是 >= 0 的整数（0 表示用模型默认）')
      return
    }
    const multiplierNum = Number(contextUsageMultiplier.trim() || '1')
    if (!Number.isFinite(multiplierNum) || multiplierNum < 0.1 || multiplierNum > 10) {
      toast.error('上下文放大系数必须在 0.1 到 10.0 之间')
      return
    }

    setSaving(true)
    try {
      await Promise.all([
        updateAccessSettings({
          apiKey: settings.apiKey || '',
          requireApiKey: settings.requireApiKey,
          allowOverUsage: settings.allowOverUsage,
          ...(newPassword.trim() ? { password: newPassword.trim() } : {}),
        }),
        updateCommonConfig({
          credentialMachineIdStrategy: common.credentialMachineIdStrategy,
        }),
        updateThinkingConfig({
          suffix: thinking.suffix || '-thinking',
          openaiFormat: thinking.openaiFormat,
          claudeFormat: thinking.claudeFormat,
        }),
        updateEndpointConfig(endpoint),
        updateProxyConfig({
          proxyUrl: buildProxyURL(),
          proxyUsername: null,
          proxyPassword: null,
        }),
        updatePromptFilterConfig(promptFilter),
        updateModelMappings({ rules: modelMappings }),
        updateGlobalConfig({
          contextWindowOverride: Math.trunc(overrideNum),
          contextUsageMultiplier: multiplierNum,
        }),
      ])
      setNewPassword('')
      toast.success('设置已保存')
      await loadSettings()
    } catch (error) {
      toast.error(`保存失败: ${extractErrorMessage(error)}`)
    } finally {
      setSaving(false)
    }
  }

  const parseProxyURL = (value: string) => {
    if (!value) {
      setProxyType('none')
      setProxyHost('')
      setProxyPort('')
      setProxyUsername('')
      setProxyPassword('')
      return
    }

    try {
      const parsed = new URL(value)
      setProxyType(parsed.protocol.replace(':', '') as typeof proxyType)
      setProxyHost(parsed.hostname)
      setProxyPort(parsed.port)
      setProxyUsername(decodeURIComponent(parsed.username))
      setProxyPassword(decodeURIComponent(parsed.password))
    } catch {
      setProxyType('none')
      setProxyHost('')
      setProxyPort('')
      setProxyUsername('')
      setProxyPassword('')
    }
  }

  const buildProxyURL = () => {
    if (proxyType === 'none') return ''
    if (!proxyHost.trim() || !proxyPort.trim()) {
      throw new Error('代理 host 和端口不能为空')
    }
    const auth = proxyUsername.trim()
      ? `${encodeURIComponent(proxyUsername.trim())}${proxyPassword ? `:${encodeURIComponent(proxyPassword)}` : ''}@`
      : ''
    return `${proxyType}://${auth}${proxyHost.trim()}:${proxyPort.trim()}`
  }

  const updateRule = (id: string, patch: Partial<PromptFilterRule>) => {
    setPromptFilter(prev => ({
      ...prev,
      rules: prev.rules.map(rule => rule.id === id ? { ...rule, ...patch } : rule),
    }))
  }

  const addRule = () => {
    const id = globalThis.crypto?.randomUUID?.() || `rule-${Date.now()}`
    setPromptFilter(prev => ({
      ...prev,
      rules: [
        ...prev.rules,
        {
          id,
          name: '自定义规则',
          enabled: true,
          type: 'contains',
          match: '',
          replace: '',
        },
      ],
    }))
  }

  const removeRule = (id: string) => {
    setPromptFilter(prev => ({
      ...prev,
      rules: prev.rules.filter(rule => rule.id !== id),
    }))
  }

  const updateMapping = (id: string, patch: Partial<ModelMappingRule>) => {
    setModelMappings(prev => prev.map(rule => rule.id === id ? { ...rule, ...patch } : rule))
  }

  const addMapping = () => {
    const id = globalThis.crypto?.randomUUID?.() || `mapping-${Date.now()}`
    setModelMappings(prev => [
      ...prev,
      {
        id,
        name: '模型映射',
        enabled: true,
        ruleType: 'replace',
        sourceModel: '',
        targetModels: [''],
        weights: [],
      },
    ])
  }

  const removeMapping = (id: string) => {
    setModelMappings(prev => prev.filter(rule => rule.id !== id))
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-4xl p-0 gap-0 h-[82vh] max-h-[760px] flex flex-col overflow-hidden">
        <DialogHeader className="px-6 pt-5 pb-4 border-b bg-muted/30">
          <DialogTitle className="flex items-center gap-2">
            <Settings2 className="h-5 w-5" />
            xkiro.rs 设置
          </DialogTitle>
        </DialogHeader>

        <div className="flex flex-1 min-h-0">
          <nav className="w-44 shrink-0 border-r bg-muted/20 p-2 overflow-y-auto">
            {TABS.map(tab => (
              <button
                key={tab.id}
                onClick={() => setActiveTab(tab.id)}
                className={`w-full rounded-lg px-3 py-2 text-left text-sm transition-colors ${
                  activeTab === tab.id
                    ? 'bg-primary text-primary-foreground shadow-sm'
                    : 'text-muted-foreground hover:bg-muted hover:text-foreground'
                }`}
              >
                {tab.label}
              </button>
            ))}
          </nav>

          <div className="flex-1 flex flex-col min-w-0">
            <div className="flex-1 overflow-y-auto px-6 py-5">
              {loading ? (
                <div className="flex h-full items-center justify-center">
                  <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
                </div>
              ) : settings ? (
                <div className="space-y-5">
                  {activeTab === 'common' && (
                    <Section
                      title="常用"
                      desc="控制后续登录或导入凭证时缺失 machineId 的补全方式；已有凭证不会被修改。"
                    >
                      <ReadonlyPair label="本机 machineId" value={common.machineId || '-'} />
                      <SelectRow
                        label="凭证 machineId 策略"
                        value={common.credentialMachineIdStrategy}
                        options={[
                          { value: 'random', label: '随机 machineId' },
                          { value: 'local', label: '使用本机 machineId' },
                        ]}
                        onChange={value => setCommon({
                          ...common,
                          credentialMachineIdStrategy: value as CommonConfig['credentialMachineIdStrategy'],
                        })}
                      />
                    </Section>
                  )}

                  {activeTab === 'access' && (
                    <Section title="访问控制">
                      <Field label="API 密钥">
                        <Input
                          value={settings.apiKey || ''}
                          onChange={event => setSettings({ ...settings, apiKey: event.target.value })}
                          placeholder="留空可配合关闭 API 密钥校验"
                        />
                      </Field>
                      <ToggleRow
                        label="启用 API 密钥校验"
                        checked={settings.requireApiKey}
                        onChange={value => setSettings({ ...settings, requireApiKey: value })}
                      />
                      <ToggleRow
                        label="允许超额使用"
                        desc="仅保存设置；不改变运行时调度策略。"
                        checked={settings.allowOverUsage}
                        onChange={value => setSettings({ ...settings, allowOverUsage: value })}
                      />
                      <Field label="新 Admin 密码">
                        <Input
                          type="password"
                          value={newPassword}
                          onChange={event => setNewPassword(event.target.value)}
                          placeholder="新的 Admin API 密钥"
                        />
                      </Field>
                      <ReadonlyPair label="监听地址" value={`${settings.host}:${settings.port}`} />
                    </Section>
                  )}

                  {activeTab === 'thinking' && (
                    <Section title="Thinking 配置">
                      <Field label="模型后缀">
                        <Input
                          value={thinking.suffix}
                          onChange={event => setThinking({ ...thinking, suffix: event.target.value })}
                          placeholder="-thinking"
                        />
                      </Field>
                      <SelectRow
                        label="OpenAI 输出格式"
                        value={thinking.openaiFormat}
                        options={thinkingFormatOptions}
                        onChange={value => setThinking({ ...thinking, openaiFormat: value as ThinkingConfig['openaiFormat'] })}
                      />
                      <SelectRow
                        label="Claude 输出格式"
                        value={thinking.claudeFormat}
                        options={thinkingFormatOptions}
                        onChange={value => setThinking({ ...thinking, claudeFormat: value as ThinkingConfig['claudeFormat'] })}
                      />
                    </Section>
                  )}

                  {activeTab === 'endpoint' && (
                    <Section title="端点配置">
                      <SelectRow
                        label="首选端点"
                        value={endpoint.preferredEndpoint}
                        options={[
                          { value: 'auto', label: '自动' },
                          { value: 'kiro', label: '默认端点' },
                          { value: 'codewhisperer', label: 'CodeWhisperer' },
                          { value: 'amazonq', label: 'AmazonQ' },
                        ]}
                        onChange={value => setEndpoint({ ...endpoint, preferredEndpoint: value as EndpointConfig['preferredEndpoint'] })}
                      />
                      <ToggleRow
                        label="端点故障转移"
                        checked={endpoint.endpointFallback}
                        onChange={value => setEndpoint({ ...endpoint, endpointFallback: value })}
                      />
                    </Section>
                  )}

                  {activeTab === 'proxy' && (
                    <Section title="代理配置">
                      <SelectRow
                        label="代理类型"
                        value={proxyType}
                        options={[
                          { value: 'none', label: 'None' },
                          { value: 'http', label: 'HTTP' },
                          { value: 'https', label: 'HTTPS' },
                          { value: 'socks5', label: 'SOCKS5' },
                          { value: 'socks5h', label: 'SOCKS5H' },
                        ]}
                        onChange={value => setProxyType(value as typeof proxyType)}
                      />
                      {proxyType !== 'none' && (
                        <>
                          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
                            <Field label="Host">
                              <Input value={proxyHost} onChange={event => setProxyHost(event.target.value)} placeholder="127.0.0.1" />
                            </Field>
                            <Field label="Port">
                              <Input value={proxyPort} onChange={event => setProxyPort(event.target.value)} placeholder="7890" />
                            </Field>
                          </div>
                          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
                            <Field label="Username">
                              <Input value={proxyUsername} onChange={event => setProxyUsername(event.target.value)} />
                            </Field>
                            <Field label="Password">
                              <Input type="password" value={proxyPassword} onChange={event => setProxyPassword(event.target.value)} />
                            </Field>
                          </div>
                        </>
                      )}
                    </Section>
                  )}

                  {activeTab === 'prompt-filter' && (
                    <Section title="Prompt Filter">
                      <ToggleRow
                        label="Filter Claude Code"
                        checked={promptFilter.filterClaudeCode}
                        onChange={value => setPromptFilter({ ...promptFilter, filterClaudeCode: value })}
                      />
                      <ToggleRow
                        label="Filter Env Noise"
                        checked={promptFilter.filterEnvNoise}
                        onChange={value => setPromptFilter({ ...promptFilter, filterEnvNoise: value })}
                      />
                      <ToggleRow
                        label="Filter Strip Boundaries"
                        checked={promptFilter.filterStripBoundaries}
                        onChange={value => setPromptFilter({ ...promptFilter, filterStripBoundaries: value })}
                      />
                      <div className="flex items-center justify-between border-t pt-4">
                        <div>
                          <div className="text-sm font-medium">自定义规则</div>
                          <p className="text-xs text-muted-foreground">支持 regex、lines-containing、contains。</p>
                        </div>
                        <Button type="button" size="sm" variant="outline" onClick={addRule}>
                          <Plus className="mr-1 h-4 w-4" />
                          添加规则
                        </Button>
                      </div>
                      {promptFilter.rules.length === 0 ? (
                        <p className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground">暂无规则。</p>
                      ) : (
                        <div className="space-y-3">
                          {promptFilter.rules.map(rule => (
                            <div key={rule.id} className="rounded-xl border bg-card p-4 shadow-sm">
                              <div className="mb-3 flex items-center justify-between gap-3">
                                <Input
                                  value={rule.name}
                                  onChange={event => updateRule(rule.id, { name: event.target.value })}
                                  className="h-8 max-w-xs"
                                />
                                <div className="flex items-center gap-2">
                                  <Switch checked={rule.enabled} onCheckedChange={value => updateRule(rule.id, { enabled: value })} />
                                  <Button type="button" size="icon" variant="outline" onClick={() => removeRule(rule.id)}>
                                    <Trash2 className="h-4 w-4" />
                                  </Button>
                                </div>
                              </div>
                              <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
                                <SelectRow
                                  label="类型"
                                  value={rule.type}
                                  options={[
                                    { value: 'regex', label: 'regex' },
                                    { value: 'lines-containing', label: 'lines-containing' },
                                    { value: 'contains', label: 'contains' },
                                  ]}
                                  onChange={value => updateRule(rule.id, { type: value as PromptFilterRule['type'] })}
                                />
                                <Field label="替换内容">
                                  <Input value={rule.replace || ''} onChange={event => updateRule(rule.id, { replace: event.target.value })} />
                                </Field>
                              </div>
                              <Field label="匹配内容">
                                <textarea
                                  value={rule.match}
                                  onChange={event => updateRule(rule.id, { match: event.target.value })}
                                  className="min-h-20 w-full rounded-md border border-input bg-background px-3 py-2 text-sm shadow-sm outline-none focus-visible:ring-1 focus-visible:ring-ring"
                                />
                              </Field>
                            </div>
                          ))}
                        </div>
                      )}
                    </Section>
                  )}

                  {activeTab === 'model-mappings' && (
                    <Section
                      title="模型映射"
                      desc="命中 sourceModel 时把请求模型改写为 targetModels 之一，作为归一化前的覆盖层。仅作用于 OpenAI / OpenAI Responses 路径，不影响 Anthropic Messages。"
                    >
                      <div className="flex items-center justify-between border-b pb-4">
                        <div>
                          <div className="text-sm font-medium">映射规则</div>
                          <p className="text-xs text-muted-foreground">replace/alias 取第一个目标；loadbalance 按权重随机（权重为空则轮询）。</p>
                        </div>
                        <Button type="button" size="sm" variant="outline" onClick={addMapping}>
                          <Plus className="mr-1 h-4 w-4" />
                          添加规则
                        </Button>
                      </div>
                      {modelMappings.length === 0 ? (
                        <p className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground">暂无映射规则。</p>
                      ) : (
                        <div className="space-y-3">
                          {modelMappings.map(rule => (
                            <div key={rule.id} className="rounded-xl border bg-card p-4 shadow-sm">
                              <div className="mb-3 flex items-center justify-between gap-3">
                                <Input
                                  value={rule.name}
                                  onChange={event => updateMapping(rule.id, { name: event.target.value })}
                                  className="h-8 max-w-xs"
                                />
                                <div className="flex items-center gap-2">
                                  <Switch checked={rule.enabled} onCheckedChange={value => updateMapping(rule.id, { enabled: value })} />
                                  <Button type="button" size="icon" variant="outline" onClick={() => removeMapping(rule.id)}>
                                    <Trash2 className="h-4 w-4" />
                                  </Button>
                                </div>
                              </div>
                              <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
                                <SelectRow
                                  label="类型"
                                  value={rule.ruleType}
                                  options={[
                                    { value: 'replace', label: 'replace' },
                                    { value: 'alias', label: 'alias' },
                                    { value: 'loadbalance', label: 'loadbalance' },
                                  ]}
                                  onChange={value => updateMapping(rule.id, { ruleType: value as ModelMappingRule['ruleType'] })}
                                />
                                <Field label="源模型 sourceModel">
                                  <Input value={rule.sourceModel} onChange={event => updateMapping(rule.id, { sourceModel: event.target.value })} placeholder="gpt-4.1" />
                                </Field>
                              </div>
                              <Field label="目标模型 targetModels（每行一个）">
                                <textarea
                                  value={rule.targetModels.join('\n')}
                                  onChange={event => updateMapping(rule.id, { targetModels: event.target.value.split('\n').map(s => s.trim()).filter(Boolean) })}
                                  className="min-h-20 w-full rounded-md border border-input bg-background px-3 py-2 text-sm shadow-sm outline-none focus-visible:ring-1 focus-visible:ring-ring"
                                  placeholder="claude-sonnet-4-20250514"
                                />
                              </Field>
                              {rule.ruleType === 'loadbalance' && (
                                <Field label="权重 weights（逗号分隔，可留空=轮询）">
                                  <Input
                                    value={rule.weights.join(',')}
                                    onChange={event => updateMapping(rule.id, { weights: event.target.value.split(',').map(s => Number(s.trim())).filter(n => Number.isFinite(n) && n > 0) })}
                                    placeholder="1,1"
                                  />
                                </Field>
                              )}
                            </div>
                          ))}
                        </div>
                      )}
                    </Section>
                  )}

                  {activeTab === 'context' && (
                    <Section
                      title="Auto-compact 调参"
                      desc="调整上报给客户端的上下文用量，从而提前或推迟 Claude Code 的自动压缩。真正的触发阈值在客户端，这里只改变换算基准，默认值下行为与不配置完全一致。"
                    >
                      <Field label="上下文窗口覆盖值" desc="0 表示用模型默认（大窗口模型 1M，其余 200K）。设更大的值会让同一占比换算出更多 token，客户端更早压缩。">
                        <Input
                          value={contextWindowOverride}
                          onChange={event => setContextWindowOverride(event.target.value)}
                          placeholder="0"
                          inputMode="numeric"
                        />
                      </Field>
                      <Field label="上下文放大系数" desc="范围 0.1 ~ 10.0，默认 1.0。>1 提前触发压缩，<1 推迟。与窗口覆盖值叠加相乘。">
                        <Input
                          value={contextUsageMultiplier}
                          onChange={event => setContextUsageMultiplier(event.target.value)}
                          placeholder="1.0"
                          inputMode="decimal"
                        />
                      </Field>
                    </Section>
                  )}
                </div>
              ) : (
                <p className="text-sm text-muted-foreground">加载失败，请关闭后重试。</p>
              )}
            </div>

            {!loading && settings && (
              <div className="flex items-center justify-between border-t bg-muted/20 px-6 py-3">
                <div />
                <Button onClick={handleSave} disabled={saving}>
                  {saving ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
                  保存
                </Button>
              </div>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

const thinkingFormatOptions = [
  { value: 'reasoning_content', label: 'reasoning_content' },
  { value: 'thinking', label: 'thinking' },
  { value: 'think', label: 'think' },
]

function Section({ title, desc, children }: { title: string; desc?: string; children: ReactNode }) {
  return (
    <section className="space-y-4">
      <div>
        <h3 className="text-base font-semibold">{title}</h3>
        {desc && <p className="mt-1 text-sm text-muted-foreground">{desc}</p>}
      </div>
      <div className="space-y-4 rounded-2xl border bg-card p-5 shadow-sm">{children}</div>
    </section>
  )
}

function Field({ label, desc, children }: { label: string; desc?: string; children: ReactNode }) {
  return (
    <label className="block space-y-2">
      <span className="text-sm font-medium">{label}</span>
      {children}
      {desc && <span className="block text-xs text-muted-foreground">{desc}</span>}
    </label>
  )
}

function ToggleRow({ label, desc, checked, onChange }: { label: string; desc?: string; checked: boolean; onChange: (value: boolean) => void }) {
  return (
    <div className="flex items-center justify-between gap-4 rounded-lg border bg-muted/30 px-4 py-3">
      <div>
        <div className="text-sm font-medium">{label}</div>
        {desc && <p className="mt-0.5 text-xs text-muted-foreground">{desc}</p>}
      </div>
      <Switch checked={checked} onCheckedChange={onChange} />
    </div>
  )
}

function SelectRow({ label, value, options, onChange }: {
  label: string
  value: string
  options: { value: string; label: string }[]
  onChange: (value: string) => void
}) {
  return (
    <label className="flex items-center justify-between gap-4">
      <span className="text-sm font-medium">{label}</span>
      <select
        value={value}
        onChange={event => onChange(event.target.value)}
        className="h-9 min-w-48 rounded-md border border-input bg-background px-3 text-sm shadow-sm outline-none focus-visible:ring-1 focus-visible:ring-ring"
      >
        {options.map(option => (
          <option key={option.value} value={option.value}>{option.label}</option>
        ))}
      </select>
    </label>
  )
}

function ReadonlyPair({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between rounded-lg border bg-muted/30 px-4 py-3">
      <span className="text-sm font-medium">{label}</span>
      <span className="font-mono text-sm text-muted-foreground">{value}</span>
    </div>
  )
}
