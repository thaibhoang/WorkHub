use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    benchmark::BenchmarkContext,
    event::DomainEvent,
    ids::{SessionId, TaskId},
};

/// Pseudo stage id used for shipping (push + pull request) after the last
/// pipeline stage. Gates and blocks at shipping time name this stage.
pub const SHIP_STAGE: &str = "ship";

/// Pseudo stage id of the human gate (and block) for a pull request that
/// conflicts with its base branch.
pub const CONFLICT_STAGE: &str = "conflict";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    Cli,
    Web,
    /// A benchmark case created by the benchmark runner.
    Benchmark,
    /// Created from a labelled GitHub issue.
    Github,
}

impl TaskSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Web => "web",
            Self::Benchmark => "benchmark",
            Self::Github => "github",
        }
    }
}

/// Events on a `task-<id>` stream. Stage ids are strings because stages are
/// defined per factory in factory-as-code, not hard-coded. Fields added after
/// the first release carry `#[serde(default)]` so older events still decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TaskEvent {
    TaskCreated {
        title: String,
        body: Option<String>,
        source: TaskSource,
        /// Name of the factory that should drive this task (None = manual only).
        #[serde(default)]
        factory: Option<String>,
        /// Set when the task is a benchmark case (driven by the benchmark runner).
        #[serde(default)]
        benchmark: Option<BenchmarkContext>,
        /// The external item this task came from (a GitHub issue).
        #[serde(default)]
        external_ref: Option<ExternalRef>,
    },
    StageEntered {
        stage: String,
        /// Present when the stage is re-run to address review / gate feedback.
        #[serde(default)]
        feedback: Option<String>,
    },
    StageExited {
        stage: String,
        outcome: StageOutcome,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        session: Option<SessionId>,
    },
    GateRequested {
        gate: String,
        stage: String,
    },
    GateDecided {
        gate: String,
        stage: String,
        approved: bool,
        reviewer: String,
        comment: Option<String>,
    },
    /// Needs a human: stage failed after retries, rework limit hit, bad verdict...
    TaskBlocked {
        stage: Option<String>,
        reason: String,
    },
    TaskResumed {
        stage: Option<String>,
        comment: Option<String>,
    },
    TaskCompleted {
        branch: Option<String>,
    },
    TaskCancelled {
        reason: String,
    },
    /// WorkHub pushed the task branch and opened (or updated) its pull request.
    /// The task now waits for the PR to be reviewed and merged.
    TaskShipped {
        /// `owner/name` of the GitHub repository.
        repo: String,
        pr_number: u64,
        pr_url: String,
        branch: String,
        /// Head commit that was pushed.
        commit: String,
        /// True when this ship opened the PR, false when it pushed to an existing one.
        #[serde(default)]
        created: bool,
    },
    /// A review on the task's PR (recorded once per review by the GitHub poller).
    PrReviewed {
        review_id: String,
        reviewer: String,
        /// `APPROVED` | `CHANGES_REQUESTED` | `COMMENTED` | `DISMISSED`.
        state: String,
        #[serde(default)]
        body: String,
        /// Inline comments the reviewer left with this review.
        #[serde(default)]
        comments: Vec<PrComment>,
        #[serde(default)]
        submitted_at: Option<DateTime<Utc>>,
    },
    /// Every check on the PR head `commit` finished (recorded once per commit).
    PrChecksCompleted {
        commit: String,
        /// `success` or `failure`.
        conclusion: String,
        #[serde(default)]
        failed: Vec<FailedCheck>,
    },
    /// The PR conflicts with its base (recorded once per head commit).
    PrConflicted {
        /// Base branch of the PR.
        base: String,
        /// Head commit of the PR that conflicts.
        commit: String,
    },
    PrMerged {
        #[serde(default)]
        merged_at: Option<DateTime<Utc>>,
        #[serde(default)]
        merged_by: Option<String>,
    },
    /// The PR was closed without being merged.
    PrClosed {
        #[serde(default)]
        closed_at: Option<DateTime<Utc>>,
    },
}

