import { api } from '../api'
import { Badge, PollState, Section, Time } from '../components'
import { bytes } from '../format'
import { usePoll } from '../hooks'

export function Nests() {
  const nests = usePoll(api.nests, 5000)
  return (
    <Section title="Nests">
      <PollState loading={nests.loading} error={nests.error} hasData={!!nests.data} />
      {nests.data && nests.data.length === 0 && <p className="muted">No nests yet: deploy an app.</p>}
      {nests.data && nests.data.length > 0 && (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Nest</th>
                <th>Node</th>
                <th>Apps</th>
                <th>Shared volumes</th>
                <th className="num">Memory</th>
                <th className="num">Disk</th>
                <th>Placed</th>
              </tr>
            </thead>
            <tbody>
              {nests.data.map((n) => (
                <tr key={n.name}>
                  <td>
                    <strong>{n.name}</strong>
                  </td>
                  <td>
                    {n.node} {!n.node_online && <Badge kind="bad">offline</Badge>}
                  </td>
                  <td>
                    {n.apps.length ? (
                      n.apps.map((a, i) => (
                        <span key={a}>
                          {i > 0 && ', '}
                          <a href={`#/apps/${encodeURIComponent(a)}`}>{a}</a>
                        </span>
                      ))
                    ) : (
                      <span className="muted">none</span>
                    )}
                  </td>
                  <td>{n.volumes.length ? n.volumes.join(', ') : <span className="muted">none</span>}</td>
                  <td className="num">{bytes(n.memory_bytes)}</td>
                  <td className="num">
                    {n.disk_bytes === undefined ? <span className="muted">not measured yet</span> : bytes(n.disk_bytes)}
                  </td>
                  <td>
                    <Time at={n.created_at} />
                    <div className="small muted">{n.placement}</div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Section>
  )
}
