// The colour theme: the system's unless the reader picked one, which this browser then remembers.
//
// The stylesheet's colours are `light-dark()` pairs, so the theme is only the root's `color-scheme`, which
// `data-theme` on the root element sets (see style.css).

export type Theme = 'auto' | 'light' | 'dark'

const KEY = 'kn-theme'
const ORDER: Theme[] = ['auto', 'light', 'dark']

export function storedTheme(): Theme {
  try {
    const t = window.localStorage.getItem(KEY)
    return t === 'light' || t === 'dark' ? t : 'auto'
  } catch {
    return 'auto'
  }
}

export function applyTheme(theme: Theme) {
  if (theme === 'auto') delete document.documentElement.dataset.theme
  else document.documentElement.dataset.theme = theme
  try {
    if (theme === 'auto') window.localStorage.removeItem(KEY)
    else window.localStorage.setItem(KEY, theme)
  } catch {
    // Without storage, the choice lasts until the page is closed.
  }
}

export function nextTheme(theme: Theme): Theme {
  return ORDER[(ORDER.indexOf(theme) + 1) % ORDER.length]
}
