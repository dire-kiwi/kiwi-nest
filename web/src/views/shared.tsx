import type { ReactNode } from 'react'
import type { BackupResult, NodeInfo } from '../api'
import { appUrl, lowSpace } from '../cluster'
import { Meter, Time } from '../components'
import { bytes, duration, percent } from '../format'
import { IconExternal } from '../icons'

/** An app's last activity through the gateway. */
export function LastActivity({ open, at }: { open: number; at?: string }) {
  if (open > 0) {
    return (
      <span className="live">
        <i className="dot tone-ok pulse" />
        active now <span className="muted">· {open} open</span>
      </span>
    )
  }
  return <Time at={at} empty="none since the node started" />
}

export function IdleTimeout({ secs }: { secs: number }) {
  return secs === 0 ? <span>always on</span> : <span>idles after {duration(secs)}</span>
}

/** An app's domain, as a link to it through the ingress, and whether only the LAN gets to it. */
export function Domain({ domain, lan }: { domain: string; lan?: boolean }) {
  return (
    <span className="domain">
      <a href={appUrl(domain)} target="_blank" rel="noreferrer" title="Open the app (which wakes it if it's idle)">
        {domain}
        <IconExternal />
      </a>
      {lan && (
        <span className="tag" title="The ingress serves it only to clients on the cluster's LAN">
          LAN only
        </span>
      )}
    </span>
  )
}

/** A measure with its bar: what it is, the numbers, and a line under it. */
function Gauge({ label, value, meter, hint }: { label: string; value: ReactNode; meter: ReactNode; hint?: ReactNode }) {
  return (
    <div className="gauge">
      <div className="gauge-head">
        <span className="gauge-label">{label}</span>
        <span className="gauge-value">{value}</span>
      </div>
      {meter}
      {hint && <div className="gauge-hint">{hint}</div>}
    </div>
  )
}

/** A node's memory: its apps' limits against what it has. `full` adds the headroom placement counts. */
export function MemoryGauge({ node: n, full }: { node: NodeInfo; full?: boolean }) {
  const f = n.facts
  if (!f) return <Gauge label="Memory" value={<span className="muted">not reported</span>} meter={null} />
  const over = n.headroom_bytes < 0
  return (
    <Gauge
      label="Memory"
      value={
        <>
          {bytes(n.app_memory_bytes)} <span className="muted">of {bytes(f.memory_bytes)}</span>
        </>
      }
      meter={
        <Meter
          value={n.app_memory_bytes}
          max={f.memory_bytes}
          tone={over ? 'bad' : 'accent'}
          label={`Apps' memory limits: ${bytes(n.app_memory_bytes)} of ${bytes(f.memory_bytes)}`}
        />
      }
      hint={
        full && (
          <>
            limits of {n.apps} app{n.apps === 1 ? '' : 's'} ·{' '}
            {over ? (
              <span className="bad">overcommitted by {bytes(-n.headroom_bytes)}</span>
            ) : (
              <>headroom {bytes(n.headroom_bytes)}</>
            )}{' '}
            · {bytes(f.memory_available_bytes)} available now
          </>
        )
      }
    />
  )
}

/** A node's btrfs filesystem: what's used of it. `full` adds what's free and unallocated, and the warning. */
export function DiskGauge({ node: n, full }: { node: NodeInfo; full?: boolean }) {
  const v = n.facts?.volumes
  if (!v) return <Gauge label="Disk" value={<span className="muted">not reported</span>} meter={null} />
  const low = lowSpace(n)
  const used = Math.max(0, v.total_bytes - v.free_bytes)
  const tone =
    low.free || low.unallocated ? 'bad' : v.total_bytes > 0 && v.free_bytes / v.total_bytes < 0.25 ? 'warn' : 'accent'
  return (
    <Gauge
      label="Disk"
      value={
        <>
          {bytes(used)} <span className="muted">of {bytes(v.total_bytes)}</span>
        </>
      }
      meter={
        <Meter
          value={used}
          max={v.total_bytes}
          tone={tone}
          label={`btrfs: ${bytes(used)} used of ${bytes(v.total_bytes)}`}
        />
      }
      hint={
        full && (
          <>
            <span className={low.free ? 'bad' : undefined}>
              {bytes(v.free_bytes)} free ({percent(v.free_bytes, v.total_bytes)})
            </span>{' '}
            ·{' '}
            <span className={low.unallocated ? 'bad' : undefined}>
              {v.unallocated_bytes === undefined
                ? 'unallocated not reported'
                : `${bytes(v.unallocated_bytes)} unallocated (${percent(v.unallocated_bytes, v.total_bytes)})`}
            </span>
            {(low.free || low.unallocated) && (
              <div className="callout tone-warn">Low on space: a full btrfs filesystem fails writes unpredictably.</div>
            )}
          </>
        )
      }
    />
  )
}

/** What a backup made, and where it failed. */
export function BackupResultView({
  subject,
  result,
  onClose,
}: {
  subject?: ReactNode
  result: BackupResult
  onClose?: () => void
}) {
  return (
    <div className={`callout ${result.failed.length ? 'tone-warn' : 'tone-ok'}`} role="status">
      {onClose && (
        <button className="callout-close" onClick={onClose} aria-label="Close">
          ×
        </button>
      )}
      {result.made.length > 0 && (
        <>
          <strong>Backed up{subject && <> {subject}</>}:</strong>
          <ul>
            {result.made.map((b) => (
              <li key={`${b.target}:${b.key}`}>
                <code>
                  {b.target}:{b.key}
                </code>{' '}
                {b.size !== undefined && <span className="muted">({bytes(b.size)})</span>}
              </li>
            ))}
          </ul>
        </>
      )}
      {result.failed.length > 0 && (
        <>
          <strong>Failed{subject && result.made.length === 0 && <> to back up {subject}</>}:</strong>
          <ul>
            {result.failed.map((f) => (
              <li key={f.target}>
                {f.target}: {f.error}
              </li>
            ))}
          </ul>
        </>
      )}
      {result.made.length === 0 && result.failed.length === 0 && (
        <span>Nothing{subject && <> of {subject}</>} was backed up.</span>
      )}
      {result.notes && result.notes.length > 0 && (
        <ul className="muted">
          {result.notes.map((n, i) => (
            <li key={i}>{n}</li>
          ))}
        </ul>
      )}
    </div>
  )
}
