import { useState } from 'react'
import { api, type BackupResult } from '../api'
import { Action, Badge, ErrorBox, PollState, Section, SpaceText, Time } from '../components'
import { usePoll } from '../hooks'
import { BackupResultView } from './shared'

export function Volumes() {
  const volumes = usePoll(api.volumes, 5000)
  const apps = usePoll(api.apps, 5000)
  const [backup, setBackup] = useState<{ name: string; result: BackupResult }>()
  const [actionError, setActionError] = useState<{ name: string; error: Error }>()

  const appVolumes = (apps.data ?? [])
    .filter((a) => a.space && Object.keys(a.space).length > 0)
    .sort((a, b) => a.name.localeCompare(b.name))

  return (
    <>
      <Section title="Shared volumes">
        <PollState loading={volumes.loading} error={volumes.error} hasData={!!volumes.data} />
        {backup && (
          <div>
            <p>
              Backup of volume <strong>{backup.name}</strong>:
            </p>
            <BackupResultView result={backup.result} onClose={() => setBackup(undefined)} />
          </div>
        )}
        {actionError && <ErrorBox error={actionError.error} prefix={actionError.name} />}
        {volumes.data && volumes.data.length === 0 && <p className="muted">No shared volumes.</p>}
        {volumes.data && volumes.data.length > 0 && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Volume</th>
                  <th>Node / nest</th>
                  <th>Data</th>
                  <th>Mounted by</th>
                  <th>Snapshots</th>
                  <th>Created / loaded</th>
                  <th>Actions</th>
                </tr>
              </thead>
              <tbody>
                {volumes.data.map((v) => (
                  <tr key={v.name}>
                    <td>
                      <strong>{v.name}</strong>
                      {v.pending !== undefined && <div className="small muted">generation {v.pending} pending</div>}
                      {v.problem && <div className="small bad">{v.problem}</div>}
                    </td>
                    <td>
                      {v.node ?? '—'}
                      <div className="small muted">nest {v.nest ?? '—'}</div>
                    </td>
                    <td className="small">
                      <SpaceText space={v.space} />
                    </td>
                    <td className="small">
                      {v.mounts.length ? (
                        v.mounts.map((m) => (
                          <div key={m.app}>
                            <a href={`#/apps/${encodeURIComponent(m.app)}`}>{m.app}</a> <Badge kind="idle">{m.mode}</Badge>
                          </div>
                        ))
                      ) : (
                        <span className="muted">nobody</span>
                      )}
                    </td>
                    <td className="small">
                      {v.snapshots.length ? (
                        v.snapshots.map((app) => (
                          <div key={app}>
                            {app}: <SpaceText space={v.snapshot_space?.[app]} />
                          </div>
                        ))
                      ) : (
                        <span className="muted">none</span>
                      )}
                    </td>
                    <td className="small">
                      <Time at={v.created_at} />
                      {v.loaded_at && (
                        <div>
                          loaded <Time at={v.loaded_at} />
                        </div>
                      )}
                    </td>
                    <td className="row-actions">
                      <Action
                        label="Back up now"
                        busyLabel="Backing up…"
                        run={() => api.backupVolume(v.name)}
                        onDone={(r) => setBackup({ name: v.name, result: r as BackupResult })}
                        onError={(error) => setActionError({ name: v.name, error })}
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Section>

      <Section title="App volumes">
        <PollState loading={apps.loading} error={apps.error} hasData={!!apps.data} />
        {apps.data && appVolumes.length === 0 && (
          <p className="muted">No app volumes measured yet (nodes measure them every few minutes).</p>
        )}
        {appVolumes.length > 0 && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>App</th>
                  <th>Node / nest</th>
                  <th>Volume</th>
                  <th>Space</th>
                </tr>
              </thead>
              <tbody>
                {appVolumes.flatMap((a) =>
                  Object.entries(a.space ?? {})
                    .sort(([x], [y]) => x.localeCompare(y))
                    .map(([vol, space], i) => (
                      <tr key={`${a.name}/${vol}`}>
                        <td>{i === 0 && <a href={`#/apps/${encodeURIComponent(a.name)}`}>{a.name}</a>}</td>
                        <td className="small">
                          {i === 0 && (
                            <>
                              {a.node ?? '—'} <span className="muted">nest {a.nest ?? '—'}</span>
                            </>
                          )}
                        </td>
                        <td>{vol}</td>
                        <td className="small">
                          <SpaceText space={space} />
                        </td>
                      </tr>
                    )),
                )}
              </tbody>
            </table>
          </div>
        )}
      </Section>
    </>
  )
}
