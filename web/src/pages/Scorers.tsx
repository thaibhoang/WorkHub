import { useQuery } from '@tanstack/react-query'
import { Link, useSearchParams } from 'react-router'
import { PageHeader } from '@/components/PageHeader'
import { ScoreChip, scoreResult } from '@/components/scores'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { api, fmtAgo, fmtPct, fmtUsd, shortId, type ScorersResponse } from '@/lib/api'
import { cn } from '@/lib/utils'
import { ApiDown } from './Runs'

/** LLM-as-judge scorers: pass rate and cost per scorer, and recent results. */
export default function Scorers() {
  const [params, setParams] = useSearchParams()
  const scorer = params.get('scorer') ?? ''
  const failing = params.get('failing') === '1'
  const list = useQuery({ queryKey: ['scorers'], queryFn: api.scorers })
  const results = useQuery({
    queryKey: ['scores', scorer, failing],
    queryFn: () => api.scores({ scorer: scorer || undefined, failing, limit: 100 }),
  })
  const set = (next: { scorer?: string; failing?: boolean }) => {
    const s = next.scorer ?? scorer
    const f = next.failing ?? failing
    setParams({ ...(s ? { scorer: s } : {}), ...(f ? { failing: '1' } : {}) })
  }

  if (list.error) return <ApiDown error={list.error} />
  const totalCost = list.data?.scorers.reduce((a, s) => a + (s.stats?.cost_usd ?? 0), 0) ?? 0

  return (
    <>
      <PageHeader
        title="Scorers"
        description={
          <>
            LLM-as-judge checks on finished agent sessions, defined under <span className="font-mono">scorers:</span> in
            factory.yaml.
            {list.data ? (
              <>
                {' '}
                All-time scorer cost <span className="tabular-nums">{fmtUsd(totalCost, 4)}</span>.
              </>
            ) : null}
          </>
        }
      />
      {!list.data ? (
        <Skeleton className="h-40" />
      ) : list.data.scorers.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No scorers yet. Add a <span className="font-mono">scorers:</span> section to factory.yaml and run{' '}
          <span className="font-mono">wh serve --foreman</span> (or <span className="font-mono">wh scorer</span>).
        </p>
      ) : (
        <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
          {list.data.scorers.map((s) => (
            <ScorerCard key={s.id} s={s} byVersion={list.data.by_version} active={scorer === s.id} onSelect={() => set({ scorer: scorer === s.id ? '' : s.id })} />
          ))}
        </div>
      )}

      <div className="mt-8 mb-3 flex flex-wrap items-center gap-2">
        <h2 className="mr-2 text-base font-semibold">Recent results</h2>
        <button
          onClick={() => set({ scorer: '' })}
          className={cn('rounded-md px-2.5 py-1 text-sm text-muted-foreground hover:bg-muted', !scorer && 'bg-muted font-medium text-foreground')}
        >
          All scorers
        </button>
        {scorer ? <span className="rounded-md bg-muted px-2.5 py-1 font-mono text-sm">{scorer}</span> : null}
        <label className="ml-auto flex items-center gap-2 text-sm text-muted-foreground">
          <input type="checkbox" checked={failing} onChange={(e) => set({ failing: e.target.checked })} />
          Failing only
        </label>
      </div>
      {results.error ? (
        <ApiDown error={results.error} />
      ) : !results.data ? (
        <Skeleton className="h-40" />
      ) : results.data.length === 0 ? (
        <p className="text-sm text-muted-foreground">No results{failing ? ' that failed' : ''} yet.</p>
      ) : (
        <div className="overflow-hidden rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Result</TableHead>
                <TableHead>Task / session</TableHead>
                <TableHead className="hidden md:table-cell">Finding</TableHead>
                <TableHead className="hidden text-right sm:table-cell">Cost</TableHead>
                <TableHead className="text-right">When</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {results.data.map((r) => (
                <TableRow key={r.score_id}>
                  <TableCell className="align-top">
                    <ScoreChip r={r} />
                    <div className={cn('mt-1 text-xs', scoreResult(r).tone)}>{scoreResult(r).label}</div>
                  </TableCell>
                  <TableCell className="max-w-64 align-top">
                    <Link to={`/runs/${r.task_id}`} className="line-clamp-1 font-medium hover:underline">
                      {r.task_title ?? shortId(r.task_id)}
                    </Link>
                    <div className="text-xs text-muted-foreground">
                      {r.agent} · {r.stage} · <span className="font-mono">{shortId(r.target_session)}</span>
                    </div>
                  </TableCell>
                  <TableCell className="hidden max-w-xl align-top md:table-cell">
                    {r.classification ? <code className="rounded bg-muted px-1.5 py-0.5 text-xs">{r.classification}</code> : null}
                    <p className="mt-1 line-clamp-3 text-xs whitespace-normal text-muted-foreground">{r.rationale || r.error}</p>
                  </TableCell>
                  <TableCell className="hidden text-right align-top tabular-nums sm:table-cell">{fmtUsd(r.cost_usd, 4)}</TableCell>
                  <TableCell className="text-right align-top text-xs whitespace-nowrap text-muted-foreground">
                    {fmtAgo(r.started_at)}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}
    </>
  )
}

function ScorerCard({
  s,
  byVersion,
  active,
  onSelect,
}: {
  s: ScorersResponse['scorers'][number]
  byVersion: ScorersResponse['by_version']
  active: boolean
  onSelect: () => void
}) {
  const d = s.definition
  const st = s.stats
  const rate = st?.pass_rate ?? null
  const threshold = 0.8
  const versions = byVersion.filter((v) => v.scorer === s.id).slice(-4)
  return (
    <Card className={cn('gap-3', active && 'ring-2 ring-ring')}>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <button onClick={onSelect} className="font-mono hover:underline">
            {s.id}
          </button>
          {d ? (
            <span className="text-sm font-normal text-muted-foreground">→ {d.target}</span>
          ) : (
            <span className="rounded border px-1.5 text-xs font-normal text-muted-foreground">not in factory</span>
          )}
        </CardTitle>
        {d?.description ? <CardDescription>{d.description}</CardDescription> : null}
      </CardHeader>
      <CardContent className="grid gap-3">
        <div className="grid grid-cols-3 gap-2">
          <Metric
            label="Pass rate"
            value={fmtPct(rate)}
            tone={rate == null ? undefined : rate >= threshold ? 'text-ok' : rate < 0.5 ? 'text-fail' : 'text-warn'}
            hint={st ? `${st.passed}/${st.scored} graded` : 'not run yet'}
          />
          <Metric label="Last 7 days" value={fmtPct(st?.pass_rate_7d)} hint={`${st?.scored_7d ?? 0} graded`} />
          <Metric
            label="Cost"
            value={fmtUsd(st?.cost_usd ?? 0, 3)}
            hint={st?.avg_cost_usd != null ? `${fmtUsd(st.avg_cost_usd, 4)} each` : undefined}
          />
        </div>
        {rate != null ? (
          <div className="h-1.5 overflow-hidden rounded-full bg-fail/25">
            <div className="h-full bg-ok" style={{ width: `${rate * 100}%` }} />
          </div>
        ) : null}
        {versions.length > 1 ? (
          <div className="text-xs text-muted-foreground">
            <div className="mb-1">Pass rate by factory version of the graded session</div>
            <div className="flex flex-wrap gap-1">
              {versions.map((v) => (
                <span key={v.factory_version ?? 'none'} className="rounded border px-1.5 py-0.5 tabular-nums">
                  <span className="font-mono">{v.factory_version?.slice(0, 7) ?? 'manual'}</span>{' '}
                  {fmtPct(v.scored ? v.passed / v.scored : null)} <span className="opacity-70">n={v.scored}</span>
                </span>
              ))}
            </div>
          </div>
        ) : null}
        {d ? (
          <>
            <div className="text-xs text-muted-foreground tabular-nums">
              {d.model} · pass ≥ {d.pass_threshold} · samples {fmtPct(d.sampling_rate)} · budget {fmtUsd(d.budget_usd)}
              {d.include_failed ? ' · includes failed sessions' : ''}
              {st?.errors ? <span className="text-fail"> · {st.errors} errors</span> : null}
              {st?.last_run_at ? ` · last ${fmtAgo(st.last_run_at)}` : ''}
            </div>
            <details className="text-sm">
              <summary className="cursor-pointer text-muted-foreground">Rubric</summary>
              <pre className="mt-2 max-h-80 overflow-auto rounded-md bg-muted/60 p-3 text-xs whitespace-pre-wrap">{d.prompt}</pre>
            </details>
          </>
        ) : null}
      </CardContent>
    </Card>
  )
}

function Metric({ label, value, hint, tone }: { label: string; value: string; hint?: string; tone?: string }) {
  return (
    <div>
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className={cn('text-xl font-semibold tabular-nums', tone)}>{value}</div>
      {hint ? <div className="truncate text-xs text-muted-foreground">{hint}</div> : null}
    </div>
  )
}
