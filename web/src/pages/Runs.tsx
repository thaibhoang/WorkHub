import { useQuery } from '@tanstack/react-query'
import { useNavigate, useSearchParams } from 'react-router'
import { NewTaskDialog } from '@/components/NewTaskDialog'
import { PageHeader } from '@/components/PageHeader'
import { TaskStatusBadge } from '@/components/status'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { api, firstLine, fmtAgo, fmtMs, fmtUsd, shortId, type TaskSummary } from '@/lib/api'
import { cn } from '@/lib/utils'

const FILTERS = [
  { id: 'all', label: 'All' },
  { id: 'active', label: 'Active' },
  { id: 'awaiting_gate', label: 'Needs review' },
  { id: 'blocked', label: 'Blocked' },
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
          {data?.length ? 'No tasks match this filter.' : 'No tasks yet. Create one to start the pipeline.'}
        </p>
      ) : (
        <RunsTable rows={rows} />
      )}
    </>
  )
}

export function RunsTable({ rows }: { rows: TaskSummary[] }) {
  const navigate = useNavigate()
  return (
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
          {rows.map((t) => {
            const active = t.status !== 'done' && t.status !== 'cancelled'
            const end = new Date(t.completed_at ?? (active ? Date.now() : t.updated_at)).getTime()
            return (
              <TableRow key={t.task_id} className="cursor-pointer" onClick={() => navigate(`/runs/${t.task_id}`)}>
                <TableCell className="max-w-0 w-full">
                  <div className="truncate font-medium">{t.title}</div>
                  <div className="truncate text-xs text-muted-foreground">
                    <span className="font-mono">{shortId(t.task_id)}</span>
                    {t.factory ? ` · ${t.factory}` : ' · manual'}
                    {t.blocked_reason ? <span className="text-fail"> · {firstLine(t.blocked_reason)}</span> : null}
                  </div>
                </TableCell>
                <TableCell>
                  <TaskStatusBadge status={t.status} />
                </TableCell>
                <TableCell className="hidden font-mono text-xs md:table-cell">{t.current_stage ?? '–'}</TableCell>
                <TableCell className="hidden text-right tabular-nums lg:table-cell">
                  {t.rework_loops || <span className="text-muted-foreground">0</span>}
                </TableCell>
                <TableCell className="hidden text-right tabular-nums sm:table-cell">{fmtUsd(t.cost_usd)}</TableCell>
                <TableCell className="hidden text-right tabular-nums text-muted-foreground lg:table-cell">
                  {t.status === 'open' && t.sessions === 0 ? '–' : fmtMs(end - new Date(t.created_at).getTime())}
                </TableCell>
                <TableCell className="text-right whitespace-nowrap text-muted-foreground">{fmtAgo(t.updated_at)}</TableCell>
              </TableRow>
            )
          })}
        </TableBody>
      </Table>
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
