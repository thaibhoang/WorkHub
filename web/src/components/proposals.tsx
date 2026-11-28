import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Sparkles } from 'lucide-react'
import { useState } from 'react'
import { useNavigate } from 'react-router'
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
import { Label } from '@/components/ui/label'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Textarea } from '@/components/ui/textarea'
import { api, firstLine, fmtAgo, fmtPct, fmtUsd, shortId, type ProposalRow, type ProposalStatus, type VersionStats } from '@/lib/api'
import { cn } from '@/lib/utils'

const STATUS: Record<ProposalStatus, { label: string; dot: string; text: string }> = {
  requested: { label: 'Queued', dot: 'bg-muted-foreground', text: 'text-muted-foreground' },
  running: { label: 'Running', dot: 'bg-info animate-pulse', text: 'text-info' },
  open: { label: 'Needs review', dot: 'bg-warn', text: 'text-warn' },
  no_proposal: { label: 'No change', dot: 'bg-muted-foreground/50', text: 'text-muted-foreground' },
  failed: { label: 'Failed', dot: 'bg-fail', text: 'text-fail' },
  approved: { label: 'Approved', dot: 'bg-info', text: 'text-info' },
  rejected: { label: 'Rejected', dot: 'bg-muted-foreground/50', text: 'text-muted-foreground' },
  withdrawn: { label: 'Withdrawn', dot: 'bg-muted-foreground/50', text: 'text-muted-foreground' },
  rolled_out: { label: 'Rolled out', dot: 'bg-ok', text: 'text-ok' },
}

export function ProposalStatusBadge({ status }: { status: ProposalStatus }) {
  const s = STATUS[status] ?? STATUS.requested
  return (
    <span className={cn('inline-flex items-center gap-1.5 text-xs font-medium whitespace-nowrap', s.text)}>
      <span className={cn('size-2 rounded-full', s.dot)} />
      {s.label}
    </span>
  )
}

export function proposalTitle(p: ProposalRow): string {
  if (p.title) return p.title
  if (p.status === 'requested' || p.status === 'running') return p.focus ? `Improver run: ${firstLine(p.focus)}` : 'Improver run'
  return p.message ? firstLine(p.message) : 'No proposal'
}

export function ProposalsTable({ rows }: { rows: ProposalRow[] }) {
  const navigate = useNavigate()
  return (
    <div className="overflow-hidden rounded-lg border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Proposal</TableHead>
            <TableHead>Status</TableHead>
            <TableHead className="hidden lg:table-cell">Trigger</TableHead>
            <TableHead className="hidden text-right sm:table-cell">Cost</TableHead>
            <TableHead className="text-right">Requested</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((p) => (
            <TableRow key={p.proposal_id} className="cursor-pointer" onClick={() => navigate(`/proposals/${p.proposal_id}`)}>
              <TableCell className="w-full max-w-0">
                <div className={cn('truncate font-medium', !p.title && 'text-muted-foreground')}>{proposalTitle(p)}</div>
                <div className="truncate text-xs text-muted-foreground">
                  <span className="font-mono">{shortId(p.proposal_id)}</span>
                  {` · ${p.factory}`}
                  {p.files.length ? ` · ${p.files.join(', ')}` : ''}
                  {p.base_factory_version ? (
                    <>
                      {' · '}
                      <span className="font-mono">
                        {p.base_factory_version.slice(0, 7)}
                        {p.proposed_factory_version ? ` → ${p.proposed_factory_version.slice(0, 7)}` : ''}
                      </span>
                    </>
                  ) : null}
                </div>
              </TableCell>
              <TableCell>
                <ProposalStatusBadge status={p.status} />
              </TableCell>
              <TableCell className="hidden max-w-56 truncate text-xs text-muted-foreground lg:table-cell">{p.trigger}</TableCell>
              <TableCell className="hidden text-right tabular-nums sm:table-cell">{fmtUsd(p.cost_usd, 3)}</TableCell>
              <TableCell className="text-right whitespace-nowrap text-muted-foreground">{fmtAgo(p.requested_at)}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  )
}

export function RunImproverDialog() {
  const [open, setOpen] = useState(false)
  const [focus, setFocus] = useState('')
  const qc = useQueryClient()
  const navigate = useNavigate()
  const { data: factory } = useQuery({ queryKey: ['factory'], queryFn: api.factory, staleTime: Infinity })
  const im = factory?.improvement
  const run = useMutation({
    mutationFn: () => api.requestImprovement(focus || undefined),
    onSuccess: (r) => {
      qc.invalidateQueries({ queryKey: ['proposals'] })
      setOpen(false)
      setFocus('')
      toast.success(r.message)
      navigate(`/proposals/${r.proposal_id}`)
    },
    onError: (e) => toast.error(e.message),
  })
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button disabled={!im}>
          <Sparkles /> Run improver
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
            <DialogTitle>Run the improver</DialogTitle>
            <DialogDescription>
              {im
                ? `${im.model} reads the last ${im.window_days} days of metrics, failures, scorer findings and feedback, and may propose one change to the factory (budget ${fmtUsd(im.budget_usd)}). It runs in the foreman process.`
                : 'The loaded factory has no improvement section.'}
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <Label htmlFor="focus">Focus (optional)</Label>
            <Textarea
              id="focus"
              rows={4}
              value={focus}
              onChange={(e) => setFocus(e.target.value)}
              placeholder="e.g. Why does code review reject so often?"
            />
          </div>
          <DialogFooter>
            <Button type="submit" disabled={run.isPending}>
              {run.isPending ? 'Requesting…' : 'Request run'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

type Metric = {
  label: string
  get: (v: VersionStats) => number | null
  fmt: (x: number | null) => string
  kind: 'rate' | 'usd'
  /** true when a lower value is an improvement */
  lowerIsBetter: boolean
}

export const VERSION_METRICS: Metric[] = [
  { label: 'Fail rate', get: (v) => v.fail_rate, fmt: (x) => fmtPct(x), kind: 'rate', lowerIsBetter: true },
  { label: 'Reject rate', get: (v) => v.reject_rate, fmt: (x) => fmtPct(x), kind: 'rate', lowerIsBetter: true },
  { label: 'Avg cost / session', get: (v) => v.avg_cost_usd, fmt: (x) => fmtUsd(x, 3), kind: 'usd', lowerIsBetter: true },
  { label: 'Tool error rate', get: (v) => v.tool_error_rate, fmt: (x) => fmtPct(x), kind: 'rate', lowerIsBetter: true },
  { label: 'Scorer pass rate', get: (v) => (v.scored ? v.score_pass_rate : null), fmt: (x) => fmtPct(x), kind: 'rate', lowerIsBetter: false },
]

/** Coloured change between two values of a metric. */
export function Delta({ before, after, m }: { before: number | null; after: number | null; m: Metric }) {
  if (before == null || after == null) return <span className="text-muted-foreground">–</span>
  const d = after - before
  if (Math.abs(d) < 1e-9) return <span className="text-muted-foreground">±0</span>
  const better = m.lowerIsBetter ? d < 0 : d > 0
  const pct = m.kind === 'usd' ? fmtUsd(Math.abs(d), 3) : `${(Math.abs(d) * 100).toFixed(0)} pt`
  return (
    <span className={cn('tabular-nums', better ? 'text-ok' : 'text-fail')}>
      {d > 0 ? '▲' : '▼'} {pct}
    </span>
  )
}
