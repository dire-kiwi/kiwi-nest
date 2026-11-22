import { useState } from 'react'
import { api, type AppStatus, type AppSummary, type BackupResult, type NodeInfo } from '../api'
import { countByStatus, plural, STATUSES, useCluster } from '../cluster'
import { Action, Badge, Empty, ErrorBox, PageHead, PollState, StatusBadge } from '../components'
import { bytes } from '../format'
import { IconSearch } from '../icons'
import { BackupResultView, Domain, IdleTimeout, LastActivity } from './shared'

const UNPLACED = '\u0000unplaced'

export function Apps() {
  const { apps, nodes } = useCluster()
  const [query, setQuery] = useState('')
  const [status, setStatus] = useState<AppStatus>()
  const [backup, setBackup] = useState<{ app: string; result: BackupResult }>()
  const [actionError, setActionError] = useState<{ app: string; error: Error }>()

  const all = apps.data ?? []
  const counts = countByStatus(all)
  const q = query.trim().toLowerCase()
  const filtering = q !== '' || status !== undefined
  const shown = all.filter(
    (a) =>
      (status === undefined || a.status === status) &&
      (q === '' || [a.name, a.domain, a.nest ?? ''].some((s) => s.toLowerCase().includes(q))),
  )

  const byNode = new Map<string, AppSummary[]>()
  for (const a of shown) {
    const key = a.node ?? UNPLACED
    byNode.set(key, [...(byNode.get(key) ?? []), a])
  }
  const nodeInfo = new Map<string, NodeInfo>((nodes.data ?? []).map((n) => [n.name, n]))
  // Every node gets a section, even without apps, so it's clear where nothing runs (unless the list is filtered).
  if (!filtering) for (const n of nodes.data ?? []) if (!byNode.has(n.name)) byNode.set(n.name, [])
  const groups = [...byNode.entries()].sort(([a], [b]) =>
    a === UNPLACED ? 1 : b === UNPLACED ? -1 : a.localeCompare(b),
  )

  const done = () => void apps.refresh()
  const failed = (app: string) => (error: Error) => setActionError({ app, error })

  return (
    <>
      <PageHead title="Apps" sub={apps.data ? `${plural(all.length, 'app')}, by node` : undefined} />
      <div className="toolbar">
        <label className="search">
          <IconSearch />
          <input
            type="search"
            placeholder="Filter by name, domain or nest"
            aria-label="Filter apps"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </label>
        <div className="filters" role="group" aria-label="Status">
          <button className="filter" aria-pressed={status === undefined} onClick={() => setStatus(undefined)}>
            All <span className="filter-count">{all.length}</span>
          </button>
          {STATUSES.filter((s) => counts[s] > 0 || s === status).map((s) => (
            <button
              key={s}
              className="filter"
              aria-pressed={status === s}
              onClick={() => setStatus(status === s ? undefined : s)}
            >
              {s} <span className="filter-count">{counts[s]}</span>
            </button>
          ))}
        </div>
      </div>

      <PollState loading={apps.loading} error={apps.error} hasData={!!apps.data} />
      {backup && (
        <BackupResultView
          subject={<strong>{backup.app}</strong>}
          result={backup.result}
          onClose={() => setBackup(undefined)}
        />
      )}
      {actionError && <ErrorBox error={actionError.error} prefix={actionError.app} />}
      {apps.data && all.length === 0 && <Empty>No apps are deployed.</Empty>}
      {filtering && all.length > 0 && shown.length === 0 && <Empty>No apps match.</Empty>}

      {groups.map(([node, list]) => {
        const info = nodeInfo.get(node)
        return (
          <section className="panel" key={node} data-node-group={node === UNPLACED ? '' : node}>
            <header className="panel-head">
              <h2>
                {node === UNPLACED ? (
                  'Not on a node'
                ) : (
                  <>
                    <span className="muted">Node</span> <span className="name">{node}</span>
                  </>
                )}
              </h2>
              <span className="badges">
                {info && !info.online && <Badge kind="bad">offline</Badge>}
                {info?.cordoned && <Badge kind="warn">cordoned</Badge>}
              </span>
              <span className="panel-note">
                {plural(list.length, 'app')}
                {info && !filtering && <> · {bytes(info.app_memory_bytes)} of memory limits</>}
              </span>
            </header>
            {list.length === 0 ? (
              <Empty>No apps.</Empty>
            ) : (
              <div className="table-wrap">
                <table className="apps-table">
                  <thead>
                    <tr>
                      <th>App</th>
                      <th>Status</th>
                      <th>Nest</th>
                      <th>Last activity</th>
                      <th className="col-actions">Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    {list
                      .sort((a, b) => (a.nest ?? '').localeCompare(b.nest ?? '') || a.name.localeCompare(b.name))
                      .map((a) => (
                        <tr key={a.name} data-app={a.name}>
                          <td>
                            <a className="name strong" href={`#/apps/${encodeURIComponent(a.name)}`}>
                              {a.name}
                            </a>
                            <div className="sub">
                              <Domain domain={a.domain} lan={a.access === 'lan'} />
                            </div>
                          </td>
                          <td>
                            <StatusBadge status={a.status} />
                            <div className="sub">
                              {a.containers} · generation {a.generation}
                              {a.pending !== undefined && <> · {a.pending} pending</>}
                            </div>
                            {a.error && <div className="sub bad">{a.error}</div>}
                          </td>
                          <td>
                            {a.nest ? (
                              <a className="name" href="#/nests">
                                {a.nest}
                              </a>
                            ) : (
                              '—'
                            )}
                          </td>
                          <td>
                            <LastActivity open={a.open_requests} at={a.last_request_at} />
                            <div className="sub">
                              <IdleTimeout secs={a.idle_timeout_secs} />
                            </div>
                          </td>
                          <td className="col-actions">
                            <span className="row-actions">
                              {a.status !== 'running' && a.status !== 'deploying' ? (
                                <Action
                                  label="Resume"
                                  busyLabel="Waking…"
                                  // An idle app wakes on its next request anyway; the others stay down until resumed.
                                  kind={a.status === 'idle' ? 'secondary' : 'primary'}
                                  small
                                  run={() => api.resume(a.name)}
                                  onDone={done}
                                  onError={failed(a.name)}
                                />
                              ) : null}
                              {a.status === 'running' || a.status === 'idle' ? (
                                <Action
                                  label="Suspend"
                                  small
                                  run={() => api.suspend(a.name)}
                                  onDone={done}
                                  onError={failed(a.name)}
                                />
                              ) : null}
                              <a
                                className="btn btn-secondary btn-sm"
                                href={`#/apps/${encodeURIComponent(a.name)}/logs`}
                              >
                                Logs
                              </a>
                              <Action
                                label="Back up"
                                busyLabel="Backing up…"
                                small
                                title="Back up now, to every target"
                                run={() => api.backupApp(a.name)}
                                onDone={(r) => setBackup({ app: a.name, result: r as BackupResult })}
                                onError={failed(a.name)}
                              />
                            </span>
                          </td>
                        </tr>
                      ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>
        )
      })}
    </>
  )
}
