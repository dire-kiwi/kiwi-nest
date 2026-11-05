import type { BackupResult } from '../api'
import { Time } from '../components'
import { bytes, duration } from '../format'

/** An app's last activity through the gateway. */
export function LastActivity({ open, at }: { open: number; at?: string }) {
  if (open > 0) return <span className="ok">active now ({open} open)</span>
  return <Time at={at} empty="none since the node started" />
}

export function IdleTimeout({ secs }: { secs: number }) {
  return secs === 0 ? <span>always on</span> : <span>idles after {duration(secs)}</span>
}

/** What a backup made, and where it failed. */
export function BackupResultView({ result, onClose }: { result: BackupResult; onClose?: () => void }) {
  return (
    <div className={`notice ${result.failed.length ? 'notice-bad' : 'notice-ok'}`} role="status">
      {onClose && (
        <button className="notice-close" onClick={onClose} aria-label="Close">
          ×
        </button>
      )}
      {result.made.length > 0 && (
        <>
          <strong>Backed up:</strong>
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
          <strong>Failed:</strong>
          <ul>
            {result.failed.map((f) => (
              <li key={f.target}>
                {f.target}: {f.error}
              </li>
            ))}
          </ul>
        </>
      )}
      {result.made.length === 0 && result.failed.length === 0 && <span>Nothing was backed up.</span>}
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

