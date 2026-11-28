// Typed client for the `wh serve` API (see crates/wh-api).

export type TaskStatus =
  | 'draft'
  | 'open'
  | 'running'
  | 'awaiting_gate'
  | 'awaiting_input'
  | 'blocked'
  | 'shipped'
  | 'done'
  | 'cancelled'

export type PrState = 'open' | 'merged' | 'closed'

/** Tasks in tree order (parents before their subtasks) with their depth. Roots keep
 * the given order; subtasks are oldest first. A task whose parent is not in `rows`
 * is shown as a root. */
export function taskTree<T extends { task_id: string; parent_id: string | null; created_at: string }>(rows: T[]): { task: T; depth: number }[] {
  const ids = new Set(rows.map((r) => r.task_id))
  const kids = new Map<string, T[]>()
  const roots: T[] = []
  for (const r of rows) {
    if (r.parent_id && r.parent_id !== r.task_id && ids.has(r.parent_id)) {
      kids.set(r.parent_id, [...(kids.get(r.parent_id) ?? []), r])
    } else roots.push(r)
  }
  for (const v of kids.values()) v.sort((a, b) => a.created_at.localeCompare(b.created_at))
  const out: { task: T; depth: number }[] = []
  const seen = new Set<string>()
  const walk = (t: T, depth: number) => {
    if (seen.has(t.task_id)) return
    seen.add(t.task_id)
    out.push({ task: t, depth })
    for (const c of kids.get(t.task_id) ?? []) walk(c, depth + 1)
  }
  roots.forEach((r) => walk(r, 0))
  return out
}

/** Ids of `id` and every task below it. */
export function subtreeIds(rows: { task_id: string; parent_id: string | null }[], id: string): Set<string> {
  const out = new Set([id])
  let grew = true
  while (grew) {
    grew = false
    for (const r of rows) {
      if (r.parent_id && out.has(r.parent_id) && !out.has(r.task_id)) {
        out.add(r.task_id)
        grew = true
      }
    }
  }
  return out
}

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
  /** Set for benchmark cases (they never appear in the task list). */
  benchmark_run: string | null
  benchmark_case: string | null
  /** GitHub (phase 8): `owner/name#N` of the source issue and the task's PR. */
  external_ref: string | null
  pr_url: string | null
  pr_number: number | null
  pr_state: PrState | null
  shipped_at: string | null
  first_review_at: string | null
  merged_at: string | null
  pr_reviews: number
  changes_requested: number
  ci_failures: number
  pr_rework_loops: number
  /** Parent task (tasks form a tree). */
  parent_id: string | null
  /** Gates and questions are decided by the autopilot agent. */
  autopilot: boolean
}

/** One gate decision / question set handled by the autopilot. */
export interface AutopilotRun {
  request_version: number
  kind: 'gate' | 'questions'
  stage: string
  gate: string | null
  model: string | null
  /** failed = left to a human */
  status: 'running' | 'decided' | 'failed'
  decision: 'approve' | 'reject' | 'answered' | null
  /** Comment for the agent, or the answers. */
  detail: string | null
  rationale: string | null
  message: string | null
  cost_usd: number
  started_at: string
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
  /** `worktree` or `docker:<image>`; null for sessions before sandboxes (worktree). */
  sandbox: string | null
  transcript: string | null
  factory_version: string | null
  /** Pipeline verdict for the session's stage. */
  stage_outcome: 'succeeded' | 'failed' | 'rejected' | null
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
  gate_decided: boolean
  blocked_reason: string | null
  blocked_stage: string | null
  body: string | null
  rework_loops: number
  results: StageResult[]
  pr: { repo: string; number: number; url: string; branch: string; commit: string; closed: boolean } | null
  pr_rework_loops: number
  external_ref: { repo: string; issue: number; url: string } | null
  parent: string | null
  /** Gates and questions are decided by the autopilot agent. */
  autopilot: boolean
}

export interface TaskDetail {
  task: TaskSummary | null
  state: TaskState
  /** Direct subtasks, oldest first. */
  children: TaskSummary[]
  /** From the root down to the parent. */
  ancestors: TaskSummary[]
  events: WhEvent[]
  sessions: SessionDetail[]
  scores: ScoreRow[]
  /** What the autopilot decided (autopilot tasks only), oldest first. Missing from older servers. */
  autopilot?: AutopilotRun[]
}

