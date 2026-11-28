import { useMutation, useQuery } from '@tanstack/react-query'
import { Check, ChevronRight, Circle, Loader2, Minus, RefreshCw, X } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
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
import { api, type SystemInfo, type UpdateJob, type UpdateStepStatus } from '@/lib/api'
import { cn } from '@/lib/utils'

/** Server info; polled fast while an update runs, slowly otherwise (to notice restarts). */
function useSystem(fast: boolean) {
  return useQuery({
    queryKey: ['system'],
    queryFn: api.system,
    refetchInterval: fast ? 1000 : 30_000,
    retry: false,
  })
}

/**
 * Reloads the page once the server comes back with a new boot id. The tab that
 * started the update reloads by itself; other tabs get a toast.
 */
export function useRestartWatcher(auto: boolean, data: SystemInfo | undefined) {
  const first = useRef<string | null>(null)
  useEffect(() => {
    if (!data) return
    if (first.current === null) first.current = data.boot_id
    else if (data.boot_id !== first.current) {
      first.current = data.boot_id
      if (auto) window.location.reload()
      else
        toast('WorkHub was restarted', {
          description: 'Reload to use the new version of the UI.',
          duration: Infinity,
          action: { label: 'Reload', onClick: () => window.location.reload() },
        })
    }
  }, [data, auto])
}

export function SystemUpdate() {
  const [open, setOpen] = useState(false)
  const [started, setStarted] = useState(false)
  const { data, error } = useSystem(open || started)
  const job = data?.job ?? null
  const active = job?.state === 'running' || job?.state === 'restarting'
  useRestartWatcher(started, data)

  // While the server restarts, react-query keeps the last data, so the dialog stays.
  if (!data?.self_update.enabled) return null
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="ghost" size="sm" className="justify-start gap-2 text-muted-foreground" title="Pull the latest code and restart">
          <RefreshCw className={cn('size-4', active && 'animate-spin')} />
          <span className="hidden md:inline">{active ? 'Updating…' : 'Update & restart'}</span>
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Update &amp; restart</DialogTitle>
          <DialogDescription>
            Pulls <span className="font-mono">{data?.self_update.branch ?? 'main'}</span> into{' '}
            <span className="font-mono">{data?.self_update.repo ?? 'the repository'}</span>, rebuilds the server and the UI, migrates the
            database, waits for running stages to finish, then restarts this server.
          </DialogDescription>
        </DialogHeader>
        {open ? <UpdateBody data={data} lost={!!error} onStarted={() => setStarted(true)} /> : null}
      </DialogContent>
    </Dialog>
  )
}

function UpdateBody({ data, lost, onStarted }: { data: SystemInfo | undefined; lost: boolean; onStarted: () => void }) {
  const job = data?.job ?? null
  const active = job?.state === 'running' || job?.state === 'restarting'
  const check = useQuery({ queryKey: ['system', 'check'], queryFn: api.checkUpdate, enabled: !active, staleTime: 0, retry: false })
  const start = useMutation({
    mutationFn: (force: boolean) => api.startUpdate(force),
    onSuccess: () => onStarted(),
    onError: (e) => toast.error(e.message),
  })
  const c = check.data
  const draining = job?.state === 'running' && job.steps.find((s) => s.id === 'drain')?.status === 'running'

  return (
    <div className="grid gap-4">
      <div className="text-sm text-muted-foreground">
        Running <span className="font-mono text-foreground">{data?.commit?.slice(0, 9) ?? '…'}</span>
        {data?.busy ? <> · {data.busy} stage run(s) in progress</> : null}
      </div>

      {job ? (
        <Steps job={job} lost={lost} busy={data?.busy ?? 0} />
      ) : check.isLoading ? (
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="size-4 animate-spin" /> Checking for new commits…
        </p>
      ) : check.error ? (
        <p className="text-sm text-fail">{check.error.message}</p>
      ) : c ? (
        <div className="grid gap-2 text-sm">
          {c.commits.length ? (
            <>
              <p>
                {c.commits.length} new commit{c.commits.length > 1 ? 's' : ''} on {c.expected_branch}:
              </p>
              <ul className="max-h-48 overflow-y-auto rounded-md border p-2 font-mono text-xs">
                {c.commits.map((m) => (
                  <li key={m.sha} className="truncate">
                    <span className="text-muted-foreground">{m.sha}</span> {m.subject}
                  </li>
                ))}
              </ul>
            </>
          ) : (
            <p className="text-muted-foreground">Already up to date. You can still rebuild and restart.</p>
          )}
          {c.blocked ? <p className="rounded-md border border-warn/50 bg-warn/5 p-2 text-warn">{c.blocked}</p> : null}
        </div>
      ) : null}

      <DialogFooter className="gap-2">
        {draining ? (
          <Button variant="outline" onClick={() => start.mutate(true)} disabled={start.isPending || job?.force}>
            Restart now (interrupt running stages)
          </Button>
        ) : null}
        {!active ? (
          <Button onClick={() => start.mutate(false)} disabled={start.isPending || !c || !!c.blocked}>
            <RefreshCw /> {job?.state === 'failed' ? 'Try again' : 'Update & restart'}
          </Button>
        ) : null}
      </DialogFooter>
    </div>
  )
}

