// Typed client for the `wh serve` API (see crates/wh-api).

export type TaskStatus = 'open' | 'running' | 'awaiting_gate' | 'blocked' | 'done' | 'cancelled'

export interface TaskSummary {
  task_id: string
  title: string
  source: string
  factory: string | null
  status: TaskStatus
  current_stage: string | null
  rework_loops: number
  gate_rejections: number
  blocked_reason: string | null
  created_at: string
  updated_at: string
  completed_at: string | null
  sessions: number
  cost_usd: number
}

export interface EventMeta {
  occurred_at: string
  correlation_id: string
  causation_id: string | null
  actor: { kind: 'human'; user: string } | { kind: 'agent'; session: string } | { kind: 'system'; component: string }
  factory_version: string | null
}

export interface WhEvent {
  seq: number
  event_id: string
  stream_id: string
  stream_version: number
  type: string
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  payload: Record<string, any>
  meta: EventMeta
  recorded_at: string
}

export interface SessionDetail {
  session_id: string
  task_id: string
  agent: string
  stage: string
  runtime: string
  model: string | null
  status: 'dispatched' | 'running' | 'succeeded' | 'failed'
  failure_class: string | null
  failure_message: string | null
  attempt: number
  retry_of: string | null
  dispatched_at: string
  ended_at: string | null
  duration_ms: number | null
  num_turns: number | null
  tool_calls: number
  tool_errors: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  cost_usd: number
  workdir: string | null
  branch: string | null
  transcript: string | null
  factory_version: string | null
}

export interface StageResult {
  stage: string
  outcome: 'succeeded' | 'failed' | 'rejected' | 'skipped'
  summary: string | null
}

export interface TaskState {
  version: number
  status: TaskStatus
  stage: string | null
  gate: string | null
  blocked_reason: string | null
  blocked_stage: string | null
  body: string | null
  rework_loops: number
  results: StageResult[]
}

export interface TaskDetail {
  task: TaskSummary | null
  state: TaskState
  events: WhEvent[]
  sessions: SessionDetail[]
}

export interface AgentStats {
  agent: string
  runs: number
  succeeded: number
  failed: number
  retries: number
  total_cost_usd: number
  avg_cost_usd: number
  p50_duration_ms: number | null
  p95_duration_ms: number | null
  avg_turns: number | null
  tool_calls: number
  tool_errors: number
}

export interface StageStats {
  stage: string
  runs: number
  succeeded: number
  failed: number
  retries: number
  total_cost_usd: number
  avg_cost_usd: number
  p50_duration_ms: number | null
  p95_duration_ms: number | null
}

export interface Stats {
  agents: AgentStats[]
  stages: StageStats[]
  failures: { class: string; count: number }[]
}

export interface Dashboard {
  days: number
  totals: {
    tasks_created: number
    tasks_done: number
    tasks_blocked: number
    tasks_in_flight: number
    awaiting_gate: number
    sessions: number
    sessions_failed: number
    cost_usd: number
    cycle_p50_ms: number | null
    cycle_p90_ms: number | null
    avg_rework: number | null
    gate_rejections: number
  }
  daily: {
    day: string
    cost_usd: number
    sessions_succeeded: number
    sessions_failed: number
    tasks_created: number
    tasks_done: number
  }[]
}

export interface FactoryDef {
  name: string
  version: string
  repo: string
  base_ref: string
  max_rework_loops: number
  stages: {
    id: string
    gate: string | null
    on_reject: string | null
    agent: {
      name: string
      description: string | null
      model: string | null
      budget_usd: number
      timeout_min: number
      max_retries: number
      permission_mode: string
      allowed_tools: string[]
      prompt: string
    }
  }[]
}

export interface Transcript {
  path: string
  truncated: boolean
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  lines: any[]
}

export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`/api${path}`, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  const text = await res.text()
  const data = text ? JSON.parse(text) : null
  if (!res.ok) throw new ApiError(res.status, data?.error ?? `${res.status} ${res.statusText}`)
  return data as T
}

export const api = {
  tasks: (status?: string) => request<TaskSummary[]>('GET', `/tasks${status ? `?status=${status}` : ''}`),
  task: (id: string) => request<TaskDetail>('GET', `/tasks/${id}`),
  createTask: (b: { title: string; body?: string; pipeline?: boolean }) =>
    request<{ task_id: string; factory: string | null }>('POST', '/tasks', b),
  decideGate: (id: string, approved: boolean, comment?: string) =>
    request<{ message: string }>('POST', `/tasks/${id}/gate`, { approved, comment }),
  resume: (id: string, stage?: string, comment?: string) =>
    request<{ message: string }>('POST', `/tasks/${id}/resume`, { stage, comment }),
  cancel: (id: string, reason: string) => request<{ message: string }>('POST', `/tasks/${id}/cancel`, { reason }),
  sessions: (limit = 100) => request<SessionDetail[]>('GET', `/sessions?limit=${limit}`),
  transcript: (id: string) => request<Transcript>('GET', `/sessions/${id}/transcript`),
  stats: () => request<Stats>('GET', '/stats'),
  dashboard: (days: number) => request<Dashboard>('GET', `/dashboard?days=${days}`),
  factory: () => request<FactoryDef | null>('GET', '/factory'),
  events: (after = 0, limit = 100) => request<WhEvent[]>('GET', `/events?after=${after}&limit=${limit}`),
}

// ---------- formatting ----------

export function fmtUsd(v: number | null | undefined, digits = 2): string {
  if (v == null) return '–'
  if (v > 0 && v < 0.01) return `$${v.toFixed(4)}`
  return `$${v.toFixed(digits)}`
}

export function fmtMs(ms: number | null | undefined): string {
  if (ms == null) return '–'
  const s = ms / 1000
  if (s < 60) return `${s.toFixed(1)}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ${String(Math.floor(s % 60)).padStart(2, '0')}s`
  const h = Math.floor(m / 60)
  if (h < 48) return `${h}h ${String(m % 60).padStart(2, '0')}m`
  return `${Math.floor(h / 24)}d ${h % 24}h`
}

export function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`
  return String(n)
}

const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' })

export function fmtAgo(iso: string, now = Date.now()): string {
  const diff = (new Date(iso).getTime() - now) / 1000
  const abs = Math.abs(diff)
  if (abs < 45) return 'just now'
  if (abs < 3600) return rtf.format(Math.round(diff / 60), 'minute')
  if (abs < 86400) return rtf.format(Math.round(diff / 3600), 'hour')
  return rtf.format(Math.round(diff / 86400), 'day')
}

export function fmtTime(iso: string): string {
  return new Date(iso).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  })
}

export function shortId(id: string): string {
  return id.slice(-8)
}

export function actorLabel(a: EventMeta['actor']): string {
  switch (a.kind) {
    case 'human':
      return a.user
    case 'agent':
      return `agent ${shortId(a.session)}`
    case 'system':
      return a.component
  }
}

export function firstLine(s: string): string {
  return s.split('\n').find((l) => l.trim())?.trim() ?? ''
}