/// Where a task came from outside WorkHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRef {
    /// `owner/name`.
    pub repo: String,
    pub issue: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrComment {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedCheck {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    /// Tail of the failed job's log, when it could be fetched.
    #[serde(default)]
    pub log: Option<String>,
}

pub const CHANGES_REQUESTED: &str = "CHANGES_REQUESTED";

const MAX_LOG: usize = 4_000;

/// Rework instructions for a `CHANGES_REQUESTED` review.
pub fn review_feedback(reviewer: &str, body: &str, comments: &[PrComment]) -> String {
    let mut s = format!("A human reviewer ({reviewer}) requested changes on the pull request.\n");
    if !body.trim().is_empty() {
        s.push_str(&format!("\n{}\n", body.trim()));
    }
    if !comments.is_empty() {
        s.push_str("\nInline comments:\n");
        for c in comments {
            let at = match (&c.path, c.line) {
                (Some(p), Some(l)) => format!("{p}:{l}"),
                (Some(p), None) => p.clone(),
                _ => "general".into(),
            };
            s.push_str(&format!("- {at}: {}\n", c.body.trim()));
        }
    }
    s
}

/// Rework instructions for failed PR checks (names, links, log tails).
pub fn ci_feedback(failed: &[FailedCheck]) -> String {
    let mut s = String::from("CI failed on the pull request. Make these checks pass:\n");
    if failed.is_empty() {
        s.push_str("- (no failed check names reported)\n");
    }
    for c in failed {
        s.push_str(&format!("\n### {}{}\n", c.name, c.url.as_deref().map(|u| format!(" ({u})")).unwrap_or_default()));
        if let Some(log) = c.log.as_deref().filter(|l| !l.trim().is_empty()) {
            let chars: Vec<char> = log.chars().collect();
            let tail: String = chars[chars.len().saturating_sub(MAX_LOG)..].iter().collect();
            s.push_str(&format!("\n```\n{}\n```\n", tail.trim_end()));
        }
    }
    s
}

/// Git branch shared by every stage of a task (and the head of its PR).
pub fn task_branch(task: TaskId) -> String {
    format!("wh/task/{}", &task.0.simple().to_string()[20..])
}

impl TaskId {
    /// Stable task id for an external item such as `github:owner/name#12`, so
    /// polling the same issue twice never creates two tasks.
    pub fn for_external(key: &str) -> Self {
        let ns = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"workhub-task-external");
        Self(uuid::Uuid::new_v5(&ns, key.as_bytes()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageOutcome {
    Succeeded,
    Failed,
    Rejected,
    Skipped,
}

impl StageOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Skipped => "skipped",
        }
    }
}

impl DomainEvent for TaskEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::TaskCreated { .. } => "TaskCreated",
            Self::StageEntered { .. } => "StageEntered",
            Self::StageExited { .. } => "StageExited",
            Self::GateRequested { .. } => "GateRequested",
            Self::GateDecided { .. } => "GateDecided",
            Self::TaskBlocked { .. } => "TaskBlocked",
            Self::TaskResumed { .. } => "TaskResumed",
            Self::TaskCompleted { .. } => "TaskCompleted",
            Self::TaskCancelled { .. } => "TaskCancelled",
            Self::TaskShipped { .. } => "TaskShipped",
            Self::PrReviewed { .. } => "PrReviewed",
            Self::PrChecksCompleted { .. } => "PrChecksCompleted",
            Self::PrConflicted { .. } => "PrConflicted",
            Self::PrMerged { .. } => "PrMerged",
            Self::PrClosed { .. } => "PrClosed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TaskStatus {
    #[default]
    Open,
    Running { stage: String },
    AwaitingGate { gate: String, stage: String },
    Blocked { reason: String },
    /// PR open, waiting for review / CI / merge.
    Shipped,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageResult {
    pub stage: String,
    pub outcome: StageOutcome,
    pub summary: Option<String>,
}

/// The task's pull request, from the latest `TaskShipped`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub branch: String,
    /// Head commit of the latest push.
    pub commit: String,
    /// Merged or closed.
    pub closed: bool,
}

/// A merge conflict with the base that has not been resolved yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub base: String,
    pub commit: String,
    /// Decision on the conflict gate (None = not decided, or no gate).
    pub decision: Option<bool>,
}

