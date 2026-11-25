import { useQuery } from '@tanstack/react-query'
import { ArrowLeft } from 'lucide-react'
import { Link, useParams } from 'react-router'
import { BenchStatusBadge, passText, Variant } from '@/components/benchmarks'
import { PageHeader } from '@/components/PageHeader'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { benchApi, fmtMs, fmtPct, fmtTime, fmtUsd, type BenchCase, type BenchRun } from '@/lib/api'
import { cn } from '@/lib/utils'
import { ApiDown } from './Runs'

export default function BenchmarkRun() {
  const { id = '' } = useParams()
  const { data, error } = useQuery({
    queryKey: ['benchmarks', 'run', id],
    queryFn: () => benchApi.run(id),
    refetchInterval: (q) => (['requested', 'running'].includes(q.state.data?.run?.status ?? '') ? 5000 : false),
  })
  if (error) return <ApiDown error={error} />
  if (!data) return <Skeleton className="h-64" />
  const r = data.run
  if (!r) return <p className="text-sm text-muted-foreground">Projection is catching up…</p>
  return (
    <>
      <Link to="/benchmarks" className="mb-3 inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground">
        <ArrowLeft className="size-4" /> Benchmarks
      </Link>
      <PageHeader
        title={`${r.suite} benchmark`}
        description={
          <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
            <BenchStatusBadge status={r.status} />
            <Variant r={r} />
            <span>by {r.requested_by}</span>
            <span>{fmtTime(r.requested_at)}</span>
            {r.proposal_id ? (
              <Link to={`/proposals/${r.proposal_id}`} className="hover:underline">
                proposal {r.proposal_id.slice(-8)}
              </Link>
            ) : null}
          </span>
        }
      />
      <div className="grid gap-4">
        {r.status === 'requested' ? (
          <Note>
            Queued. It runs in <code>wh serve --foreman</code>, <code>wh foreman</code> or <code>wh bench worker</code>, one run at a
            time.
          </Note>
        ) : null}
        {r.message ? <Note tone={r.status === 'failed' ? 'text-fail' : 'text-warn'}>{r.message}</Note> : null}
        <Summary r={r} />
        {data.cases.length ? <Cases cases={data.cases} /> : null}
      </div>
    </>
  )
}

function Summary({ r }: { r: BenchRun }) {
  const stats = [
    { label: 'Passed', value: passText(r), hint: fmtPct(r.pass_rate) },
    { label: 'Cost', value: fmtUsd(r.cost_usd, 3), hint: `${fmtUsd(r.avg_case_cost_usd, 3)} / case` },
    { label: 'p50 case time', value: fmtMs(r.p50_case_duration_ms), hint: r.duration_ms ? `${fmtMs(r.duration_ms)} total` : undefined },
    { label: 'Avg rework loops', value: r.avg_rework == null ? '–' : r.avg_rework.toFixed(1) },
  ]
  return (
    <Card className="gap-3">
      <CardHeader>
        <CardTitle>Result</CardTitle>
      </CardHeader>
      <CardContent className="grid grid-cols-2 gap-3 md:grid-cols-4">
        {stats.map((s) => (
          <div key={s.label}>
            <div className="text-xs text-muted-foreground">{s.label}</div>
            <div className="text-xl font-semibold tabular-nums">{s.value}</div>
            {s.hint ? <div className="text-xs text-muted-foreground">{s.hint}</div> : null}
          </div>
        ))}
        <div className="col-span-full text-xs text-muted-foreground">
          factory {r.factory ?? '–'} · version <code>{r.factory_version ?? '–'}</code>
          {r.factory_commit ? (
            <>
              {' '}
              · commit <code>{r.factory_commit.slice(0, 10)}</code>
            </>
          ) : null}{' '}
          · suite version <code>{r.suite_version ?? '–'}</code> · {r.suite_file}
        </div>
      </CardContent>
    </Card>
  )
}

const CASE_TEXT: Record<BenchCase['status'], string> = {
  running: 'text-info',
  done: 'text-ok',
  blocked: 'text-fail',
  cancelled: 'text-muted-foreground',
  error: 'text-fail',
}

function Cases({ cases }: { cases: BenchCase[] }) {
  return (
    <div className="overflow-x-auto rounded-lg border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Case</TableHead>
            <TableHead>Pipeline</TableHead>
            <TableHead>Result</TableHead>
            <TableHead className="text-right">Cost</TableHead>
            <TableHead className="text-right">Time</TableHead>
            <TableHead className="hidden text-right md:table-cell">Sessions</TableHead>
            <TableHead className="hidden text-right md:table-cell">Rework</TableHead>
            <TableHead>Details</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {cases.map((c) => (
            <TableRow key={`${c.case_id}-${c.repeat}`} className="align-top">
              <TableCell>
                <Link to={`/runs/${c.task_id}`} className="font-medium hover:underline">
                  {c.case_id}
                </Link>
                <span className="ml-1 text-xs text-muted-foreground">#{c.repeat}</span>
              </TableCell>
              <TableCell className={cn('text-xs font-medium', CASE_TEXT[c.status])}>{c.status}</TableCell>
              <TableCell className="text-xs font-medium">
                {c.passed == null ? (
                  <span className="text-muted-foreground">…</span>
                ) : c.passed ? (
                  <span className="text-ok">pass</span>
                ) : (
                  <span className="text-fail">fail</span>
                )}
              </TableCell>
              <TableCell className="text-right tabular-nums">{fmtUsd(c.cost_usd, 3)}</TableCell>
              <TableCell className="text-right tabular-nums">{fmtMs(c.duration_ms)}</TableCell>
              <TableCell className="hidden text-right tabular-nums md:table-cell">{c.sessions ?? '–'}</TableCell>
              <TableCell className="hidden text-right tabular-nums md:table-cell">{c.rework_loops ?? '–'}</TableCell>
              <TableCell className="max-w-md text-xs">
                {c.message ? <div className="whitespace-pre-wrap text-muted-foreground">{c.message}</div> : null}
                {c.check_output ? (
                  <details className="mt-1">
                    <summary className="cursor-pointer text-muted-foreground">
                      check output{c.check_exit_code != null ? ` (exit ${c.check_exit_code})` : ''}
                    </summary>
                    <pre className="mt-1 max-h-64 overflow-auto rounded bg-muted/60 p-2 whitespace-pre-wrap">{c.check_output}</pre>
                  </details>
                ) : null}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  )
}

function Note({ children, tone }: { children: React.ReactNode; tone?: string }) {
  return <p className={cn('rounded-lg border border-dashed p-4 text-sm text-muted-foreground', tone)}>{children}</p>
}
