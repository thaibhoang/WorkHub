import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowLeft, Check, GitMerge, X } from 'lucide-react'
import { useState } from 'react'
import { Link, useParams } from 'react-router'
import { toast } from 'sonner'
import { PageHeader } from '@/components/PageHeader'
import { Delta, ProposalStatusBadge, proposalTitle, VERSION_METRICS } from '@/components/proposals'
import { SessionStatusBadge } from '@/components/status'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Label } from '@/components/ui/label'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Textarea } from '@/components/ui/textarea'
import { actorLabel, api, fmtMs, fmtTime, fmtUsd, shortId, type ProposalDetail as Detail, type ProposalRow } from '@/lib/api'
import { cn } from '@/lib/utils'
import { ApiDown } from './Runs'

export default function ProposalDetail() {
  const { id = '' } = useParams()
  const { data, error } = useQuery({
    queryKey: ['proposal', id],
    queryFn: () => api.proposal(id),
    refetchInterval: (q) => (['requested', 'running'].includes(q.state.data?.proposal?.status ?? '') ? 5000 : false),
  })
  if (error) return <ApiDown error={error} />
  if (!data) return <Skeleton className="h-64" />
  const p = data.proposal
  if (!p) return <p className="text-sm text-muted-foreground">Projection is catching up…</p>
  return (
    <>
      <Link to="/improvement" className="mb-3 inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground">
        <ArrowLeft className="size-4" /> Self-improvement
      </Link>
      <PageHeader
        title={proposalTitle(p)}
        description={
          <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
            <ProposalStatusBadge status={p.status} />
            <span>{p.trigger}</span>
            <span>by {p.requested_by}</span>
            <span>{fmtTime(p.requested_at)}</span>
            <span className="tabular-nums">{fmtUsd(p.cost_usd, 4)}</span>
            {p.duration_ms != null ? <span className="tabular-nums">{fmtMs(p.duration_ms)}</span> : null}
          </span>
        }
      />
      <div className="grid gap-4">
        <Actions p={p} />
        {p.status === 'requested' ? (
          <Note>Waiting for the improver. It runs in <code>wh serve --foreman</code> or <code>wh improver</code>.</Note>
        ) : p.status === 'running' ? (
          <Note>
            The improver is reading {p.sessions_observed ?? 0} sessions from factory version{' '}
            <code>{p.base_factory_version?.slice(0, 7)}</code>…
          </Note>
        ) : null}
        {p.focus ? <Section title="Focus requested">{p.focus}</Section> : null}
        {p.message ? (
          <Section title={p.status === 'failed' ? 'Why it failed' : 'Why no change'} tone={p.status === 'failed' ? 'text-fail' : undefined}>
            {p.message}
          </Section>
        ) : null}
        {p.rationale ? <Section title="Diagnosis and rationale">{p.rationale}</Section> : null}
        {p.expected_impact ? <Section title="Expected impact">{p.expected_impact}</Section> : null}
        {p.decision_comment ? <Section title={`Reviewer (${p.decided_by})`}>{p.decision_comment}</Section> : null}
        <Impact d={data} />
        {data.evidence.length ? <Evidence d={data} /> : null}
        {p.diff ? <Diff p={p} /> : null}
        <Events d={data} />
      </div>
    </>
  )
}

function Actions({ p }: { p: ProposalRow }) {
  const qc = useQueryClient()
  const [comment, setComment] = useState('')
  const done = (r: { message: string }) => {
    toast.success(r.message)
    for (const key of [['proposal', p.proposal_id], ['proposals'], ['improvement']]) qc.invalidateQueries({ queryKey: key })
  }
  const apply = useMutation({ mutationFn: () => api.applyProposal(p.proposal_id), onSuccess: done, onError: (e) => toast.error(e.message) })
  const decide = useMutation({
    mutationFn: (approved: boolean) => api.decideProposal(p.proposal_id, approved, comment || undefined),
    onSuccess: (r, approved) => {
      done(r)
      setComment('')
      if (approved) apply.mutate()
    },
    onError: (e) => toast.error(e.message),
  })
  const busy = decide.isPending || apply.isPending

  if (p.status === 'open') {
    return (
      <Card className="border-warn/50">
        <CardHeader>
          <CardTitle>Review this proposal</CardTitle>
          <CardDescription>
            Approving cherry-picks the commit from <code>{p.branch}</code> onto the factory repo's current branch. Restart the foreman
            afterwards to use version <code>{p.proposed_factory_version?.slice(0, 7)}</code>. When you reject, the improver reads your
            comment on its next run.
          </CardDescription>
        </CardHeader>
        <CardContent className="grid gap-4">
          <div className="grid gap-2">
            <Label htmlFor="p-comment">Comment (required to reject)</Label>
            <Textarea id="p-comment" rows={3} value={comment} onChange={(e) => setComment(e.target.value)} />
          </div>
          <div className="flex flex-wrap gap-2">
            <Button onClick={() => decide.mutate(true)} disabled={busy}>
              <Check /> Approve &amp; apply
            </Button>
            <Button variant="outline" onClick={() => decide.mutate(false)} disabled={busy || !comment.trim()}>
              <X /> Reject with comment
            </Button>
          </div>
        </CardContent>
      </Card>
    )
  }
  if (p.status === 'approved') {
    return (
      <Card className="border-info/50">
        <CardHeader>
          <CardTitle>Approved, not rolled out yet</CardTitle>
          <CardDescription>
            Apply cherry-picks <code>{p.commit_sha?.slice(0, 10)}</code> onto the factory repo. If you merge <code>{p.branch}</code> by
            hand, the rollout is detected when the foreman starts with version <code>{p.proposed_factory_version?.slice(0, 7)}</code>.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Button onClick={() => apply.mutate()} disabled={busy}>
            <GitMerge /> Apply now
          </Button>
        </CardContent>
      </Card>
    )
  }
  return null
}

