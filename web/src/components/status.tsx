import { cn } from '@/lib/utils'
import type { SessionDetail, StageResult, TaskStatus } from '@/lib/api'

const TASK: Record<TaskStatus, { label: string; dot: string; text: string }> = {
  open: { label: 'Queued', dot: 'bg-muted-foreground', text: 'text-muted-foreground' },
  running: { label: 'Running', dot: 'bg-info animate-pulse', text: 'text-info' },
  awaiting_gate: { label: 'Needs review', dot: 'bg-warn', text: 'text-warn' },
  blocked: { label: 'Blocked', dot: 'bg-fail', text: 'text-fail' },
  done: { label: 'Done', dot: 'bg-ok', text: 'text-ok' },
  cancelled: { label: 'Cancelled', dot: 'bg-muted-foreground/50', text: 'text-muted-foreground' },
}

export function TaskStatusBadge({ status, className }: { status: TaskStatus; className?: string }) {
  const s = TASK[status] ?? TASK.open
  return (
    <span className={cn('inline-flex items-center gap-1.5 text-xs font-medium whitespace-nowrap', s.text, className)}>
      <span className={cn('size-2 rounded-full', s.dot)} />
      {s.label}
    </span>
  )
}

export const SESSION_COLOR: Record<SessionDetail['status'], string> = {
  dispatched: 'bg-muted-foreground/40',
  running: 'bg-info',
  succeeded: 'bg-ok',
  failed: 'bg-fail',
}

export function SessionStatusBadge({ status }: { status: SessionDetail['status'] }) {
  const text = { dispatched: 'text-muted-foreground', running: 'text-info', succeeded: 'text-ok', failed: 'text-fail' }[status]
  return (
    <span className={cn('inline-flex items-center gap-1.5 text-xs font-medium', text)}>
      <span className={cn('size-2 rounded-full', SESSION_COLOR[status], status === 'running' && 'animate-pulse')} />
      {status}
    </span>
  )
}

export const OUTCOME_TEXT: Record<StageResult['outcome'], string> = {
  succeeded: 'text-ok',
  rejected: 'text-warn',
  failed: 'text-fail',
  skipped: 'text-muted-foreground',
}
