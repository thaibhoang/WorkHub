import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowLeft, Check, ChevronRight, FolderTree, Play, Plus, X } from 'lucide-react'
import { useState } from 'react'
import { Link, useParams } from 'react-router'
import { toast } from 'sonner'
import { NewTaskDialog } from '@/components/NewTaskDialog'
import { PageHeader } from '@/components/PageHeader'
import { ScoreChip } from '@/components/scores'
import { SessionSheet } from '@/components/SessionSheet'
import { OUTCOME_TEXT, PrLink, SessionStatusBadge, TaskStatusBadge } from '@/components/status'
import { Timeline } from '@/components/Timeline'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
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
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs'
import { Textarea } from '@/components/ui/textarea'
import {
  actorLabel,
  api,
  fmtMs,
  fmtTime,
  fmtUsd,
  shortId,
  subtreeIds,
  taskTree,
  type FactoryDef,
  type ScoreRow,
  type SessionDetail,
  type TaskDetail,
  type WhEvent,
} from '@/lib/api'
import { cn } from '@/lib/utils'
import { ApiDown } from './Runs'

export default function RunDetail() {
  const { id = '' } = useParams()
  const { data, isLoading, error } = useQuery({ queryKey: ['task', id], queryFn: () => api.task(id) })
  const { data: factory } = useQuery({ queryKey: ['factory'], queryFn: api.factory, staleTime: Infinity })
  const [selected, setSelected] = useState<SessionDetail | null>(null)

  if (error) return <ApiDown error={error} />
  if (isLoading || !data) return <Skeleton className="h-64" />

  const { task, state, events, sessions, scores } = data
  const graded = scores.filter((r) => r.status === 'scored')
  const passed = graded.filter((r) => r.pass).length
  const title = task?.title ?? events[0]?.payload.title ?? 'Task'
  const pipeline = factory && task?.factory === factory.name ? factory : null
  const cost = sessions.reduce((a, s) => a + s.cost_usd, 0)
  // Keep the open sheet in sync with live updates.
  const current = selected ? (sessions.find((s) => s.session_id === selected.session_id) ?? selected) : null

  return (
    <>
      <Link to="/" className="mb-3 inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground">
        <ArrowLeft className="size-4" /> Runs
      </Link>
      {data.ancestors.length ? (
        <nav className="mb-1 flex flex-wrap items-center gap-1 text-sm text-muted-foreground">
          {data.ancestors.map((a) => (
            <span key={a.task_id} className="flex items-center gap-1">
              <Link to={`/runs/${a.task_id}`} className="max-w-64 truncate hover:text-foreground hover:underline">
                {a.title}
              </Link>
              <ChevronRight className="size-3.5" />
            </span>
          ))}
        </nav>
      ) : null}
      <PageHeader
        title={title}
        description={
          <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
            <TaskStatusBadge status={state.status} />
            <span className="font-mono">{shortId(id)}</span>
            <span>{task?.factory ?? 'manual task'}</span>
            <span className="tabular-nums">{fmtUsd(cost)} · {sessions.length} sessions</span>
            {state.rework_loops ? <span className="text-warn">{state.rework_loops} rework loops</span> : null}
            {state.external_ref ? (
              <a href={state.external_ref.url} target="_blank" rel="noreferrer" className="font-mono hover:underline">
                {state.external_ref.repo}#{state.external_ref.issue}
              </a>
            ) : null}
            {state.pr ? (
              <PrLink url={state.pr.url} number={state.pr.number} state={task?.pr_state ?? (state.pr.closed ? 'closed' : 'open')} />
            ) : null}
            {graded.length ? (
              <span className={cn('tabular-nums', passed < graded.length && 'text-fail')}>
                {passed}/{graded.length} scores passed
              </span>
            ) : null}
          </span>
        }
        actions={
          <>
            {task?.benchmark_run ? null : <MoveDialog id={id} parent={state.parent} />}
            <CancelButton id={id} status={state.status} />
          </>
        }
      />

      {task?.benchmark_run ? (
        <p className="mb-4 rounded-lg border border-dashed p-3 text-sm text-muted-foreground">
          Benchmark case <span className="font-mono">{task.benchmark_case}</span> of{' '}
          <Link to={`/benchmarks/${task.benchmark_run}`} className="font-medium text-foreground hover:underline">
            run {shortId(task.benchmark_run)}
          </Link>
          . Gates are approved automatically, and this task is left out of the live metrics and the improver.
        </p>
      ) : null}
      {pipeline ? <PipelineStrip factory={pipeline} detail={data} /> : null}
      <ActionPanel key={id} id={id} detail={data} factory={pipeline} />
      <PrPanel detail={data} />
      {task?.benchmark_run ? null : <Subtasks id={id} title={title} detail={data} />}

      <Tabs defaultValue="timeline" className="mt-6">
        <TabsList>
          <TabsTrigger value="timeline">Timeline</TabsTrigger>
          <TabsTrigger value="outputs">Stage outputs</TabsTrigger>
          <TabsTrigger value="sessions">Sessions</TabsTrigger>
          <TabsTrigger value="events">Events</TabsTrigger>
        </TabsList>
        <TabsContent value="timeline" className="grid gap-6 pt-3">
          <Timeline sessions={sessions} events={events} onSelect={setSelected} />
          <EventLog events={events} />
        </TabsContent>
        <TabsContent value="outputs" className="pt-3">
          <StageOutputs detail={data} />
        </TabsContent>
        <TabsContent value="sessions" className="pt-3">
          <SessionsTable sessions={sessions} scores={scores} onSelect={setSelected} />
        </TabsContent>
        <TabsContent value="events" className="pt-3">
          <RawEvents events={events} />
        </TabsContent>
      </Tabs>
      <SessionSheet session={current} scores={scores} onClose={() => setSelected(null)} />
    </>
  )
}

