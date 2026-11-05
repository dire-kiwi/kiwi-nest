import { useState } from 'react'
import { api, type AppSummary, type BackupResult, type NodeInfo } from '../api'
import { Action, Badge, ErrorBox, PollState, Section, StatusBadge } from '../components'
import { go, usePoll } from '../hooks'
import { BackupResultView, IdleTimeout, LastActivity } from './shared'

const UNPLACED = '\u0000unplaced'

export function Apps() {
  const apps = usePoll(api.apps, 5000)
  const nodes = usePoll(api.nodes, 5000)
  const [backup, setBackup] = useState<{ app: string; result: BackupResult }>()
  const [actionError, setActionError] = useState<{ app: string; error: Error }>()

  const byNode = new Map<string, AppSummary[]>()
  for (const a of apps.data ?? []) {
    const key = a.node ?? UNPLACED
    byNode.set(key, [...(byNode.get(key) ?? []), a])
  }
  const nodeInfo = new Map<string, NodeInfo>((nodes.data ?? []).map((n) => [n.name, n]))
  // Every node gets a section, even without apps, so it's clear where nothing runs.
  for (const n of nodes.data ?? []) if (!byNode.has(n.name)) byNode.set(n.name, [])
  const groups = [...byNode.entries()].sort(([a], [b]) =>
    a === UNPLACED ? 1 : b === UNPLACED ? -1 : a.localeCompare(b),
  )

  const done = () => void apps.refresh()
  const failed = (app: string) => (error: Error) => setActionError({ app, error })

  return (
    <Section title="Apps">
      <PollState loading={apps.loading} error={apps.error} hasData={!!apps.data} />
      {backup && (
        <div>
          <p>
            Backup of <strong>{backup.app}</strong>:
          </p>
          <BackupResultView result={backup.result} onClose={() => setBackup(undefined)} />
        </div>
      )}
      {actionError && <ErrorBox error={actionError.error} prefix={actionError.app} />}
      {apps.data && apps.data.length === 0 && <p className="muted">No apps are deployed.</p>}
      {groups.map(([node, list]) => {
        const info = nodeInfo.get(node)
        return (
          <div className="group" key={node}>
            <h3 className="group-head">
              {node === UNPLACED ? 'Not on a node' : `Node ${node}`}{' '}
              {info && !info.online && <Badge kind="bad">offline</Badge>}
              {info?.cordoned && <Badge kind="warn">CORDONED</Badge>}
            </h3>
            {list.length === 0 ? (
              <p className="muted">No apps.</p>
            ) : (
              <div className="table-wrap">
                <table>
                  <thead>
                    <tr>
                      <th>App</th>
                      <th>Status</th>
                      <th>Containers</th>
                      <th>Domain</th>
                      <th>Nest</th>
                      <th>Idle timeout</th>
                      <th>Last activity</th>
                      <th>Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    {list
                      .sort((a, b) => a.name.localeCompare(b.name))
                      .map((a) => (
                        <tr key={a.name} data-app={a.name}>
                          <td>
                            <a href={`#/apps/${encodeURIComponent(a.name)}`}>
                              <strong>{a.name}</strong>
                            </a>
                            <div className="small muted">
                              generation {a.generation}
                              {a.pending !== undefined && <> · {a.pending} pending</>}
                            </div>
                          </td>
                          <td>
                            <StatusBadge status={a.status} />
                            {a.error && (
                              <div className="small bad" title={a.error}>
                                {a.error}
                              </div>
                            )}
                          </td>
                          <td className="small">{a.containers}</td>
                          <td className="small">{a.domain}</td>
                          <td>{a.nest ?? '—'}</td>
                          <td className="small">
                            <IdleTimeout secs={a.idle_timeout_secs} />
                          </td>
                          <td className="small">
                            <LastActivity open={a.open_requests} at={a.last_request_at} />
                          </td>
                          <td className="row-actions">
                            {a.status === 'running' || a.status === 'idle' ? (
                              <Action
                                label="Suspend"
                                run={() => api.suspend(a.name)}
                                onDone={done}
                                onError={failed(a.name)}
                              />
                            ) : null}
                            {a.status !== 'running' && a.status !== 'deploying' ? (
                              <Action
                                label="Resume"
                                busyLabel="Waking…"
                                kind="primary"
                                run={() => api.resume(a.name)}
                                onDone={done}
                                onError={failed(a.name)}
                              />
                            ) : null}
                            <button className="btn btn-secondary" onClick={() => go(`/apps/${a.name}/logs`)}>
                              Logs
                            </button>
                            <Action
                              label="Back up now"
                              busyLabel="Backing up…"
                              run={() => api.backupApp(a.name)}
                              onDone={(r) => setBackup({ app: a.name, result: r as BackupResult })}
                              onError={failed(a.name)}
                            />
                          </td>
                        </tr>
                      ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        )
      })}
    </Section>
  )
}
