import { useState, useEffect, useCallback } from 'react'
import { KeyRound, Eye, EyeOff, Moon, Sun } from 'lucide-react'
import { storage } from '@/lib/storage'
import { Card, CardContent } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'

interface LoginPageProps {
  onLogin: (apiKey: string) => void
}

// Floating background decoration shapes
function FloatingShapes() {
  return (
    <div className="pointer-events-none fixed inset-0 overflow-hidden" aria-hidden="true">
      <div
        className="absolute -top-24 -left-24 h-96 w-96 rounded-full opacity-[0.04] dark:opacity-[0.06]"
        style={{
          background: 'radial-gradient(circle, var(--primary) 0%, transparent 70%)',
          animation: 'float-slow 20s ease-in-out infinite',
        }}
      />
      <div
        className="absolute top-1/3 -right-16 h-72 w-72 rounded-full opacity-[0.03] dark:opacity-[0.05]"
        style={{
          background: 'radial-gradient(circle, var(--primary) 0%, transparent 70%)',
          animation: 'float-slow 25s ease-in-out infinite reverse',
        }}
      />
      <div
        className="absolute -bottom-20 left-1/3 h-80 w-80 rounded-full opacity-[0.03] dark:opacity-[0.04]"
        style={{
          background: 'radial-gradient(circle, var(--primary) 0%, transparent 70%)',
          animation: 'float-slow 18s ease-in-out infinite',
          animationDelay: '-5s',
        }}
      />
      {/* Subtle geometric shapes */}
      <div
        className="absolute top-1/4 left-1/4 h-2 w-2 rotate-45 bg-primary/10 dark:bg-primary/15"
        style={{ animation: 'drift 15s linear infinite' }}
      />
      <div
        className="absolute top-2/3 right-1/4 h-1.5 w-1.5 rotate-12 bg-primary/10 dark:bg-primary/15"
        style={{ animation: 'drift 20s linear infinite reverse' }}
      />
      <div
        className="absolute top-1/2 left-3/4 h-1 w-1 rounded-full bg-primary/15 dark:bg-primary/20"
        style={{ animation: 'drift 12s linear infinite', animationDelay: '-3s' }}
      />
    </div>
  )
}