function PipelineStrip({ factory, detail }: { factory: FactoryDef; detail: TaskDetail }) {
  const { state } = detail
  const last = new Map(state.results.map((r) => [r.stage, r]))
  const current = state.status === 'done' || state.status === 'cancelled' ? null : state.stage
  return (
    <ol className="mb-4 flex flex-wrap items-center gap-1 text-sm">
      {factory.stages.map((st, i) => {
        const r = last.get(st.id)
        const isCurrent = current === st.id
        const tone = isCurrent
          ? state.status === 'blocked'
            ? 'border-fail text-fail'
            : state.status === 'awaiting_gate'
              ? 'border-warn text-warn'
              : 'border-info text-info'
          : r
            ? r.outcome === 'succeeded'
              ? 'border-ok/50 text-ok'
              : r.outcome === 'rejected'
                ? 'border-warn/50 text-warn'
                : 'border-fail/50 text-fail'
            : 'border-dashed text-muted-foreground'
        return (
          <li key={st.id} className="flex items-center gap-1">
            {i > 0 ? <span className="text-muted-foreground">→</span> : null}
            <span className={cn('rounded-md border px-2 py-0.5 font-mono text-xs', tone, isCurrent && 'font-semibold')}>
              {st.id}
              {st.gate ? <span className="ml-1 opacity-70">⏸</span> : null}
            </span>
          </li>
        )
      })}
      {factory.github ? (
        <li className="flex items-center gap-1">
          <span className="text-muted-foreground">→</span>
          <span
            className={cn(
              'rounded-md border px-2 py-0.5 font-mono text-xs',
              current === 'ship'
                ? state.status === 'blocked'
                  ? 'border-fail text-fail font-semibold'
                  : 'border-warn text-warn font-semibold'
                : state.pr
                  ? state.status === 'done'
                    ? 'border-ok/50 text-ok'
                    : 'border-info/60 text-info'
                  : 'border-dashed text-muted-foreground',
            )}
          >
            ship
            {factory.github.ship_gate ? <span className="ml-1 opacity-70">⏸</span> : null}
          </span>
          <span className="text-muted-foreground">→</span>
          <span
            className={cn(
              'rounded-md border px-2 py-0.5 font-mono text-xs',
              state.status === 'done' && state.pr ? 'border-ok/50 text-ok' : 'border-dashed text-muted-foreground',
            )}
          >
            merge
          </span>
        </li>
      ) : null}
      {state.status === 'done' ? <span className="ml-2 text-xs text-ok">✓ complete</span> : null}
    </ol>
  )
}