/** One scorer run (LLM-as-judge grading one session); `score_results` row. */
export interface ScoreRow {
  score_id: string
  scorer: string
  target_session: string
  task_id: string
  task_title: string | null
  agent: string
  stage: string
  model: string | null
  pass_threshold: number
  manual: boolean
  status: 'running' | 'scored' | 'failed'
  score: number | null
  pass: boolean | null
  classification: string | null
  rationale: string | null
  error: string | null
  cost_usd: number
  duration_ms: number | null
  transcript: string | null
  started_at: string
  ended_at: string | null
  factory_version: string | null
}

export interface ScorerDef {
  id: string
  target: string
  description: string | null
  model: string
  pass_threshold: number
  sampling_rate: number
  budget_usd: number
  include_failed: boolean
  prompt: string
}

export interface ScorerStats {
  scorer: string
  agent: string
  runs: number
  scored: number
  passed: number
  errors: number
  pass_rate: number | null
  avg_score: number | null
  pass_rate_7d: number | null
  scored_7d: number
  cost_usd: number
  avg_cost_usd: number | null
  last_run_at: string | null
}

export interface ScorersResponse {
  scorers: { id: string; definition: ScorerDef | null; stats: ScorerStats | null }[]
  by_version: {
    scorer: string
    factory_version: string | null
    scored: number
    passed: number
    avg_score: number | null
    first_at: string
  }[]
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
    scorer_cost_usd: number
    scores: number
    scores_passed: number
  }
  daily: {
    day: string
    cost_usd: number
    sessions_succeeded: number
    sessions_failed: number
    tasks_created: number
    tasks_done: number
  }[]
  cycle: CycleTimes
}

/** Pull request cycle times over tasks shipped in the window (`wh_metrics::queries::cycle_times`). */
export interface CycleTimes {
  shipped: number
  merged: number
  open: number
  changes_requested_tasks: number
  ci_failed_tasks: number
  avg_pr_rework: number | null
  kickoff_to_pr_p50_ms: number | null
  kickoff_to_pr_p95_ms: number | null
  pr_to_first_review_p50_ms: number | null
  pr_to_first_review_p95_ms: number | null
  review_to_merge_p50_ms: number | null
  review_to_merge_p95_ms: number | null
  kickoff_to_merge_p50_ms: number | null
  kickoff_to_merge_p95_ms: number | null
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
      sandbox: Sandbox
      prompt: string
    }
  }[]
  scorers: ScorerDef[]
  improvement: ImprovementDef | null
  github: GithubDef | null
}

export interface GithubDef {
  repo: string | null
  remote: string
  base: string | null
  draft: boolean
  labels: string[]
  poll_interval_sec: number
  ship_gate: string | null
  conflict_gate: string | null
  rework_stage: string
  issue_label: string | null
}

export interface ImprovementDef {
  model: string
  budget_usd: number
  window_days: number
  min_sessions: number
  interval_hours: number | null
  max_open_proposals: number
  triggers: { metric: string; subject: string; above: number }[]
  prompt: string
}

export type ProposalStatus =
  | 'requested'
  | 'running'
  | 'open'
  | 'no_proposal'
  | 'failed'
  | 'approved'
  | 'rejected'
  | 'rolled_out'

/** One improver run / proposal; `proposals_view` row. */
export interface ProposalRow {
  proposal_id: string
  factory: string
  status: ProposalStatus
  trigger: string
  focus: string | null
  requested_by: string
  requested_at: string
  started_at: string | null
  finished_at: string | null
  base_factory_version: string | null
  sessions_observed: number | null
  title: string | null
  rationale: string | null
  expected_impact: string | null
  evidence: string[]
  files: string[]
  diff: string | null
  branch: string | null
  commit_sha: string | null
  proposed_factory_version: string | null
  message: string | null
  cost_usd: number
  duration_ms: number | null
  transcript: string | null
  decided_by: string | null
  decided_at: string | null
  decision_comment: string | null
  rolled_out_at: string | null
  rolled_out_version: string | null
}

/** Pipeline metrics of one factory_version. */
export interface VersionStats {
  factory_version: string
  tasks: number
  sessions: number
  failed: number
  retries: number
  rejected: number
  fail_rate: number | null
  reject_rate: number | null
  avg_cost_usd: number | null
  cost_usd: number
  p50_duration_ms: number | null
  tool_error_rate: number | null
  scored: number
  score_pass_rate: number | null
  first_at: string
  last_at: string
}

export interface ImproverStats {
  runs: number
  proposals: number
  no_proposal: number
  failed: number
  open: number
  approved: number
  rejected: number
  rolled_out: number
  approval_rate: number | null
  cost_usd: number
  last_run_at: string | null
}

