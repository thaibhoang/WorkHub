import type { ScoreRow } from '@/lib/api'
import { fmtUsd } from '@/lib/api'
import { cn } from '@/lib/utils'

/** Result of one scorer run, from its status and pass flag. */
export function scoreResult(r: ScoreRow): { label: string; tone: string; border: string } {
  if (r.status === 'running') return { label: 'grading', tone: 'text-info', border: 'border-info/40' }
  if (r.status === 'failed') return { label: 'error', tone: 'text-muted-foreground', border: 'border-border' }
  return r.pass
    ? { label: 'pass', tone: 'text-ok', border: 'border-ok/40' }
    : { label: 'fail', tone: 'text-fail', border: 'border-fail/40' }
}

/** Compact chip: scorer name + score, colored by pass / fail. */
export function ScoreChip({ r, showName = true }: { r: ScoreRow; showName?: boolean }) {
  const res = scoreResult(r)
  const title = [
    `${r.scorer}: ${res.label}`,
    r.score != null ? `score ${r.score.toFixed(2)} (pass ≥ ${r.pass_threshold})` : null,
    r.classification,
    r.rationale ?? r.error,
  ]
    .filter(Boolean)
    .join('\n')
  return (
    <span
      title={title}
      className={cn('inline-flex items-center gap-1 rounded border px-1.5 py-0.5 text-xs whitespace-nowrap', res.border)}
    >
      {showName ? <span className="text-muted-foreground">{r.scorer}</span> : null}
      <span className={cn('font-medium tabular-nums', res.tone)}>
        {r.score != null ? r.score.toFixed(2) : res.label}
      </span>
    </span>
  )
}

/** Full result: verdict, classification, rationale and cost. */
export function ScoreCard({ r }: { r: ScoreRow }) {
  const res = scoreResult(r)
  return (
    <div className={cn('rounded-md border p-3 text-sm', res.border)}>
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <span className="font-medium">{r.scorer}</span>
        <span className={cn('text-xs font-semibold uppercase', res.tone)}>{res.label}</span>
        {r.score != null ? (
          <span className="text-xs text-muted-foreground tabular-nums">
            {r.score.toFixed(2)} / pass ≥ {r.pass_threshold}
          </span>
        ) : null}
        {r.classification ? <code className="rounded bg-muted px-1.5 py-0.5 text-xs">{r.classification}</code> : null}
        {r.manual ? <span className="text-xs text-muted-foreground">manual</span> : null}
        <span className="ml-auto text-xs text-muted-foreground tabular-nums">
          {r.model ?? ''} · {fmtUsd(r.cost_usd, 4)}
        </span>
      </div>
      {r.rationale || r.error ? (
        <p className={cn('mt-1.5 text-xs whitespace-pre-wrap', r.error && 'text-muted-foreground')}>
          {r.rationale || r.error}
        </p>
      ) : null}
    </div>
  )
}
