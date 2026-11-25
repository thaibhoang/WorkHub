import { useQueryClient } from '@tanstack/react-query'
import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from 'react'
import type { WhEvent } from './api'

type LiveStatus = 'connecting' | 'live' | 'offline'

interface LiveValue {
  status: LiveStatus
  /** Newest first, at most RECENT events since the page loaded. */
  recent: WhEvent[]
}

const RECENT = 100
const LiveContext = createContext<LiveValue>({ status: 'connecting', recent: [] })

/**
 * One SSE connection for the whole app. Every event refreshes the cached
 * queries it can affect (after a short debounce, so projections — which run in
 * the same server process — have caught up). EventSource reconnects on its own
 * and resumes with Last-Event-ID, so nothing is missed across a reconnect.
 */
export function LiveProvider({ children }: { children: ReactNode }) {
  const qc = useQueryClient()
  const [status, setStatus] = useState<LiveStatus>('connecting')
  const [recent, setRecent] = useState<WhEvent[]>([])
  const pending = useRef<{ tasks: Set<string>; global: boolean; timer?: number }>({ tasks: new Set(), global: false })

  useEffect(() => {
    const es = new EventSource('/api/events/stream')
    const flush = () => {
      const p = pending.current
      p.timer = undefined
      for (const id of p.tasks) {
        qc.invalidateQueries({ queryKey: ['task', id] })
        qc.invalidateQueries({ queryKey: ['proposal', id] })
      }
      if (p.global) {
        for (const key of ['tasks', 'sessions', 'stats', 'dashboard', 'scorers', 'scores', 'proposals', 'improvement'])
          qc.invalidateQueries({ queryKey: [key] })
      }
      p.tasks.clear()
      p.global = false
    }
    es.onopen = () => setStatus('live')
    es.onerror = () => setStatus(es.readyState === EventSource.CLOSED ? 'offline' : 'connecting')
    es.addEventListener('event', (msg) => {
      const ev: WhEvent = JSON.parse((msg as MessageEvent<string>).data)
      setRecent((r) => [ev, ...r].slice(0, RECENT))
      const p = pending.current
      p.tasks.add(ev.meta.correlation_id)
      p.global = true
      p.timer ??= window.setTimeout(flush, 350)
    })
    return () => {
      es.close()
      if (pending.current.timer) window.clearTimeout(pending.current.timer)
    }
  }, [qc])

  return <LiveContext.Provider value={{ status, recent }}>{children}</LiveContext.Provider>
}

// eslint-disable-next-line react-refresh/only-export-components
export function useLive() {
  return useContext(LiveContext)
}
