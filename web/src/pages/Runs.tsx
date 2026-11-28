import { useQuery } from '@tanstack/react-query'
import { ChevronLeft, ChevronRight } from 'lucide-react'
import { Fragment, useMemo, useState } from 'react'
import { useNavigate, useSearchParams } from 'react-router'
import { NewTaskDialog } from '@/components/NewTaskDialog'
import { PageHeader } from '@/components/PageHeader'
import { Button } from '@/components/ui/button'
import { PrLink, TaskStatusBadge } from '@/components/status'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { api, firstLine, fmtAgo, fmtMs, fmtUsd, shortId, taskTree, type TaskSummary } from '@/lib/api'
import { cn } from '@/lib/utils'

const FILTERS = [
  { id: 'all', label: 'All' },
  { id: 'draft', label: 'Draft' },
  { id: 'active', label: 'Active' },
  { id: 'awaiting_gate', label: 'Needs review' },
  { id: 'blocked', label: 'Blocked' },
  { id: 'shipped', label: 'PR open' },
  { id: 'done', label: 'Done' },
] as const

function matches(t: TaskSummary, f: string) {
  if (f === 'all') return true
  if (f === 'active') return t.status === 'open' || t.status === 'running'
  return t.status === f
}

export default function Runs() {
  const [params, setParams] = useSearchParams()
  const filter = params.get('filter') ?? 'all'
  const page = Math.max(0, Number(params.get('page') ?? 1) - 1) || 0
  const { data, isLoading, error } = useQuery({ queryKey: ['tasks'], queryFn: () => api.tasks() })
  const rows = data?.filter((t) => matches(t, filter)) ?? []

  return (
    <>
      <PageHeader title="Runs" description="Every task and where it is in the pipeline." actions={<NewTaskDialog />} />
      <div className="mb-3 flex flex-wrap gap-1">
        {FILTERS.map((f) => {
          const n = data?.filter((t) => matches(t, f.id)).length
          return (
            <button
              key={f.id}
              onClick={() => setParams(f.id === 'all' ? {} : { filter: f.id })}
              className={cn(
                'rounded-md px-2.5 py-1 text-sm text-muted-foreground hover:bg-muted',
                filter === f.id && 'bg-muted font-medium text-foreground',
              )}
            >
              {f.label}
              {n ? <span className="ml-1.5 text-xs tabular-nums opacity-70">{n}</span> : null}
            </button>
          )
        })}
      </div>
      {error ? (
        <ApiDown error={error} />
      ) : isLoading ? (
        <div className="grid gap-2">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-10" />
          ))}
        </div>
      ) : rows.length === 0 ? (
        <p className="rounded-lg border border-dashed p-10 text-center text-sm text-muted-foreground">
          {data?.length ? 'No tasks match this filter.' : 'No tasks yet. Create one, then start it to run the pipeline.'}
        </p>
      ) : (
        <RunsTable
          rows={rows}
          page={page}
          onPage={(p) => {
            const next = new URLSearchParams(params)
            if (p) next.set('page', String(p + 1))
            else next.delete('page')
            setParams(next)
          }}
        />
      )}
    </>
  )
}

/** Top-level tasks per page; a task's subtasks always stay on its page. */
const PAGE_SIZE = 25

/**
 * Tasks as a tree, paged by top-level task. Pass `page`/`onPage` to keep the
 * page somewhere else (the URL); otherwise it is local state.
 */
