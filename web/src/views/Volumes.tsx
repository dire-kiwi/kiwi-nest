import { useState } from 'react'
import { api, type BackupResult } from '../api'
import { plural, useCluster } from '../cluster'
import { Action, AppLink, Empty, ErrorBox, PageHead, Panel, PollState, SpaceText, Time } from '../components'
import { BackupResultView } from './shared'

/** What the bars in the space columns mean. */
const SPACE_KEY = (
  <span className="space-key">
    <i className="space-own" /> its own <i className="space-shared" /> shared with snapshots
  </span>
)

export function Volumes() {
  const { volumes, apps } = useCluster()
  const [backup, setBackup] = useState<{ name: string; result: BackupResult }>()
  const [actionError, setActionError] = useState<{ name: string; error: Error }>()

  const shared = volumes.data ?? []
  const appVolumes = (apps.data ?? [])
    .filter((a) => a.space && Object.keys(a.space).length > 0)
    .sort((a, b) => a.name.localeCompare(b.name))
  // The bars in a table are drawn against its largest volume, so they compare.
  const largestShared = Math.max(
    0,
    ...shared.flatMap((v) => [v.space?.bytes ?? 0, ...Object.values(v.snapshot_space ?? {}).map((s) => s.bytes)]),
  )
  const largestApp = Math.max(0, ...appVolumes.flatMap((a) => Object.values(a.space ?? {}).map((s) => s.bytes)))

  return (
    <>
      <PageHead
        title="Volumes"
        sub={volumes.data ? `${plural(shared.length, 'shared volume')}, and every app's own volumes` : undefined}
      />
      {backup && (
        <BackupResultView
          subject={
            <>
              volume <strong>{backup.name}</strong>
            </>
          }
          result={backup.result}
          onClose={() => setBackup(undefined)}
        />
      )}
      {actionError && <ErrorBox error={actionError.error} prefix={actionError.name} />}

      <Panel title="Shared volumes" note={shared.length > 0 && SPACE_KEY} flush>
        <PollState loading={volumes.loading} error={volumes.error} hasData={!!volumes.data} />
        {volumes.data && shared.length === 0 && <Empty>No shared volumes.</Empty>}
        {shared.length > 0 && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Volume</th>
                  <th>Nest</th>
                  <th>Data</th>
                  <th>Mounted by</th>
                  <th>Snapshots</th>
                  <th>Created</th>
                  <th className="col-actions">Actions</th>
                </tr>
              </thead>
              <tbody>
                {shared.map((v) => (
                  <tr key={v.name} data-volume={v.name}>
                    <td>
                      <span className="name strong">{v.name}</span>
                      {v.pending !== undefined && <div className="sub">generation {v.pending} pending</div>}
                      {v.problem && <div className="sub bad">{v.problem}</div>}
                    </td>
                    <td>
                      <a className="name" href="#/nests">
                        {v.nest ?? '—'}
                      </a>
                      <div className="sub">node {v.node ?? '—'}</div>
                    </td>
                    <td>
                      <SpaceText space={v.space} max={largestShared} />
                    </td>
                    <td>
                      {v.mounts.length ? (
                        <span className="stack">
                          {v.mounts.map((m) => (
                            <span key={m.app}>
                              <AppLink name={m.app} /> <span className="tag">{m.mode}</span>
                            </span>
                          ))}
                        </span>
                      ) : (
                        <span className="muted">nobody</span>
                      )}
                    </td>
                    <td>
                      {v.snapshots.length ? (
                        <span className="stack">
                          {v.snapshots.map((app) => (
                            <span key={app} className="snapshot">
                              <AppLink name={app} />
                              <SpaceText space={v.snapshot_space?.[app]} max={largestShared} />
                            </span>
                          ))}
                        </span>
                      ) : (
                        <span className="muted">none</span>
                      )}
                    </td>
                    <td>
                      <Time at={v.created_at} />
                      {v.loaded_at && (
                        <div className="sub">
                          loaded <Time at={v.loaded_at} />
                        </div>
                      )}
                    </td>
                    <td className="col-actions">
                      <Action
                        label="Back up"
                        busyLabel="Backing up…"
                        small
                        title="Back up now, to every target"
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
      </Panel>

      <Panel title="App volumes" note={appVolumes.length > 0 && SPACE_KEY} flush>
        <PollState loading={apps.loading} error={apps.error} hasData={!!apps.data} />
        {apps.data && appVolumes.length === 0 && (
          <Empty>No app volumes measured yet (nodes measure them every few minutes).</Empty>
        )}
        {appVolumes.length > 0 && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>App</th>
                  <th>Nest</th>
                  <th>Volume</th>
                  <th>Space</th>
                </tr>
              </thead>
              <tbody>
                {appVolumes.flatMap((a) =>
                  Object.entries(a.space ?? {})
                    .sort(([x], [y]) => x.localeCompare(y))
                    .map(([vol, space], i, all) => (
                      <tr key={`${a.name}/${vol}`} className={i < all.length - 1 ? 'has-more' : undefined}>
                        <td>{i === 0 && <AppLink name={a.name} />}</td>
                        <td>
                          {i === 0 && (
                            <>
                              <a className="name" href="#/nests">
                                {a.nest ?? '—'}
                              </a>{' '}
                              <span className="muted small">node {a.node ?? '—'}</span>
                            </>
                          )}
                        </td>
                        <td className="name">{vol}</td>
                        <td>
                          <SpaceText space={space} max={largestApp} />
                        </td>
                      </tr>
                    )),
                )}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </>
  )
}