export interface ImprovementResponse {
  factory: string | null
  current_version: string | null
  config: ImprovementDef | null
  stats: ImproverStats
  versions: VersionStats[]
}

export interface ProposalDetail {
  proposal: ProposalRow | null
  events: WhEvent[]
  evidence: SessionDetail[]
  before: VersionStats | null
  after: VersionStats | null
}

export interface Transcript {
  path: string
  truncated: boolean
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  lines: any[]
}

export interface Me {
  auth_enabled: boolean
  user: string | null
}

/** Fired when the server answers 401 (session expired or logged out elsewhere). */
export const UNAUTHORIZED_EVENT = 'wh:unauthorized'

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
  if (res.status === 401 && !path.startsWith('/auth/')) window.dispatchEvent(new Event(UNAUTHORIZED_EVENT))
  if (!res.ok) throw new ApiError(res.status, data?.error ?? `${res.status} ${res.statusText}`)
  return data as T
}

export type UpdateStepStatus = 'pending' | 'running' | 'done' | 'skipped' | 'failed'
export interface UpdateJob {
  state: 'running' | 'failed' | 'restarting'
  steps: { id: string; label: string; status: UpdateStepStatus; output: string }[]
  started_at: string
  finished_at: string | null
  error: string | null
  from: string | null
  to: string | null
  force: boolean
}
export interface SystemInfo {
  boot_id: string
  version: string
  commit: string | null
  self_update: { enabled: boolean; reason: string | null; branch: string | null; repo: string | null }
  job: UpdateJob | null
  busy: number
}
export interface UpdateCheck {
  branch: string
  expected_branch: string
  dirty: boolean
  head: string
  commits: { sha: string; subject: string }[]
  blocked: string | null
}

export const api = {
  system: () => request<SystemInfo>('GET', '/system'),
  checkUpdate: () => request<UpdateCheck>('POST', '/system/update/check'),
  startUpdate: (force = false) => request<{ message: string }>('POST', '/system/update', { force }),
  me: () => request<Me>('GET', '/auth/me'),
  login: (username: string, password: string) => request<Me>('POST', '/auth/login', { username, password }),
  logout: () => request<null>('POST', '/auth/logout'),
  // The Runs page pages through the list itself; the server returns at most 1000.
  tasks: (status?: string) => request<TaskSummary[]>('GET', `/tasks?limit=1000${status ? `&status=${status}` : ''}`),
  task: (id: string) => request<TaskDetail>('GET', `/tasks/${id}`),
  createTask: (b: { title: string; body?: string; pipeline?: boolean; parent?: string; start?: boolean; autopilot?: boolean }) =>
    request<{ task_id: string; factory: string | null }>('POST', '/tasks', b),
  decideGate: (id: string, approved: boolean, comment?: string) =>
    request<{ message: string }>('POST', `/tasks/${id}/gate`, { approved, comment }),
  resume: (id: string, stage?: string, comment?: string) =>
    request<{ message: string }>('POST', `/tasks/${id}/resume`, { stage, comment }),
  cancel: (id: string, reason: string) => request<{ message: string }>('POST', `/tasks/${id}/cancel`, { reason }),
  startTask: (id: string, subtree = false, autopilot = false) =>
    request<{ message: string }>('POST', `/tasks/${id}/start`, { subtree, autopilot }),
  moveTask: (id: string, parent: string | null) => request<{ message: string }>('POST', `/tasks/${id}/move`, { parent }),
  sessions: (limit = 100) => request<SessionDetail[]>('GET', `/sessions?limit=${limit}`),
  transcript: (id: string) => request<Transcript>('GET', `/sessions/${id}/transcript`),
  stats: () => request<Stats>('GET', '/stats'),
  dashboard: (days: number) => request<Dashboard>('GET', `/dashboard?days=${days}`),
  factory: () => request<FactoryDef | null>('GET', '/factory'),
  scorers: () => request<ScorersResponse>('GET', '/scorers'),
  scores: (q: { scorer?: string; failing?: boolean; limit?: number } = {}) => {
    const p = new URLSearchParams()
    if (q.scorer) p.set('scorer', q.scorer)
    if (q.failing) p.set('failing', 'true')
    p.set('limit', String(q.limit ?? 100))
    return request<ScoreRow[]>('GET', `/scores?${p}`)
  },
  proposals: (status?: string) => request<ProposalRow[]>('GET', `/proposals${status ? `?status=${status}` : ''}`),
  proposal: (id: string) => request<ProposalDetail>('GET', `/proposals/${id}`),
  requestImprovement: (focus?: string) =>
    request<{ proposal_id: string; message: string }>('POST', '/proposals', { focus }),
  decideProposal: (id: string, approved: boolean, comment?: string) =>
    request<{ message: string }>('POST', `/proposals/${id}/decide`, { approved, comment }),
  applyProposal: (id: string) => request<{ message: string }>('POST', `/proposals/${id}/apply`),
  improvement: () => request<ImprovementResponse>('GET', '/improvement'),
  events: (after = 0, limit = 100) => request<WhEvent[]>('GET', `/events?after=${after}&limit=${limit}`),
}

