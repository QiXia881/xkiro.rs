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
  getEndpointConfig,
  getXkiroProxyConfig,
  getXkiroSettings,
  getPromptFilterConfig,
  getThinkingConfig,
  updateEndpointConfig,
  updateXkiroProxyConfig,
  updateXkiroSettings,
  updatePromptFilterConfig,
  updateThinkingConfig,
  type EndpointConfig,
  type XkiroSettings,
  type PromptFilterConfig,
  type PromptFilterRule,
  type ThinkingConfig,
} from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface SettingsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

type SettingsTab = 'access' | 'thinking' | 'endpoint' | 'proxy' | 'prompt-filter'

const TABS: { id: SettingsTab; label: string }[] = [
  { id: 'access', label: '访问控制' },
  { id: 'thinking', label: 'Thinking' },
  { id: 'endpoint', label: '端点' },
  { id: 'proxy', label: '代理' },
  { id: 'prompt-filter', label: 'Prompt Filter' },
]

const DEFAULT_THINKING: ThinkingConfig = {
  suffix: '-thinking',
  openaiFormat: 'reasoning_content',
  claudeFormat: 'thinking',
}

export function SettingsDialog({ open, onOpenChange }: SettingsDialogProps) {
  const [activeTab, setActiveTab] = useState<SettingsTab>('access')
  const [loading, setLoading] = useState(false)
  const [saving, setSaving] = useState(false)
  const [settings, setSettings] = useState<XkiroSettings | null>(null)
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

  useEffect(() => {
    if (open) loadSettings()
  }, [open])

  const loadSettings = async () => {
    setLoading(true)
    try {
      const [nextSettings, nextThinking, nextEndpoint, nextProxy, nextPromptFilter] =
        await Promise.all([
          getXkiroSettings(),
          getThinkingConfig(),
          getEndpointConfig(),
          getXkiroProxyConfig(),
          getPromptFilterConfig(),
        ])
      setSettings(nextSettings)
      setThinking(nextThinking)
      setEndpoint(nextEndpoint)
      setPromptFilter(nextPromptFilter)
      parseProxyURL(nextProxy.proxyURL || '')
    } catch (error) {
      toast.error(`加载设置失败: ${extractErrorMessage(error)}`)
    } finally {
      setLoading(false)
    }
  }

  const handleSave = async () => {
    if (!settings) return
    setSaving(true)
    try {
      await Promise.all([
        updateXkiroSettings({
          apiKey: settings.apiKey || '',
          requireApiKey: settings.requireApiKey,
          allowOverUsage: settings.allowOverUsage,
          ...(newPassword.trim() ? { password: newPassword.trim() } : {}),
        }),
        updateThinkingConfig({
          suffix: thinking.suffix || '-thinking',
          openaiFormat: thinking.openaiFormat,
          claudeFormat: thinking.claudeFormat,
        }),
        updateEndpointConfig(endpoint),
        updateXkiroProxyConfig(buildProxyURL()),
        updatePromptFilterConfig(promptFilter),
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
                  {activeTab === 'access' && (
                    <Section title="访问控制">
                      <Field label="API Key">
                        <Input
                          value={settings.apiKey || ''}
                          onChange={event => setSettings({ ...settings, apiKey: event.target.value })}
                          placeholder="留空可配合关闭 Require API Key"
                        />
                      </Field>
                      <ToggleRow
                        label="Require API Key"
                        checked={settings.requireApiKey}
                        onChange={value => setSettings({ ...settings, requireApiKey: value })}
                      />
                      <ToggleRow
                        label="Allow Over Usage"
                        desc="仅保存设置；不改变运行时调度策略。"
                        checked={settings.allowOverUsage}
                        onChange={value => setSettings({ ...settings, allowOverUsage: value })}
                      />
                      <Field label="新 Admin 密码">
                        <Input
                          type="password"
                          value={newPassword}
                          onChange={event => setNewPassword(event.target.value)}
                          placeholder="新的 Admin API Key"
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
                        label="Preferred Endpoint"
                        value={endpoint.preferredEndpoint}
                        options={[
                          { value: 'auto', label: 'Auto' },
                          { value: 'kiro', label: 'Kiro' },
                          { value: 'codewhisperer', label: 'CodeWhisperer' },
                          { value: 'amazonq', label: 'AmazonQ' },
                        ]}
                        onChange={value => setEndpoint({ ...endpoint, preferredEndpoint: value as EndpointConfig['preferredEndpoint'] })}
                      />
                      <ToggleRow
                        label="Endpoint Fallback"
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
