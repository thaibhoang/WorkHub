//! Scorers: LLM-as-judge quality checks on agent sessions.
//!
//! [`ScorerService`] consumes the event log. When a session of an agent that a
//! factory scorer targets finishes, it builds a grading prompt (rubric + task +
//! the agent's final message, committed diff and condensed transcript), runs a
//! cheap model with no tools, and records `ScoreRecorded` (or `ScoreFailed`) on
//! a `score-<id>` stream, including the judge's own cost.
//!
//! Exactly-once: the automatic score id is derived from (session, scorer) and
//! `ScoreStarted` is appended with `ExpectedVersion::NoStream`, so redelivered
//! events never grade a session twice.

pub mod judge;

use std::{path::PathBuf, sync::Arc, time::Instant};

use anyhow::{Context, bail};
use tokio::sync::Semaphore;
use wh_core::{
    Actor, EventMeta, NewEvent, RecordedEvent, ScoreId, SessionId, TaskId,
    score::ScoreEvent,
    session::SessionEvent,
    task::TaskEvent,
};
use wh_eventstore::{EventHandler, EventStore, ExpectedVersion, StoreError};
use wh_factory::{Factory, Scorer};
use wh_runner::{AgentRequest, AgentRuntime, OutcomeStatus};

pub use judge::{SessionContext, build_prompt, condense_transcript, parse_judgement};

/// How grading runs are executed.
#[derive(Clone)]
pub enum Dispatch {
    /// Await grading inside the event handler (deterministic; used by tests).
    Inline,
    /// Grade in the background, at most `n` at a time.
    Spawn(Arc<Semaphore>),
}

#[derive(Debug, Clone)]
pub struct ScorerConfig {
    /// Judge transcripts go to `<data_dir>/transcripts/score-<id>.jsonl`.
    pub data_dir: PathBuf,
}

/// Result of grading one session with one scorer.
#[derive(Debug, Clone, PartialEq)]
pub enum Graded {
    Scored { score: f64, pass: bool, classification: Option<String>, rationale: String, cost_usd: f64 },
    Failed { message: String, cost_usd: f64 },
    /// Already graded (another delivery or process got there first).
    Duplicate,
}

pub struct ScorerService<R> {
    inner: Arc<Inner<R>>,
    dispatch: Dispatch,
    name: String,
}

struct Inner<R> {
    store: EventStore,
    factory: Arc<Factory>,
    runtime: R,
    config: ScorerConfig,
}

