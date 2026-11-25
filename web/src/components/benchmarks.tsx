import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Gauge } from 'lucide-react'
import { useState } from 'react'
import { Link, useNavigate } from 'react-router'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import {
  benchApi,
  fmtAgo,
  fmtMs,
  fmtPct,
  fmtUsd,
  type BenchComparison,
  type BenchRun,
  type BenchRunStatus,
  type CaseStats,
} from '@/lib/api'
import { cn } from '@/lib/utils'

const STATUS: Record<BenchRunStatus, { label: string; dot: string; text: string }> = {
  requested: { label: 'Queued', dot: 'bg-muted-foreground', text: 'text-muted-foreground' },
  running: { label: 'Running', dot: 'bg-info animate-pulse', text: 'text-info' },
  completed: { label: 'Completed', dot: 'bg-ok', text: 'text-ok' },
  failed: { label: 'Failed', dot: 'bg-fail', text: 'text-fail' },
}

export function BenchStatusBadge({ status }: { status: BenchRunStatus }) {
  const s = STATUS[status] ?? STATUS.requested
  return (
    <span className={cn('inline-flex items-center gap-1.5 text-xs font-medium whitespace-nowrap', s.text)}>
      <span className={cn('size-2 rounded-full', s.dot)} />
      {s.label}
    </span>
  )
}

/** "label 1a2b3c4 @ref" — what variant of the factory a run measured. */
export function Variant({ r }: { r: BenchRun }) {
  return (
    <span className="inline-flex flex-wrap items-baseline gap-x-1.5">
      {r.label ? <span className="font-medium">{r.label}</span> : null}
      <code className="text-xs">{r.factory_version?.slice(0, 7) ?? '…'}</code>
      <span className="text-xs text-muted-foreground">{r.git_ref ? `@${r.git_ref}` : 'working tree'}</span>
    </span>
  )
}

export function passText(r: BenchRun) {
  const total = r.status === 'completed' ? r.cases_done : (r.planned ?? r.cases_done)
  return `${r.passed}/${total}`
}

