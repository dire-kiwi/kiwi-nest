import { api, type NodeInfo } from '../api'
import { plural, useCluster } from '../cluster'
import { Action, Badge, Empty, Labels, PageHead, PollState, SpaceText, Time } from '../components'
import { bytes } from '../format'
import { DiskGauge, MemoryGauge } from './shared'

export function Nodes() {
  const { nodes } = useCluster()
  const list = nodes.data ?? []
  const online = list.filter((n) => n.online).length
  return (
    <>
      <PageHead title="Nodes" sub={nodes.data ? `${online} of ${plural(list.length, 'node')} online` : undefined} />
      <PollState loading={nodes.loading} error={nodes.error} hasData={!!nodes.data} />
      {nodes.data && list.length === 0 && <Empty>No nodes have joined yet.</Empty>}
      <div className="cards">
        {list.map((n) => (
          <NodeCard key={n.id} node={n} refresh={nodes.refresh} />
        ))}
      </div>
    </>
  )
}

function NodeCard({ node: n, refresh }: { node: NodeInfo; refresh: () => Promise<void> }) {
  const f = n.facts
  return (
    <article
      className={`card${n.cordoned ? ' card-cordoned' : ''}${n.online ? '' : ' card-offline'}`}
      data-node={n.name}
    >
      <header className="card-head">
        <div>
          <h2 className="name">{n.name}</h2>
          <p className="sub">
            {f ? (
              <span title={`commit ${f.commit}, link v${f.protocol ?? 1}`}>
                {f.arch} · v{f.version} · link v{f.protocol ?? 1}
              </span>
            ) : (
              'not reported yet'
            )}
          </p>
        </div>
        <span className="badges">
          {n.online ? <Badge kind="ok">online</Badge> : <Badge kind="bad">offline</Badge>}
          {n.cordoned && (
            <Badge kind="warn" title="Gets no new nests; its nests still take new members">
              cordoned
            </Badge>
          )}
        </span>
      </header>

      <div className="gauges">
        <MemoryGauge node={n} full />
        <DiskGauge node={n} full />
      </div>

      <dl className="facts">
        <dt>Last seen</dt>
        <dd>{n.online ? 'now' : <Time at={n.last_seen} empty="never" />}</dd>
        <dt>Data</dt>
        <dd>
          {n.usage ? (
            <>
              {bytes(n.usage.data_bytes)} in volumes{' '}
              <span className="muted">
                · measured <Time at={n.usage.measured_at} />
              </span>
            </>
          ) : (
            <span className="muted">not measured yet</span>
          )}
        </dd>
        {n.usage && (
          <>
            <dt>Backup snapshots</dt>
            <dd>
              <SpaceText space={n.usage.backup_snapshots} />
            </dd>
          </>
        )}
        <dt>Labels</dt>
        <dd>
          <Labels labels={n.labels} />
        </dd>
        <dt>Nests</dt>
        <dd>
          {n.nests.length ? (
            <span className="chips">
              {n.nests.map((nest) => (
                <a className="chip" href="#/nests" key={nest}>
                  {nest}
                </a>
              ))}
            </span>
          ) : (
            <span className="muted">none</span>
          )}
        </dd>
      </dl>

      <footer className="card-actions">
        <span className="muted small">
          joined <Time at={n.joined_at} />
        </span>
        {n.cordoned ? (
          <Action label="Uncordon" small run={() => api.uncordon(n.name)} onDone={refresh} />
        ) : (
          <Action
            label="Cordon"
            small
            run={() => api.cordon(n.name)}
            onDone={refresh}
            title="Keep new nests off this node"
          />
        )}
      </footer>
    </article>
  )
}
