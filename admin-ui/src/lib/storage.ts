const API_KEY_STORAGE_KEY = 'adminApiKey'
const THEME_STORAGE_KEY = 'adminTheme'
const UI_SCALE_STORAGE_KEY = 'adminUiScale'
const PRIVACY_MODE_STORAGE_KEY = 'adminPrivacyMode'

export type UiScale = 90 | 100 | 115 | 130

export const storage = {
  getApiKey: () => localStorage.getItem(API_KEY_STORAGE_KEY),
  setApiKey: (key: string) => localStorage.setItem(API_KEY_STORAGE_KEY, key),
  removeApiKey: () => localStorage.removeItem(API_KEY_STORAGE_KEY),

  getTheme: (): 'dark' | 'light' | null => localStorage.getItem(THEME_STORAGE_KEY) as 'dark' | 'light' | null,
  setTheme: (theme: 'dark' | 'light') => localStorage.setItem(THEME_STORAGE_KEY, theme),

  getUiScale: (): UiScale | null => {
    const raw = localStorage.getItem(UI_SCALE_STORAGE_KEY)
    const n = raw ? Number(raw) : NaN
    return [90, 100, 115, 130].includes(n) ? (n as UiScale) : null
  },
  setUiScale: (scale: UiScale) => localStorage.setItem(UI_SCALE_STORAGE_KEY, String(scale)),

  getPrivacyMode: (): boolean => {
    const raw = localStorage.getItem(PRIVACY_MODE_STORAGE_KEY)
    if (raw === null) return true
    return raw === '1' || raw === 'true'
  },
  setPrivacyMode: (enabled: boolean) => localStorage.setItem(PRIVACY_MODE_STORAGE_KEY, enabled ? '1' : '0'),
}