/// Task aggregate state, folded from its event stream.
#[derive(Debug, Clone, Default)]
pub struct TaskState {
    /// Stream version of the last applied event (for optimistic concurrency).
    pub version: i64,
    pub title: String,
    pub body: Option<String>,
    pub factory: Option<String>,
    pub benchmark: Option<BenchmarkContext>,
    pub status: TaskStatus,
    pub results: Vec<StageResult>,
    /// Stages re-run because of feedback (review reject, gate reject, resume comment).
    pub rework_loops: u32,
    /// Stage named by the latest `TaskBlocked` (where `resume` restarts by default).
    pub blocked_stage: Option<String>,
    pub external_ref: Option<ExternalRef>,
    pub pr: Option<PullRequest>,
    /// Stages re-run because of PR feedback (changes requested, failed CI).
    pub pr_rework_loops: u32,
    /// PR feedback received while waiting on the PR and not yet handed to a stage
    /// (several reviews / a failed CI run can arrive in the same poll).
    pub pr_feedback: Vec<String>,
    /// Kept until a stage succeeds, so a failed merge stage merges again on resume.
    pub conflict: Option<Conflict>,
    pub last: Option<TaskEvent>,
}

impl TaskState {
    pub fn fold(events: impl IntoIterator<Item = (i64, TaskEvent)>) -> Self {
        let mut s = Self::default();
        for (version, ev) in events {
            s.apply(version, ev);
        }
        s
    }

    pub fn apply(&mut self, version: i64, ev: TaskEvent) {
        self.version = version;
        match &ev {
            TaskEvent::TaskCreated { title, body, factory, benchmark, external_ref, .. } => {
                self.title = title.clone();
                self.body = body.clone();
                self.factory = factory.clone();
                self.benchmark = benchmark.clone();
                self.external_ref = external_ref.clone();
                self.status = TaskStatus::Open;
            }
            TaskEvent::StageEntered { stage, feedback } => {
                self.pr_feedback.clear();
                // After the conflict gate (or a failed merge) the status is no longer `Shipped`.
                if self.status == TaskStatus::Shipped || self.conflict.is_some() {
                    self.pr_rework_loops += 1;
                } else if feedback.is_some() {
                    self.rework_loops += 1;
                }
                self.status = TaskStatus::Running { stage: stage.clone() };
            }
            TaskEvent::StageExited { stage, outcome, summary, .. } => {
                if *outcome == StageOutcome::Succeeded {
                    self.conflict = None;
                }
                self.results.push(StageResult { stage: stage.clone(), outcome: *outcome, summary: summary.clone() });
            }
            TaskEvent::GateRequested { gate, stage } => {
                self.status = TaskStatus::AwaitingGate { gate: gate.clone(), stage: stage.clone() }
            }
            TaskEvent::GateDecided { stage, approved, .. } => {
                if stage == CONFLICT_STAGE
                    && let Some(c) = &mut self.conflict
                {
                    c.decision = Some(*approved);
                }
            }
            TaskEvent::TaskBlocked { stage, reason } => {
                self.blocked_stage = stage.clone();
                self.status = TaskStatus::Blocked { reason: reason.clone() }
            }
            TaskEvent::TaskResumed { .. } => self.status = TaskStatus::Open,
            TaskEvent::TaskCompleted { .. } => self.status = TaskStatus::Done,
            TaskEvent::TaskCancelled { .. } => self.status = TaskStatus::Cancelled,
            TaskEvent::TaskShipped { repo, pr_number, pr_url, branch, commit, .. } => {
                self.pr = Some(PullRequest {
                    repo: repo.clone(),
                    number: *pr_number,
                    url: pr_url.clone(),
                    branch: branch.clone(),
                    commit: commit.clone(),
                    closed: false,
                });
                // Feedback that arrived during a rework was about the previous push.
                self.pr_feedback.clear();
                self.conflict = None;
                self.status = TaskStatus::Shipped;
            }
            // Reviews and checks are facts; the foreman decides whether they cause rework.
            TaskEvent::PrReviewed { reviewer, state, body, comments, .. } => {
                if state == CHANGES_REQUESTED && self.collects_feedback() {
                    self.pr_feedback.push(review_feedback(reviewer, body, comments));
                }
            }
            TaskEvent::PrChecksCompleted { commit, conclusion, failed } => {
                let current = self.pr.as_ref().is_some_and(|pr| &pr.commit == commit);
                if conclusion != "success" && current && self.collects_feedback() {
                    self.pr_feedback.push(ci_feedback(failed));
                }
            }
            TaskEvent::PrConflicted { base, commit } => {
                let current = self.pr.as_ref().is_some_and(|pr| &pr.commit == commit);
                if current && self.status == TaskStatus::Shipped {
                    self.conflict = Some(Conflict { base: base.clone(), commit: commit.clone(), decision: None });
                }
            }
            TaskEvent::PrMerged { .. } => {
                self.conflict = None;
                if let Some(pr) = &mut self.pr {
                    pr.closed = true;
                }
                self.status = TaskStatus::Done;
            }
            TaskEvent::PrClosed { .. } => {
                self.conflict = None;
                if let Some(pr) = &mut self.pr {
                    pr.closed = true;
                }
                if !matches!(self.status, TaskStatus::Done | TaskStatus::Cancelled) {
                    self.blocked_stage = None;
                    self.status = TaskStatus::Blocked { reason: "pull request closed without merging".into() };
                }
            }
        }
        self.last = Some(ev);
    }

