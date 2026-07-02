import { useState, useCallback } from 'react'
import {
  Building2,
  ChevronRight,
  Cookie,
  FileJson,
  FolderOpen,
  Github,
  KeyRound,
  Landmark,
  Plus,
  Shield,
} from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { BuilderIdLoginDialog } from '@/components/builderid-login-dialog'
import { IdcLoginDialog } from '@/components/idc-login-dialog'
import { SsoTokenImportDialog } from '@/components/sso-token-import-dialog'
import { LocalCacheImportDialog } from '@/components/local-cache-import-dialog'
import { WebCookieImportDialog } from '@/components/web-cookie-import-dialog'
import { KiroSsoLoginDialog } from '@/components/kiro-sso-login-dialog'
import { SocialLoginDialog } from '@/components/social-login-dialog'
import { CredentialImportDialog } from '@/components/credential-import-dialog'
import { CREDENTIAL_AUTH_LABELS } from '@/lib/credential-metadata'

// ─── Types ────────────────────────────────────────────────────────────

type MethodId =
  | 'builder-id'
  | 'social'
  | 'kiro-sso'
  | 'idc'
  | 'sso-token'
  | 'local-cache'
  | 'credential-import'
  | 'web-cookie'

interface MethodCard {
  id: MethodId
  icon: React.ElementType
  title: string
  description: string
  color: string
  bgColor: string
}

interface AddCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
}

// ─── Method definitions ───────────────────────────────────────────────

const METHODS: MethodCard[] = [
  {
    id: 'builder-id',
    icon: Building2,
    title: CREDENTIAL_AUTH_LABELS.awsBuilderId,
    description: `通过 ${CREDENTIAL_AUTH_LABELS.awsBuilderId} 设备授权流程添加个人凭据`,
    color: 'text-blue-600 dark:text-blue-400',
    bgColor: 'bg-blue-500/10',
  },
  {
    id: 'social',
    icon: Github,
    title: `${CREDENTIAL_AUTH_LABELS.google} / ${CREDENTIAL_AUTH_LABELS.github}`,
    description: `通过浏览器 OAuth 授权添加 ${CREDENTIAL_AUTH_LABELS.google} 或 ${CREDENTIAL_AUTH_LABELS.github} 凭据`,
    color: 'text-slate-700 dark:text-slate-300',
    bgColor: 'bg-slate-500/10',
  },
  {
    id: 'idc',
    icon: Shield,
    title: CREDENTIAL_AUTH_LABELS.iamIdentityCenter,
    description: `通过 ${CREDENTIAL_AUTH_LABELS.iamIdentityCenter} 授权码流程添加企业凭据`,
    color: 'text-orange-600 dark:text-orange-400',
    bgColor: 'bg-orange-500/10',
  },
  {
    id: 'kiro-sso',
    icon: Landmark,
    title: CREDENTIAL_AUTH_LABELS.microsoftEntra,
    description: `通过企业 SSO 远程登录添加 ${CREDENTIAL_AUTH_LABELS.microsoftEntra} 租户凭据`,
    color: 'text-sky-600 dark:text-sky-400',
    bgColor: 'bg-sky-500/10',
  },
  {
    id: 'sso-token',
    icon: KeyRound,
    title: 'SSO 令牌',
    description: '从浏览器 DevTools 导入 x-amz-sso_authn cookie',
    color: 'text-green-600 dark:text-green-400',
    bgColor: 'bg-green-500/10',
  },
  {
    id: 'local-cache',
    icon: FolderOpen,
    title: '本地缓存',
    description: '导入本地客户端缓存中的刷新令牌数据',
    color: 'text-purple-600 dark:text-purple-400',
    bgColor: 'bg-purple-500/10',
  },
  {
    id: 'credential-import',
    icon: FileJson,
    title: '凭据导入',
    description: '自动识别缓存凭据、xkiro.rs 完整备份和扁平凭据',
    color: 'text-cyan-600 dark:text-cyan-400',
    bgColor: 'bg-cyan-500/10',
  },
  {
    id: 'web-cookie',
    icon: Cookie,
    title: '浏览器 Cookie',
    description: '从 app.kiro.dev 浏览器 Cookie 提取刷新令牌',
    color: 'text-pink-600 dark:text-pink-400',
    bgColor: 'bg-pink-500/10',
  },
]

// ─── Method picker grid ───────────────────────────────────────────────

