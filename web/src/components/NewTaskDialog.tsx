import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Plus } from 'lucide-react'
import { useState, type ReactNode } from 'react'
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
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Textarea } from '@/components/ui/textarea'
import { api } from '@/lib/api'

/** Create a task, or a subtask of `parent`. New tasks are drafts unless "Start right away" is ticked. */
export function NewTaskDialog({ parent, trigger }: { parent?: { id: string; title: string }; trigger?: ReactNode } = {}) {
  const [open, setOpen] = useState(false)
  const [title, setTitle] = useState('')
  const [body, setBody] = useState('')
  const [start, setStart] = useState(false)
  const qc = useQueryClient()
  const navigate = useNavigate()
  const { data: factory } = useQuery({ queryKey: ['factory'], queryFn: api.factory, staleTime: Infinity })

  const create = useMutation({
    mutationFn: () => api.createTask({ title, body: body || undefined, parent: parent?.id, start }),
    onSuccess: (r) => {
      qc.invalidateQueries({ queryKey: ['tasks'] })
      if (parent) qc.invalidateQueries({ queryKey: ['task', parent.id] })
      setOpen(false)
      setTitle('')
      setBody('')
      setStart(false)
      toast.success(
        !r.factory ? 'Task created (no pipeline)' : start ? `Task queued for ${r.factory}` : 'Draft created; start it when you are ready',
      )
      navigate(`/runs/${r.task_id}`)
    },
    onError: (e) => toast.error(e.message),
  })

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        {trigger ?? (
          <Button>
            <Plus /> New task
          </Button>
        )}
      </DialogTrigger>
      <DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-lg">
        <form
          className="grid gap-4"
          onSubmit={(e) => {
            e.preventDefault()
            if (title.trim()) create.mutate()
          }}
        >
          <DialogHeader>
            <DialogTitle>{parent ? 'New subtask' : 'New task'}</DialogTitle>
            <DialogDescription>
              {parent ? <>Subtask of “{parent.title}”. </> : null}
              {factory
                ? `Runs through the ${factory.name} pipeline: ${factory.stages.map((s) => s.id).join(' → ')}, once started. The foreman must be running.`
                : 'The server has no factory loaded, so this task will not run through a pipeline.'}
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <Label htmlFor="title">Title</Label>
            <Input id="title" value={title} onChange={(e) => setTitle(e.target.value)} autoFocus required />
          </div>
          <div className="grid gap-2">
            <Label htmlFor="body">Details</Label>
            <Textarea
              id="body"
              className="max-h-[50vh] overflow-y-auto"
              rows={6}
              value={body}
              onChange={(e) => setBody(e.target.value)}
              placeholder="What should be built, constraints, acceptance criteria…"
            />
          </div>
          <label className="flex items-center gap-2 text-sm">
            <input type="checkbox" className="size-4 accent-primary" checked={start} onChange={(e) => setStart(e.target.checked)} />
            Start right away (otherwise it stays a draft until you start it)
          </label>
          <DialogFooter>
            <Button type="submit" disabled={!title.trim() || create.isPending}>
              {create.isPending ? 'Creating…' : start ? 'Create and start' : 'Create draft'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