// ---------- benchmarks (crates/wh-api/src/bench.rs, wh_metrics::benchmarks) ----------

export type BenchRunStatus = 'requested' | 'running' | 'completed' | 'failed'

export interface BenchRun {
  run_id: string
  suite: string
  suite_file: string
  git_ref: string | null
  proposal_id: string | null
  label: string | null
  repeat: number
  status: BenchRunStatus
  requested_by: string
  requested_at: string
  started_at: string | null
  finished_at: string | null
  factory: string | null
  factory_version: string | null
  factory_commit: string | null
  suite_version: string | null
  planned: number | null
  cases_done: number
  passed: number
  cost_usd: number
  duration_ms: number | null
  message: string | null
  pass_rate: number | null
  avg_case_cost_usd: number | null
  p50_case_duration_ms: number | null
  avg_rework: number | null
}

export interface BenchCase {
  run_id: string
  case_id: string
  repeat: number
  task_id: string
  status: 'running' | 'done' | 'blocked' | 'cancelled' | 'error'
  passed: boolean | null
  check_passed: boolean | null
  check_exit_code: number | null
  check_output: string | null
  cost_usd: number
  duration_ms: number | null
  sessions: number | null
  rework_loops: number | null
  message: string | null
  started_at: string
  finished_at: string | null
}

export interface CaseStats {
  case_id: string
  runs: number
  passed: number
  avg_cost_usd: number | null
  avg_duration_ms: number | null
  avg_rework: number | null
}

export interface BenchRunDetail {
  run: BenchRun | null
  cases: BenchCase[]
  case_stats: CaseStats[]
  events: WhEvent[]
}

export interface BenchComparison {
  a: BenchRun
  b: BenchRun
  comparable: boolean
  cases: { case_id: string; a: CaseStats | null; b: CaseStats | null }[]
}

export interface BenchSuite {
  key: string
  file: string
  name: string | null
  description: string | null
  version: string | null
  factory_file: string | null
  max_cost_usd: number | null
  cases: { id: string; title: string; repo: 'factory' | 'repo' | 'fixture'; check: string | null }[]
  error: string | null
}

export interface BenchRequest {
  suite: string
  git_ref?: string
  proposal?: string
  label?: string
  repeat?: number
  cases?: string[]
}

export const benchApi = {
  suites: () => request<BenchSuite[]>('GET', '/benchmarks/suites'),
  runs: (q: { suite?: string; proposal?: string; limit?: number } = {}) => {
    const p = new URLSearchParams()
    if (q.suite) p.set('suite', q.suite)
    if (q.proposal) p.set('proposal', q.proposal)
    p.set('limit', String(q.limit ?? 100))
    return request<BenchRun[]>('GET', `/benchmarks?${p}`)
  },
  run: (id: string) => request<BenchRunDetail>('GET', `/benchmarks/${id}`),
  request: (b: BenchRequest) => request<{ run_ids: string[]; message: string }>('POST', '/benchmarks', b),
  compare: (a: string, b: string) => request<BenchComparison>('GET', `/benchmarks/compare?a=${a}&b=${b}`),
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

export function fmtPct(v: number | null | undefined, digits = 0): string {
  return v == null ? '–' : `${(v * 100).toFixed(digits)}%`
}

export function firstLine(s: string): string {
  return s.split('\n').find((l) => l.trim())?.trim() ?? ''
}

/** Where an agent's sessions run (`sandbox:` in factory.yaml). */
export type Sandbox =
  | { kind: 'worktree' }
  | {
      kind: 'docker'
      image: string
      network: string
      cpus: number | null
      memory: string | null
      pids_limit: number | null
      env: string[]
      volumes: string[]
      copy_credentials: boolean
    }
