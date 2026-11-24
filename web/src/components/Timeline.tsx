import { useEffect, useState } from 'react'
import { SESSION_COLOR } from '@/components/status'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { fmtMs, fmtTime, fmtUsd, type SessionDetail, type WhEvent } from '@/lib/api'
import { cn } from '@/lib/utils'

/** Gantt of a task's agent sessions (one lane per stage) with human-gate waits. */
export function Timeline({
  sessions,
  events,
  onSelect,
}: {
  sessions: SessionDetail[]
  events: WhEvent[]
  onSelect: (s: SessionDetail) => void
}) {
  const running = sessions.some((s) => !s.ended_at)
  const now = useNow(running ? 1000 : null)
  if (events.length === 0) return null

  const t0 = new Date(events[0].recorded_at).getTime()
  const last = new Date(events[events.length - 1].recorded_at).getTime()
  const ends = sessions.map((s) => (s.ended_at ? new Date(s.ended_at).getTime() : now))
  const t1 = Math.max(last, ...ends, t0 + 1000)
  const span = t1 - t0
  const pct = (t: number) => `${((t - t0) / span) * 100}%`

  // Lanes in first-seen order.
  const lanes: string[] = []
  for (const s of sessions) if (!lanes.includes(s.stage)) lanes.push(s.stage)

  // Gate waits: GateRequested → GateDecided (or now).
  const gates: { stage: string; from: number; to: number; open: boolean; approved?: boolean }[] = []
  for (const e of events) {
    if (e.type === 'GateRequested') {
      gates.push({ stage: e.payload.stage, from: new Date(e.recorded_at).getTime(), to: t1, open: true })
    } else if (e.type === 'GateDecided') {
      const g = [...gates].reverse().find((g) => g.open && g.stage === e.payload.stage)
      if (g) Object.assign(g, { to: new Date(e.recorded_at).getTime(), open: false, approved: e.payload.approved })
    }
  }

  if (sessions.length === 0) {
    return <p className="text-sm text-muted-foreground">No agent sessions yet.</p>
  }

  return (
    <div className="grid gap-2">
      <div className="flex justify-between pl-28 text-xs text-muted-foreground tabular-nums">
        <span>{fmtTime(events[0].recorded_at)}</span>
        <span>{fmtMs(span)}</span>
      </div>
      {lanes.map((lane) => (
        <div key={lane} className="flex items-center gap-2">
          <div className="w-26 shrink-0 truncate text-right font-mono text-xs text-muted-foreground">{lane}</div>
          <div className="relative h-7 flex-1 rounded bg-muted/50">
            {gates
              .filter((g) => g.stage === lane)
              .map((g, i) => (
                <Tooltip key={`g${i}`}>
                  <TooltipTrigger asChild>
                    <div
                      className={cn(
                        'absolute inset-y-1.5 rounded-sm border border-dashed border-warn bg-warn/15',
                        g.open && 'animate-pulse',
                      )}
                      style={{ left: pct(g.from), width: `max(4px, calc(${pct(g.to)} - ${pct(g.from)}))` }}
                    />
                  </TooltipTrigger>
                  <TooltipContent>
                    Human gate · {g.open ? 'waiting' : g.approved ? 'approved' : 'rejected'} · {fmtMs(g.to - g.from)}
                  </TooltipContent>
                </Tooltip>
              ))}
            {sessions
              .filter((s) => s.stage === lane)
              .map((s) => {
                const from = new Date(s.dispatched_at).getTime()
                const to = s.ended_at ? new Date(s.ended_at).getTime() : now
                return (
                  <Tooltip key={s.session_id}>
                    <TooltipTrigger asChild>
                      <button
                        onClick={() => onSelect(s)}
                        className={cn(
                          'absolute inset-y-1 rounded-sm opacity-90 ring-offset-background transition hover:opacity-100 hover:ring-2 hover:ring-ring',
                          SESSION_COLOR[s.status],
                          !s.ended_at && 'animate-pulse',
                        )}
                        style={{ left: pct(from), width: `max(6px, calc(${pct(to)} - ${pct(from)}))` }}
                        aria-label={`${s.agent} session`}
                      />
                    </TooltipTrigger>
                    <TooltipContent>
                      <div className="font-medium">
                        {s.agent} · try {s.attempt} · {s.status}
                      </div>
                      <div className="tabular-nums">
                        {fmtMs(to - from)} · {fmtUsd(s.cost_usd)} · {s.num_turns ?? '–'} turns
                      </div>
                      {s.failure_class ? <div>{s.failure_class}</div> : null}
                    </TooltipContent>
                  </Tooltip>
                )
              })}
          </div>
        </div>
      ))}
      <div className="flex flex-wrap gap-4 pt-1 pl-28 text-xs text-muted-foreground">
        <Legend className="bg-ok" label="succeeded" />
        <Legend className="bg-fail" label="failed" />
        <Legend className="bg-info" label="running" />
        <Legend className="border border-dashed border-warn bg-warn/15" label="waiting for human" />
      </div>
    </div>
  )
}

function Legend({ className, label }: { className: string; label: string }) {
  return (
    <span className="flex items-center gap-1.5">
      <span className={cn('h-2.5 w-4 rounded-sm', className)} />
      {label}
    </span>
  )
}

function useNow(intervalMs: number | null) {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (intervalMs == null) return
    const id = window.setInterval(() => setNow(Date.now()), intervalMs)
    return () => window.clearInterval(id)
  }, [intervalMs])
  return now
}
