import { useQuery } from '@tanstack/react-query'
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet'
import { SessionStatusBadge } from '@/components/status'
import { Skeleton } from '@/components/ui/skeleton'
import { api, fmtMs, fmtTokens, fmtUsd, shortId, type SessionDetail } from '@/lib/api'
import { cn } from '@/lib/utils'

/** Side panel with a session's facts and its Claude Code transcript. */
export function SessionSheet({ session, onClose }: { session: SessionDetail | null; onClose: () => void }) {
  return (
    <Sheet open={!!session} onOpenChange={(o) => !o && onClose()}>
      <SheetContent className="w-full gap-0 sm:max-w-2xl">
        {session ? <Body s={session} /> : null}
      </SheetContent>
    </Sheet>
  )
}

function Body({ s }: { s: SessionDetail }) {
  const live = !s.ended_at
  const { data, isLoading, error } = useQuery({
    queryKey: ['transcript', s.session_id, s.status],
    queryFn: () => api.transcript(s.session_id),
    enabled: !!s.transcript,
    retry: false,
  })
  const facts: [string, string][] = [
    ['Model', s.model ?? '–'],
    ['Attempt', String(s.attempt)],
    ['Duration', fmtMs(s.duration_ms)],
    ['Cost', fmtUsd(s.cost_usd, 4)],
    ['Turns', String(s.num_turns ?? '–')],
    ['Tools', `${s.tool_calls} (${s.tool_errors} errors)`],
    ['Tokens in / out', `${fmtTokens(s.input_tokens)} / ${fmtTokens(s.output_tokens)}`],
    ['Cache read / write', `${fmtTokens(s.cache_read_tokens)} / ${fmtTokens(s.cache_write_tokens)}`],
  ]
  return (
    <>
      <SheetHeader className="border-b">
        <SheetTitle className="flex items-center gap-3">
          {s.agent} <span className="font-mono text-xs font-normal text-muted-foreground">{shortId(s.session_id)}</span>
          <SessionStatusBadge status={s.status} />
        </SheetTitle>
        <SheetDescription>
          stage <span className="font-mono">{s.stage}</span>
          {s.branch ? (
            <>
              {' '}
              · branch <span className="font-mono">{s.branch}</span>
            </>
          ) : null}
        </SheetDescription>
      </SheetHeader>
      <div className="flex-1 overflow-y-auto">
        <dl className="grid grid-cols-2 gap-x-4 gap-y-2 border-b p-4 text-sm sm:grid-cols-4">
          {facts.map(([k, v]) => (
            <div key={k}>
              <dt className="text-xs text-muted-foreground">{k}</dt>
              <dd className="tabular-nums">{v}</dd>
            </div>
          ))}
        </dl>
        {s.failure_class ? (
          <div className="border-b bg-fail/5 p-4 text-sm">
            <span className="font-medium text-fail">{s.failure_class}</span>
            {s.failure_message ? <pre className="mt-1 text-xs whitespace-pre-wrap">{s.failure_message}</pre> : null}
          </div>
        ) : null}
        <div className="p-4">
          <h3 className="mb-3 text-sm font-medium">Transcript</h3>
          {!s.transcript ? (
            <p className="text-sm text-muted-foreground">
              {live ? 'The transcript is available when the session ends.' : 'No transcript was recorded.'}
            </p>
          ) : isLoading ? (
            <Skeleton className="h-40" />
          ) : error ? (
            <p className="text-sm text-fail">{error.message}</p>
          ) : (
            <>
              {data?.truncated ? (
                <p className="mb-2 text-xs text-muted-foreground">Showing the last 4 MB of {data.path}</p>
              ) : null}
              <div className="grid gap-2">
                {data?.lines.map((line, i) => <TranscriptLine key={i} line={line} />)}
              </div>
            </>
          )}
        </div>
      </div>
    </>
  )
}

// Claude Code `--output-format stream-json` lines.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function TranscriptLine({ line }: { line: any }) {
  if (line.type === 'system') {
    return (
      <div className="text-xs text-muted-foreground">
        session start{line.model ? ` · ${line.model}` : ''}
        {line.cwd ? ` · ${line.cwd}` : ''}
      </div>
    )
  }
  if (line.type === 'result') {
    return (
      <div className={cn('rounded-md border p-3 text-sm', line.is_error ? 'border-fail/40' : 'border-ok/40')}>
        <div className="mb-1 text-xs text-muted-foreground tabular-nums">
          result · {line.subtype} · {line.num_turns ?? '–'} turns · {fmtUsd(line.total_cost_usd, 4)}
        </div>
        {typeof line.result === 'string' ? <pre className="text-xs whitespace-pre-wrap">{line.result}</pre> : null}
      </div>
    )
  }
  const content = line.message?.content
  if (!Array.isArray(content)) return null
  return (
    <>
      {/* eslint-disable-next-line @typescript-eslint/no-explicit-any */}
      {content.map((c: any, i: number) => {
        if (c.type === 'text' && c.text?.trim()) {
          return (
            <div key={i} className={cn('text-sm whitespace-pre-wrap', line.type === 'user' && 'text-muted-foreground')}>
              {c.text}
            </div>
          )
        }
        if (c.type === 'thinking' && c.thinking?.trim()) {
          return (
            <details key={i} className="text-xs text-muted-foreground">
              <summary className="cursor-pointer">thinking</summary>
              <pre className="mt-1 whitespace-pre-wrap">{c.thinking}</pre>
            </details>
          )
        }
        if (c.type === 'tool_use') {
          return (
            <details key={i} className="rounded-md bg-muted/60 px-3 py-1.5 text-xs">
              <summary className="cursor-pointer font-mono">
                <span className="font-semibold">{c.name}</span> <span className="text-muted-foreground">{toolHint(c.input)}</span>
              </summary>
              <pre className="mt-2 overflow-x-auto whitespace-pre-wrap">{JSON.stringify(c.input, null, 2)}</pre>
            </details>
          )
        }
        if (c.type === 'tool_result') {
          const text = Array.isArray(c.content)
            ? // eslint-disable-next-line @typescript-eslint/no-explicit-any
              c.content.map((x: any) => x.text ?? '').join('\n')
            : String(c.content ?? '')
          return (
            <details key={i} className={cn('ml-4 rounded-md border px-3 py-1 text-xs', c.is_error && 'border-fail/50')}>
              <summary className={cn('cursor-pointer text-muted-foreground', c.is_error && 'text-fail')}>
                {c.is_error ? 'tool error' : 'tool result'} · {text.length.toLocaleString()} chars
              </summary>
              <pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap">{text}</pre>
            </details>
          )
        }
        return null
      })}
    </>
  )
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function toolHint(input: any): string {
  if (!input || typeof input !== 'object') return ''
  const v = input.command ?? input.file_path ?? input.pattern ?? input.path ?? input.url ?? input.description
  if (typeof v !== 'string') return ''
  return v.length > 80 ? `${v.slice(0, 80)}…` : v
}
