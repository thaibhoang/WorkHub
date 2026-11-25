import { useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import { Bar, BarChart, CartesianGrid, XAxis, YAxis } from 'recharts'
import { PageHeader } from '@/components/PageHeader'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import {
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from '@/components/ui/chart'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { api, fmtMs, fmtUsd, type Dashboard as DashboardData, type Stats } from '@/lib/api'
import { cn } from '@/lib/utils'
import { ApiDown } from './Runs'

const RANGES = [7, 30, 90] as const

const costConfig = { cost_usd: { label: 'Cost', color: 'var(--info)' } } satisfies ChartConfig
const sessionConfig = {
  sessions_succeeded: { label: 'Succeeded', color: 'var(--ok)' },
  sessions_failed: { label: 'Failed', color: 'var(--fail)' },
} satisfies ChartConfig

export default function Dashboard() {
  const [days, setDays] = useState<number>(30)
  const dash = useQuery({ queryKey: ['dashboard', days], queryFn: () => api.dashboard(days) })
  const stats = useQuery({ queryKey: ['stats'], queryFn: api.stats })

  return (
    <>
      <PageHeader
        title="Dashboard"
        description="Throughput, spend and failure rate, derived from the event log."
        actions={
          <div className="flex rounded-md border p-0.5">
            {RANGES.map((d) => (
              <button
                key={d}
                onClick={() => setDays(d)}
                className={cn(
                  'rounded px-2.5 py-1 text-sm text-muted-foreground',
                  days === d && 'bg-muted font-medium text-foreground',
                )}
              >
                {d}d
              </button>
            ))}
          </div>
        }
      />
      {dash.error ? (
        <ApiDown error={dash.error} />
      ) : !dash.data ? (
        <Skeleton className="h-96" />
      ) : (
        <div className="grid gap-4">
          <Kpis d={dash.data} />
          {dash.data.cycle.shipped ? <PrCycle d={dash.data} /> : null}
          <div className="grid gap-4 lg:grid-cols-2">
            <Card>
              <CardHeader>
                <CardTitle>Spend per day</CardTitle>
                <CardDescription>Agent session cost (USD, UTC days)</CardDescription>
              </CardHeader>
              <CardContent>
                <ChartContainer config={costConfig} className="h-56 w-full">
                  <BarChart data={dash.data.daily} margin={{ left: 4, right: 4 }}>
                    <CartesianGrid vertical={false} strokeOpacity={0.5} />
                    <XAxis dataKey="day" tickLine={false} axisLine={false} tickFormatter={shortDay} minTickGap={16} />
                    <YAxis tickLine={false} axisLine={false} width={44} tickFormatter={(v) => `$${v}`} />
                    <ChartTooltip
                      cursor={{ fillOpacity: 0.4 }}
                      content={<ChartTooltipContent labelFormatter={longDay} formatter={(v) => fmtUsd(Number(v), 4)} />}
                    />
                    <Bar isAnimationActive={false} dataKey="cost_usd" fill="var(--color-cost_usd)" radius={[4, 4, 0, 0]} maxBarSize={28} />
                  </BarChart>
                </ChartContainer>
              </CardContent>
            </Card>
            <Card>
              <CardHeader>
                <CardTitle>Sessions per day</CardTitle>
                <CardDescription>Finished agent sessions by outcome</CardDescription>
              </CardHeader>
              <CardContent>
                <ChartContainer config={sessionConfig} className="h-56 w-full">
                  <BarChart data={dash.data.daily} margin={{ left: 4, right: 4 }}>
                    <CartesianGrid vertical={false} strokeOpacity={0.5} />
                    <XAxis dataKey="day" tickLine={false} axisLine={false} tickFormatter={shortDay} minTickGap={16} />
                    <YAxis tickLine={false} axisLine={false} width={44} allowDecimals={false} />
                    <ChartTooltip cursor={{ fillOpacity: 0.4 }} content={<ChartTooltipContent labelFormatter={longDay} />} />
                    <ChartLegend content={<ChartLegendContent />} />
                    <Bar
                      isAnimationActive={false}
                      dataKey="sessions_succeeded"
                      stackId="s"
                      fill="var(--color-sessions_succeeded)"
                      stroke="var(--card)"
                      strokeWidth={2}
                      maxBarSize={28}
                    />
                    <Bar
                      isAnimationActive={false}
                      dataKey="sessions_failed"
                      stackId="s"
                      fill="var(--color-sessions_failed)"
                      stroke="var(--card)"
                      strokeWidth={2}
                      radius={[4, 4, 0, 0]}
                      maxBarSize={28}
                    />
                  </BarChart>
                </ChartContainer>
              </CardContent>
            </Card>
          </div>
          {stats.data ? <StatsTables s={stats.data} /> : <Skeleton className="h-48" />}
        </div>
      )}
    </>
  )
}

/** Pull request cycle times (only shown once a task has shipped to GitHub). */
function PrCycle({ d }: { d: DashboardData }) {
  const c = d.cycle
  const pct = (n: number) => `${((n / c.shipped) * 100).toFixed(0)}%`
  const tiles = [
    { label: 'Kickoff → PR', p50: c.kickoff_to_pr_p50_ms, p95: c.kickoff_to_pr_p95_ms },
    { label: 'PR → first review', p50: c.pr_to_first_review_p50_ms, p95: c.pr_to_first_review_p95_ms },
    { label: 'First review → merge', p50: c.review_to_merge_p50_ms, p95: c.review_to_merge_p95_ms },
    { label: 'Kickoff → merge', p50: c.kickoff_to_merge_p50_ms, p95: c.kickoff_to_merge_p95_ms },
  ]
  return (
    <Card>
      <CardHeader>
        <CardTitle>Pull requests</CardTitle>
        <CardDescription>
          {c.shipped} shipped · {c.merged} merged · {c.open} open · changes requested on {pct(c.changes_requested_tasks)} ·
          CI failed on {pct(c.ci_failed_tasks)} · {c.avg_pr_rework == null ? '–' : c.avg_pr_rework.toFixed(2)} PR rework
          loops / PR
        </CardDescription>
      </CardHeader>
      <CardContent className="grid grid-cols-2 gap-3 md:grid-cols-4">
        {tiles.map((t) => (
          <div key={t.label} className="rounded-lg border p-3">
            <div className="text-xs text-muted-foreground">{t.label} p50</div>
            <div className="mt-1 text-xl font-semibold tabular-nums">{fmtMs(t.p50)}</div>
            <div className="mt-0.5 text-xs text-muted-foreground tabular-nums">p95 {fmtMs(t.p95)}</div>
          </div>
        ))}
      </CardContent>
    </Card>
  )
}

function Kpis({ d }: { d: DashboardData }) {
  const t = d.totals
  const failRate = t.sessions ? t.sessions_failed / t.sessions : null
  const tiles: { label: string; value: string; hint?: string; tone?: string }[] = [
    { label: 'Tasks completed', value: String(t.tasks_done), hint: `${t.tasks_created} created` },
    { label: 'Cycle time p50', value: fmtMs(t.cycle_p50_ms), hint: `p90 ${fmtMs(t.cycle_p90_ms)} · kickoff → done` },
    {
      label: 'Spend',
      value: fmtUsd(t.cost_usd),
      hint: t.tasks_done ? `${fmtUsd(t.cost_usd / t.tasks_done)} per completed task` : 'agent sessions (scorers excluded)',
    },
    {
      label: 'Session failure rate',
      value: failRate == null ? '–' : `${(failRate * 100).toFixed(1)}%`,
      hint: `${t.sessions_failed} of ${t.sessions} sessions`,
      tone: failRate && failRate > 0.25 ? 'text-fail' : undefined,
    },
    { label: 'Rework loops / task', value: t.avg_rework == null ? '–' : t.avg_rework.toFixed(2), hint: `${t.gate_rejections} gate rejections` },
    {
      label: 'Scorer pass rate',
      value: t.scores ? `${((t.scores_passed / t.scores) * 100).toFixed(0)}%` : '–',
      hint: `${t.scores_passed}/${t.scores} graded · ${fmtUsd(t.scorer_cost_usd, 3)} judge cost`,
      tone: t.scores && t.scores_passed / t.scores < 0.7 ? 'text-warn' : undefined,
    },
    {
      label: 'Needs a human',
      value: String(t.awaiting_gate + t.tasks_blocked),
      hint: `${t.awaiting_gate} at a gate · ${t.tasks_blocked} blocked`,
      tone: t.awaiting_gate + t.tasks_blocked ? 'text-warn' : undefined,
    },
  ]
  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-4 2xl:grid-cols-7">
      {tiles.map((k) => (
        <Card key={k.label} className="gap-1 py-4">
          <CardContent className="px-4">
            <div className="text-xs text-muted-foreground">{k.label}</div>
            <div className={cn('mt-1 text-2xl font-semibold tabular-nums', k.tone)}>{k.value}</div>
            {k.hint ? <div className="mt-0.5 truncate text-xs text-muted-foreground">{k.hint}</div> : null}
          </CardContent>
        </Card>
      ))}
    </div>
  )
}

function pct(n: number, d: number) {
  return d ? `${((n / d) * 100).toFixed(0)}%` : '–'
}

function StatsTables({ s }: { s: Stats }) {
  return (
    <div className="grid gap-4 xl:grid-cols-[2fr_1fr]">
      <Card>
        <CardHeader>
          <CardTitle>By stage</CardTitle>
          <CardDescription>All finished sessions (all time)</CardDescription>
        </CardHeader>
        <CardContent className="overflow-x-auto">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Stage</TableHead>
                <TableHead className="text-right">Runs</TableHead>
                <TableHead className="text-right">Fail</TableHead>
                <TableHead className="text-right">Retries</TableHead>
                <TableHead className="text-right">Avg cost</TableHead>
                <TableHead className="text-right">Total</TableHead>
                <TableHead className="text-right">p50</TableHead>
                <TableHead className="text-right">p95</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {s.stages.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={8} className="text-center text-muted-foreground">
                    No finished sessions yet.
                  </TableCell>
                </TableRow>
              ) : (
                s.stages.map((r) => (
                  <TableRow key={r.stage}>
                    <TableCell className="font-mono text-xs">{r.stage}</TableCell>
                    <TableCell className="text-right tabular-nums">{r.runs}</TableCell>
                    <TableCell className={cn('text-right tabular-nums', r.failed && 'text-fail')}>{pct(r.failed, r.runs)}</TableCell>
                    <TableCell className="text-right tabular-nums">{r.retries}</TableCell>
                    <TableCell className="text-right tabular-nums">{fmtUsd(r.avg_cost_usd, 3)}</TableCell>
                    <TableCell className="text-right tabular-nums">{fmtUsd(r.total_cost_usd)}</TableCell>
                    <TableCell className="text-right tabular-nums">{fmtMs(r.p50_duration_ms)}</TableCell>
                    <TableCell className="text-right tabular-nums">{fmtMs(r.p95_duration_ms)}</TableCell>
                  </TableRow>
                ))
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle>Failures by class</CardTitle>
          <CardDescription>What went wrong, from the failure taxonomy</CardDescription>
        </CardHeader>
        <CardContent>
          {s.failures.length === 0 ? (
            <p className="text-sm text-muted-foreground">No failures recorded.</p>
          ) : (
            <ul className="grid gap-2">
              {s.failures.map((f) => {
                const max = s.failures[0].count
                return (
                  <li key={f.class} className="grid grid-cols-[8rem_1fr_2rem] items-center gap-2 text-sm">
                    <span className="truncate font-mono text-xs">{f.class}</span>
                    <span className="h-2 rounded-full bg-muted">
                      <span className="block h-2 rounded-full bg-fail" style={{ width: `${(f.count / max) * 100}%` }} />
                    </span>
                    <span className="text-right tabular-nums">{f.count}</span>
                  </li>
                )
              })}
            </ul>
          )}
        </CardContent>
      </Card>
      <Card className="xl:col-span-2">
        <CardHeader>
          <CardTitle>By agent</CardTitle>
        </CardHeader>
        <CardContent className="overflow-x-auto">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Agent</TableHead>
                <TableHead className="text-right">Runs</TableHead>
                <TableHead className="text-right">Fail</TableHead>
                <TableHead className="text-right">Avg cost</TableHead>
                <TableHead className="text-right">p50</TableHead>
                <TableHead className="text-right">Avg turns</TableHead>
                <TableHead className="text-right">Tool errors</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {s.agents.map((a) => (
                <TableRow key={a.agent}>
                  <TableCell>{a.agent}</TableCell>
                  <TableCell className="text-right tabular-nums">{a.runs}</TableCell>
                  <TableCell className={cn('text-right tabular-nums', a.failed && 'text-fail')}>{pct(a.failed, a.runs)}</TableCell>
                  <TableCell className="text-right tabular-nums">{fmtUsd(a.avg_cost_usd, 3)}</TableCell>
                  <TableCell className="text-right tabular-nums">{fmtMs(a.p50_duration_ms)}</TableCell>
                  <TableCell className="text-right tabular-nums">{a.avg_turns?.toFixed(1) ?? '–'}</TableCell>
                  <TableCell className="text-right tabular-nums">{pct(a.tool_errors, a.tool_calls)}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>
    </div>
  )
}

function shortDay(d: string) {
  return new Date(`${d}T00:00:00Z`).toLocaleDateString(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' })
}

function longDay(d: unknown) {
  return new Date(`${String(d)}T00:00:00Z`).toLocaleDateString(undefined, {
    weekday: 'short',
    month: 'short',
    day: 'numeric',
    timeZone: 'UTC',
  })
}
