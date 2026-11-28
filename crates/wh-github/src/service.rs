//! The GitHub worker: turns labelled issues into tasks and polls the PRs of
//! shipped tasks, recording only what changed. The task streams are the state:
//! what was already recorded is folded from them, so no extra table is needed.

use std::{sync::Arc, time::Duration};

use serde::Deserialize;
use wh_core::{
    Actor, EventMeta, NewEvent, TaskId,
    task::{CHANGES_REQUESTED, ExternalRef, FailedCheck, PrComment, TaskEvent, TaskSource, TaskState, TaskStatus},
};
use wh_eventstore::{EventStore, ExpectedVersion, StoreError};
use wh_factory::Factory;

use crate::{
    cli::{CliResult, Gh},
    poll::{self, Observed, PR_VIEW_FIELDS, PrView, Seen},
    ship::resolve_repo,
};

const MAX_LOG: usize = 4_000;

#[derive(Debug, Default)]
pub struct SyncReport {
    /// Tasks created from issues.
    pub created: Vec<(TaskId, u64)>,
    /// Event types recorded per task.
    pub recorded: Vec<(TaskId, Vec<&'static str>)>,
    /// Non-fatal problems (one PR could not be read, ...).
    pub errors: Vec<String>,
}

#[derive(Clone)]
pub struct GithubService {
    store: EventStore,
    factory: Arc<Factory>,
    gh: Gh,
}

#[derive(Deserialize)]
struct Issue {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    url: String,
}

#[derive(Deserialize)]
struct RestReview {
    id: u64,
    node_id: String,
}

#[derive(Deserialize)]
struct RestComment {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    original_line: Option<u64>,
    body: String,
}

impl GithubService {
    pub fn new(store: EventStore, factory: Factory, gh: Gh) -> anyhow::Result<Self> {
        anyhow::ensure!(factory.github.is_some(), "factory '{}' has no `github:` section", factory.name);
        Ok(Self { store, factory: Arc::new(factory), gh })
    }

    pub fn factory(&self) -> &Factory {
        &self.factory
    }

    fn meta(&self, task: TaskId) -> EventMeta {
        let mut meta = EventMeta::new(task.0, Actor::System { component: "github".into() });
        meta.factory_version = Some(self.factory.version.clone());
        meta
    }

