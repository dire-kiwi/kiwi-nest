import type { ReactNode } from 'react'
import { countByStatus, plural, problems, STATUSES, useCluster } from '../cluster'
import { AppLink, Badge, Empty, Meter, PageHead, Panel, PollState, STATUS_TONE, Time } from '../components'
import { bytes } from '../format'
import { IconAlert, IconCheck } from '../icons'
import { DiskGauge, LastActivity, MemoryGauge } from './shared'

/** The cluster at a glance: what needs attention, how full it is, and what its apps were last doing. */
export function Overview() {
  const cluster = useCluster()
  const apps = cluster.apps.data ?? []
  const nodes = cluster.nodes.data ?? []
  const loaded = !!cluster.apps.data && !!cluster.nodes.data
  // A cluster without nodes has nothing to measure, so it only says so.
  const ready = loaded && nodes.length > 0
  const issues = problems(cluster)

  const counts = countByStatus(apps)
  const online = nodes.filter((n) => n.online).length
  const reporting = nodes.filter((n) => n.facts)
  const memory = reporting.reduce((sum, n) => sum + (n.facts?.memory_bytes ?? 0), 0)
  const limits = reporting.reduce((sum, n) => sum + n.app_memory_bytes, 0)
  const disk = reporting.reduce((sum, n) => sum + (n.facts?.volumes.total_bytes ?? 0), 0)
  const free = reporting.reduce((sum, n) => sum + (n.facts?.volumes.free_bytes ?? 0), 0)

  // Busy apps first, then the ones asked for most recently.
  const active = apps
    .filter((a) => a.open_requests > 0 || a.last_request_at)
    .sort(
      (a, b) =>
        Number(b.open_requests > 0) - Number(a.open_requests > 0) ||
        (b.last_request_at ?? '').localeCompare(a.last_request_at ?? ''),
    )
    .slice(0, 6)
  const deployed = [...apps].sort((a, b) => b.updated_at.localeCompare(a.updated_at)).slice(0, 6)

  return (
    <>
      <PageHead
        title="Overview"
        sub={
          loaded
            ? `${plural(apps.length, 'app')} in ${plural(cluster.nests.data?.length ?? 0, 'nest')} on ${plural(nodes.length, 'node')}`
            : undefined
        }
      />
      <PollState loading={cluster.apps.loading} error={cluster.apps.error ?? cluster.nodes.error} hasData={loaded} />
      {loaded && !ready && (
        <Empty>
          No nodes have joined this cluster yet. Join one with <code>kiwi-nest node join</code>, then deploy to it.
        </Empty>
      )}

      {ready &&
        (issues.length === 0 ? (
          <div className="health">
            <IconCheck />
            <span>
              <strong>Nothing needs attention.</strong> Every node is online and has room, and no app has failed.
            </span>
          </div>
        ) : (
          <Panel
            title={
              <>
                <IconAlert /> Needs attention
              </>
            }
            note={plural(issues.length, 'thing')}
            flush
          >
            <ul className="issues">
              {issues.map((p, i) => (
                <li key={i}>
                  <a href={`#${p.route}`}>
                    <Badge kind={p.tone}>{p.tone === 'bad' ? 'problem' : 'warning'}</Badge>
                    <span className="issue-text">
                      <strong>{p.title}</strong>
                      {p.detail && <span className="muted"> · {p.detail}</span>}
                    </span>
                  </a>
                </li>
              ))}
            </ul>
          </Panel>
        ))}

      {ready && (
        <div className="tiles">
          <Tile label="Apps" value={apps.length} href="#/apps">
            {apps.length > 0 && (
              <>
                <div
                  className="segbar"
                  role="img"
                  aria-label={STATUSES.filter((s) => counts[s])
                    .map((s) => `${counts[s]} ${s}`)
                    .join(', ')}
                >
                  {STATUSES.filter((s) => counts[s]).map((s) => (
                    <i
                      key={s}
                      className={`tone-${STATUS_TONE[s]}`}
                      style={{ flexGrow: counts[s] }}
                      title={`${counts[s]} ${s}`}
                    />
                  ))}
                </div>
                <ul className="legend">
                  {STATUSES.filter((s) => counts[s]).map((s) => (
                    <li key={s}>
                      <i className={`dot tone-${STATUS_TONE[s]}`} />
                      {counts[s]} {s}
                    </li>
                  ))}
                </ul>
              </>
            )}
          </Tile>
          <Tile
            label="Nodes online"
            value={
              <>
                {online} <span className="tile-of">of {nodes.length}</span>
              </>
            }
            href="#/nodes"
          >
            <Meter
              value={online}
              max={nodes.length}
              tone={online < nodes.length ? 'bad' : 'accent'}
              label={`${online} of ${plural(nodes.length, 'node')} online`}
            />
            <p className="tile-note">
              {nodes.filter((n) => n.cordoned).length > 0
                ? `${plural(nodes.filter((n) => n.cordoned).length, 'node')} cordoned`
                : 'none cordoned'}
            </p>
          </Tile>
          <Tile
            label="Memory limits"
            value={
              <>
                {bytes(limits)} <span className="tile-of">of {bytes(memory)}</span>
              </>
            }
            href="#/nodes"
          >
            <Meter value={limits} max={memory} label={`Apps' memory limits: ${bytes(limits)} of ${bytes(memory)}`} />
            <p className="tile-note">what the apps may use, of the nodes' memory</p>
          </Tile>
          <Tile
            label="Disk used"
            value={
              <>
                {bytes(disk - free)} <span className="tile-of">of {bytes(disk)}</span>
              </>
            }
            href="#/nodes"
          >
            <Meter value={disk - free} max={disk} label={`btrfs: ${bytes(disk - free)} used of ${bytes(disk)}`} />
            <p className="tile-note">{bytes(free)} free on the nodes' btrfs filesystems</p>
          </Tile>
        </div>
      )}

      {ready && (
        <Panel title="Nodes" actions={<a href="#/nodes">All nodes</a>} flush>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Node</th>
                  <th>State</th>
                  <th className="col-gauge">Memory limits</th>
                  <th className="col-gauge">Disk</th>
                  <th className="num">Apps</th>
                  <th className="num">Nests</th>
                </tr>
              </thead>
              <tbody>
                {nodes.map((n) => (
                  <tr key={n.id}>
                    <td>
                      <a className="name" href="#/nodes">
                        {n.name}
                      </a>
                    </td>
                    <td>
                      <span className="badges">
                        {n.online ? <Badge kind="ok">online</Badge> : <Badge kind="bad">offline</Badge>}
                        {n.cordoned && <Badge kind="warn">cordoned</Badge>}
                      </span>
                    </td>
                    <td className="col-gauge">
                      <MemoryGauge node={n} />
                    </td>
                    <td className="col-gauge">
                      <DiskGauge node={n} />
                    </td>
                    <td className="num">{n.apps}</td>
                    <td className="num">{n.nests.length}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Panel>
      )}

      {ready && apps.length > 0 && (
        <div className="columns">
          <Panel title="Recent activity" note="requests through the gateways" flush>
            {active.length === 0 ? (
              <Empty>No requests since the nodes started.</Empty>
            ) : (
              <ul className="rows">
                {active.map((a) => (
                  <li key={a.name}>
                    <AppLink name={a.name} />
                    <span className="rows-end">
                      <LastActivity open={a.open_requests} at={a.last_request_at} />
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </Panel>
          <Panel title="Latest deploys" flush>
            <ul className="rows">
              {deployed.map((a) => (
                <li key={a.name}>
                  <AppLink name={a.name} />
                  <span className="muted small">generation {a.generation}</span>
                  <span className="rows-end">
                    <Time at={a.updated_at} />
                  </span>
                </li>
              ))}
            </ul>
          </Panel>
        </div>
      )}
    </>
  )
}

/** A headline number, with what's under it. The whole tile goes to the view that has the details. */
function Tile({
  label,
  value,
  href,
  children,
}: {
  label: string
  value: ReactNode
  href: string
  children?: ReactNode
}) {
  return (
    <a className="tile" href={href}>
      <span className="tile-label">{label}</span>
      <span className="tile-value">{value}</span>
      {children}
    </a>
  )
}
