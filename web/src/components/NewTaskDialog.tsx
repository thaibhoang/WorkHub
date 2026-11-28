import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Plus } from 'lucide-react'
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
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Textarea } from '@/components/ui/textarea'
import { api } from '@/lib/api'

export function NewTaskDialog() {
  const [open, setOpen] = useState(false)
  const [title, setTitle] = useState('')
  const [body, setBody] = useState('')
  const qc = useQueryClient()
  const navigate = useNavigate()
  const { data: factory } = useQuery({ queryKey: ['factory'], queryFn: api.factory, staleTime: Infinity })

  const create = useMutation({
    mutationFn: () => api.createTask({ title, body: body || undefined }),
    onSuccess: (r) => {
      qc.invalidateQueries({ queryKey: ['tasks'] })
      setOpen(false)
      setTitle('')
      setBody('')
      toast.success(r.factory ? `Task queued for ${r.factory}` : 'Task created (no pipeline)')
      navigate(`/runs/${r.task_id}`)
    },
    onError: (e) => toast.error(e.message),
  })

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button>
          <Plus /> New task
        </Button>
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
            <DialogTitle>New task</DialogTitle>
            <DialogDescription>
              {factory
                ? `Runs through the ${factory.name} pipeline: ${factory.stages.map((s) => s.id).join(' → ')}. The foreman must be running.`
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
          <DialogFooter>
            <Button type="submit" disabled={!title.trim() || create.isPending}>
              {create.isPending ? 'Creating…' : 'Create task'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
