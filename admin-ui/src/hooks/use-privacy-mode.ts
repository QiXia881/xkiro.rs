import { useEffect, useState, useCallback } from 'react'
import { storage } from '@/lib/storage'
import { getGlobalConfig, updateGlobalConfig } from '@/api/credentials'

const EVENT = 'admin-privacy-mode-change'

function emit(enabled: boolean) {
  window.dispatchEvent(new CustomEvent<boolean>(EVENT, { detail: enabled }))
}

let fetchPromise: Promise<boolean> | null = null

function fetchOnce(): Promise<boolean> {
  if (!fetchPromise) {
    fetchPromise = getGlobalConfig()
      .then(cfg => {
        storage.setPrivacyMode(cfg.privacyMode)
        emit(cfg.privacyMode)
        return cfg.privacyMode
      })
      .catch(() => storage.getPrivacyMode())
  }
  return fetchPromise
}

export function usePrivacyMode() {
  const [enabled, setEnabledState] = useState<boolean>(() => storage.getPrivacyMode())

  useEffect(() => {
    const handler = (e: Event) => {
      const ev = e as CustomEvent<boolean>
      setEnabledState(ev.detail)
    }
    window.addEventListener(EVENT, handler)
    fetchOnce().then(v => setEnabledState(v))
    return () => window.removeEventListener(EVENT, handler)
  }, [])

  const setEnabled = useCallback(async (next: boolean) => {
    storage.setPrivacyMode(next)
    setEnabledState(next)
    emit(next)
    try {
      await updateGlobalConfig({ privacyMode: next })
    } catch (err) {
      console.error('save privacyMode failed', err)
    }
  }, [])

  return { privacyMode: enabled, setPrivacyMode: setEnabled }
}