function MethodPicker({ onSelect }: { onSelect: (id: MethodId) => void }) {
  return (
    <div className="grid grid-cols-2 gap-3 py-2">
      {METHODS.map((method) => {
        const Icon = method.icon
        return (
          <button
            key={method.id}
            type="button"
            onClick={() => onSelect(method.id)}
            className="group flex items-center gap-3 rounded-lg border border-border p-4 text-left transition-all duration-150 hover:-translate-y-0.5 hover:shadow-md hover:border-primary/30 cursor-pointer"
          >
            <div className={`flex h-10 w-10 shrink-0 items-center justify-center rounded-lg ${method.bgColor}`}>
              <Icon className={`h-5 w-5 ${method.color}`} />
            </div>
            <div className="min-w-0 flex-1">
              <div className="text-sm font-medium leading-tight">{method.title}</div>
              <div className="mt-0.5 text-xs text-muted-foreground leading-snug line-clamp-2">
                {method.description}
              </div>
            </div>
            <ChevronRight className="h-4 w-4 shrink-0 text-muted-foreground opacity-0 transition-all group-hover:opacity-100 group-hover:translate-x-0.5" />
          </button>
        )
      })}
    </div>
  )
}

// ─── Main unified dialog ──────────────────────────────────────────────

export function AddCredentialDialog({ open, onOpenChange, onSuccess }: AddCredentialDialogProps) {
  // null = picker view; MethodId = that method's sub-dialog is open
  const [activeMethod, setActiveMethod] = useState<MethodId | null>(null)

  const handlePickerClose = useCallback(
    (nextOpen: boolean) => {
      if (!nextOpen) {
        setActiveMethod(null)
      }
      onOpenChange(nextOpen)
    },
    [onOpenChange],
  )

  const handleMethodSelect = useCallback((id: MethodId) => {
    setActiveMethod(id)
  }, [])

  const handleSubDialogClose = useCallback(() => {
    setActiveMethod(null)
  }, [])

  const handleSubSuccess = useCallback(() => {
    setActiveMethod(null)
    onSuccess()
  }, [onSuccess])

  // The picker is visible when the parent says open AND no sub-dialog is active
  const pickerVisible = open && activeMethod === null

  return (
    <>
      {/* ── Method picker ─────────────────────────────────────────── */}
      <Dialog open={pickerVisible} onOpenChange={handlePickerClose}>
        <DialogContent className="sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Plus className="h-5 w-5" />
              添加凭据
            </DialogTitle>
            <DialogDescription>
              选择一种方式添加新的 xkiro.rs 凭据
            </DialogDescription>
          </DialogHeader>
          <MethodPicker onSelect={handleMethodSelect} />
        </DialogContent>
      </Dialog>

      <BuilderIdLoginDialog
        open={open && activeMethod === 'builder-id'}
        onOpenChange={(o) => { if (!o) handleSubDialogClose() }}
        onSuccess={handleSubSuccess}
      />

      <SocialLoginDialog
        open={open && activeMethod === 'social'}
        onOpenChange={(o) => { if (!o) handleSubDialogClose() }}
        onSuccess={handleSubSuccess}
      />

      <IdcLoginDialog
        open={open && activeMethod === 'idc'}
        onOpenChange={(o) => { if (!o) handleSubDialogClose() }}
        onSuccess={handleSubSuccess}
      />

      <KiroSsoLoginDialog
        open={open && activeMethod === 'kiro-sso'}
        onOpenChange={(o) => { if (!o) handleSubDialogClose() }}
        onSuccess={handleSubSuccess}
      />

      {/* ── SSO 令牌导入 ─────────────────────────────────────────── */}
      <SsoTokenImportDialog
        open={open && activeMethod === 'sso-token'}
        onOpenChange={(o) => { if (!o) handleSubDialogClose() }}
        onSuccess={handleSubSuccess}
      />

      {/* ── Local cache import ───────────────────────────────────── */}
      <LocalCacheImportDialog
        open={open && activeMethod === 'local-cache'}
        onOpenChange={(o) => { if (!o) handleSubDialogClose() }}
        onSuccess={handleSubSuccess}
      />

      {/* ── 凭据导入 ─────────────────────────────────────────────── */}
      <CredentialImportDialog
        open={open && activeMethod === 'credential-import'}
        onOpenChange={(o) => { if (!o) handleSubDialogClose() }}
        onSuccess={handleSubSuccess}
      />

      {/* ── 浏览器 Cookie 导入 ────────────────────────────────────── */}
      <WebCookieImportDialog
        open={open && activeMethod === 'web-cookie'}
        onOpenChange={(o) => { if (!o) handleSubDialogClose() }}
        onSuccess={handleSubSuccess}
      />
    </>
  )
}