    /// Poll every `poll_interval` until `stop` resolves.
    pub async fn run(&self, stop: impl std::future::Future<Output = ()>) -> anyhow::Result<()> {
        let g = self.factory.github.as_ref().expect("checked in new");
        let mut every = tokio::time::interval(g.poll_interval.max(Duration::from_secs(5)));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = &mut stop => return Ok(()),
                _ = every.tick() => match self.sync_once().await {
                    Ok(r) => {
                        for (task, issue) in &r.created {
                            tracing::info!(%task, issue, "task created from GitHub issue");
                        }
                        for (task, evs) in &r.recorded {
                            tracing::info!(%task, events = ?evs, "pull request changed");
                        }
                        for e in &r.errors {
                            tracing::warn!("github: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("github sync failed: {e:#}"),
                },
            }
        }
    }

    /// One pass: issue intake, then every open PR of this factory's tasks.
    pub async fn sync_once(&self) -> anyhow::Result<SyncReport> {
        let mut report = SyncReport::default();
        let g = self.factory.github.as_ref().expect("checked in new");
        let repo = resolve_repo(&self.factory, g).await?;
        if let Some(issues) = &g.issues {
            match self.intake(&repo, &issues.label, issues.comment).await {
                Ok(created) => report.created = created,
                Err(e) => report.errors.push(format!("issue intake: {e}")),
            }
        }
        wh_metrics::catch_up_all(&self.store).await?;
        let open: Vec<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT task_id FROM tasks_view WHERE factory = $1 AND benchmark_run IS NULL \
             AND pr_number IS NOT NULL AND pr_state = 'open' ORDER BY shipped_at",
        )
        .bind(&self.factory.name)
        .fetch_all(self.store.pool())
        .await?;
        for (id,) in open {
            let task = TaskId(id);
            match self.sync_task(task).await {
                Ok(evs) if !evs.is_empty() => report.recorded.push((task, evs)),
                Ok(_) => {}
                Err(e) => report.errors.push(format!("task {task}: {e:#}")),
            }
        }
        if !report.created.is_empty() || !report.recorded.is_empty() {
            wh_metrics::catch_up_all(&self.store).await?;
        }
        Ok(report)
    }

    /// Create a task for each open issue with `label` (once per issue).
    async fn intake(&self, repo: &str, label: &str, comment: bool) -> CliResult<Vec<(TaskId, u64)>> {
        let issues: Vec<Issue> = self
            .gh
            .json(None, &[
                "issue", "list", "--repo", repo, "--label", label, "--state", "open", "--limit", "100", "--json",
                "number,title,body,url",
            ])
            .await?;
        let mut created = Vec::new();
        for i in issues {
            let task = TaskId::for_external(&format!("github:{}#{}", repo.to_ascii_lowercase(), i.number));
            let ev = TaskEvent::TaskCreated {
                title: i.title.trim().to_owned(),
                body: Some(i.body.clone()).filter(|b| !b.trim().is_empty()),
                source: TaskSource::Github,
                factory: Some(self.factory.name.clone()),
                benchmark: None,
                external_ref: Some(ExternalRef { repo: repo.to_owned(), issue: i.number, url: i.url.clone() }),
                parent: None,
                draft: false,
                autopilot: false,
            };
            let Ok(new) = NewEvent::from_domain(&ev, self.meta(task)) else { continue };
            match self.store.append(&task.stream_id(), ExpectedVersion::NoStream, vec![new]).await {
                Ok(_) => {
                    created.push((task, i.number));
                    if comment {
                        let body = format!("WorkHub picked this up as task `{task}`. A pull request will link back here.");
                        let n = i.number.to_string();
                        if let Err(e) = self.gh.run(None, &["issue", "comment", &n, "--repo", repo, "--body", &body]).await {
                            tracing::warn!(issue = i.number, "could not comment on issue: {e}");
                        }
                    }
                }
                Err(StoreError::Conflict { .. }) => {} // already a task
                Err(e) => tracing::warn!(issue = i.number, "could not create task: {e}"),
            }
        }
        Ok(created)
    }

    /// Read one task's PR and record what is new. Returns the recorded event types.
    pub async fn sync_task(&self, task: TaskId) -> anyhow::Result<Vec<&'static str>> {
        let events = self.store.read_stream(&task.stream_id()).await?;
        let mut state = TaskState::default();
        let mut decoded = Vec::with_capacity(events.len());
        for e in &events {
            let ev: TaskEvent = e.decode()?;
            state.apply(e.stream_version, ev.clone());
            decoded.push(ev);
        }
        let Some(pr) = state.pr.clone() else { return Ok(Vec::new()) };
        let seen = Seen::fold(&decoded);
        let n = pr.number.to_string();
        let view: PrView =
            self.gh.json(None, &["pr", "view", &n, "--repo", &pr.repo, "--json", PR_VIEW_FIELDS]).await?;
        let mut new = Vec::new();
        for o in poll::observe(&seen, &view) {
            // Recording marks the commit as seen, so only record a conflict the task
            // can act on: waiting on its PR, about the commit it pushed.
            if let Observed::Conflict { commit, .. } = &o
                && (state.status != TaskStatus::Shipped || *commit != pr.commit)
            {
                continue;
            }
            new.push(match o {
                Observed::Review { id, reviewer, state: review, body, submitted_at } => {
                    let comments = if review == CHANGES_REQUESTED {
                        self.review_comments(&pr.repo, pr.number, &id).await
                    } else {
                        Vec::new()
                    };
                    TaskEvent::PrReviewed { review_id: id, reviewer, state: review, body, comments, submitted_at }
                }
                Observed::Checks { commit, passed, failed } => {
                    let mut out = Vec::new();
                    for c in failed {
                        let log = match c.url.as_deref().and_then(poll::actions_run_id) {
                            Some(run) => self.failed_log(&pr.repo, run).await,
                            None => None,
                        };
                        out.push(FailedCheck { name: c.name, url: c.url, log });
                    }
                    let conclusion = if passed { "success" } else { "failure" }.to_owned();
                    TaskEvent::PrChecksCompleted { commit, conclusion, failed: out }
                }
                Observed::Conflict { base, commit } => TaskEvent::PrConflicted { base, commit },
                Observed::Merged { at, by } => TaskEvent::PrMerged { merged_at: at, merged_by: by },
                Observed::Closed { at } => TaskEvent::PrClosed { closed_at: at },
            });
        }
        if new.is_empty() {
            return Ok(Vec::new());
        }
        let types: Vec<&'static str> = new.iter().map(wh_core::DomainEvent::event_type).collect();
        let batch = new.iter().map(|e| NewEvent::from_domain(e, self.meta(task))).collect::<Result<Vec<_>, _>>()?;
        // One append: the foreman only acts on the stream head, so a merge seen in the
        // same poll as a failed check never triggers a rework.
        match self.store.append(&task.stream_id(), ExpectedVersion::Exact(state.version), batch).await {
            Ok(_) => Ok(types),
            Err(StoreError::Conflict { .. }) => Ok(Vec::new()), // task moved on; next poll retries
            Err(e) => Err(e.into()),
        }
    }

    /// Inline comments of one review (best effort: empty on any error).
    async fn review_comments(&self, repo: &str, pr: u64, node_id: &str) -> Vec<PrComment> {
        let reviews: Vec<RestReview> = match self.gh.json(None, &["api", &format!("repos/{repo}/pulls/{pr}/reviews")]).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("review list: {e}");
                return Vec::new();
            }
        };
        let Some(id) = reviews.iter().find(|r| r.node_id == node_id).map(|r| r.id) else { return Vec::new() };
        let path = format!("repos/{repo}/pulls/{pr}/reviews/{id}/comments");
        match self.gh.json::<Vec<RestComment>>(None, &["api", &path]).await {
            Ok(cs) => cs
                .into_iter()
                .map(|c| PrComment { path: c.path, line: c.line.or(c.original_line), body: c.body })
                .collect(),
            Err(e) => {
                tracing::debug!("review comments: {e}");
                Vec::new()
            }
        }
    }

    /// Tail of a failed Actions run's log (best effort).
    async fn failed_log(&self, repo: &str, run: u64) -> Option<String> {
        let raw = self.gh.run(None, &["run", "view", &run.to_string(), "--repo", repo, "--log-failed"]).await.ok()?;
        let out = poll::clean_actions_log(&raw);
        let chars: Vec<char> = out.chars().collect();
        let tail: String = chars[chars.len().saturating_sub(MAX_LOG)..].iter().collect();
        Some(tail).filter(|t| !t.trim().is_empty())
    }
}