export function RunsTable({ rows, page: pageProp, onPage }: { rows: TaskSummary[]; page?: number; onPage?: (page: number) => void }) {
  const navigate = useNavigate()
  const [localPage, setLocalPage] = useState(0)
  const groups = useMemo(() => {
    const out: { task: TaskSummary; depth: number }[][] = []
    for (const item of taskTree(rows)) {
      if (item.depth === 0 || !out.length) out.push([item])
      else out[out.length - 1].push(item)
    }
    return out
  }, [rows])
  const pages = Math.max(1, Math.ceil(groups.length / PAGE_SIZE))
  const page = Math.min(pageProp ?? localPage, pages - 1)
  const setPage = onPage ?? setLocalPage
  const items = groups.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE).flat()
  return (
    <>
    <div className="overflow-hidden rounded-lg border">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Task</TableHead>
            <TableHead>Status</TableHead>
            <TableHead className="hidden md:table-cell">Stage</TableHead>
            <TableHead className="hidden text-right lg:table-cell">Rework</TableHead>
            <TableHead className="hidden text-right sm:table-cell">Cost</TableHead>
            <TableHead className="hidden text-right lg:table-cell">Duration</TableHead>
            <TableHead className="text-right">Updated</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {items.map(({ task: t, depth }) => {
            const active = t.status !== 'done' && t.status !== 'cancelled'
            const end = new Date(t.completed_at ?? (active ? Date.now() : t.updated_at)).getTime()
            return (
              <TableRow key={t.task_id} className="cursor-pointer" onClick={() => navigate(`/runs/${t.task_id}`)}>
                <TableCell className="max-w-0 w-full" style={depth ? { paddingLeft: `${0.5 + depth * 1.25}rem` } : undefined}>
                  <div className="truncate font-medium">
                    {depth ? <span className="mr-1 text-muted-foreground">└</span> : null}
                    {t.title}
                  </div>
                  <div className={cn('truncate text-xs text-muted-foreground', depth && 'pl-4')}>
                    <span className="font-mono">{shortId(t.task_id)}</span>
                    {t.factory ? ` · ${t.factory}` : ' · manual'}
                    {t.autopilot ? <span className="text-info"> · autopilot</span> : null}
                    {t.external_ref ? ` · ${t.external_ref}` : null}
                    {t.blocked_reason ? <span className="text-fail"> · {firstLine(t.blocked_reason)}</span> : null}
                  </div>
                </TableCell>
                <TableCell>
                  <div className="flex flex-col gap-0.5">
                    <TaskStatusBadge status={t.status} />
                    {t.pr_url ? <PrLink url={t.pr_url} number={t.pr_number} state={t.pr_state} /> : null}
                  </div>
                </TableCell>
                <TableCell className="hidden font-mono text-xs md:table-cell">{t.current_stage ?? '–'}</TableCell>
                <TableCell className="hidden text-right tabular-nums lg:table-cell">
                  {t.rework_loops || <span className="text-muted-foreground">0</span>}
                </TableCell>
                <TableCell className="hidden text-right tabular-nums sm:table-cell">{fmtUsd(t.cost_usd)}</TableCell>
                <TableCell className="hidden text-right tabular-nums text-muted-foreground lg:table-cell">
                  {(t.status === 'open' || t.status === 'draft') && t.sessions === 0 ? '–' : fmtMs(end - new Date(t.created_at).getTime())}
                </TableCell>
                <TableCell className="text-right whitespace-nowrap text-muted-foreground">{fmtAgo(t.updated_at)}</TableCell>
              </TableRow>
            )
          })}
        </TableBody>
      </Table>
    </div>
    {pages > 1 ? <Pager page={page} pages={pages} groups={groups.length} onPage={setPage} /> : null}
    </>
  )
}

function Pager({ page, pages, groups, onPage }: { page: number; pages: number; groups: number; onPage: (p: number) => void }) {
  // First, last, and two around the current page; gaps become "…".
  const shown = [...new Set([0, page - 2, page - 1, page, page + 1, page + 2, pages - 1])].filter((p) => p >= 0 && p < pages).sort((a, b) => a - b)
  const first = page * PAGE_SIZE + 1
  const last = Math.min(groups, (page + 1) * PAGE_SIZE)
  return (
    <div className="mt-3 flex flex-wrap items-center justify-between gap-2 text-sm">
      <span className="text-muted-foreground tabular-nums">
        {first}–{last} of {groups} top-level tasks
      </span>
      <div className="flex items-center gap-1">
        <Button variant="outline" size="sm" onClick={() => onPage(page - 1)} disabled={page === 0}>
          <ChevronLeft /> Prev
        </Button>
        {shown.map((p, i) => (
          <Fragment key={p}>
            {i > 0 && p - shown[i - 1] > 1 ? <span className="px-1 text-muted-foreground">…</span> : null}
            <Button variant={p === page ? 'secondary' : 'ghost'} size="sm" className="min-w-8 tabular-nums" onClick={() => onPage(p)}>
              {p + 1}
            </Button>
          </Fragment>
        ))}
        <Button variant="outline" size="sm" onClick={() => onPage(page + 1)} disabled={page >= pages - 1}>
          Next <ChevronRight />
        </Button>
      </div>
    </div>
  )
}

export function ApiDown({ error }: { error: Error }) {
  return (
    <div className="rounded-lg border border-fail/40 bg-fail/5 p-4 text-sm">
      <p className="font-medium text-fail">Cannot reach the WorkHub API</p>
      <p className="mt-1 text-muted-foreground">
        {error.message}. Start it with <code className="font-mono">cargo run --bin wh -- serve --foreman</code>.
      </p>
    </div>
  )
}
