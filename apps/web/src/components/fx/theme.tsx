import { useEffect, useState } from 'react'

/*
 * Dark mode.
 *
 * Every component is written against `:where(.dark, [data-theme="dark"])` on an ancestor, never a
 * media query, which is what makes an override possible at all: the operating system decides
 * until the owner says otherwise (Appearance, in the sidebar's account menu). So one place sets
 * `data-theme` on <html>, and nothing else in the app looks at the colour scheme.
 */

export type Mode = 'system' | 'light' | 'dark'

const KEY = 'fleet.theme'

/** What the owner chose last, or `system`. Storage can throw or be empty; that is not an error. */
function stored(): Mode {
  try {
    const v = localStorage.getItem(KEY)
    return v === 'light' || v === 'dark' || v === 'system' ? v : 'system'
  } catch {
    return 'system'
  }
}

function dark(mode: Mode): boolean {
  if (mode === 'system') {
    return typeof matchMedia === 'function' && matchMedia('(prefers-color-scheme: dark)').matches
  }
  return mode === 'dark'
}

/** Writes the resolved scheme onto <html>, where every component's dark rules look for it. */
function apply(mode: Mode) {
  const root = document.documentElement
  root.dataset.theme = dark(mode) ? 'dark' : 'light'
  root.style.colorScheme = dark(mode) ? 'dark' : 'light'
}

/**
 * Applied before React renders, so the first paint is already in the right scheme instead of
 * flashing white. Called from `main.tsx`.
 */
export function startTheme() {
  apply(stored())
}

export function useTheme() {
  const [mode, setMode] = useState<Mode>(stored)

  useEffect(() => {
    apply(mode)
    try {
      if (mode === 'system') localStorage.removeItem(KEY)
      else localStorage.setItem(KEY, mode)
    } catch {
      // A private window keeps nothing. The choice still holds for this tab.
    }
    if (mode !== 'system' || typeof matchMedia !== 'function') return
    // Following the system means following it as it changes, not only at startup.
    const mq = matchMedia('(prefers-color-scheme: dark)')
    const onChange = () => apply('system')
    mq.addEventListener('change', onChange)
    return () => mq.removeEventListener('change', onChange)
  }, [mode])

  return { mode, setMode }
}
