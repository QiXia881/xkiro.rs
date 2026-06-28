const API_KEY_STORAGE_KEY = 'adminApiKey'
const THEME_STORAGE_KEY = 'adminTheme'
const PRIVACY_MODE_STORAGE_KEY = 'adminPrivacyMode'

export const storage = {
  getApiKey: () => localStorage.getItem(API_KEY_STORAGE_KEY),
  setApiKey: (key: string) => localStorage.setItem(API_KEY_STORAGE_KEY, key),
  removeApiKey: () => localStorage.removeItem(API_KEY_STORAGE_KEY),

  getTheme: (): 'dark' | 'light' | null => localStorage.getItem(THEME_STORAGE_KEY) as 'dark' | 'light' | null,
  setTheme: (theme: 'dark' | 'light') => localStorage.setItem(THEME_STORAGE_KEY, theme),

  getPrivacyMode: (): boolean => {
    const raw = localStorage.getItem(PRIVACY_MODE_STORAGE_KEY)
    if (raw === null) return true
    return raw === '1' || raw === 'true'
  },
  setPrivacyMode: (enabled: boolean) => localStorage.setItem(PRIVACY_MODE_STORAGE_KEY, enabled ? '1' : '0'),
}