function Impact({ d }: { d: Detail }) {
  const p = d.proposal!
  if (!['approved', 'rolled_out'].includes(p.status) || !d.before) return null
  const { before, after } = d
  return (
    <Card className="gap-3">
      <CardHeader>
        <CardTitle>Before → after</CardTitle>
        <CardDescription>
          Version <code>{before.factory_version.slice(0, 7)}</code> ({before.sessions} sessions) →{' '}
          {after ? (
            <>
              <code>{after.factory_version.slice(0, 7)}</code> ({after.sessions} sessions)
            </>
          ) : (
            <>
              <code>{(p.rolled_out_version ?? p.proposed_factory_version)?.slice(0, 7)}</code>, no finished sessions yet
            </>
          )}
          {after && after.sessions < 5 ? '. Too few sessions to draw a conclusion yet.' : ''}
        </CardDescription>
      </CardHeader>
      <CardContent className="overflow-x-auto">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Metric</TableHead>
              <TableHead className="text-right">Before</TableHead>
              <TableHead className="text-right">After</TableHead>
              <TableHead className="text-right">Change</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {VERSION_METRICS.map((m) => (
              <TableRow key={m.label}>
                <TableCell>{m.label}</TableCell>
                <TableCell className="text-right tabular-nums">{m.fmt(m.get(before))}</TableCell>
                <TableCell className="text-right tabular-nums">{after ? m.fmt(m.get(after)) : '–'}</TableCell>
                <TableCell className="text-right">
                  <Delta before={m.get(before)} after={after ? m.get(after) : null} m={m} />
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  )
}

function Evidence({ d }: { d: Detail }) {
  return (
    <Card className="gap-3">
      <CardHeader>
        <CardTitle>Evidence ({d.evidence.length} sessions)</CardTitle>
      </CardHeader>
      <CardContent className="grid gap-1.5 text-sm">
        {d.evidence.map((s) => (
          <Link
            key={s.session_id}
            to={`/runs/${s.task_id}`}
            className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md px-2 py-1 hover:bg-muted"
          >
            <span className="font-mono text-xs">{shortId(s.session_id)}</span>
            <span>
              {s.agent} · {s.stage}
            </span>
            <SessionStatusBadge status={s.status} />
            {s.stage_outcome === 'rejected' ? <span className="text-xs text-warn">verdict REJECT</span> : null}
            {s.failure_class ? <span className="text-xs text-fail">{s.failure_class}</span> : null}
            <span className="ml-auto text-xs text-muted-foreground tabular-nums">{fmtUsd(s.cost_usd, 4)}</span>
          </Link>
        ))}
      </CardContent>
    </Card>
  )
}

function Diff({ p }: { p: ProposalRow }) {
  return (
    <Card className="gap-3">
      <CardHeader>
        <CardTitle>Change</CardTitle>
        <CardDescription>
          {p.files.join(', ')} · branch <code>{p.branch}</code> · commit <code>{p.commit_sha?.slice(0, 10)}</code>
        </CardDescription>
      </CardHeader>
      <CardContent>
        <pre className="max-h-[32rem] overflow-auto rounded-md bg-muted/60 p-3 text-xs leading-5">
          {p.diff!.split('\n').map((line, i) => (
            <div
              key={i}
              className={cn(
                line.startsWith('+') && !line.startsWith('+++') && 'bg-ok/15 text-ok',
                line.startsWith('-') && !line.startsWith('---') && 'bg-fail/15 text-fail',
                line.startsWith('@@') && 'text-info',
              )}
            >
              {line || ' '}
            </div>
          ))}
        </pre>
      </CardContent>
    </Card>
  )
}

function Events({ d }: { d: Detail }) {
  return (
    <details className="text-sm">
      <summary className="cursor-pointer text-muted-foreground">Events ({d.events.length})</summary>
      <div className="mt-2 grid gap-1">
        {d.events.map((e) => (
          <div key={e.event_id} className="flex flex-wrap gap-x-3 text-xs">
            <span className="text-muted-foreground tabular-nums">{fmtTime(e.meta.occurred_at)}</span>
            <span className="font-medium">{e.type}</span>
            <span className="text-muted-foreground">{actorLabel(e.meta.actor)}</span>
            {e.meta.factory_version ? <span className="font-mono text-muted-foreground">{e.meta.factory_version.slice(0, 7)}</span> : null}
          </div>
        ))}
      </div>
    </details>
  )
}

function Section({ title, children, tone }: { title: string; children: React.ReactNode; tone?: string }) {
  return (
    <section>
      <h2 className="mb-1 text-sm font-medium">{title}</h2>
      <p className={cn('text-sm whitespace-pre-wrap text-muted-foreground', tone)}>{children}</p>
    </section>
  )
}

function Note({ children }: { children: React.ReactNode }) {
  return <p className="rounded-lg border border-dashed p-4 text-sm text-muted-foreground">{children}</p>
}
