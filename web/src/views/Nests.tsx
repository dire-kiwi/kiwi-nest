import { plural, useCluster } from '../cluster'
import { AppLink, Badge, Empty, PageHead, PollState, Time } from '../components'
import { bytes } from '../format'

export function Nests() {
  const { nests } = useCluster()
  const list = nests.data ?? []
  return (
    <>
      <PageHead
        title="Nests"
        sub="Apps that share data live together on one node: an app and its previews, or a dataset and the apps that mount it."
      />
      <PollState loading={nests.loading} error={nests.error} hasData={!!nests.data} />
      {nests.data && list.length === 0 && <Empty>No nests yet: deploy an app.</Empty>}
      {list.length > 0 && (
        <section className="panel">
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Nest</th>
                  <th>Node</th>
                  <th>Apps</th>
                  <th>Shared volumes</th>
                  <th className="num">Memory limits</th>
                  <th className="num">Disk</th>
                  <th>Placed</th>
                </tr>
              </thead>
              <tbody>
                {list.map((n) => (
                  <tr key={n.name} data-nest={n.name}>
                    <td>
                      <span className="name strong">{n.name}</span>
                      <div className="sub">{plural(n.apps.length, 'app')}</div>
                    </td>
                    <td>
                      <a className="name" href="#/nodes">
                        {n.node}
                      </a>{' '}
                      {!n.node_online && <Badge kind="bad">offline</Badge>}
                    </td>
                    <td>
                      {n.apps.length ? (
                        <span className="chips">
                          {n.apps.map((a) => (
                            <AppLink key={a} name={a} />
                          ))}
                        </span>
                      ) : (
                        <span className="muted">none</span>
                      )}
                    </td>
                    <td>
                      {n.volumes.length ? (
                        <span className="chips">
                          {n.volumes.map((v) => (
                            <a className="chip" href="#/volumes" key={v}>
                              {v}
                            </a>
                          ))}
                        </span>
                      ) : (
                        <span className="muted">none</span>
                      )}
                    </td>
                    <td className="num">{bytes(n.memory_bytes)}</td>
                    <td className="num">
                      {n.disk_bytes === undefined ? (
                        <span className="muted">not measured yet</span>
                      ) : (
                        bytes(n.disk_bytes)
                      )}
                    </td>
                    <td className="col-wide">
                      <Time at={n.created_at} />
                      <div className="sub">{n.placement}</div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>
      )}
    </>
  )
}