const ICON: Record<UpdateStepStatus, typeof Check> = {
  pending: Circle,
  running: Loader2,
  done: Check,
  skipped: Minus,
  failed: X,
}

function Steps({ job, lost, busy }: { job: UpdateJob; lost: boolean; busy: number }) {
  const [openStep, setOpenStep] = useState<string | null>(null)
  const current = job.steps.find((s) => s.status === 'running' || s.status === 'failed')?.id ?? null
  const shown = openStep ?? current
  return (
    <div className="grid gap-1 text-sm">
      {job.steps.map((s) => {
        const Icon = ICON[s.status]
        return (
          <div key={s.id}>
            <button
              type="button"
              onClick={() => setOpenStep(shown === s.id ? '' : s.id)}
              className="flex w-full items-center gap-2 rounded-md px-1 py-1 text-left hover:bg-muted"
            >
              <Icon
                className={cn(
                  'size-4 shrink-0',
                  s.status === 'running' && 'animate-spin text-info',
                  s.status === 'done' && 'text-ok',
                  s.status === 'failed' && 'text-fail',
                  (s.status === 'pending' || s.status === 'skipped') && 'text-muted-foreground',
                )}
              />
              <span className={cn(s.status === 'pending' && 'text-muted-foreground')}>{s.label}</span>
              {s.id === 'drain' && s.status === 'running' && busy ? (
                <span className="text-xs text-muted-foreground">{busy} running</span>
              ) : null}
              {s.output ? <ChevronRight className={cn('ml-auto size-4 text-muted-foreground', shown === s.id && 'rotate-90')} /> : null}
            </button>
            {shown === s.id && s.output ? <Log text={s.output} /> : null}
          </div>
        )
      })}
      {job.state === 'failed' ? <p className="mt-2 text-fail">{job.error}</p> : null}
      {job.state === 'restarting' || (lost && job.state !== 'failed') ? (
        <p className="mt-2 flex items-center gap-2 text-muted-foreground">
          <Loader2 className="size-4 animate-spin" /> Restarting; the page reloads when the server is back.
        </p>
      ) : null}
    </div>
  )
}

function Log({ text }: { text: string }) {
  const ref = useRef<HTMLPreElement>(null)
  useEffect(() => {
    if (ref.current) ref.current.scrollTop = ref.current.scrollHeight
  }, [text])
  return (
    <pre ref={ref} className="mt-1 mb-2 max-h-64 overflow-auto rounded-md bg-muted p-2 font-mono text-xs whitespace-pre-wrap">
      {text}
    </pre>
  )
}

/** For tabs that did not start the update: offer a reload after any restart. */
export function RestartNotice() {
  const { data } = useSystem(false)
  useRestartWatcher(false, data)
  return null
}