impl<R: AgentRuntime + 'static> ScorerService<R> {
    pub fn new(store: EventStore, factory: Factory, runtime: R, config: ScorerConfig, dispatch: Dispatch) -> Self {
        let name = format!("scorer:{}", factory.name);
        Self { inner: Arc::new(Inner { store, factory: Arc::new(factory), runtime, config }), dispatch, name }
    }

    /// Consumer name (checkpoint key).
    pub fn consumer_name(&self) -> &str {
        &self.name
    }

    /// On the very first start, skip the existing history so enabling scorers
    /// does not grade (and pay for) every past session. Use `wh score session`
    /// to grade old sessions explicitly.
    pub async fn start_from_head_if_new(&self) -> anyhow::Result<bool> {
        let store = &self.inner.store;
        if store.load_checkpoint(&self.name).await? != 0 {
            return Ok(false);
        }
        let head = store.head_seq().await?;
        store.save_checkpoint(&self.name, head).await?;
        Ok(head > 0)
    }

    /// Call once at startup: a grading that was running when the previous process
    /// died is marked failed so it does not stay "running" forever.
    pub async fn recover_interrupted(&self) -> anyhow::Result<usize> {
        let store = &self.inner.store;
        let mut n = 0;
        for (stream, version, head) in store.stream_heads(&format!("{}-", ScoreId::STREAM_PREFIX)).await? {
            if head != "ScoreStarted" {
                continue;
            }
            let Some(first) = store.read_stream(&stream).await?.into_iter().next() else { continue };
            let ev = ScoreEvent::ScoreFailed {
                message: "interrupted: the scorer stopped while grading".into(),
                cost_usd: 0.0,
                duration_ms: 0,
                transcript: None,
            };
            let meta = self.inner.meta(TaskId(first.meta.correlation_id), Some(first.event_id));
            if store.append(&stream, ExpectedVersion::Exact(version), vec![NewEvent::from_domain(&ev, meta)?]).await.is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Grade `session` now, ignoring sampling and the factory match (a human asked).
    /// Uses every scorer targeting the session's agent, or only `scorer`.
    pub async fn grade_session(&self, session: SessionId, scorer: Option<&str>) -> anyhow::Result<Vec<(String, Graded)>> {
        let info = self.inner.session_info(session).await?.context("session not found or still running")?;
        let scorers: Vec<Scorer> = match scorer {
            Some(id) => vec![self.inner.factory.scorer(id).with_context(|| format!("unknown scorer '{id}'"))?.clone()],
            None => self.inner.factory.scorers_for(&info.agent).cloned().collect(),
        };
        if scorers.is_empty() {
            bail!("no scorer in factory '{}' targets agent '{}'", self.inner.factory.name, info.agent);
        }
        let mut out = Vec::new();
        for sc in scorers {
            let graded = self.inner.grade(&sc, &info, ScoreId::new(), true).await?;
            out.push((sc.id.clone(), graded));
        }
        Ok(out)
    }
}

/// Deterministic sampling: the same (session, scorer) always gets the same decision.
pub fn sampled(session: SessionId, scorer: &Scorer) -> bool {
    if scorer.sampling_rate >= 1.0 {
        return true;
    }
    let bytes = ScoreId::for_session(session, &scorer.id).0.into_bytes();
    let x = u64::from_be_bytes(bytes[8..16].try_into().expect("8 bytes")) as f64 / u64::MAX as f64;
    x < scorer.sampling_rate
}

/// Everything known about a finished session.
struct SessionInfo {
    session: SessionId,
    task: TaskId,
    agent: String,
    stage: String,
    prompt: String,
    failed: bool,
    outcome: String,
    summary: Option<String>,
    transcript: Option<String>,
    commit: Option<String>,
    /// Event that finished the session (cause of the score).
    finished_event: uuid::Uuid,
    /// Session of a benchmark case (not graded automatically).
    benchmark: bool,
}

impl<R: AgentRuntime> Inner<R> {
    fn meta(&self, task: TaskId, cause: Option<uuid::Uuid>) -> EventMeta {
        let mut meta = EventMeta::new(task.0, Actor::System { component: "scorer".into() });
        meta.causation_id = cause;
        meta.factory_version = Some(self.factory.version.clone());
        meta
    }

    async fn session_info(&self, session: SessionId) -> anyhow::Result<Option<SessionInfo>> {
        let events = self.store.read_stream(&session.stream_id()).await?;
        let Some(SessionEvent::AgentDispatched { task_id, agent, stage, prompt, benchmark, .. }) =
            events.first().map(|e| e.decode()).transpose()?
        else {
            return Ok(None);
        };
        let Some(last) = events.last() else { return Ok(None) };
        let (failed, outcome, summary, transcript, commit) = match last.decode::<SessionEvent>()? {
            SessionEvent::AgentSessionCompleted { summary, transcript, commit, .. } => {
                (false, "succeeded".to_owned(), summary, transcript, commit)
            }
            SessionEvent::AgentSessionFailed { failure_class, message, transcript, .. } => {
                (true, format!("failed [{}]: {message}", failure_class.as_str()), None, transcript, None)
            }
            _ => return Ok(None),
        };
        Ok(Some(SessionInfo {
            session,
            task: task_id,
            agent,
            stage,
            prompt,
            failed,
            outcome,
            summary,
            transcript,
            commit,
            finished_event: last.event_id,
            benchmark: benchmark.is_some(),
        }))
    }

    async fn context(&self, info: &SessionInfo) -> anyhow::Result<SessionContext> {
        let (mut title, mut body) = (String::new(), None);
        for e in self.store.read_stream(&info.task.stream_id()).await? {
            if let Ok(TaskEvent::TaskCreated { title: t, body: b, .. }) = e.decode() {
                (title, body) = (t, b);
                break;
            }
        }
        let diff = match &info.commit {
            Some(sha) => git_show(&self.factory.repo, sha).await,
            None => None,
        };
        let transcript = match &info.transcript {
            Some(path) => tokio::fs::read_to_string(path).await.ok().map(|t| condense_transcript(&t)),
            None => None,
        };
        Ok(SessionContext {
            task_title: title,
            task_body: body,
            agent: info.agent.clone(),
            stage: info.stage.clone(),
            agent_prompt: info.prompt.clone(),
            outcome: info.outcome.clone(),
            summary: info.summary.clone(),
            diff,
            transcript,
        })
    }

    async fn task_factory(&self, task: TaskId) -> anyhow::Result<Option<String>> {
        let events = self.store.read_stream(&task.stream_id()).await?;
        Ok(events.first().and_then(|e| match e.decode() {
            Ok(TaskEvent::TaskCreated { factory, .. }) => factory,
            _ => None,
        }))
    }

    async fn grade(&self, scorer: &Scorer, info: &SessionInfo, id: ScoreId, manual: bool) -> anyhow::Result<Graded> {
        let stream = id.stream_id();
        let started = ScoreEvent::ScoreStarted {
            scorer: scorer.id.clone(),
            target_session: info.session,
            task_id: info.task,
            agent: info.agent.clone(),
            stage: info.stage.clone(),
            model: Some(scorer.model.clone()),
            pass_threshold: scorer.pass_threshold,
            manual,
        };
        let meta = self.meta(info.task, Some(info.finished_event));
        let rec = match self.store.append(&stream, ExpectedVersion::NoStream, vec![NewEvent::from_domain(&started, meta)?]).await {
            Ok(r) => r,
            Err(StoreError::Conflict { .. }) => return Ok(Graded::Duplicate),
            Err(e) => return Err(e.into()),
        };
        let started_id = rec.last().map(|r| r.event_id);

        let (event, graded) = match self.judge(scorer, info, id).await {
            Ok(r) => r,
            Err(e) => {
                let message = format!("{e:#}");
                let ev = ScoreEvent::ScoreFailed { message: message.clone(), cost_usd: 0.0, duration_ms: 0, transcript: None };
                (ev, Graded::Failed { message, cost_usd: 0.0 })
            }
        };
        let meta = self.meta(info.task, started_id);
        self.store.append(&stream, ExpectedVersion::Exact(1), vec![NewEvent::from_domain(&event, meta)?]).await?;
        Ok(graded)
    }

    /// Run the judge. `Err` only for WorkHub-side problems before the model runs.
    async fn judge(&self, scorer: &Scorer, info: &SessionInfo, id: ScoreId) -> anyhow::Result<(ScoreEvent, Graded)> {
        let t0 = Instant::now();
        let ctx = self.context(info).await?;
        let workdir = self.config.data_dir.join("scorer-work").join(id.to_string());
        let transcripts = self.config.data_dir.join("transcripts");
        tokio::fs::create_dir_all(&workdir).await.context("create scorer workdir")?;
        tokio::fs::create_dir_all(&transcripts).await.context("create transcripts dir")?;
        let transcript_path = transcripts.join(format!("score-{id}.jsonl"));
        let req = AgentRequest {
            prompt: build_prompt(scorer, &ctx),
            workdir: workdir.clone(),
            transcript_path: transcript_path.clone(),
            model: Some(scorer.model.clone()),
            budget_usd: Some(scorer.budget_usd),
            permission_mode: None,
            allowed_tools: Vec::new(),
            with_user_mcp: false,
            timeout: scorer.timeout,
            tools: Some(Vec::new()),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let o = self.runtime.run(&req, tx).await;
        let _ = tokio::fs::remove_dir_all(&workdir).await;
        let transcript = transcript_path.exists().then(|| transcript_path.display().to_string());
        let duration_ms = t0.elapsed().as_millis() as u64;
        let cost_usd = o.cost_usd;

        let parsed = match &o.status {
            OutcomeStatus::Failed { class, message, .. } => Err(format!("judge failed [{}]: {message}", class.as_str())),
            OutcomeStatus::Succeeded => parse_judgement(o.summary.as_deref().unwrap_or("")),
        };
        Ok(match parsed {
            Ok(j) => {
                let pass = j.score >= scorer.pass_threshold;
                let ev = ScoreEvent::ScoreRecorded {
                    score: j.score,
                    pass,
                    classification: j.classification.clone(),
                    rationale: j.rationale.clone(),
                    cost_usd,
                    usage: o.usage,
                    duration_ms,
                    transcript,
                };
                let g = Graded::Scored {
                    score: j.score,
                    pass,
                    classification: j.classification,
                    rationale: j.rationale,
                    cost_usd,
                };
                (ev, g)
            }
            Err(message) => (
                ScoreEvent::ScoreFailed { message: message.clone(), cost_usd, duration_ms, transcript },
                Graded::Failed { message, cost_usd },
            ),
        })
    }
}

async fn git_show(repo: &std::path::Path, sha: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", "--stat", "--patch", "--format=commit %H%n%s%n", sha])
        .output()
        .await
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl<R: AgentRuntime + 'static> EventHandler for ScorerService<R> {
    fn name(&self) -> &str {
        &self.name
    }

    async fn handle(&mut self, ev: &RecordedEvent) -> anyhow::Result<()> {
        if !matches!(ev.event_type.as_str(), "AgentSessionCompleted" | "AgentSessionFailed") {
            return Ok(());
        }
        let Some(session) = SessionId::from_stream_id(&ev.stream_id) else { return Ok(()) };
        let inner = &self.inner;
        let Some(info) = inner.session_info(session).await? else { return Ok(()) };
        // Benchmark sessions are measured by their suite's checks; grading them would
        // mix benchmark runs into the live scorer metrics the improver reads.
        if info.benchmark {
            return Ok(());
        }
        let scorers: Vec<Scorer> = inner
            .factory
            .scorers_for(&info.agent)
            .filter(|s| !info.failed || s.include_failed)
            .filter(|s| sampled(session, s))
            .cloned()
            .collect();
        if scorers.is_empty() {
            return Ok(());
        }
        // Agent names are only meaningful within a factory: grade only that factory's tasks.
        if inner.task_factory(info.task).await?.as_deref() != Some(inner.factory.name.as_str()) {
            return Ok(());
        }
        let info = Arc::new(info);
        for sc in scorers {
            let (inner, info) = (self.inner.clone(), info.clone());
            let run = async move {
                let id = ScoreId::for_session(info.session, &sc.id);
                match inner.grade(&sc, &info, id, false).await {
                    Ok(Graded::Duplicate) => {}
                    Ok(g) => tracing::info!(session = %info.session, scorer = sc.id, result = ?g, "graded"),
                    Err(e) => tracing::error!(session = %info.session, scorer = sc.id, "grading failed: {e:#}"),
                }
            };
            match &self.dispatch {
                Dispatch::Inline => run.await,
                Dispatch::Spawn(limit) => {
                    let limit = limit.clone();
                    tokio::spawn(async move {
                        let _permit = limit.acquire_owned().await;
                        run.await
                    });
                }
            }
        }
        Ok(())
    }
}
