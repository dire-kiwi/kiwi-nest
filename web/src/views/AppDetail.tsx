import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { api, streamLogs, type BackupResult } from '../api'
import { Action, Badge, ErrorBox, PollState, Section, SpaceText, StatusBadge, Time } from '../components'
import { usePoll } from '../hooks'
import { BackupResultView, IdleTimeout, LastActivity } from './shared'

export function AppDetail({ name, logsOpen }: { name: string; logsOpen: boolean }) {
  // The list is cheap (the server's own view); inspecting asks the node (and is a command, kept a day), so it's
  // refreshed less often. Every poll pauses while the tab is hidden.
  const summary = usePoll(async () => (await api.apps()).find((a) => a.name === name), 5000, [name])
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
    void summary.refresh()
    void info.refresh()
    void commands.refresh()
  }
  const failed = (e: Error) => {
    setActionError(e)
    void commands.refresh()
  }
  const s = summary.data
  const i = info.data
  const status = s?.status ?? i?.status

  return (
    <>
      <Section
        title={
          <>
            <a href="#/apps" className="muted">
              Apps
            </a>{' '}
            / {name} {status && <StatusBadge status={status} />}
            {i?.stale && (
              <Badge kind="bad" title="The node is offline: this is what it last reported">
                stale
              </Badge>
            )}
          </>
        }
        actions={
          <>
            {(status === 'running' || status === 'idle') && (
              <Action
                label="Suspend"
                run={() => api.suspend(name)}
                onDone={() => {
                  setActionError(undefined)
                  refresh()
                }}
                onError={failed}
              />
            )}
            {status && status !== 'running' && status !== 'deploying' && (
              <Action
                label="Resume"
                busyLabel="Waking…"
                kind="primary"
                run={() => api.resume(name)}
                onDone={() => {
                  setActionError(undefined)
                  refresh()
                }}
                onError={failed}
              />
            )}
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
          </>
        }
      >
        <PollState loading={summary.loading} error={summary.error} hasData={!!s} />
        {!summary.loading && !s && !summary.error && <p className="muted">There's no app {name}.</p>}
        <ErrorBox error={actionError} />
        {backup && <BackupResultView result={backup} onClose={() => setBackup(undefined)} />}
        {(s || i) && (
          <dl className="facts facts-wide">
            <dt>Node</dt>
            <dd>{s?.node ?? i?.node ?? '—'}</dd>
            <dt>Nest</dt>
            <dd>
              {s?.nest ?? i?.nest ?? '—'}
              {i?.placement && <div className="small muted">{i.placement}</div>}
            </dd>
            <dt>Domain</dt>
            <dd>{s?.domain ?? i?.domain}</dd>
            <dt>Generation</dt>
            <dd>
              {s?.generation ?? i?.generation}
              {(s?.pending ?? i?.pending) !== undefined && (
                <span className="muted"> ({s?.pending ?? i?.pending} saved, not yet applied)</span>
              )}
            </dd>
            {(s?.error ?? i?.error) && (
              <>
                <dt>Error</dt>
                <dd className="bad">{s?.error ?? i?.error}</dd>
              </>
            )}
            <dt>Idle timeout</dt>
            <dd>
              <IdleTimeout secs={s?.idle_timeout_secs ?? i?.idle_timeout_secs ?? 0} />
            </dd>
            <dt>Last activity</dt>
            <dd>
              <LastActivity
                open={i?.open_requests ?? s?.open_requests ?? 0}
                at={s?.last_request_at ?? i?.last_request_at}
              />
            </dd>
            {i?.service && (
              <>
                <dt>Routed to</dt>
                <dd>
                  {i.service}:{i.container_port} <span className="muted">(127.0.0.1:{i.port} on the node)</span>
                </dd>
              </>
            )}
            {i?.from && (
              <>
                <dt>From</dt>
                <dd>
                  <a href={`#/apps/${encodeURIComponent(i.from)}`}>{i.from}</a>
                </dd>
              </>
            )}
            {i?.restored_from && (
              <>
                <dt>Restored from</dt>
                <dd>
                  <code>{i.restored_from}</code>
                </dd>
              </>
            )}
            <dt>Deployed</dt>
            <dd>
              <Time at={s?.updated_at ?? i?.updated_at} />
            </dd>
          </dl>
        )}
        <ErrorBox error={info.error} prefix="Couldn't ask the node" />
      </Section>

      {i && i.containers.length > 0 && (
        <Section title="Containers">
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Service</th>
                  <th>Container</th>
                  <th>State</th>
                  <th>Health</th>
                  <th>Status</th>
                </tr>
              </thead>
              <tbody>
                {i.containers.map((c) => (
                  <tr key={c.name}>
                    <td>{c.service}</td>
                    <td className="small">{c.name}</td>
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
        </Section>
      )}

      <Section title="Volumes">
        {(() => {
          const space = s?.space ?? i?.space ?? {}
          const names = [...new Set([...(i?.volumes ?? []), ...Object.keys(space)])].sort()
          const shared = i?.shared ?? []
          if (names.length === 0 && shared.length === 0) return <p className="muted">No volumes.</p>
          return (
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
                  {names.map((v) => (
                    <tr key={`own-${v}`}>
                      <td>{v}</td>
                      <td>its own</td>
                      <td>
                        <SpaceText space={space[v]} />
                      </td>
                    </tr>
                  ))}
                  {shared.map((m) => (
                    <tr key={`shared-${m.name}`}>
                      <td>
                        <a href="#/volumes">{m.name}</a> <span className="muted">at {m.target}</span>
                      </td>
                      <td>shared, {m.mode}</td>
                      <td className="muted">see Volumes</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )
        })()}
      </Section>

      <Section title="Recent commands" actions={<Action label="Refresh" run={commands.refresh} />}>
        <PollState loading={commands.loading} error={commands.error} hasData={!!commands.data} />
        {commands.data && commands.data.length === 0 && <p className="muted">No commands yet.</p>}
        {commands.data && commands.data.length > 0 && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Action</th>
                  <th>By</th>
                  <th>Node</th>
                  <th>State</th>
                  <th>Asked</th>
                  <th>Finished</th>
                </tr>
              </thead>
              <tbody>
                {commands.data.map((c) => (
                  <tr key={c.id}>
                    <td>
                      {c.action}
                      {c.detail && <div className="small muted">{c.detail}</div>}
                    </td>
                    <td className="small">{c.by}</td>
                    <td>{c.node}</td>
                    <td>
                      {c.state === 'done' ? (
                        c.ok === false ? (
                          <Badge kind="bad">failed</Badge>
                        ) : (
                          <Badge kind="ok">done</Badge>
                        )
                      ) : (
                        <Badge kind={c.state === 'lost' ? 'bad' : c.state === 'cancelled' ? 'idle' : 'info'}>
                          {c.state === 'delivered' ? 'running' : c.state}
                        </Badge>
                      )}
                      {c.error && <div className="small bad">{c.error}</div>}
                    </td>
                    <td className="small">
                      <Time at={c.created_at} />
                    </td>
                    <td className="small">
                      <Time at={c.finished_at} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Section>

      <Logs name={name} initiallyOpen={logsOpen} onRun={() => void commands.refresh()} />
    </>
  )
}

/** The cap on what the panel keeps, so following a chatty app doesn't grow the page for ever. */
const MAX_LOG_CHARS = 2_000_000

function Logs({ name, initiallyOpen, onRun }: { name: string; initiallyOpen: boolean; onRun: () => void }) {
  const [open, setOpen] = useState(initiallyOpen)
  const [tail, setTail] = useState(200)
  const [timestamps, setTimestamps] = useState(false)
  const [follow, setFollow] = useState(false)
  const [text, setText] = useState('')
  const [running, setRunning] = useState(false)
  const [error, setError] = useState<Error>()
  const abort = useRef<AbortController | null>(null)
  const pre = useRef<HTMLPreElement>(null)
  const stick = useRef(true)

  const stop = () => {
    abort.current?.abort()
    abort.current = null
  }

  const load = async () => {
    stop()
    const ctl = new AbortController()
    abort.current = ctl
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
      if (abort.current === ctl) abort.current = null
      setRunning(false)
      onRun()
    }
  }

  // Open with the logs already loaded when asked to (the list's Logs button).
  useEffect(() => {
    if (initiallyOpen) void load()
    return stop
  }, [name])

  // Keep to the bottom while following, unless the reader has scrolled up.
  useLayoutEffect(() => {
    const el = pre.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [text])

  const onScroll = () => {
    const el = pre.current
    if (el) stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40
  }

  return (
    <Section
      title="Logs"
      actions={
        open ? (
          <>
            <label className="inline">
              Lines{' '}
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
              <input type="checkbox" checked={timestamps} onChange={(e) => setTimestamps(e.target.checked)} />{' '}
              Timestamps
            </label>
            <label className="inline">
              <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} /> Follow
            </label>
            {running ? (
              <button className="btn btn-secondary" onClick={stop}>
                Stop
              </button>
            ) : (
              <button className="btn btn-primary" onClick={() => void load()}>
                {text ? 'Reload' : 'Show logs'}
              </button>
            )}
          </>
        ) : (
          <button
            className="btn btn-primary"
            onClick={() => {
              setOpen(true)
              void load()
            }}
          >
            Show logs
          </button>
        )
      }
    >
      <ErrorBox error={error} />
      {open && (
        <>
          {running && <p className="small muted">{follow ? 'Following… (Stop to end)' : 'Loading…'}</p>}
          <pre className="logs" ref={pre} onScroll={onScroll} data-testid="logs">
            {text || (running ? '' : 'No output.')}
          </pre>
        </>
      )}
    </Section>
  )
}
