// Formatting sizes and times.

const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB']

/** Bytes in binary units: `1.5 GiB`. Negative numbers keep their sign. */
export function bytes(n: number | undefined | null): string {
  if (n === undefined || n === null) return '—'
  const sign = n < 0 ? '-' : ''
  let v = Math.abs(n)
  let i = 0
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024
    i++
  }
  const digits = i === 0 || v >= 100 ? 0 : v >= 10 ? 1 : 2
  return `${sign}${v.toFixed(digits)} ${UNITS[i]}`
}

/** A share of a total, as a percentage. */
export function percent(part: number, total: number): string {
  if (!total) return '—'
  return `${Math.round((part / total) * 100)}%`
}

/** How long ago (or how far ahead) an RFC 3339 time is: `3 min ago`, `in 2 h`. */
export function relative(rfc3339: string | undefined | null, now = Date.now()): string {
  if (!rfc3339) return '—'
  const t = Date.parse(rfc3339)
  if (Number.isNaN(t)) return rfc3339
  const secs = Math.round((now - t) / 1000)
  const future = secs < 0
  const s = Math.abs(secs)
  let text: string
  if (s < 10) return 'just now'
  else if (s < 60) text = `${s} s`
  else if (s < 3600) text = `${Math.floor(s / 60)} min`
  else if (s < 86400) text = `${Math.floor(s / 3600)} h`
  else text = `${Math.floor(s / 86400)} d`
  return future ? `in ${text}` : `${text} ago`
}

/** An RFC 3339 time in the browser's local time. */
export function local(rfc3339: string | undefined | null): string {
  if (!rfc3339) return ''
  const t = Date.parse(rfc3339)
  return Number.isNaN(t) ? rfc3339 : new Date(t).toLocaleString()
}

/** A duration in seconds: `10 min`, `1 h 30 min`. */
export function duration(secs: number): string {
  if (secs < 60) return `${secs} s`
  const h = Math.floor(secs / 3600)
  const m = Math.floor((secs % 3600) / 60)
  const s = secs % 60
  const parts: string[] = []
  if (h) parts.push(`${h} h`)
  if (m) parts.push(`${m} min`)
  if (s && !h) parts.push(`${s} s`)
  return parts.join(' ')
}
