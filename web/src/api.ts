// The server's API, as the CLI uses it (see crates/kn-proto/src/api.rs and backup.rs for what each field means).
//
// The UI is served from the server's own origin, and is logged in with a session cookie, which the browser sends
// with every request. Requests that change anything are JSON (the server refuses any that aren't, so no other site
// can make them).

export const PREFIX = '/v1'

export type AppStatus = 'deploying' | 'running' | 'idle' | 'suspended' | 'failed'
export type SharedMode = 'snapshot' | 'ro' | 'rw'
export type TokenKind = 'admin' | 'deploy' | 'join' | 'node'
export type BackupKind = 'app' | 'volume' | 'server'
export type BackupFormat = 'send' | 'tar'

export interface ErrorBody {
  error: string
  problems?: string[]
}

export interface VersionInfo {
  version: string
  commit: string
}

export interface WhoAmI {
  kind: TokenKind
  name: string
  user?: string
  patterns?: string[]
  from?: string[]
}

export interface SessionInfo {
  user: string
  created_at: string
  idle_expires_at: string
  expires_at: string
}

export interface Space {
  bytes: number
  exclusive_bytes: number
}

export interface AppSummary {
  name: string
  status: AppStatus
  generation: number
  domain: string
  port: number
  containers: string
  updated_at: string
  error?: string
  idle_timeout_secs: number
  last_request_at?: string
  open_requests: number
  space?: Record<string, Space>
  node?: string
  nest?: string
  pending?: number
}

export interface ContainerInfo {
  service: string
  name: string
  state: string
  health: string
  status: string
}

export interface SharedMountInfo {
  name: string
  target: string
  mode: SharedMode
}

export interface AppInfo {
  name: string
  status: AppStatus
  generation: number
  error?: string
  domain: string
  service: string
  container_port: number
  port: number
  idle_timeout_secs: number
  project: string
  bundle_digest: string
  volumes: string[]
  shared: SharedMountInfo[]
  from?: string
  restored_from?: string
  notes?: string[]
  created_at: string
  updated_at: string
  containers: ContainerInfo[]
  open_requests: number
  last_request_at?: string
  space?: Record<string, Space>
  node?: string
  nest?: string
  placement?: string
  pending?: number
  stale?: boolean
}

export interface VolumeMount {
  app: string
  mode: SharedMode
}

export interface VolumeInfo {
  name: string
  created_at: string
  loaded_at?: string
  mounts: VolumeMount[]
  snapshots: string[]
  problem?: string
  space?: Space
  snapshot_space?: Record<string, Space>
  node?: string
  nest?: string
  pending?: number
}

export interface DiskSpace {
  total_bytes: number
  free_bytes: number
  unallocated_bytes?: number
}

export interface NodeFacts {
  arch: string
  memory_bytes: number
  memory_available_bytes: number
  volumes: DiskSpace
  state: DiskSpace
  labels?: Record<string, string>
  version: string
  commit: string
  allowed_host_paths?: string[]
  project_prefix?: string
  gateway?: string
  ingress?: string
  protocol?: number
}

export interface NodeUsage {
  measured_at: string
  data_bytes: number
  backup_snapshots: Space
}

export interface NodeInfo {
  id: number
  name: string
  online: boolean
  joined_at: string
  last_seen?: string
  facts?: NodeFacts
  cordoned: boolean
  labels: Record<string, string>
  nests: string[]
  apps: number
  app_memory_bytes: number
  headroom_bytes: number
  usage?: NodeUsage
}

export interface NestInfo {
  name: string
  node: string
  node_online: boolean
  apps: string[]
  volumes: string[]
  memory_bytes: number
  disk_bytes?: number
  placement: string
  created_at: string
}

export interface BackupInfo {
  target: string
  key: string
  kind: BackupKind
  name: string
  format: BackupFormat
  taken_at: string
  parent?: string
  size?: number
}

export interface TargetError {
  target: string
  error: string
}

export interface BackupResult {
  made: BackupInfo[]
  failed: TargetError[]
  notes?: string[]
}

export interface CommandInfo {
  id: number
  action: string
  detail?: string
  node: string
  by: string
  state: 'pending' | 'delivered' | 'done' | 'cancelled' | 'lost'
  ok?: boolean
  error?: string
  created_at: string
  finished_at?: string
}

/** An error answered by the server (or a failed request). */
export class ApiError extends Error {
  status: number
  problems: string[]
  constructor(status: number, message: string, problems: string[] = []) {
    super(message)
    this.status = status
    this.problems = problems
  }
}

