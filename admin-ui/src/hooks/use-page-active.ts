import { useSyncExternalStore } from 'react'

/**
 * 全局「页面是否处于活跃状态」检测，给周期性轮询用。
 *
 * 活跃 = tab 可见 && 窗口聚焦 && 最近 IDLE_MS 内有用户交互。
 * 不活跃时调用方可以把 refetchInterval 置 false，停掉所有定时请求，
 * 减轻服务器负荷以及未停留时段的无谓刷新。
 *
 * 实现：模块级 store + useSyncExternalStore。无 Provider 依赖，所有
 * 订阅者共享同一份状态，监听器只注册一次。
 */
const IDLE_MS = 120_000 // 2 分钟无交互视为离开

const isBrowser = typeof window !== 'undefined' && typeof document !== 'undefined'

let active = isBrowser ? !document.hidden && document.hasFocus() : true
let idleTimer: ReturnType<typeof setTimeout> | null = null
const listeners = new Set<() => void>()

function emit() {
  listeners.forEach((l) => l())
}

function setActive(next: boolean) {
  if (active === next) return
  active = next
  emit()
}

function armIdleTimer() {
  if (idleTimer) clearTimeout(idleTimer)
  idleTimer = setTimeout(() => setActive(false), IDLE_MS)
}

function onInteraction() {
  if (!isBrowser) return
  if (!document.hidden && document.hasFocus()) {
    setActive(true)
  }
  armIdleTimer()
}

function onVisibilityChange() {
  if (document.hidden) {
    setActive(false)
    if (idleTimer) {
      clearTimeout(idleTimer)
      idleTimer = null
    }
  } else if (document.hasFocus()) {
    setActive(true)
    armIdleTimer()
  }
}

function onFocus() {
  setActive(true)
  armIdleTimer()
}

function onBlur() {
  setActive(false)
  if (idleTimer) {
    clearTimeout(idleTimer)
    idleTimer = null
  }
}

if (isBrowser) {
  document.addEventListener('visibilitychange', onVisibilityChange)
  window.addEventListener('focus', onFocus)
  window.addEventListener('blur', onBlur)
  const interactionEvents = ['mousemove', 'mousedown', 'keydown', 'scroll', 'touchstart', 'wheel']
  interactionEvents.forEach((ev) =>
    window.addEventListener(ev, onInteraction, { passive: true } as AddEventListenerOptions),
  )
  armIdleTimer()
}

function subscribe(cb: () => void) {
  listeners.add(cb)
  return () => {
    listeners.delete(cb)
  }
}

export function usePageActive(): boolean {
  return useSyncExternalStore(
    subscribe,
    () => active,
    () => true,
  )
}
