import { useQuery } from '@tanstack/react-query'
import { PageHeader } from '@/components/PageHeader'
import { Delta, ProposalsTable, RunImproverDialog, VERSION_METRICS } from '@/components/proposals'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { api, fmtAgo, fmtMs, fmtPct, fmtUsd, type ImprovementResponse } from '@/lib/api'
import { cn } from '@/lib/utils'
import { ApiDown } from './Runs'

/** Self-improvement: the improver's proposals, its own track record, and metrics per factory version. */
export default function Improvement() {
  const info = useQuery({ queryKey: ['improvement'], queryFn: api.improvement })
  const list = useQuery({ queryKey: ['proposals', 'all'], queryFn: () => api.proposals() })
  if (info.error) return <ApiDown error={info.error} />
  const d = info.data
  return (
    <>
      <PageHeader
        title="Self-improvement"
        description="An agent reads the factory's metrics and feedback and proposes changes to factory.yaml and its prompts. A human approves every change, and its effect shows up in the next factory version."
        actions={<RunImproverDialog />}
      />
      {!d ? (
        <Skeleton className="h-28" />
      ) : (
        <>
          <ImproverCards d={d} />
          {!d.config ? (
            <p className="mt-3 text-sm text-muted-foreground">
              The loaded factory has no <span className="font-mono">improvement:</span> section, so the improver is off.
            </p>
          ) : null}
        </>
      )}

      <h2 className="mt-8 mb-3 text-base font-semibold">Proposals</h2>
      {list.error ? (
        <ApiDown error={list.error} />
      ) : !list.data ? (
        <Skeleton className="h-40" />
      ) : list.data.length === 0 ? (
        <p className="rounded-lg border border-dashed p-10 text-center text-sm text-muted-foreground">
          No improver runs yet. Use <span className="font-medium">Run improver</span> or{' '}
          <span className="font-mono">wh improve</span>. Triggers and the schedule start runs automatically.
        </p>
      ) : (
        <ProposalsTable rows={list.data} />
      )}

      <h2 className="mt-8 mb-1 text-base font-semibold">Metrics by factory version</h2>
      <p className="mb-3 text-sm text-muted-foreground">
        Every change to factory.yaml or a prompt produces a new version. Each row is compared with the one before it.
      </p>
      {d ? <VersionsTable d={d} /> : <Skeleton className="h-32" />}
    </>
  )
}

function ImproverCards({ d }: { d: ImprovementResponse }) {
  const s = d.stats
  const c = d.config
  return (
    <div className="grid gap-3 md:grid-cols-[2fr_3fr]">
      <Card className="gap-3">
        <CardHeader>
          <CardTitle>Improver track record</CardTitle>
          <CardDescription>{s.last_run_at ? `Last run ${fmtAgo(s.last_run_at)}` : 'Has not run yet'}</CardDescription>
        </CardHeader>
        <CardContent className="grid grid-cols-3 gap-2">
          <Stat
            label="Approval rate"
            value={fmtPct(s.approval_rate)}
            hint={`${s.approved}/${s.approved + s.rejected} decided`}
          />
          <Stat label="Rolled out" value={String(s.rolled_out)} hint={`${s.proposals} proposed · ${s.open} open`} />
          <Stat label="Cost" value={fmtUsd(s.cost_usd, 3)} hint={`${s.runs} runs · ${s.no_proposal} no-change · ${s.failed} failed`} />
        </CardContent>
      </Card>
      <Card className="gap-3">
        <CardHeader>
          <CardTitle>When it runs</CardTitle>
          <CardDescription>
            {c ? (
              <>
                {c.model} · budget {fmtUsd(c.budget_usd)} · looks at {c.window_days} days · at least {c.min_sessions} sessions
                · at most {c.max_open_proposals} open at a time
              </>
            ) : (
              'Off'
            )}
          </CardDescription>
        </CardHeader>
        {c ? (
          <CardContent className="grid gap-2 text-sm">
            <div className="flex flex-wrap gap-1.5">
              <Chip>on request</Chip>
              {c.interval_hours ? <Chip>every {c.interval_hours}h when there are new sessions</Chip> : null}
              {c.triggers.map((t) => (
                <Chip key={`${t.metric}${t.subject}`}>
                  <span className="font-mono">{t.metric}</span> of {t.subject} &gt; {t.metric.endsWith('_usd') ? fmtUsd(t.above) : fmtPct(t.above)}
                </Chip>
              ))}
            </div>
            <p className="text-xs text-muted-foreground">Each trigger fires at most once per factory version.</p>
            <details>
              <summary className="cursor-pointer text-muted-foreground">Improver instructions</summary>
              <pre className="mt-2 max-h-80 overflow-auto rounded-md bg-muted/60 p-3 text-xs whitespace-pre-wrap">{c.prompt}</pre>
            </details>
          </CardContent>
        ) : null}
      </Card>
    </div>
  )
}

function VersionsTable({ d }: { d: ImprovementResponse }) {
  const rows = d.versions
  if (rows.length === 0) return <p className="text-sm text-muted-foreground">No finished sessions with a factory version yet.</p>
  return (
    <div className="overflow-x-auto rounded-lg border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Version</TableHead>
            <TableHead className="text-right">Sessions</TableHead>
            {VERSION_METRICS.map((m) => (
              <TableHead key={m.label} className="text-right">
                {m.label}
              </TableHead>
            ))}
            <TableHead className="hidden text-right lg:table-cell">p50</TableHead>
            <TableHead className="text-right">First seen</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {[...rows].reverse().map((v, i, arr) => {
            const prev = arr[i + 1]
            return (
              <TableRow key={v.factory_version}>
                <TableCell className="font-mono text-xs whitespace-nowrap">
                  {v.factory_version.slice(0, 7)}
                  {v.factory_version === d.current_version ? (
                    <span className="ml-2 rounded bg-primary px-1.5 py-0.5 font-sans text-[10px] text-primary-foreground">current</span>
                  ) : null}
                </TableCell>
                <TableCell className="text-right tabular-nums">
                  {v.sessions}
                  <div className="text-xs text-muted-foreground">{v.tasks} tasks</div>
                </TableCell>
                {VERSION_METRICS.map((m) => (
                  <TableCell key={m.label} className="text-right tabular-nums">
                    {m.fmt(m.get(v))}
                    {prev ? (
                      <div className="text-xs">
                        <Delta before={m.get(prev)} after={m.get(v)} m={m} />
                      </div>
                    ) : null}
                  </TableCell>
                ))}
                <TableCell className="hidden text-right tabular-nums lg:table-cell">{fmtMs(v.p50_duration_ms)}</TableCell>
                <TableCell className="text-right text-xs whitespace-nowrap text-muted-foreground">{fmtAgo(v.first_at)}</TableCell>
              </TableRow>
            )
          })}
        </TableBody>
      </Table>
    </div>
  )
}

function Stat({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="min-w-0">
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="text-xl font-semibold tabular-nums">{value}</div>
      {hint ? <div className="text-xs text-muted-foreground">{hint}</div> : null}
    </div>
  )
}

function Chip({ children, className }: { children: React.ReactNode; className?: string }) {
  return <span className={cn('rounded-md border px-2 py-0.5 text-xs', className)}>{children}</span>
}
