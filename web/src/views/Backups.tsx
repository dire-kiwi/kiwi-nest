import { api, ApiError, type BackupInfo } from '../api'
import { Action, PollState, Section, Time } from '../components'
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
    <Section title="Backups" actions={<Action label="Refresh" busyLabel="Listing…" run={backups.refresh} />}>
      {noTargets ? (
        <p className="muted">{backups.error?.message}</p>
      ) : (
        <PollState loading={backups.loading} error={backups.error} hasData={!!backups.data} />
      )}
      {backups.data && backups.data.length === 0 && <p className="muted">The targets hold no backups yet.</p>}
      {[...byTarget.entries()]
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([target, groups]) => (
          <div className="group" key={target}>
            <h3 className="group-head">Target {target}</h3>
            {[...groups.entries()]
              .sort(([a], [b]) => a.localeCompare(b))
              .map(([what, list]) => (
                <div key={what} className="subgroup">
                  <h4>{what}</h4>
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
                        {[...list]
                          .sort((a, b) => b.taken_at.localeCompare(a.taken_at))
                          .map((b) => (
                            <tr key={b.key}>
                              <td>
                                <Time at={b.taken_at} />
                              </td>
                              <td>
                                {describe(b)}
                                {b.parent && (
                                  <div className="small muted">
                                    after <Time at={b.parent} />
                                  </div>
                                )}
                              </td>
                              <td className="num">{b.size === undefined ? '—' : bytes(b.size)}</td>
                              <td>
                                <code className="small">{b.key}</code>
                              </td>
                            </tr>
                          ))}
                      </tbody>
                    </table>
                  </div>
                </div>
              ))}
          </div>
        ))}
    </Section>
  )
}
