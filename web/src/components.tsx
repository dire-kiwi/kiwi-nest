import { useState, type ReactNode } from 'react'
import { ApiError, type AppStatus, type Space } from './api'
import { bytes, local, relative } from './format'

/** A time shown relative to now, with the local time on hover. */
export function Time({ at, empty = '—' }: { at?: string | null; empty?: string }) {
  if (!at) return <span className="muted">{empty}</span>
  return (
    <time dateTime={at} title={local(at)}>
      {relative(at)}
    </time>
  )
}

export function Badge({ kind, children, title }: { kind: string; children: ReactNode; title?: string }) {
  return (
    <span className={`badge badge-${kind}`} title={title}>
      {children}
    </span>
  )
}

const STATUS_KIND: Record<AppStatus, string> = {
  running: 'ok',
  idle: 'idle',
  suspended: 'warn',
  failed: 'bad',
  deploying: 'info',
}

export function StatusBadge({ status }: { status: AppStatus }) {
  return <Badge kind={STATUS_KIND[status] ?? 'idle'}>{status}</Badge>
}

export function Labels({ labels }: { labels?: Record<string, string> }) {
  const entries = Object.entries(labels ?? {})
  if (entries.length === 0) return <span className="muted">none</span>
  return (
    <span className="chips">
      {entries.map(([k, v]) => (
        <span className="chip" key={k}>
          {k}={v}
        </span>
      ))}
    </span>
  )
}

export function SpaceText({ space }: { space?: Space }) {
  if (!space) return <span className="muted">not measured yet</span>
  return (
    <span title={`${space.bytes} bytes, ${space.exclusive_bytes} of them its own`}>
      {bytes(space.bytes)} <span className="muted">({bytes(space.exclusive_bytes)} exclusive)</span>
    </span>
  )
}

/** An error from the server, with its problems if it had any. */
export function ErrorBox({ error, prefix }: { error?: Error | null; prefix?: string }) {
  if (!error) return null
  const problems = error instanceof ApiError ? error.problems : []
  return (
    <div className="error" role="alert">
      {prefix ? `${prefix}: ` : ''}
      {error.message}
      {problems.length > 0 && (
        <ul>
          {problems.map((p, i) => (
            <li key={i}>{p}</li>
          ))}
        </ul>
      )}
    </div>
  )
}

/** A button that runs an action, shows that it's busy, and reports a failure. */
export function Action({
  label,
  busyLabel,
  run,
  onDone,
  onError,
  kind = 'secondary',
  disabled,
  title,
}: {
  label: string
  busyLabel?: string
  run: () => Promise<unknown>
  onDone?: (result: unknown) => void
  onError?: (e: Error) => void
  kind?: 'primary' | 'secondary' | 'danger'
  disabled?: boolean
  title?: string
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<Error>()
  const click = async () => {
    setBusy(true)
    setError(undefined)
    try {
      const r = await run()
      onDone?.(r)
    } catch (e) {
      const err = e as Error
      if (onError) onError(err)
      else setError(err)
    } finally {
      setBusy(false)
    }
  }
  return (
    <span className="action">
      <button className={`btn btn-${kind}`} onClick={click} disabled={busy || disabled} title={title}>
        {busy ? (busyLabel ?? `${label}…`) : label}
      </button>
      {error && (
        <span className="action-error" role="alert" title={error.message}>
          {error.message}
        </span>
      )}
    </span>
  )
}

export function Section({ title, children, actions }: { title: ReactNode; children: ReactNode; actions?: ReactNode }) {
  return (
    <section className="section">
      <div className="section-head">
        <h2>{title}</h2>
        {actions && <div className="section-actions">{actions}</div>}
      </div>
      {children}
    </section>
  )
}

/** The state of a polled view: loading the first time, or a failed refresh (keeping what was shown). */
export function PollState({ loading, error, hasData }: { loading: boolean; error?: Error; hasData: boolean }) {
  if (error) return <ErrorBox error={error} prefix={hasData ? "Couldn't refresh" : undefined} />
  if (loading && !hasData) return <p className="muted">Loading…</p>
  return null
}
