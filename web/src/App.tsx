import { useEffect, useState, type FormEvent, type ReactNode } from 'react'
import { api, ApiError, setUnauthorizedHandler, type SessionInfo, type VersionInfo } from './api'
import { ClusterProvider, problems, useCluster } from './cluster'
import { ErrorBox } from './components'
import { useRoute } from './hooks'
import {
  IconApps,
  IconAuto,
  IconBackups,
  IconLogout,
  IconMoon,
  IconNests,
  IconNodes,
  IconOverview,
  IconSun,
  IconVolumes,
  Logo,
} from './icons'
import { applyTheme, nextTheme, storedTheme, type Theme } from './theme'
import { AppDetail } from './views/AppDetail'
import { Apps } from './views/Apps'
import { Backups } from './views/Backups'
import { Nests } from './views/Nests'
import { Nodes } from './views/Nodes'
import { Overview } from './views/Overview'
import { Volumes } from './views/Volumes'

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
  return (
    <ClusterProvider>
      <Shell session={session} version={version} onLogout={() => setSession(null)} />
    </ClusterProvider>
  )
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
        <div className="login-brand">
          <Logo size={44} />
          <h1>kiwi-nest</h1>
          <p className="muted">Log in to this cluster</p>
        </div>
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
        <button className="btn btn-primary btn-lg" type="submit" disabled={busy}>
          {busy ? 'Logging in…' : 'Log in'}
        </button>
        {version && <p className="login-version">server {version.version}</p>}
      </form>
    </main>
  )
}

const THEME_ICON: Record<Theme, () => ReactNode> = { auto: IconAuto, light: IconSun, dark: IconMoon }
const THEME_LABEL: Record<Theme, string> = { auto: 'System theme', light: 'Light theme', dark: 'Dark theme' }

function Shell({ session, version, onLogout }: { session: SessionInfo; version?: VersionInfo; onLogout: () => void }) {
  const route = useRoute()
  const cluster = useCluster()
  const [theme, setTheme] = useState<Theme>(storedTheme)
  const logout = async () => {
    try {
      await api.logout()
    } finally {
      onLogout()
    }
  }
  const cycleTheme = () => {
    const t = nextTheme(theme)
    applyTheme(t)
    setTheme(t)
  }

  let view
  let current = route
  const app = route.match(/^\/apps\/([^/]+)(\/logs)?$/)
  if (app) {
    view = <AppDetail key={app[1]} name={app[1]} tab={app[2] ? 'logs' : 'overview'} />
    current = '/apps'
  } else if (route === '/apps') view = <Apps />
  else if (route === '/nodes') view = <Nodes />
  else if (route === '/nests') view = <Nests />
  else if (route === '/volumes') view = <Volumes />
  else if (route === '/backups') view = <Backups />
  else {
    view = <Overview />
    current = '/'
  }

  // The worst of what needs attention in each view, as a mark on its link.
  const marks = new Map<string, 'bad' | 'warn'>()
  for (const p of problems(cluster)) {
    const path = p.route.startsWith('/apps') ? '/apps' : p.route
    if (marks.get(path) !== 'bad') marks.set(path, p.tone)
  }

  const links: [string, string, () => ReactNode, number | undefined][] = [
    ['/', 'Overview', IconOverview, undefined],
    ['/apps', 'Apps', IconApps, cluster.apps.data?.length],
    ['/nodes', 'Nodes', IconNodes, cluster.nodes.data?.length],
    ['/nests', 'Nests', IconNests, cluster.nests.data?.length],
    ['/volumes', 'Volumes', IconVolumes, cluster.volumes.data?.length],
    ['/backups', 'Backups', IconBackups, undefined],
  ]
  const ThemeIcon = THEME_ICON[theme]

  return (
    <div className="shell">
      <aside className="side">
        <a className="brand" href="#/">
          <Logo />
          <span>kiwi-nest</span>
        </a>
        <nav className="nav" aria-label="Views">
          {links.map(([path, label, NavIcon, count]) => (
            <a key={path} href={`#${path}`} aria-current={current === path ? 'page' : undefined}>
              <NavIcon />
              <span className="nav-label">{label}</span>
              {marks.has(path) && (
                <i className={`dot tone-${marks.get(path)}`} title="Something here needs attention" />
              )}
              {count !== undefined && <span className="nav-count">{count}</span>}
            </a>
          ))}
        </nav>
        <div className="side-foot">
          <div className="who">
            <span className="who-name" title={`Logged in as ${session.user}`}>
              {session.user}
            </span>
            {version && (
              <span className="who-version" title={`commit ${version.commit}`}>
                v{version.version}
              </span>
            )}
          </div>
          <button
            className="btn btn-ghost btn-icon"
            onClick={cycleTheme}
            title={`${THEME_LABEL[theme]} (click to change)`}
            aria-label={`${THEME_LABEL[theme]}; change theme`}
          >
            <ThemeIcon />
          </button>
          <button className="btn btn-ghost btn-icon" onClick={() => void logout()} title="Log out" aria-label="Log out">
            <IconLogout />
          </button>
        </div>
      </aside>
      <main className="content">{view}</main>
    </div>
  )
}
