// The cluster's lists (apps, nodes, nests, shared volumes), fetched once for every view and the sidebar, and what
// in them needs somebody's attention.

import { createContext, useContext, type ReactNode } from 'react'
import { api, type AppStatus, type AppSummary, type NestInfo, type NodeInfo, type VolumeInfo } from './api'
import { bytes, percent, relative } from './format'
import { usePoll, type Polled } from './hooks'

export interface Cluster {
  apps: Polled<AppSummary[]>
  nodes: Polled<NodeInfo[]>
  nests: Polled<NestInfo[]>
  volumes: Polled<VolumeInfo[]>
}

const Context = createContext<Cluster | null>(null)

/** The lists are the server's own view, so they're cheap to ask for every few seconds. */
const EVERY_MS = 5000

export function ClusterProvider({ children }: { children: ReactNode }) {
  const cluster: Cluster = {
    apps: usePoll(api.apps, EVERY_MS),
    nodes: usePoll(api.nodes, EVERY_MS),
    nests: usePoll(api.nests, EVERY_MS),
    volumes: usePoll(api.volumes, EVERY_MS),
  }
  return <Context.Provider value={cluster}>{children}</Context.Provider>
}

export function useCluster(): Cluster {
  const c = useContext(Context)
  if (!c) throw new Error('useCluster outside ClusterProvider')
  return c
}

/** Below this share of the btrfs filesystem free (or unallocated), a node is shown with a warning. */
const LOW_SPACE = 0.1

/** What's short on a node's btrfs filesystem, if anything. */
export function lowSpace(n: NodeInfo): { free: boolean; unallocated: boolean } {
  const v = n.facts?.volumes
  if (!v || v.total_bytes <= 0) return { free: false, unallocated: false }
  return {
    free: v.free_bytes / v.total_bytes < LOW_SPACE,
    unallocated: v.unallocated_bytes !== undefined && v.unallocated_bytes / v.total_bytes < LOW_SPACE,
  }
}

/** The statuses in the order they're listed and drawn. */
export const STATUSES: AppStatus[] = ['running', 'idle', 'deploying', 'suspended', 'failed']

export function countByStatus(apps: AppSummary[]): Record<AppStatus, number> {
  const counts: Record<AppStatus, number> = { running: 0, idle: 0, deploying: 0, suspended: 0, failed: 0 }
  for (const a of apps) counts[a.status] = (counts[a.status] ?? 0) + 1
  return counts
}

export interface Problem {
  tone: 'bad' | 'warn'
  /** The view that shows it. */
  route: string
  title: string
  detail?: string
}

/** What needs attention, worst first. Cordoned nodes and suspended apps were asked for, so they aren't listed. */
export function problems(c: Cluster): Problem[] {
  const out: Problem[] = []
  for (const n of c.nodes.data ?? []) {
    if (!n.online) {
      out.push({
        tone: 'bad',
        route: '/nodes',
        title: `Node ${n.name} is offline`,
        detail: `${n.last_seen ? `last seen ${relative(n.last_seen)}` : 'never seen'} · ${plural(n.apps, 'app')} on it`,
      })
    }
  }
  for (const a of c.apps.data ?? []) {
    if (a.status === 'failed') {
      out.push({ tone: 'bad', route: `/apps/${a.name}`, title: `${a.name} failed`, detail: a.error })
    }
  }
  for (const n of c.nodes.data ?? []) {
    const low = lowSpace(n)
    const v = n.facts?.volumes
    if (v && (low.free || low.unallocated)) {
      out.push({
        tone: 'warn',
        route: '/nodes',
        title: `Node ${n.name} is low on space`,
        detail: `${bytes(v.free_bytes)} free (${percent(v.free_bytes, v.total_bytes)}) of ${bytes(v.total_bytes)}`,
      })
    }
    if (n.headroom_bytes < 0) {
      out.push({
        tone: 'warn',
        route: '/nodes',
        title: `Node ${n.name} is overcommitted`,
        detail: `its apps' memory limits are ${bytes(-n.headroom_bytes)} over what placement allows`,
      })
    }
  }
  for (const v of c.volumes.data ?? []) {
    if (v.problem) out.push({ tone: 'warn', route: '/volumes', title: `Volume ${v.name}`, detail: v.problem })
  }
  return out
}

export function plural(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? '' : 's'}`
}

/**
 * An app's address through the ingress. When this page came through the ingress on a port of its own (as on the dev
 * cluster), so do the apps.
 */
export function appUrl(domain: string): string {
  const { protocol, port } = window.location
  return `https://${domain}${protocol === 'https:' && port ? `:${port}` : ''}/`
}