/** Called when the server says the session is gone (401), so the app can show the login page. */
let onUnauthorized: () => void = () => {}
export function setUnauthorizedHandler(f: () => void) {
  onUnauthorized = f
}

const seg = (s: string) => encodeURIComponent(s)

function query(q: Record<string, string | number | boolean | undefined>): string {
  const p = new URLSearchParams()
  for (const [k, v] of Object.entries(q)) {
    if (v !== undefined && v !== '') p.set(k, String(v))
  }
  const s = p.toString()
  return s ? `?${s}` : ''
}

async function failure(r: Response, path: string): Promise<ApiError> {
  let body: ErrorBody | undefined
  try {
    body = (await r.json()) as ErrorBody
  } catch {
    body = undefined
  }
  if (r.status === 401 && path !== '/session') onUnauthorized()
  return new ApiError(r.status, body?.error || `${r.status} ${r.statusText}`, body?.problems ?? [])
}

/** Sends a request. Anything but a GET is JSON, with `{}` when there's nothing to send. */
async function send(method: string, path: string, body?: unknown, signal?: AbortSignal): Promise<Response> {
  const init: RequestInit = { method, credentials: 'same-origin', signal, headers: { Accept: 'application/json' } }
  if (method !== 'GET') {
    init.headers = { ...init.headers, 'Content-Type': 'application/json' }
    init.body = JSON.stringify(body ?? {})
  }
  let r: Response
  try {
    r = await fetch(PREFIX + path, init)
  } catch (e) {
    if ((e as Error).name === 'AbortError') throw e
    throw new ApiError(0, `couldn't reach the server: ${(e as Error).message}`)
  }
  if (!r.ok) throw await failure(r, path)
  return r
}

async function json<T>(method: string, path: string, body?: unknown): Promise<T> {
  const r = await send(method, path, body)
  if (r.status === 204) return undefined as T
  return (await r.json()) as T
}

export const api = {
  session: () => json<SessionInfo>('GET', '/session'),
  login: (user: string, password: string) => json<SessionInfo>('POST', '/session', { user, password }),
  logout: () => json<void>('DELETE', '/session'),
  version: () => json<VersionInfo>('GET', '/version'),
  whoami: () => json<WhoAmI>('GET', '/whoami'),

  nodes: () => json<NodeInfo[]>('GET', '/nodes'),
  cordon: (node: string) => json<NodeInfo>('POST', `/nodes/${seg(node)}/cordon`),
  uncordon: (node: string) => json<NodeInfo>('POST', `/nodes/${seg(node)}/uncordon`),

  nests: () => json<NestInfo[]>('GET', '/nests'),

  apps: () => json<AppSummary[]>('GET', '/apps'),
  inspect: (app: string) => json<AppInfo>('GET', `/apps/${seg(app)}`),
  suspend: (app: string) => json<AppInfo>('POST', `/apps/${seg(app)}/suspend`),
  resume: (app: string) => json<AppInfo>('POST', `/apps/${seg(app)}/resume`),
  commands: (app: string, limit = 20) => json<CommandInfo[]>('GET', `/apps/${seg(app)}/commands${query({ limit })}`),

  volumes: () => json<VolumeInfo[]>('GET', '/volumes'),

  backups: () => json<BackupInfo[]>('GET', '/backups'),
  backupApp: (app: string, target?: string) =>
    json<BackupResult>('POST', `/backups/apps/${seg(app)}${query({ target })}`),
  backupVolume: (name: string, target?: string) =>
    json<BackupResult>('POST', `/backups/volumes/${seg(name)}${query({ target })}`),
}

export interface LogsOptions {
  tail?: number
  follow: boolean
  timestamps: boolean
}

/**
 * Reads an app's logs, calling `onText` with each piece as it arrives (the whole log at once, unless following).
 * Resolves when the stream ends; abort `signal` to stop following.
 */
export async function streamLogs(
  app: string,
  opts: LogsOptions,
  onText: (text: string) => void,
  signal: AbortSignal,
): Promise<void> {
  const r = await send(
    'GET',
    `/apps/${seg(app)}/logs${query({ tail: opts.tail, follow: opts.follow, timestamps: opts.timestamps })}`,
    undefined,
    signal,
  )
  if (!r.body) {
    onText(await r.text())
    return
  }
  const reader = r.body.getReader()
  const decoder = new TextDecoder()
  try {
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      onText(decoder.decode(value, { stream: true }))
    }
    const rest = decoder.decode()
    if (rest) onText(rest)
  } finally {
    reader.releaseLock()
  }
}
