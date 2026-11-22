import { api, ApiError, type BackupInfo } from '../api'
import { plural } from '../cluster'
import { Action, Empty, PageHead, PollState, Time } from '../components'
import { bytes } from '../format'
import { usePoll } from '../hooks'

function describe(b: BackupInfo): string {
  if (b.format === 'tar') return 'tar'
  return b.parent ? 'incremental' : 'full'
}

export function Backups() {
  // Listing reads every target, so it's only done on request.
  const backups = usePoll(api.backups)
  // A server without targets answers 400, which is a fact to show rather than a failure.
  const noTargets = backups.error instanceof ApiError && backups.error.status === 400

  const byTarget = new Map<string, Map<string, BackupInfo[]>>()
  for (const b of backups.data ?? []) {
    const t = byTarget.get(b.target) ?? new Map<string, BackupInfo[]>()
    const what = b.kind === 'server' ? 'server' : `${b.kind} ${b.name}`
    t.set(what, [...(t.get(what) ?? []), b])
    byTarget.set(b.target, t)
  }

  return (
    <>
      <PageHead
        title="Backups"
        sub="What the targets hold. Listing reads every target, so it's done when asked."
        actions={<Action label="Refresh" busyLabel="Listing…" run={backups.refresh} />}
      />
      {noTargets ? (
        <Empty>{backups.error?.message}</Empty>
      ) : (
        <PollState loading={backups.loading} error={backups.error} hasData={!!backups.data} />
      )}
      {backups.data && backups.data.length === 0 && <Empty>The targets hold no backups yet.</Empty>}
      {[...byTarget.entries()]
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([target, groups]) => {
          const all = [...groups.values()].flat()
          const size = all.reduce((sum, b) => sum + (b.size ?? 0), 0)
          return (
            <section className="panel" key={target} data-target={target}>
              <header className="panel-head">
                <h2>
                  <span className="muted">Target</span> <span className="name">{target}</span>
                </h2>
                <span className="panel-note">
                  {plural(all.length, 'backup')} of {plural(groups.size, 'thing')} · {bytes(size)}
                </span>
              </header>
              {[...groups.entries()]
                .sort(([a], [b]) => a.localeCompare(b))
                .map(([what, list]) => (
                  <Subject key={what} what={what} list={list} />
                ))}
            </section>
          )
        })}
    </>
  )
}

/** One app's, volume's or the server's backups in a target: a line to open, with the backups under it. */
function Subject({ what, list }: { what: string; list: BackupInfo[] }) {
  const sorted = [...list].sort((a, b) => b.taken_at.localeCompare(a.taken_at))
  const [kind, ...name] = what.split(' ')
  const size = list.reduce((sum, b) => sum + (b.size ?? 0), 0)
  return (
    <details className="subject">
      <summary>
        <span className="tag">{kind}</span>
        <span className="name strong">{name.join(' ')}</span>
        <span className="subject-facts">
          <span>
            latest <Time at={sorted[0].taken_at} />
          </span>
          <span>{plural(list.length, 'backup')}</span>
          <span className="num">{bytes(size)}</span>
        </span>
      </summary>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Taken</th>
              <th>Kind</th>
              <th className="num">Size</th>
              <th>Key</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((b) => (
              <tr key={b.key}>
                <td>
                  <Time at={b.taken_at} />
                </td>
                <td>
                  {describe(b)}
                  {b.parent && (
                    <span className="muted small">
                      {' '}
                      · after <Time at={b.parent} />
                    </span>
                  )}
                </td>
                <td className="num">{b.size === undefined ? '—' : bytes(b.size)}</td>
                <td>
                  <code>{b.key}</code>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </details>
  )
}