export function LoginPage({ onLogin }: LoginPageProps) {
  const [apiKey, setApiKey] = useState('')
  const [showPassword, setShowPassword] = useState(false)
  const [rememberMe, setRememberMe] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [shakeKey, setShakeKey] = useState(0)
  const [darkMode, setDarkMode] = useState(() => {
    const saved = storage.getTheme()
    if (saved) return saved === 'dark'
    if (typeof window !== 'undefined') {
      return window.matchMedia('(prefers-color-scheme: dark)').matches
    }
    return false
  })

  // Entrance animation state
  const [mounted, setMounted] = useState(false)

  useEffect(() => {
    // Trigger entrance animations after mount
    const timer = requestAnimationFrame(() => setMounted(true))
    return () => cancelAnimationFrame(timer)
  }, [])

  useEffect(() => {
    const savedKey = storage.getApiKey()
    if (savedKey) {
      setApiKey(savedKey)
      setRememberMe(true)
    }
  }, [])

  // Apply theme on mount
  useEffect(() => {
    if (darkMode) {
      document.documentElement.classList.add('dark')
    } else {
      document.documentElement.classList.remove('dark')
    }
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  const toggleDarkMode = useCallback(() => {
    const next = !darkMode
    setDarkMode(next)
    storage.setTheme(next ? 'dark' : 'light')
    document.documentElement.classList.toggle('dark')
  }, [darkMode])

  const triggerShake = () => {
    setShakeKey(k => k + 1)
  }

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    const trimmed = apiKey.trim()
    if (!trimmed) {
      setError('请输入 Admin API Key')
      triggerShake()
      return
    }
    setError(null)

    if (rememberMe) {
      storage.setApiKey(trimmed)
    } else {
      storage.removeApiKey()
    }
    onLogin(trimmed)
  }

  return (
    <>
      {/* Keyframe styles for animations */}
      <style>{`
        @keyframes float-slow {
          0%, 100% { transform: translate(0, 0); }
          33% { transform: translate(30px, -20px); }
          66% { transform: translate(-15px, 15px); }
        }
        @keyframes drift {
          0% { transform: translateY(0) rotate(0deg); opacity: 0.15; }
          50% { opacity: 0.25; }
          100% { transform: translateY(-100vh) rotate(360deg); opacity: 0.15; }
        }
        @keyframes shake {
          0%, 100% { transform: translateX(0); }
          10%, 30%, 50%, 70%, 90% { transform: translateX(-4px); }
          20%, 40%, 60%, 80% { transform: translateX(4px); }
        }
      `}</style>

      <div className="relative min-h-screen flex items-center justify-center bg-background p-4">
        <FloatingShapes />

        {/* Theme toggle - top right */}
        <Button
          variant="ghost"
          size="icon"
          className="absolute top-4 right-4 h-9 w-9 z-10 opacity-60 hover:opacity-100 transition-opacity"
          onClick={toggleDarkMode}
          title={darkMode ? '切换到亮色模式' : '切换到暗色模式'}
        >
          {darkMode ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
        </Button>

        {/* Main card with entrance animation */}
        <div
          className="w-full max-w-md transition-all duration-700 ease-out"
          style={{
            opacity: mounted ? 1 : 0,
            transform: mounted ? 'translateY(0)' : 'translateY(24px)',
          }}
        >
          {/* Header section with slide-down animation */}
          <div
            className="text-center mb-6 transition-all duration-500 ease-out"
            style={{
              opacity: mounted ? 1 : 0,
              transform: mounted ? 'translateY(0)' : 'translateY(-16px)',
              transitionDelay: '100ms',
            }}
          >
            <div className="mx-auto mb-3 flex h-14 w-14 items-center justify-center rounded-xl bg-primary/10 ring-1 ring-primary/20">
              <KeyRound className="h-7 w-7 text-primary" />
            </div>
            <h1 className="text-2xl font-bold tracking-tight">xkiro.rs</h1>
          </div>

          {/* Login card */}
          <Card className="shadow-lg shadow-black/5 dark:shadow-black/20">
            <CardContent className="pt-6">
              <form onSubmit={handleSubmit} className="space-y-5">
                {/* API Key input with show/hide toggle */}
                <div className="space-y-2">
                  <label htmlFor="api-key" className="text-sm font-medium">
                    Admin API Key
                  </label>
                  <div className="relative">
                    <Input
                      id="api-key"
                      type={showPassword ? 'text' : 'password'}
                      placeholder="输入你的 Admin API Key"
                      value={apiKey}
                      onChange={(e) => {
                        setApiKey(e.target.value)
                        if (error) setError(null)
                      }}
                      className={`pr-10 font-mono text-sm ${
                        error
                          ? 'border-destructive focus-visible:ring-destructive/30'
                          : ''
                      }`}
                      autoComplete="off"
                      autoFocus
                    />
                    <button
                      type="button"
                      className="absolute right-2 top-1/2 -translate-y-1/2 p-1 rounded-sm text-muted-foreground hover:text-foreground transition-colors"
                      onClick={() => setShowPassword(v => !v)}
                      tabIndex={-1}
                      aria-label={showPassword ? '隐藏密钥' : '显示密钥'}
                    >
                      {showPassword ? (
                        <EyeOff className="h-4 w-4" />
                      ) : (
                        <Eye className="h-4 w-4" />
                      )}
                    </button>
                  </div>

                  {/* Error message with shake animation */}
                  {error && (
                    <div
                      key={shakeKey}
                      className="text-sm text-destructive flex items-center gap-1.5"
                      style={{ animation: 'shake 0.4s ease-in-out' }}
                    >
                      <span className="inline-block h-1 w-1 rounded-full bg-destructive" />
                      {error}
                    </div>
                  )}
                </div>

                {/* Remember me checkbox */}
                <div className="flex items-center gap-2">
                  <Checkbox
                    id="remember-me"
                    checked={rememberMe}
                    onCheckedChange={(checked) => setRememberMe(checked === true)}
                  />
                  <label
                    htmlFor="remember-me"
                    className="text-sm text-muted-foreground select-none cursor-pointer"
                  >
                    记住 API Key
                  </label>
                </div>

                {/* Submit button */}
                <Button
                  type="submit"
                  className="w-full"
                  disabled={!apiKey.trim()}
                >
                  登录
                </Button>
              </form>
            </CardContent>
          </Card>

          {/* Footer hint */}
          <p
            className="mt-4 text-center text-xs text-muted-foreground/60 transition-all duration-500"
            style={{
              opacity: mounted ? 1 : 0,
              transitionDelay: '500ms',
            }}
          >
            需要 Admin API Key 才能访问管理面板
          </p>
        </div>
      </div>
    </>
  )
}