export function BenchRunsTable({ rows }: { rows: BenchRun[] }) {
  return (
    <div className="overflow-x-auto rounded-lg border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Run</TableHead>
            <TableHead>Variant</TableHead>
            <TableHead>Status</TableHead>
            <TableHead className="text-right">Passed</TableHead>
            <TableHead className="text-right">Pass rate</TableHead>
            <TableHead className="hidden text-right md:table-cell">Avg / case</TableHead>
            <TableHead className="hidden text-right md:table-cell">p50 case</TableHead>
            <TableHead className="text-right">Cost</TableHead>
            <TableHead className="text-right">Requested</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((r) => (
            <TableRow key={r.run_id}>
              <TableCell>
                <Link to={`/benchmarks/${r.run_id}`} className="font-medium hover:underline">
                  {r.suite}
                </Link>
                <div className="font-mono text-xs text-muted-foreground">
                  {r.run_id.slice(-8)}
                  {r.repeat > 1 ? ` · ×${r.repeat}` : ''}
                </div>
              </TableCell>
              <TableCell>
                <Variant r={r} />
              </TableCell>
              <TableCell>
                <BenchStatusBadge status={r.status} />
              </TableCell>
              <TableCell className="text-right tabular-nums">{passText(r)}</TableCell>
              <TableCell className="text-right tabular-nums">{fmtPct(r.pass_rate)}</TableCell>
              <TableCell className="hidden text-right tabular-nums md:table-cell">{fmtUsd(r.avg_case_cost_usd, 3)}</TableCell>
              <TableCell className="hidden text-right tabular-nums md:table-cell">{fmtMs(r.p50_case_duration_ms)}</TableCell>
              <TableCell className="text-right tabular-nums">{fmtUsd(r.cost_usd, 3)}</TableCell>
              <TableCell className="text-right text-xs whitespace-nowrap text-muted-foreground">{fmtAgo(r.requested_at)}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  )
}

/**
 * Queue a benchmark run. With `proposal`, the server queues two runs: the
 * proposal's parent commit ("base") and its commit ("proposal").
 */
export function RunBenchmarkDialog({ proposal, label = 'Run benchmark' }: { proposal?: string; label?: string }) {
  const [open, setOpen] = useState(false)
  const [suite, setSuite] = useState('')
  const [gitRef, setGitRef] = useState('')
  const [repeat, setRepeat] = useState('1')
  const qc = useQueryClient()
  const navigate = useNavigate()
  const { data: suites } = useQuery({ queryKey: ['benchmarks', 'suites'], queryFn: benchApi.suites, enabled: open })
  const valid = suites?.filter((s) => !s.error) ?? []
  const chosen = suite || valid[0]?.key || ''
  const picked = valid.find((s) => s.key === chosen)
  const run = useMutation({
    mutationFn: () =>
      benchApi.request({
        suite: chosen,
        proposal,
        git_ref: proposal ? undefined : gitRef.trim() || undefined,
        repeat: Number(repeat) || 1,
      }),
    onSuccess: (r) => {
      qc.invalidateQueries({ queryKey: ['benchmarks'] })
      setOpen(false)
      toast.success(r.message)
      if (!proposal) navigate(`/benchmarks/${r.run_ids[0]}`)
    },
    onError: (e) => toast.error(e.message),
  })
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant={proposal ? 'outline' : 'default'}>
          <Gauge /> {label}
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <form
          className="grid gap-4"
          onSubmit={(e) => {
            e.preventDefault()
            run.mutate()
          }}
        >
          <DialogHeader>
            <DialogTitle>{proposal ? 'Benchmark this proposal' : 'Run a benchmark'}</DialogTitle>
            <DialogDescription>
              {proposal
                ? 'Runs the suite twice: on the factory before the proposal (its parent commit) and with the proposal applied. Compare the two runs before you approve.'
                : 'Every case runs through the whole pipeline as a real task. Gates are approved automatically, and these tasks stay out of the live metrics.'}{' '}
              Agents spend real money: about cases × repeat × the cost of a task.
            </DialogDescription>
          </DialogHeader>
          {suites && valid.length === 0 ? (
            <p className="text-sm text-fail">
              No valid suites in the server's benchmark directory (<code>--bench-dir</code>, default <code>benchmarks/</code>).
            </p>
          ) : (
            <div className="grid gap-2">
              <Label>Suite</Label>
              <Select value={chosen} onValueChange={setSuite}>
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="Loading…" />
                </SelectTrigger>
                <SelectContent>
                  {valid.map((s) => (
                    <SelectItem key={s.key} value={s.key}>
                      {s.name} · {s.cases.length} cases
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {picked?.description ? <p className="text-xs text-muted-foreground">{picked.description}</p> : null}
              {picked?.max_cost_usd ? (
                <p className="text-xs text-muted-foreground">Stops starting new cases after {fmtUsd(picked.max_cost_usd)}.</p>
              ) : null}
            </div>
          )}
          <div className="grid grid-cols-[1fr_6rem] gap-3">
            {proposal ? (
              <div />
            ) : (
              <div className="grid gap-2">
                <Label htmlFor="b-ref">Factory at git ref</Label>
                <Input id="b-ref" value={gitRef} onChange={(e) => setGitRef(e.target.value)} placeholder="working tree (e.g. main, HEAD~1)" />
              </div>
            )}
            <div className="grid gap-2">
              <Label htmlFor="b-repeat">Repeat</Label>
              <Input id="b-repeat" type="number" min={1} max={20} value={repeat} onChange={(e) => setRepeat(e.target.value)} />
            </div>
          </div>
          <DialogFooter>
            <Button type="submit" disabled={run.isPending || !chosen}>
              {run.isPending ? 'Queuing…' : 'Queue run'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function rate(s: CaseStats | null) {
  return s && s.runs ? s.passed / s.runs : null
}

function DeltaPt({ a, b, lowerIsBetter = false, usd = false }: { a: number | null; b: number | null; lowerIsBetter?: boolean; usd?: boolean }) {
  if (a == null || b == null) return <span className="text-muted-foreground">–</span>
  const d = b - a
  if (Math.abs(d) < 1e-9) return <span className="text-muted-foreground">±0</span>
  const better = lowerIsBetter ? d < 0 : d > 0
  return (
    <span className={cn('tabular-nums', better ? 'text-ok' : 'text-fail')}>
      {d > 0 ? '▲' : '▼'} {usd ? fmtUsd(Math.abs(d), 3) : `${(Math.abs(d) * 100).toFixed(0)} pt`}
    </span>
  )
}

/** Case-by-case comparison of a baseline (A) and a candidate (B) run. */
export function BenchComparisonTable({ c }: { c: BenchComparison }) {
  return (
    <div className="grid gap-2">
      {!c.comparable ? (
        <p className="text-sm text-warn">
          The runs used different suite files or suite versions, so their numbers are not directly comparable.
        </p>
      ) : null}
      <div className="overflow-x-auto rounded-lg border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Case</TableHead>
              <TableHead className="text-right">A pass</TableHead>
              <TableHead className="text-right">B pass</TableHead>
              <TableHead className="text-right">Δ pass</TableHead>
              <TableHead className="text-right">A avg cost</TableHead>
              <TableHead className="text-right">B avg cost</TableHead>
              <TableHead className="hidden text-right md:table-cell">A time</TableHead>
              <TableHead className="hidden text-right md:table-cell">B time</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {c.cases.map((row) => (
              <TableRow key={row.case_id}>
                <TableCell className="font-medium">{row.case_id}</TableCell>
                <TableCell className="text-right tabular-nums">{row.a ? `${row.a.passed}/${row.a.runs}` : '–'}</TableCell>
                <TableCell className="text-right tabular-nums">{row.b ? `${row.b.passed}/${row.b.runs}` : '–'}</TableCell>
                <TableCell className="text-right">
                  <DeltaPt a={rate(row.a)} b={rate(row.b)} />
                </TableCell>
                <TableCell className="text-right tabular-nums">{fmtUsd(row.a?.avg_cost_usd, 3)}</TableCell>
                <TableCell className="text-right tabular-nums">
                  {fmtUsd(row.b?.avg_cost_usd, 3)}
                  <div className="text-xs">
                    <DeltaPt a={row.a?.avg_cost_usd ?? null} b={row.b?.avg_cost_usd ?? null} lowerIsBetter usd />
                  </div>
                </TableCell>
                <TableCell className="hidden text-right tabular-nums md:table-cell">{fmtMs(row.a?.avg_duration_ms)}</TableCell>
                <TableCell className="hidden text-right tabular-nums md:table-cell">{fmtMs(row.b?.avg_duration_ms)}</TableCell>
              </TableRow>
            ))}
            <TableRow className="font-medium">
              <TableCell>All cases</TableCell>
              <TableCell className="text-right tabular-nums">{fmtPct(c.a.pass_rate)}</TableCell>
              <TableCell className="text-right tabular-nums">{fmtPct(c.b.pass_rate)}</TableCell>
              <TableCell className="text-right">
                <DeltaPt a={c.a.pass_rate} b={c.b.pass_rate} />
              </TableCell>
              <TableCell className="text-right tabular-nums">{fmtUsd(c.a.avg_case_cost_usd, 3)}</TableCell>
              <TableCell className="text-right tabular-nums">
                {fmtUsd(c.b.avg_case_cost_usd, 3)}
                <div className="text-xs">
                  <DeltaPt a={c.a.avg_case_cost_usd} b={c.b.avg_case_cost_usd} lowerIsBetter usd />
                </div>
              </TableCell>
              <TableCell className="hidden text-right tabular-nums md:table-cell">{fmtMs(c.a.p50_case_duration_ms)}</TableCell>
              <TableCell className="hidden text-right tabular-nums md:table-cell">{fmtMs(c.b.p50_case_duration_ms)}</TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </div>
    </div>
  )
}