    /// Whether PR feedback is still to be handed to a stage: the task waits on
    /// its PR, or on the conflict gate.
    fn collects_feedback(&self) -> bool {
        match &self.status {
            TaskStatus::Shipped => true,
            TaskStatus::AwaitingGate { stage, .. } => stage == CONFLICT_STAGE && self.conflict.is_some(),
            _ => false,
        }
    }

    /// Most recent stage that finished, if any.
    pub fn last_stage(&self) -> Option<&str> {
        self.results.last().map(|r| r.stage.as_str())
    }

    /// Stage currently (or most recently) running.
    pub fn last_stage_entered(&self) -> Option<String> {
        match &self.status {
            TaskStatus::Running { stage } => Some(stage.clone()),
            _ => self.last_stage().map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Reject,
}

/// Find the last `VERDICT: APPROVE|REJECT` line in an agent's final message
/// (tolerates surrounding markdown like `**` or backticks).
pub fn parse_verdict(text: &str) -> Option<Verdict> {
    text.lines().rev().find_map(|line| {
        let l = line.trim().trim_matches(|c| c == '*' || c == '`' || c == '#').trim().to_ascii_uppercase();
        match l.strip_prefix("VERDICT:")?.trim().trim_matches(|c| c == '*' || c == '`').trim() {
            "APPROVE" => Some(Verdict::Approve),
            "REJECT" => Some(Verdict::Reject),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tag_matches_event_type() {
        let e = TaskEvent::StageEntered { stage: "spec".into(), feedback: None };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], e.event_type());
        assert_eq!(serde_json::from_value::<TaskEvent>(v).unwrap(), e);
    }

    #[test]
    fn old_events_without_new_fields_decode() {
        let v = serde_json::json!({"type": "StageExited", "stage": "x", "outcome": "succeeded"});
        assert!(matches!(
            serde_json::from_value::<TaskEvent>(v).unwrap(),
            TaskEvent::StageExited { summary: None, session: None, .. }
        ));
    }

    #[test]
    fn fold_tracks_status_and_rework() {
        let s = TaskState::fold([
            (1, TaskEvent::TaskCreated { title: "t".into(), body: None, source: TaskSource::Cli, factory: Some("f".into()), benchmark: None, external_ref: None }),
            (2, TaskEvent::StageEntered { stage: "impl".into(), feedback: None }),
            (3, TaskEvent::StageExited { stage: "impl".into(), outcome: StageOutcome::Succeeded, summary: None, session: None }),
            (4, TaskEvent::StageEntered { stage: "impl".into(), feedback: Some("fix".into()) }),
        ]);
        assert_eq!((s.version, s.rework_loops), (4, 1));
        assert_eq!(s.status, TaskStatus::Running { stage: "impl".into() });
        assert_eq!(s.last_stage(), Some("impl"));
    }

    #[test]
    fn fold_tracks_pull_request_feedback_and_merge() {
        let shipped = |commit: &str| TaskEvent::TaskShipped {
            repo: "a/b".into(),
            pr_number: 1,
            pr_url: "u".into(),
            branch: "wh/task/x".into(),
            commit: commit.into(),
            created: true,
        };
        let review = |state: &str| TaskEvent::PrReviewed {
            review_id: state.into(),
            reviewer: "bob".into(),
            state: state.into(),
            body: "please fix".into(),
            comments: vec![],
            submitted_at: None,
        };
        let checks = |commit: &str| TaskEvent::PrChecksCompleted { commit: commit.into(), conclusion: "failure".into(), failed: vec![] };
        let mut s = TaskState::fold([(1, shipped("c1")), (2, review("COMMENTED")), (3, review(CHANGES_REQUESTED)), (4, checks("old"))]);
        assert_eq!(s.status, TaskStatus::Shipped);
        assert_eq!(s.pr_feedback.len(), 1, "only changes requested on the current push");
        s.apply(5, checks("c1"));
        assert_eq!(s.pr_feedback.len(), 2);
        s.apply(6, TaskEvent::StageEntered { stage: "impl".into(), feedback: Some("x".into()) });
        assert_eq!((s.pr_rework_loops, s.rework_loops, s.pr_feedback.len()), (1, 0, 0));
        s.apply(7, review(CHANGES_REQUESTED)); // during the rework: ignored
        s.apply(8, shipped("c2"));
        assert!(s.pr_feedback.is_empty());
        s.apply(9, TaskEvent::PrMerged { merged_at: None, merged_by: None });
        assert_eq!(s.status, TaskStatus::Done);
        assert!(s.pr.unwrap().closed);
    }

    #[test]
    fn fold_tracks_merge_conflicts() {
        let shipped = TaskEvent::TaskShipped {
            repo: "a/b".into(),
            pr_number: 1,
            pr_url: "u".into(),
            branch: "wh/task/x".into(),
            commit: "c1".into(),
            created: true,
        };
        let conflicted = |commit: &str| TaskEvent::PrConflicted { base: "main".into(), commit: commit.into() };
        let entered = || TaskEvent::StageEntered { stage: "impl".into(), feedback: Some("merge".into()) };
        let exited = |outcome| TaskEvent::StageExited { stage: "impl".into(), outcome, summary: None, session: None };

        let mut s = TaskState::fold([(1, shipped.clone()), (2, conflicted("old"))]);
        assert_eq!(s.conflict, None, "only the current push");
        s.apply(3, entered());
        s.apply(4, conflicted("c1"));
        assert_eq!(s.conflict, None, "ignored during a rework");

        let mut s = TaskState::fold([(1, shipped), (2, conflicted("c1"))]);
        s.apply(3, TaskEvent::GateRequested { gate: "g".into(), stage: CONFLICT_STAGE.into() });
        s.apply(4, TaskEvent::PrChecksCompleted { commit: "c1".into(), conclusion: "failure".into(), failed: vec![] });
        assert_eq!(s.pr_feedback.len(), 1, "feedback during the gate is kept");
        let decided = TaskEvent::GateDecided {
            gate: "g".into(),
            stage: CONFLICT_STAGE.into(),
            approved: true,
            reviewer: "r".into(),
            comment: None,
        };
        s.apply(5, decided);
        assert_eq!(s.conflict.as_ref().unwrap().decision, Some(true));
        s.apply(6, entered());
        assert_eq!((s.pr_rework_loops, s.rework_loops), (1, 0));
        s.apply(7, exited(StageOutcome::Failed));
        assert!(s.conflict.is_some(), "a failed merge stage keeps the conflict");
        s.apply(8, entered());
        s.apply(9, exited(StageOutcome::Succeeded));
        assert_eq!((s.conflict, s.pr_rework_loops), (None, 2));
    }

    #[test]
    fn verdict_parsing() {
        assert_eq!(parse_verdict("looks good\nVERDICT: APPROVE"), Some(Verdict::Approve));
        assert_eq!(parse_verdict("**Verdict: reject**\n"), Some(Verdict::Reject));
        assert_eq!(parse_verdict("VERDICT: REJECT\nlater\n`VERDICT: APPROVE`"), Some(Verdict::Approve));
        assert_eq!(parse_verdict("no verdict here"), None);
        assert_eq!(parse_verdict("VERDICT: maybe"), None);
    }
}
