import { useCallback, useEffect, useRef, useState } from 'react'

export interface Polled<T> {
  data: T | undefined
  error: Error | undefined
  loading: boolean
  /** Fetches again now. */
  refresh: () => Promise<void>
}

/**
 * Fetches with `fetcher`, and again every `intervalMs` (if given) while the page is visible. A failed fetch keeps the
 * last good data and sets `error`.
 */
export function usePoll<T>(fetcher: () => Promise<T>, intervalMs?: number, deps: unknown[] = []): Polled<T> {
  const [data, setData] = useState<T>()
  const [error, setError] = useState<Error>()
  const [loading, setLoading] = useState(true)
  const latest = useRef(fetcher)
  latest.current = fetcher
  const generation = useRef(0)

  const refresh = useCallback(async () => {
    const mine = ++generation.current
    setLoading(true)
    try {
      const d = await latest.current()
      if (mine === generation.current) {
        setData(d)
        setError(undefined)
      }
    } catch (e) {
      if (mine === generation.current) setError(e as Error)
    } finally {
      if (mine === generation.current) setLoading(false)
    }
  }, [])

  useEffect(() => {
    setData(undefined)
    setError(undefined)
    void refresh()
    if (!intervalMs) return
    const tick = () => {
      if (document.visibilityState === 'visible') void refresh()
    }
    const id = window.setInterval(tick, intervalMs)
    const onVisible = () => {
      if (document.visibilityState === 'visible') void refresh()
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      window.clearInterval(id)
      document.removeEventListener('visibilitychange', onVisible)
      generation.current++
    }
  }, [intervalMs, refresh, ...deps])

  return { data, error, loading, refresh }
}

/** The page's hash route, without the `#`: `/apps/blog`. */
export function useRoute(): string {
  const read = () => decodeURI(window.location.hash.replace(/^#/, '')) || '/apps'
  const [route, setRoute] = useState(read)
  useEffect(() => {
    const on = () => setRoute(read())
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return route
}

export function go(route: string) {
  window.location.hash = route
}
