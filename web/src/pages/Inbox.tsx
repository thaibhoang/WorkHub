import { useQuery } from '@tanstack/react-query'
import { PageHeader } from '@/components/PageHeader'
import { ProposalsTable } from '@/components/proposals'
import { Skeleton } from '@/components/ui/skeleton'
import { api } from '@/lib/api'
import { ApiDown, RunsTable } from './Runs'

/** Everything waiting on a human: gate reviews, improvement proposals, then blocked tasks. */
export default function Inbox() {
  const { data, isLoading, error } = useQuery({ queryKey: ['tasks'], queryFn: () => api.tasks() })
  const proposals = useQuery({ queryKey: ['proposals', 'inbox'], queryFn: () => api.proposals('inbox') })
  if (error) return <ApiDown error={error} />
  const gates = data?.filter((t) => t.status === 'awaiting_gate') ?? []
  const blocked = data?.filter((t) => t.status === 'blocked') ?? []
  const props = proposals.data ?? []
  return (
    <>
      <PageHeader title="Inbox" description="Tasks and factory changes waiting for a human decision." />
      {isLoading || proposals.isLoading ? (
        <Skeleton className="h-40" />
      ) : gates.length + blocked.length + props.length === 0 ? (
        <p className="rounded-lg border border-dashed p-10 text-center text-sm text-muted-foreground">
          Nothing needs you right now.
        </p>
      ) : (
        <div className="grid gap-8">
          {gates.length ? (
            <section>
              <h2 className="mb-2 text-sm font-medium">Review gates ({gates.length})</h2>
              <RunsTable rows={gates} />
            </section>
          ) : null}
          {props.length ? (
            <section>
              <h2 className="mb-2 text-sm font-medium">Improvement proposals ({props.length})</h2>
              <ProposalsTable rows={props} />
            </section>
          ) : null}
          {blocked.length ? (
            <section>
              <h2 className="mb-2 text-sm font-medium">Blocked ({blocked.length})</h2>
              <RunsTable rows={blocked} />
            </section>
          ) : null}
        </div>
      )}
    </>
  )
}