function useCommand<T>(id: string, fn: (arg: T) => Promise<{ message: string }>, onSent?: (arg: T) => void) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: fn,
    onSuccess: (r, arg) => {
      toast.success(r.message)
      onSent?.(arg)
      // Returned so that `isPending` lasts until the fresh task data has arrived.
      return Promise.all([
        qc.invalidateQueries({ queryKey: ['task', id] }),
        qc.invalidateQueries({ queryKey: ['tasks'] }),
      ])
    },
    onError: (e) => toast.error(e.message),
  })
}

function ActionPanel({ id, detail, factory }: { id: string; detail: TaskDetail; factory: FactoryDef | null }) {
  const { state } = detail
  const [comment, setComment] = useState('')
  const [stage, setStage] = useState<string>('')
  // Task version a command succeeded on. The refetch in `useCommand` can be cancelled by a
  // live update, so the buttons also stay disabled while the data is still at that version.
  const [sentAt, setSentAt] = useState<number | null>(null)
  const sent = sentAt === state.version
  const gate = useCommand(
    id,
    (d: { approved: boolean; version: number }) => api.decideGate(id, d.approved, comment || undefined),
    (d) => setSentAt(d.version),
  )
  const resume = useCommand(id, (_version: number) => api.resume(id, stage || undefined, comment || undefined), setSentAt)
  const reset = () => setComment('')
  const start = useCommand(id, (subtree: boolean) => api.startTask(id, subtree))

  if (state.status === 'awaiting_gate') {
    const output =
      state.stage === 'ship' || state.stage === 'conflict' ? state.results.at(-1) : [...state.results].reverse().find((r) => r.stage === state.stage)
    return (
      <Card className="border-warn/50">
        <CardHeader>
          <CardTitle>Review required: {state.gate}</CardTitle>
          <CardDescription>
            {state.stage === 'conflict' ? (
              <>
                The pull request conflicts with its base branch. Approve to let WorkHub merge the base and have{' '}
                <span className="font-mono">{factory?.github?.rework_stage ?? 'the agent'}</span> resolve the conflicts,
                or reject to block the task.
              </>
            ) : state.stage === 'ship' ? (
              <>
                Every stage passed. Approve to push the branch and open a pull request on GitHub, or reject to send it
                back to <span className="font-mono">{factory?.github?.rework_stage ?? 'the agent'}</span> with your
                comment.
              </>
            ) : (
              <>
                Stage <span className="font-mono">{state.stage}</span> finished. Approve to continue the pipeline, or
                reject to send it back to the agent with your comment.
              </>
            )}
          </CardDescription>
        </CardHeader>
        <CardContent className="grid gap-4">
          {output?.summary ? <Summary text={output.summary} /> : null}
          {state.gate_decided ? (
            <p className="text-sm text-muted-foreground">Decision recorded, waiting for the foreman.</p>
          ) : (
            <>
              <div className="grid gap-2">
                <Label htmlFor="gate-comment">Comment (required to reject)</Label>
                <Textarea id="gate-comment" rows={3} value={comment} onChange={(e) => setComment(e.target.value)} />
              </div>
              <div className="flex flex-wrap gap-2">
                <Button
                  onClick={() => gate.mutate({ approved: true, version: state.version }, { onSuccess: reset })}
                  disabled={gate.isPending || sent}
                >
                  <Check /> Approve
                </Button>
                <Button
                  variant="outline"
                  onClick={() => gate.mutate({ approved: false, version: state.version }, { onSuccess: reset })}
                  disabled={gate.isPending || sent || !comment.trim()}
                >
                  <X /> Reject with comment
                </Button>
              </div>
            </>
          )}
        </CardContent>
      </Card>
    )
  }

  if (state.status === 'draft') {
    const draftKids = detail.children.some((c) => c.status === 'draft')
    return (
      <Card className="border-dashed">
        <CardHeader>
          <CardTitle>Draft, not started</CardTitle>
          <CardDescription>
            {factory
              ? `Nothing runs until you start it. Starting hands it to the ${factory.name} pipeline (${factory.stages[0]?.id ?? 'first stage'} first).`
              : 'Nothing runs until you start it.'}
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-wrap gap-2">
          <Button onClick={() => start.mutate(false)} disabled={start.isPending}>
            <Play /> Start
          </Button>
          {draftKids ? (
            <Button variant="outline" onClick={() => start.mutate(true)} disabled={start.isPending}>
              <Play /> Start with subtasks
            </Button>
          ) : null}
        </CardContent>
      </Card>
    )
  }

  if (state.status === 'blocked') {
    return (
      <Card className="border-fail/50">
        <CardHeader>
          <CardTitle>Blocked{state.blocked_stage ? ` at ${state.blocked_stage}` : ''}</CardTitle>
          <CardDescription>A human needs to decide how to continue.</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-4">
          {state.blocked_reason ? <Summary text={state.blocked_reason} /> : null}
          <div className="grid gap-4 sm:grid-cols-[12rem_1fr]">
            <div className="grid content-start gap-2">
              <Label>Restart at</Label>
              <Select value={stage || '__default'} onValueChange={(v) => setStage(v === '__default' ? '' : v)}>
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="__default">{state.blocked_stage ?? 'blocked stage'} (default)</SelectItem>
                  {[...(factory?.stages.map((s) => s.id) ?? []), ...(factory?.github ? ['ship'] : [])]
                    .filter((s) => s !== state.blocked_stage)
                    .map((s) => (
                      <SelectItem key={s} value={s}>
                        {s === 'ship' ? 'ship (push + PR, no agent)' : s}
                      </SelectItem>
                    ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid gap-2">
              <Label htmlFor="resume-comment">Guidance for the agent (optional)</Label>
              <Textarea id="resume-comment" rows={3} value={comment} onChange={(e) => setComment(e.target.value)} />
            </div>
          </div>
          <div>
            <Button onClick={() => resume.mutate(state.version, { onSuccess: reset })} disabled={resume.isPending || sent}>
              <Play /> Resume
            </Button>
          </div>
        </CardContent>
      </Card>
    )
  }
  return null
}

/** The task's pull request: what humans and CI said, and the PR rework loop. */
function PrPanel({ detail }: { detail: TaskDetail }) {
  const { state, task, events } = detail
  if (!state.pr) return null
  const pr = state.pr
  const prEvents = events.filter((e) => e.type.startsWith('Pr') || e.type === 'TaskShipped')
  return (
    <Card className="mt-4">
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          Pull request <PrLink url={pr.url} number={pr.number} state={task?.pr_state ?? null} />
        </CardTitle>
        <CardDescription>
          <span className="font-mono">{pr.branch}</span> → {pr.repo}
          {task ? (
            <span className="ml-2 tabular-nums">
              · {task.pr_reviews} reviews · {task.changes_requested} changes requested · {task.ci_failures} CI failures ·{' '}
              {state.pr_rework_loops} PR rework loops
            </span>
          ) : null}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <ul className="grid gap-1.5 text-sm">
          {prEvents.map((e) => (
            <li key={e.seq} className="flex gap-3">
              <span className="w-16 shrink-0 text-xs tabular-nums text-muted-foreground">{fmtTime(e.meta.occurred_at)}</span>
              <span className="min-w-0">{prEventText(e)}</span>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  )
}

function prEventText(e: WhEvent) {
  const p = e.payload
  switch (e.type) {
    case 'TaskShipped':
      return <>{p.created ? 'Opened' : 'Pushed'} <span className="font-mono text-xs">{String(p.commit).slice(0, 10)}</span></>
    case 'PrReviewed': {
      const tone = p.state === 'CHANGES_REQUESTED' ? 'text-warn' : p.state === 'APPROVED' ? 'text-ok' : 'text-muted-foreground'
      return (
        <>
          <span className={tone}>{String(p.state).toLowerCase().replace('_', ' ')}</span> by {p.reviewer}
          {p.body ? <span className="text-muted-foreground">: {String(p.body).split('\n')[0]}</span> : null}
          {p.comments?.length ? <span className="text-muted-foreground"> (+{p.comments.length} inline)</span> : null}
        </>
      )
    }
    case 'PrChecksCompleted':
      return p.conclusion === 'success' ? (
        <span className="text-ok">CI passed</span>
      ) : (
        <>
          <span className="text-fail">CI failed</span>
          {p.failed?.length ? `: ${p.failed.map((c: { name: string }) => c.name).join(', ')}` : null}
        </>
      )
    case 'PrConflicted':
      return (
        <span className="text-warn">
          Conflicts with {p.base} at <span className="font-mono text-xs">{String(p.commit).slice(0, 10)}</span>
        </span>
      )
    case 'PrMerged':
      return <span className="text-ok">Merged{p.merged_by ? ` by ${p.merged_by}` : ''}</span>
    case 'PrClosed':
      return <span className="text-muted-foreground">Closed without merging</span>
    default:
      return e.type
  }
}

/** Direct subtasks, a way to add one, and starting the drafts below this task. */
function Subtasks({ id, title, detail }: { id: string; title: string; detail: TaskDetail }) {
  const { data: all } = useQuery({ queryKey: ['tasks'], queryFn: () => api.tasks() })
  const startAll = useCommand(id, () => api.startTask(id, true))
  // Whole subtree from the task list (the detail only has direct children).
  const below = all ? taskTree(all.filter((t) => subtreeIds(all, id).has(t.task_id) && t.task_id !== id)) : []
  const rows = below.length ? below : detail.children.map((task) => ({ task, depth: 0 }))
  const drafts = rows.filter((r) => r.task.status === 'draft').length
  const add = (
    <NewTaskDialog
      parent={{ id, title }}
      trigger={
        <Button variant="outline" size="sm">
          <Plus /> Add subtask
        </Button>
      }
    />
  )
  if (!rows.length) return <div className="mt-4">{add}</div>
  return (
    <Card className="mt-4">
      <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
        <div>
          <CardTitle>Subtasks</CardTitle>
          <CardDescription>
            {rows.length} below this task{drafts ? ` · ${drafts} not started` : ''}
          </CardDescription>
        </div>
        <div className="flex gap-2">
          {drafts ? (
            <Button size="sm" onClick={() => startAll.mutate(undefined)} disabled={startAll.isPending}>
              <Play /> Start {detail.state.status === 'draft' ? 'all' : `${drafts} draft${drafts > 1 ? 's' : ''}`}
            </Button>
          ) : null}
          {add}
        </div>
      </CardHeader>
      <CardContent>
        <ul className="grid gap-1">
          {rows.map(({ task: c, depth }) => (
            <li key={c.task_id} className="flex items-center gap-3 text-sm" style={{ paddingLeft: `${depth * 1.25}rem` }}>
              <TaskStatusBadge status={c.status} className="w-24 shrink-0" />
              <Link to={`/runs/${c.task_id}`} className="truncate hover:underline">
                {c.title}
              </Link>
              <span className="ml-auto font-mono text-xs text-muted-foreground">{c.current_stage ?? ''}</span>
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  )
}

/** Move the task under another parent (or to the top level). Tasks below it are not offered. */
function MoveDialog({ id, parent }: { id: string; parent: string | null }) {
  const [open, setOpen] = useState(false)
  const [target, setTarget] = useState<string>(parent ?? '__root')
  const { data: all } = useQuery({ queryKey: ['tasks'], queryFn: () => api.tasks(), enabled: open })
  const move = useCommand(id, (p: string | null) => api.moveTask(id, p), () => setOpen(false))
  const banned = all ? subtreeIds(all, id) : new Set([id])
  const options = all ? taskTree(all).filter(({ task }) => !banned.has(task.task_id)) : []
  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o)
        if (o) setTarget(parent ?? '__root')
      }}
    >
      <DialogTrigger asChild>
        <Button variant="ghost" size="sm" className="text-muted-foreground">
          <FolderTree /> Move
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Move task</DialogTitle>
          <DialogDescription>Pick its new parent. Tasks form a tree, so this task and its subtasks are not offered.</DialogDescription>
        </DialogHeader>
        <Select value={target} onValueChange={setTarget}>
          <SelectTrigger className="w-full">
            <SelectValue />
          </SelectTrigger>
          <SelectContent className="max-h-80">
            <SelectItem value="__root">Top level (no parent)</SelectItem>
            {options.map(({ task, depth }) => (
              <SelectItem key={task.task_id} value={task.task_id}>
                <span style={{ paddingLeft: `${depth}rem` }} className="truncate">
                  {task.title} <span className="font-mono text-xs text-muted-foreground">{shortId(task.task_id)}</span>
                </span>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <DialogFooter>
          <Button
            onClick={() => move.mutate(target === '__root' ? null : target)}
            disabled={move.isPending || target === (parent ?? '__root')}
          >
            Move
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function CancelButton({ id, status }: { id: string; status: string }) {
  const cancel = useCommand(id, () => api.cancel(id, 'cancelled from the UI'))
  if (status === 'done' || status === 'cancelled') return null
  return (
    <Button
      variant="ghost"
      size="sm"
      className="text-muted-foreground"
      disabled={cancel.isPending}
      onClick={() => {
        if (window.confirm('Cancel this task? A running stage finishes, but nothing further is dispatched.')) cancel.mutate(undefined)
      }}
    >
      Cancel task
    </Button>
  )
}

function Summary({ text }: { text: string }) {
  return (
    <pre className="max-h-96 overflow-auto rounded-md bg-muted/60 p-3 font-sans text-sm whitespace-pre-wrap">{text}</pre>
  )
}

function StageOutputs({ detail }: { detail: TaskDetail }) {
  const { state } = detail
  return (
    <div className="grid gap-4">
      {state.body ? (
        <section>
          <h3 className="mb-1 text-sm font-medium">Task</h3>
          <Summary text={state.body} />
        </section>
      ) : null}
      {state.results.length === 0 ? <p className="text-sm text-muted-foreground">No stage has finished yet.</p> : null}
      {state.results.map((r, i) => (
        <section key={i}>
          <h3 className="mb-1 flex items-center gap-2 text-sm font-medium">
            <span className="font-mono">{r.stage}</span>
            <span className={cn('text-xs', OUTCOME_TEXT[r.outcome])}>{r.outcome}</span>
          </h3>
          {r.summary ? <Summary text={r.summary} /> : <p className="text-sm text-muted-foreground">No summary.</p>}
        </section>
      ))}
    </div>
  )
}

function SessionsTable({
  sessions,
  scores,
  onSelect,
}: {
  sessions: SessionDetail[]
  scores: ScoreRow[]
  onSelect: (s: SessionDetail) => void
}) {
  if (sessions.length === 0) return <p className="text-sm text-muted-foreground">No agent sessions yet.</p>
  return (
    <div className="overflow-hidden rounded-lg border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Agent</TableHead>
            <TableHead>Status</TableHead>
            <TableHead className="hidden sm:table-cell">Model</TableHead>
            <TableHead className="text-right">Cost</TableHead>
            <TableHead className="hidden text-right md:table-cell">Turns</TableHead>
            <TableHead className="hidden text-right md:table-cell">Tools</TableHead>
            <TableHead className="text-right">Duration</TableHead>
            <TableHead className="hidden lg:table-cell">Scores</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {sessions.map((s) => (
            <TableRow key={s.session_id} className="cursor-pointer" onClick={() => onSelect(s)}>
              <TableCell>
                <div className="font-medium">{s.agent}</div>
                <div className="text-xs text-muted-foreground">
                  {s.stage} · try {s.attempt}
                </div>
              </TableCell>
              <TableCell>
                <SessionStatusBadge status={s.status} />
                {s.failure_class ? <div className="text-xs text-fail">{s.failure_class}</div> : null}
              </TableCell>
              <TableCell className="hidden font-mono text-xs sm:table-cell">{s.model ?? '–'}</TableCell>
              <TableCell className="text-right tabular-nums">{fmtUsd(s.cost_usd, 3)}</TableCell>
              <TableCell className="hidden text-right tabular-nums md:table-cell">{s.num_turns ?? '–'}</TableCell>
              <TableCell className="hidden text-right tabular-nums md:table-cell">
                {s.tool_calls}
                {s.tool_errors ? <span className="text-fail"> ({s.tool_errors}✗)</span> : null}
              </TableCell>
              <TableCell className="text-right tabular-nums">{fmtMs(s.duration_ms)}</TableCell>
              <TableCell className="hidden lg:table-cell">
                <div className="flex flex-wrap gap-1">
                  {scores
                    .filter((r) => r.target_session === s.session_id)
                    .map((r) => (
                      <ScoreChip key={r.score_id} r={r} />
                    ))}
                </div>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  )
}

function describe(e: WhEvent): string {
  const p = e.payload
  switch (e.type) {
    case 'TaskCreated':
      return p.factory ? `pipeline ${p.factory}` : 'manual task'
    case 'StageEntered':
      return `${p.stage}${p.feedback ? ' (rework)' : ''}`
    case 'StageExited':
      return `${p.stage} → ${p.outcome}`
    case 'GateRequested':
      return `${p.gate} after ${p.stage}`
    case 'GateDecided':
      return `${p.gate} ${p.approved ? 'approved' : 'rejected'} by ${p.reviewer}${p.comment ? `: ${p.comment}` : ''}`
    case 'TaskBlocked':
      return p.reason?.split('\n')[0] ?? ''
    case 'TaskResumed':
      return [p.stage, p.comment].filter(Boolean).join(': ')
    case 'TaskCompleted':
      return p.branch ?? ''
    case 'TaskCancelled':
      return p.reason ?? ''
    default:
      return ''
  }
}

const EVENT_TONE: Record<string, string> = {
  StageExited: 'text-foreground',
  GateRequested: 'text-warn',
  GateDecided: 'text-warn',
  TaskBlocked: 'text-fail',
  TaskCompleted: 'text-ok',
}

function EventLog({ events }: { events: WhEvent[] }) {
  return (
    <ol className="grid gap-1.5 text-sm">
      {events.map((e) => (
        <li key={e.seq} className="grid grid-cols-[auto_1fr] gap-x-3 sm:grid-cols-[9rem_10rem_1fr]">
          <span className="text-xs text-muted-foreground tabular-nums">{fmtTime(e.recorded_at)}</span>
          <span className={cn('font-mono text-xs', EVENT_TONE[e.type] ?? 'text-muted-foreground')}>{e.type}</span>
          <span className="col-span-2 min-w-0 truncate sm:col-span-1">
            {describe(e)} <span className="text-xs text-muted-foreground">· {actorLabel(e.meta.actor)}</span>
          </span>
        </li>
      ))}
    </ol>
  )
}

function RawEvents({ events }: { events: WhEvent[] }) {
  return (
    <div className="grid gap-2">
      {events.map((e) => (
        <details key={e.seq} className="rounded-md border px-3 py-2 text-sm">
          <summary className="cursor-pointer">
            <span className="font-mono text-xs text-muted-foreground">
              #{e.seq} v{e.stream_version}
            </span>{' '}
            <span className="font-mono">{e.type}</span>
          </summary>
          <pre className="mt-2 overflow-x-auto text-xs">{JSON.stringify({ payload: e.payload, meta: e.meta }, null, 2)}</pre>
        </details>
      ))}
    </div>
  )
}
