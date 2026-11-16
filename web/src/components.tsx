import { useState, type ReactNode } from 'react'
import { ApiError, type AppStatus, type Space } from './api'
import { useCluster } from './cluster'
import { bytes, local, relative } from './format'

/** How a state is coloured: fine, wants a look, wrong, in progress, or at rest. */
export type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'idle'

/** A time shown relative to now, with the local time on hover. */
export function Time({ at, empty = '—' }: { at?: string | null; empty?: string }) {
  if (!at) return <span className="muted">{empty}</span>
  return (
    <time dateTime={at} title={local(at)}>
      {relative(at)}
    </time>
  )
}

/** A state in words, with a dot in its colour (the colour never says it alone). */
export function Badge({ kind, children, title }: { kind: Tone; children: ReactNode; title?: string }) {
  return (
    <span className={`badge tone-${kind}`} title={title}>
      <i className="dot" />
      {children}
    </span>
  )
}

export const STATUS_TONE: Record<AppStatus, Tone> = {
  running: 'ok',
  idle: 'idle',
  suspended: 'warn',
  failed: 'bad',
  deploying: 'info',
}

export function StatusBadge({ status }: { status: AppStatus }) {
  return <Badge kind={STATUS_TONE[status] ?? 'idle'}>{status}</Badge>
}

/** A link to an app's page, with a dot for its status when the list has it. */
export function AppLink({ name }: { name: string }) {
  const status = useCluster().apps.data?.find((a) => a.name === name)?.status
  return (
    <a className="name app-link" href={`#/apps/${encodeURIComponent(name)}`} title={status}>
      {status && <i className={`dot tone-${STATUS_TONE[status] ?? 'idle'}`} />}
      {name}
    </a>
  )
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

/**
 * A share of a whole as a bar. The fill's colour says how it's doing, and the track is a lighter step of the same
 * colour, so the state reads across the whole bar.
 */
export function Meter({
  value,
  max,
  tone = 'accent',
  label,
}: {
  value: number
  max: number
  tone?: 'accent' | 'warn' | 'bad'
  label: string
}) {
  const share = max > 0 ? Math.min(1, Math.max(0, value / max)) : 0
  return (
    <div
      className={`meter meter-${tone}`}
      role="meter"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(share * 100)}
      title={label}
    >
      <div className="meter-fill" style={{ width: `${share * 100}%` }} />
    </div>
  )
}

/**
 * What a subvolume takes. With `max` (the largest in its table), a bar compares it with the others: the solid part
 * is its own, the lighter part what it shares with snapshots.
 */
export function SpaceText({ space, max }: { space?: Space; max?: number }) {
  if (!space) return <span className="muted">not measured yet</span>
  const title = `${space.bytes} bytes, ${space.exclusive_bytes} of them its own`
  return (
    <span className="space" title={title}>
      <span className="space-text">
        {bytes(space.bytes)} <span className="muted">· {bytes(space.exclusive_bytes)} its own</span>
      </span>
      {max !== undefined && max > 0 && (
        <span className="space-bar" aria-hidden="true">
          {space.exclusive_bytes > 0 && (
            <i className="space-own" style={{ width: `${(space.exclusive_bytes / max) * 100}%` }} />
          )}
          {space.bytes > space.exclusive_bytes && (
            <i className="space-shared" style={{ width: `${((space.bytes - space.exclusive_bytes) / max) * 100}%` }} />
          )}
        </span>
      )}
    </span>
  )
}

/** An error from the server, with its problems if it had any. */
export function ErrorBox({ error, prefix }: { error?: Error | null; prefix?: string }) {
  if (!error) return null
  const problems = error instanceof ApiError ? error.problems : []
  return (
    <div className="callout tone-bad" role="alert">
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
  small,
  disabled,
  title,
}: {
  label: string
  busyLabel?: string
  run: () => Promise<unknown>
  onDone?: (result: unknown) => void
  onError?: (e: Error) => void
  kind?: 'primary' | 'secondary' | 'ghost'
  small?: boolean
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
      <button
        className={`btn btn-${kind}${small ? ' btn-sm' : ''}`}
        onClick={click}
        disabled={busy || disabled}
        title={title}
      >
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

/** A view's heading: what it is, a line about it, and its actions. */
export function PageHead({ title, sub, actions }: { title: ReactNode; sub?: ReactNode; actions?: ReactNode }) {
  return (
    <header className="page-head">
      <div className="page-title">
        <h1>{title}</h1>
        {sub && <p className="page-sub">{sub}</p>}
      </div>
      {actions && <div className="page-actions">{actions}</div>}
    </header>
  )
}

/** A titled box. `flush` drops the padding, for a table that reaches the edges. */
export function Panel({
  title,
  note,
  actions,
  flush,
  children,
}: {
  title?: ReactNode
  note?: ReactNode
  actions?: ReactNode
  flush?: boolean
  children: ReactNode
}) {
  return (
    <section className="panel">
      {(title || actions) && (
        <header className="panel-head">
          <h2>{title}</h2>
          {note && <span className="panel-note">{note}</span>}
          {actions && <div className="panel-actions">{actions}</div>}
        </header>
      )}
      <div className={flush ? 'panel-body flush' : 'panel-body'}>{children}</div>
    </section>
  )
}

/** What a list says when it has nothing to list. */
export function Empty({ children }: { children: ReactNode }) {
  return <p className="empty">{children}</p>
}

/** The state of a polled view: loading the first time, or a failed refresh (keeping what was shown). */
export function PollState({ loading, error, hasData }: { loading: boolean; error?: Error; hasData: boolean }) {
  if (error) return <ErrorBox error={error} prefix={hasData ? "Couldn't refresh" : undefined} />
  if (loading && !hasData) return <p className="empty">Loading…</p>
  return null
}
