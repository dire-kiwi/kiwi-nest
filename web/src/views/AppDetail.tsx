import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { api, streamLogs, type BackupResult, type CommandInfo } from '../api'
import { useCluster } from '../cluster'
import {
  Action,
  AppLink,
  Badge,
  Empty,
  ErrorBox,
  PageHead,
  Panel,
  PollState,
  SpaceText,
  StatusBadge,
  Time,
} from '../components'
import { usePoll } from '../hooks'
import { BackupResultView, Domain, IdleTimeout, LastActivity } from './shared'

export function AppDetail({ name, tab }: { name: string; tab: 'overview' | 'logs' }) {
  // The list is cheap (the server's own view); inspecting asks the node (and is a command, kept a day), so it's
  // refreshed less often. Every poll pauses while the tab is hidden.
  const { apps } = useCluster()
  const info = usePoll(() => api.inspect(name), 30000, [name])
  const commands = usePoll(() => api.commands(name, 20), 10000, [name])
  const [targets, setTargets] = useState<string[]>([])
  const [target, setTarget] = useState('')
  const [backup, setBackup] = useState<BackupResult>()
  const [actionError, setActionError] = useState<Error>()

  // The targets, from what they hold; without a listing, a backup goes to every target.
  useEffect(() => {
    api
      .backups()
      .then((list) => setTargets([...new Set(list.map((b) => b.target))].sort()))
      .catch(() => setTargets([]))
  }, [])

  const refresh = () => {
    void apps.refresh()
    void info.refresh()
    void commands.refresh()
  }
  const acted = () => {
    setActionError(undefined)
    refresh()
  }
  const failed = (e: Error) => {
    setActionError(e)
    void commands.refresh()
  }
  const s = apps.data?.find((a) => a.name === name)
  const i = info.data
  const status = s?.status ?? i?.status
  const domain = s?.domain ?? i?.domain
  const error = s?.error ?? i?.error
  const pending = s?.pending ?? i?.pending
  const here = `#/apps/${encodeURIComponent(name)}`

  const space = s?.space ?? i?.space ?? {}
  const volumes = [...new Set([...(i?.volumes ?? []), ...Object.keys(space)])].sort()
  const shared = i?.shared ?? []
  const largest = Math.max(0, ...Object.values(space).map((v) => v.bytes))

  return (
    <>
      <nav className="crumbs" aria-label="Breadcrumb">
        <a href="#/apps">Apps</a> <span aria-hidden="true">/</span> <span>{name}</span>
      </nav>
      <PageHead
        title={
          <>
            <span className="name">{name}</span>
            {status && <StatusBadge status={status} />}
            {i?.stale && (
              <Badge kind="bad" title="The node is offline: this is what it last reported">
                stale
              </Badge>
            )}
          </>
        }
        sub={domain && <Domain domain={domain} lan={(s?.access ?? i?.access) === 'lan'} />}
        actions={
          <>
            {status && status !== 'running' && status !== 'deploying' && (
              <Action
                label="Resume"
                busyLabel="Waking…"
                kind="primary"
                run={() => api.resume(name)}
                onDone={acted}
                onError={failed}
              />
            )}
            {(status === 'running' || status === 'idle') && (
              <Action label="Suspend" run={() => api.suspend(name)} onDone={acted} onError={failed} />
            )}
            <span className="joined">
              {targets.length > 1 && (
                <select value={target} onChange={(e) => setTarget(e.target.value)} aria-label="Backup target">
                  <option value="">every target</option>
                  {targets.map((t) => (
                    <option key={t} value={t}>
                      {t}
                    </option>
                  ))}
                </select>
              )}
              <Action
                label="Back up now"
                busyLabel="Backing up…"
                run={() => api.backupApp(name, target || undefined)}
                onDone={(r) => {
                  setActionError(undefined)
                  setBackup(r as BackupResult)
                  void commands.refresh()
                }}
                onError={failed}
              />
            </span>
          </>
        }
      />

      <div className="tabs" role="tablist">
        <a href={here} role="tab" aria-selected={tab === 'overview'}>
          Overview
        </a>
        <a href={`${here}/logs`} role="tab" aria-selected={tab === 'logs'}>
          Logs
        </a>
      </div>

      <PollState loading={apps.loading} error={apps.error} hasData={!!apps.data} />
      {apps.data && !s && !i && !info.loading && <Empty>There's no app {name}.</Empty>}
      <ErrorBox error={actionError} />
      {backup && <BackupResultView result={backup} onClose={() => setBackup(undefined)} />}

      <div className="tab" hidden={tab !== 'overview'}>
        {error && (
          <div className="callout tone-bad" role="alert">
            <strong>Failed:</strong> {error}
          </div>
        )}
        <ErrorBox error={info.error} prefix="Couldn't ask the node" />
        {(s || i) && (
          <dl className="strip">
            <Fact label="Node">
              <a className="name" href="#/nodes">
                {s?.node ?? i?.node ?? '—'}
              </a>
            </Fact>
            <Fact label="Nest">
              <a className="name" href="#/nests">
                {s?.nest ?? i?.nest ?? '—'}
              </a>
            </Fact>
            <Fact label="Last activity">
              <LastActivity
                open={i?.open_requests ?? s?.open_requests ?? 0}
                at={s?.last_request_at ?? i?.last_request_at}
              />
            </Fact>
            <Fact label="Idle timeout">
              <IdleTimeout secs={s?.idle_timeout_secs ?? i?.idle_timeout_secs ?? 0} />
            </Fact>
            <Fact label="Generation">
              {s?.generation ?? i?.generation}
              {pending !== undefined && <span className="muted"> · {pending} saved, not yet applied</span>}
            </Fact>
            <Fact label="Deployed">
              <Time at={s?.updated_at ?? i?.updated_at} />
            </Fact>
          </dl>
        )}

        <div className="columns">
          <Panel title="Containers" flush>
            {!i ? (
              <PollState loading={info.loading} hasData={false} />
            ) : i.containers.length === 0 ? (
              <Empty>None: the app isn't running.</Empty>
            ) : (
              <div className="table-wrap">
                <table>
                  <thead>
                    <tr>
                      <th>Service</th>
                      <th>State</th>
                      <th>Health</th>
                      <th>Status</th>
                    </tr>
                  </thead>
                  <tbody>
                    {i.containers.map((c) => (
                      <tr key={c.name}>
                        <td>
                          <span className="name strong">{c.service}</span>
                          <div className="sub">{c.name}</div>
                        </td>
                        <td>
                          <Badge kind={c.state === 'running' ? 'ok' : c.state === 'exited' ? 'idle' : 'warn'}>
                            {c.state}
                          </Badge>
                        </td>
                        <td>{c.health || <span className="muted">—</span>}</td>
                        <td className="small">{c.status}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </Panel>

          <Panel title="Volumes" flush>
            {volumes.length === 0 && shared.length === 0 ? (
              <Empty>No volumes.</Empty>
            ) : (
              <div className="table-wrap">
                <table>
                  <thead>
                    <tr>
                      <th>Volume</th>
                      <th>Kind</th>
                      <th>Space</th>
                    </tr>
                  </thead>
                  <tbody>
                    {volumes.map((v) => (
                      <tr key={`own-${v}`}>
                        <td className="name">{v}</td>
                        <td>its own</td>
                        <td>
                          <SpaceText space={space[v]} max={largest} />
                        </td>
                      </tr>
                    ))}
                    {shared.map((m) => (
                      <tr key={`shared-${m.name}`}>
                        <td>
                          <a className="name" href="#/volumes">
                            {m.name}
                          </a>
                          <div className="sub">at {m.target}</div>
                        </td>
                        <td>shared, {m.mode}</td>
                        <td className="muted">
                          see <a href="#/volumes">Volumes</a>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </Panel>
        </div>

        {i && (
          <Panel title="Details">
            <dl className="facts">
              <dt>Access</dt>
              <dd>{(s?.access ?? i.access) === 'lan' ? "LAN only (clients on the cluster's LAN)" : 'public'}</dd>
              {i.service && (
                <>
                  <dt>Routed to</dt>
                  <dd>
                    <span className="name">
                      {i.service}:{i.container_port}
                    </span>{' '}
                    <span className="muted">(127.0.0.1:{i.port} on the node)</span>
                  </dd>
                </>
              )}
              {i.placement && (
                <>
                  <dt>Placement</dt>
                  <dd>{i.placement}</dd>
                </>
              )}
              {i.from && (
                <>
                  <dt>From</dt>
                  <dd>
                    <AppLink name={i.from} />
                  </dd>
                </>
              )}
              {i.restored_from && (
                <>
                  <dt>Restored from</dt>
                  <dd>
                    <code>{i.restored_from}</code>
                  </dd>
                </>
              )}
              <dt>Bundle</dt>
              <dd>
                <code>{i.bundle_digest}</code>
              </dd>
              <dt>First deployed</dt>
              <dd>
                <Time at={i.created_at} />
              </dd>
            </dl>
          </Panel>
        )}

        <Panel
          title="Recent commands"
          actions={<Action label="Refresh" kind="ghost" small run={commands.refresh} />}
          flush
        >
          <PollState loading={commands.loading} error={commands.error} hasData={!!commands.data} />
          {commands.data && commands.data.length === 0 && <Empty>No commands yet.</Empty>}
          {commands.data && commands.data.length > 0 && (
            <div className="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>Action</th>
                    <th>State</th>
                    <th>By</th>
                    <th>Node</th>
                    <th>Asked</th>
                    <th>Finished</th>
                  </tr>
                </thead>
                <tbody>
                  {commands.data.map((c) => (
                    <tr key={c.id}>
                      <td>
                        <span className="strong">{c.action}</span>
                        {c.detail && <div className="sub">{c.detail}</div>}
                      </td>
                      <td>
                        <CommandState command={c} />
                        {c.error && <div className="sub bad">{c.error}</div>}
                      </td>
                      <td>{c.by}</td>
                      <td className="name">{c.node}</td>
                      <td>
                        <Time at={c.created_at} />
                      </td>
                      <td>
                        <Time at={c.finished_at} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Panel>
      </div>

      <Logs name={name} shown={tab === 'logs'} onRun={() => void commands.refresh()} />
    </>
  )
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  )
}

function CommandState({ command: c }: { command: CommandInfo }) {
  if (c.state === 'done') return c.ok === false ? <Badge kind="bad">failed</Badge> : <Badge kind="ok">done</Badge>
  return (
    <Badge kind={c.state === 'lost' ? 'bad' : c.state === 'cancelled' ? 'idle' : 'info'}>
      {c.state === 'delivered' ? 'running' : c.state}
    </Badge>
  )
}

/** The cap on what the panel keeps, so following a chatty app doesn't grow the page for ever. */
const MAX_LOG_CHARS = 2_000_000

/**
 * The Logs tab. It stays mounted while the other tab shows, so a followed log keeps coming, and it loads the first
 * time it's shown.
 */
function Logs({ name, shown, onRun }: { name: string; shown: boolean; onRun: () => void }) {
  const [tail, setTail] = useState(200)
  const [timestamps, setTimestamps] = useState(false)
  const [follow, setFollow] = useState(false)
  const [text, setText] = useState('')
  const [running, setRunning] = useState(false)
  const [error, setError] = useState<Error>()
  const abort = useRef<AbortController | null>(null)
  const pre = useRef<HTMLPreElement>(null)
  const stick = useRef(true)
  const loaded = useRef(false)

  const stop = () => {
    abort.current?.abort()
    abort.current = null
  }

  const load = async () => {
    stop()
    const ctl = new AbortController()
    abort.current = ctl
    loaded.current = true
    setText('')
    setError(undefined)
    setRunning(true)
    stick.current = true
    try {
      await streamLogs(
        name,
        { tail: tail > 0 ? tail : undefined, follow, timestamps },
        (chunk) =>
          setText((t) => {
            const all = t + chunk
            return all.length > MAX_LOG_CHARS ? all.slice(all.length - MAX_LOG_CHARS) : all
          }),
        ctl.signal,
      )
    } catch (e) {
      if ((e as Error).name !== 'AbortError') setError(e as Error)
    } finally {
      // A newer load has taken over when this one was stopped for it.
      if (abort.current === ctl || abort.current === null) setRunning(false)
      if (abort.current === ctl) abort.current = null
      onRun()
    }
  }

  useEffect(() => {
    if (shown && !loaded.current) void load()
  }, [shown])
  useEffect(
    () => () => {
      stop()
      loaded.current = false
    },
    [name],
  )

  // Keep to the bottom while following, unless the reader has scrolled up.
  useLayoutEffect(() => {
    const el = pre.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [text, shown])

  const onScroll = () => {
    const el = pre.current
    if (el) stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40
  }

  return (
    <section className="terminal" hidden={!shown}>
      <header className="terminal-bar">
        <label className="inline">
          Lines
          <input
            type="number"
            min={0}
            step={100}
            value={tail}
            onChange={(e) => setTail(Number(e.target.value))}
            className="narrow"
            title="Lines from the end of each container's log; 0 for all"
          />
        </label>
        <label className="inline">
          <input type="checkbox" checked={timestamps} onChange={(e) => setTimestamps(e.target.checked)} /> Timestamps
        </label>
        <label className="inline">
          <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} /> Follow
        </label>
        <span className="terminal-state" role="status">
          {running && (
            <>
              <i className="dot tone-ok pulse" /> {follow ? 'following' : 'loading'}
            </>
          )}
        </span>
        {running ? (
          <button className="btn btn-secondary btn-sm" onClick={stop}>
            Stop
          </button>
        ) : (
          <button className="btn btn-primary btn-sm" onClick={() => void load()}>
            Reload
          </button>
        )}
      </header>
      <ErrorBox error={error} />
      <pre className="logs" ref={pre} onScroll={onScroll} data-testid="logs">
        {text || (running ? '' : 'No output.')}
      </pre>
    </section>
  )
}
