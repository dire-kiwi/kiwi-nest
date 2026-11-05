import { useEffect, useState, type FormEvent } from 'react'
import { api, ApiError, setUnauthorizedHandler, type SessionInfo, type VersionInfo } from './api'
import { ErrorBox } from './components'
import { useRoute } from './hooks'
import { AppDetail } from './views/AppDetail'
import { Apps } from './views/Apps'
import { Backups } from './views/Backups'
import { Nests } from './views/Nests'
import { Nodes } from './views/Nodes'
import { Volumes } from './views/Volumes'

const VIEWS: [string, string][] = [
  ['/apps', 'Apps'],
  ['/nodes', 'Nodes'],
  ['/nests', 'Nests'],
  ['/volumes', 'Volumes'],
  ['/backups', 'Backups'],
]

export function App() {
  // undefined: not known yet; null: logged out.
  const [session, setSession] = useState<SessionInfo | null>()
  const [error, setError] = useState<Error>()
  const [version, setVersion] = useState<VersionInfo>()

  useEffect(() => {
    setUnauthorizedHandler(() => setSession(null))
    api
      .session()
      .then(setSession)
      .catch((e: Error) => {
        if (e instanceof ApiError && e.status === 401) setSession(null)
        else setError(e)
      })
    api.version().then(setVersion, () => {})
  }, [])

  if (session === undefined) {
    return (
      <main className="center">
        {error ? <ErrorBox error={error} prefix="Couldn't reach the server" /> : <p className="muted">Loading…</p>}
      </main>
    )
  }
  if (session === null) return <Login onLogin={setSession} version={version} />
  return <Shell session={session} version={version} onLogout={() => setSession(null)} />
}

function Login({ onLogin, version }: { onLogin: (s: SessionInfo) => void; version?: VersionInfo }) {
  const [user, setUser] = useState('')
  const [password, setPassword] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<Error>()

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      onLogin(await api.login(user, password))
    } catch (err) {
      setError(err as Error)
      setPassword('')
    } finally {
      setBusy(false)
    }
  }

  return (
    <main className="center">
      <form className="login" onSubmit={submit}>
        <h1>kiwi-nest</h1>
        <label>
          User
          <input autoFocus autoComplete="username" value={user} onChange={(e) => setUser(e.target.value)} required />
        </label>
        <label>
          Password
          <input
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
        </label>
        <ErrorBox error={error} />
        <button className="btn btn-primary" type="submit" disabled={busy}>
          {busy ? 'Logging in…' : 'Log in'}
        </button>
        {version && <p className="small muted">server {version.version}</p>}
      </form>
    </main>
  )
}

function Shell({ session, version, onLogout }: { session: SessionInfo; version?: VersionInfo; onLogout: () => void }) {
  const route = useRoute()
  const logout = async () => {
    try {
      await api.logout()
    } finally {
      onLogout()
    }
  }

  let view
  const app = route.match(/^\/apps\/([^/]+)(\/logs)?$/)
  if (app) view = <AppDetail key={app[1]} name={app[1]} logsOpen={!!app[2]} />
  else if (route === '/nodes') view = <Nodes />
  else if (route === '/nests') view = <Nests />
  else if (route === '/volumes') view = <Volumes />
  else if (route === '/backups') view = <Backups />
  else view = <Apps />
  const current = app ? '/apps' : route

  return (
    <>
      <header className="top">
        <a className="brand" href="#/apps">
          kiwi-nest
        </a>
        <nav>
          {VIEWS.map(([path, label]) => (
            <a key={path} href={`#${path}`} className={current === path ? 'active' : undefined}>
              {label}
            </a>
          ))}
        </nav>
        <div className="who">
          {version && (
            <span className="muted small" title={`commit ${version.commit}`}>
              v{version.version}
            </span>
          )}
          <span>{session.user}</span>
          <button className="btn btn-secondary" onClick={() => void logout()}>
            Log out
          </button>
        </div>
      </header>
      <main className="content">{view}</main>
    </>
  )
}
