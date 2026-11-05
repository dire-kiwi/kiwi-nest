import { api, type NodeInfo } from '../api'
import { Action, Badge, Labels, PollState, Section, SpaceText, Time } from '../components'
import { bytes, percent } from '../format'
import { usePoll } from '../hooks'

/** Below this share of the btrfs filesystem free (or unallocated), a node is shown with a warning. */
const LOW_SPACE = 0.1

export function Nodes() {
  const nodes = usePoll(api.nodes, 5000)
  return (
    <Section title="Nodes">
      <PollState loading={nodes.loading} error={nodes.error} hasData={!!nodes.data} />
      {nodes.data && nodes.data.length === 0 && <p className="muted">No nodes have joined yet.</p>}
      <div className="cards">
        {nodes.data?.map((n) => <NodeCard key={n.id} node={n} refresh={nodes.refresh} />)}
      </div>
    </Section>
  )
}

function NodeCard({ node: n, refresh }: { node: NodeInfo; refresh: () => Promise<void> }) {
  const f = n.facts
  const vol = f?.volumes
  const lowFree = vol && vol.total_bytes > 0 && vol.free_bytes / vol.total_bytes < LOW_SPACE
  const lowUnallocated =
    vol && vol.total_bytes > 0 && vol.unallocated_bytes !== undefined && vol.unallocated_bytes / vol.total_bytes < LOW_SPACE
  return (
    <article className={`card${n.cordoned ? ' card-cordoned' : ''}`} data-node={n.name}>
      <header className="card-head">
        <h3>{n.name}</h3>
        <span className="badges">
          {n.online ? <Badge kind="ok">online</Badge> : <Badge kind="bad">offline</Badge>}
          {n.cordoned && (
            <Badge kind="warn" title="Gets no new nests; its nests still take new members">
              CORDONED
            </Badge>
          )}
        </span>
      </header>
      <dl className="facts">
        <dt>Last seen</dt>
        <dd>{n.online ? 'now' : <Time at={n.last_seen} empty="never" />}</dd>
        <dt>Version</dt>
        <dd>
          {f ? (
            <>
              {f.version} <span className="muted">({f.commit.slice(0, 10)}, link v{f.protocol ?? 1})</span>
            </>
          ) : (
            <span className="muted">not reported</span>
          )}
        </dd>
        <dt>Architecture</dt>
        <dd>{f?.arch ?? '—'}</dd>
        <dt>Memory</dt>
        <dd>
          {f ? (
            <>
              {bytes(f.memory_bytes)} <span className="muted">({bytes(f.memory_available_bytes)} available)</span>
            </>
          ) : (
            '—'
          )}
        </dd>
        <dt>Apps' memory</dt>
        <dd>
          {bytes(n.app_memory_bytes)} <span className="muted">for {n.apps} app{n.apps === 1 ? '' : 's'}</span>
        </dd>
        <dt>Headroom</dt>
        <dd className={n.headroom_bytes < 0 ? 'bad' : undefined}>
          {bytes(n.headroom_bytes)}
          {n.headroom_bytes < 0 && ' (overcommitted)'}
        </dd>
        <dt>btrfs</dt>
        <dd>
          {vol ? (
            <>
              <span className={lowFree ? 'bad' : undefined}>
                {bytes(vol.free_bytes)} free ({percent(vol.free_bytes, vol.total_bytes)})
              </span>{' '}
              of {bytes(vol.total_bytes)}
              <br />
              <span className={lowUnallocated ? 'bad' : undefined}>
                {vol.unallocated_bytes === undefined
                  ? 'unallocated not reported'
                  : `${bytes(vol.unallocated_bytes)} unallocated (${percent(vol.unallocated_bytes, vol.total_bytes)})`}
              </span>
              {(lowFree || lowUnallocated) && (
                <div className="warning">Low on space: a full btrfs filesystem fails writes unpredictably.</div>
              )}
            </>
          ) : (
            '—'
          )}
        </dd>
        <dt>Data</dt>
        <dd>
          {n.usage ? (
            <>
              {bytes(n.usage.data_bytes)} in volumes; backup snapshots{' '}
              <SpaceText space={n.usage.backup_snapshots} />
              <br />
              <span className="muted">
                measured <Time at={n.usage.measured_at} />
              </span>
            </>
          ) : (
            <span className="muted">not measured yet</span>
          )}
        </dd>
        <dt>Labels</dt>
        <dd>
          <Labels labels={n.labels} />
        </dd>
        <dt>Nests</dt>
        <dd>{n.nests.length ? n.nests.join(', ') : <span className="muted">none</span>}</dd>
      </dl>
      <footer className="card-actions">
        {n.cordoned ? (
          <Action label="Uncordon" run={() => api.uncordon(n.name)} onDone={refresh} />
        ) : (
          <Action
            label="Cordon"
            run={() => api.cordon(n.name)}
            onDone={refresh}
            title="Keep new nests off this node"
          />
        )}
      </footer>
    </article>
  )
}
