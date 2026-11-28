use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    benchmark::BenchmarkContext,
    event::DomainEvent,
    ids::{SessionId, TaskId},
    question::{Answer, Question},
};

/// Pseudo stage id used for shipping (push + pull request) after the last
/// pipeline stage. Gates and blocks at shipping time name this stage.
pub const SHIP_STAGE: &str = "ship";

/// Pseudo stage id of the human gate (and block) for a pull request that
/// conflicts with its base branch.
pub const CONFLICT_STAGE: &str = "conflict";

/// Reviewer / user recorded on the gate decisions and answers the autopilot makes.
pub const AUTOPILOT_REVIEWER: &str = "autopilot";

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
        /// Parent task (tasks form a tree; see `TaskMoved`).
        #[serde(default)]
        parent: Option<TaskId>,
        /// Created as a draft: the pipeline waits for `TaskActivated`. Older events
        /// (false) start right away.
        #[serde(default)]
        draft: bool,
        /// Started in autopilot mode (only with `draft: false`): gates and agent
        /// questions are decided by the autopilot agent instead of a human.
        #[serde(default)]
        autopilot: bool,
    },
    /// A human started a draft task; the foreman runs its first stage.
    TaskActivated {
        /// Run in autopilot mode (see `TaskCreated.autopilot`).
        #[serde(default)]
        autopilot: bool,
    },
    /// The task was moved under another parent (None = top level). The command
    /// refuses a move that would make a task its own ancestor.
    TaskMoved {
        parent: Option<TaskId>,
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
    /// The agent stopped in `stage` to ask the user; the task waits for answers.
    AgentAsked {
        stage: String,
        session: SessionId,
        questions: Vec<Question>,
        /// What it takes to resume the paused session (None on old events).
        #[serde(default)]
        resume: Option<ResumeHandle>,
    },
    /// `answers[i]` answers `questions[i]` of the pending `AgentAsked`.
    UserAnswered {
        stage: String,
        answers: Vec<Answer>,
        user: String,
    },
    /// The answers are being handed to the paused `stage` (claims the resume,
    /// which records no `StageEntered`).
    StageResumed {
        stage: String,
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
            Self::TaskActivated { .. } => "TaskActivated",
            Self::TaskMoved { .. } => "TaskMoved",
            Self::StageEntered { .. } => "StageEntered",
            Self::StageExited { .. } => "StageExited",
            Self::GateRequested { .. } => "GateRequested",
            Self::GateDecided { .. } => "GateDecided",
            Self::AgentAsked { .. } => "AgentAsked",
            Self::UserAnswered { .. } => "UserAnswered",
            Self::StageResumed { .. } => "StageResumed",
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
    /// Created but not started: waits for a human to activate it.
    Draft,
    #[default]
    Open,
    Running { stage: String },
    AwaitingGate { gate: String, stage: String },
    /// The agent asked the user something; the stage has not exited.
    AwaitingInput { stage: String },
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

/// Questions the user has not answered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestions {
    pub stage: String,
    pub session: SessionId,
    pub questions: Vec<Question>,
}

/// What resuming a paused session needs besides the answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeHandle {
    /// First session of the conversation: owns the worktree and sandbox home.
    pub home_session: SessionId,
    /// The runtime's own conversation id.
    pub runtime_session_id: String,
    pub branch: String,
    /// Paths of the merge the stage is resolving (empty = no merge).
    pub conflicts: Vec<String>,
}

/// A stage that stopped to ask and has not exited yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pause {
    pub stage: String,
    /// Session that asked last.
    pub session: SessionId,
    pub resume: Option<ResumeHandle>,
}

/// One answered question set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    pub questions: Vec<Question>,
    /// `answers[i]` answers `questions[i]`.
    pub answers: Vec<Answer>,
    pub user: String,
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
    pub parent: Option<TaskId>,
    /// Gates and questions are decided by the autopilot agent (set when started).
    pub autopilot: bool,
    pub status: TaskStatus,
    /// The current gate already has a decision (only meaningful while `AwaitingGate`):
    /// the status stays until the foreman acts on it.
    pub gate_decided: bool,
    /// Set only while `AwaitingInput`.
    pub pending_questions: Option<PendingQuestions>,
    /// `AgentAsked` events since the latest `StageEntered` (limited by `max_questions`).
    pub question_rounds: u32,
    /// Set by `AgentAsked` until the stage exits or is entered anew (kept while blocked).
    pub paused: Option<Pause>,
    /// Questions answered since the latest `StageEntered` (kept while blocked).
    pub dialogue: Vec<Exchange>,
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
    /// Last pipeline event (`TaskMoved` is not one: moving a task never hides the
    /// event the foreman still has to act on).
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
            TaskEvent::TaskCreated { title, body, factory, benchmark, external_ref, parent, draft, autopilot, .. } => {
                self.autopilot = *autopilot;
                self.title = title.clone();
                self.body = body.clone();
                self.factory = factory.clone();
                self.benchmark = benchmark.clone();
                self.external_ref = external_ref.clone();
                self.parent = *parent;
                self.status = if *draft { TaskStatus::Draft } else { TaskStatus::Open };
            }
            TaskEvent::TaskActivated { autopilot } => {
                self.autopilot = *autopilot;
                self.status = TaskStatus::Open;
            }
            TaskEvent::TaskMoved { parent } => {
                self.parent = *parent;
                return;
            }
            TaskEvent::StageEntered { stage, feedback } => {
                self.pr_feedback.clear();
                self.question_rounds = 0;
                self.paused = None;
                self.dialogue.clear();
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
                self.paused = None;
                self.results.push(StageResult { stage: stage.clone(), outcome: *outcome, summary: summary.clone() });
            }
            TaskEvent::GateRequested { gate, stage } => {
                self.gate_decided = false;
                self.status = TaskStatus::AwaitingGate { gate: gate.clone(), stage: stage.clone() }
            }
            TaskEvent::GateDecided { stage, approved, .. } => {
                self.gate_decided = true;
                if stage == CONFLICT_STAGE
                    && let Some(c) = &mut self.conflict
                {
                    c.decision = Some(*approved);
                }
            }
            TaskEvent::AgentAsked { stage, session, questions, resume } => {
                self.paused = Some(Pause { stage: stage.clone(), session: *session, resume: resume.clone() });
                self.pending_questions =
                    Some(PendingQuestions { stage: stage.clone(), session: *session, questions: questions.clone() });
                self.question_rounds += 1;
                self.status = TaskStatus::AwaitingInput { stage: stage.clone() };
            }
            TaskEvent::UserAnswered { answers, user, .. } => {
                if let TaskStatus::AwaitingInput { stage } = &self.status {
                    if let Some(pending) = &self.pending_questions {
                        self.dialogue.push(Exchange {
                            questions: pending.questions.clone(),
                            answers: answers.clone(),
                            user: user.clone(),
                        });
                    }
                    self.status = TaskStatus::Running { stage: stage.clone() };
                }
            }
            TaskEvent::StageResumed { stage } => self.status = TaskStatus::Running { stage: stage.clone() },
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
        // Whatever ended the wait (answer, block, cancel, merge...) drops the questions.
        if !matches!(self.status, TaskStatus::AwaitingInput { .. }) {
            self.pending_questions = None;
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
            TaskStatus::Running { stage } | TaskStatus::AwaitingInput { stage } => Some(stage.clone()),
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
            (1, TaskEvent::TaskCreated { title: "t".into(), body: None, source: TaskSource::Cli, factory: Some("f".into()), benchmark: None, external_ref: None, parent: None, draft: false, autopilot: false }),
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
    fn drafts_wait_and_moves_keep_the_last_pipeline_event() {
        let created = |draft| TaskEvent::TaskCreated {
            title: "t".into(),
            body: None,
            source: TaskSource::Cli,
            factory: None,
            benchmark: None,
            external_ref: None,
            parent: None,
            draft,
            autopilot: false,
        };
        let parent = TaskId::new();
        let mut s = TaskState::fold([(1, created(true))]);
        assert_eq!(s.status, TaskStatus::Draft);
        s.apply(2, TaskEvent::TaskMoved { parent: Some(parent) });
        assert_eq!((s.version, s.parent, s.status.clone()), (2, Some(parent), TaskStatus::Draft));
        assert!(matches!(s.last, Some(TaskEvent::TaskCreated { draft: true, .. })));
        s.apply(3, TaskEvent::TaskActivated { autopilot: false });
        assert_eq!(s.status, TaskStatus::Open);
        assert_eq!(TaskState::fold([(1, created(false))]).status, TaskStatus::Open);

        // Events written before drafts existed start right away.
        let v = serde_json::json!({"type": "TaskCreated", "title": "t", "body": null, "source": "cli"});
        assert!(matches!(serde_json::from_value(v).unwrap(), TaskEvent::TaskCreated { draft: false, parent: None, .. }));
    }

    #[test]
    fn autopilot_is_chosen_at_start() {
        let created = |draft, autopilot| TaskEvent::TaskCreated {
            title: "t".into(),
            body: None,
            source: TaskSource::Cli,
            factory: None,
            benchmark: None,
            external_ref: None,
            parent: None,
            draft,
            autopilot,
        };
        assert!(TaskState::fold([(1, created(false, true))]).autopilot);
        let mut s = TaskState::fold([(1, created(true, false))]);
        assert!(!s.autopilot);
        s.apply(2, TaskEvent::TaskActivated { autopilot: true });
        assert!(s.autopilot);
        // Events written before autopilot existed decode as manual.
        let v = serde_json::json!({"type": "TaskActivated"});
        assert_eq!(serde_json::from_value::<TaskEvent>(v).unwrap(), TaskEvent::TaskActivated { autopilot: false });
    }

    #[test]
    fn gate_decided_is_set_by_a_decision_and_reset_by_a_request() {
        let requested = || TaskEvent::GateRequested { gate: "g".into(), stage: "spec".into() };
        let mut s = TaskState::fold([(1, requested())]);
        assert!(!s.gate_decided);
        s.apply(2, TaskEvent::GateDecided { gate: "g".into(), stage: "spec".into(), approved: true, reviewer: "r".into(), comment: None });
        assert!(s.gate_decided);
        s.apply(3, requested());
        assert!(!s.gate_decided);
    }

    fn asked() -> TaskEvent {
        TaskEvent::AgentAsked {
            stage: "impl".into(),
            session: SessionId::new(),
            questions: vec![Question { text: "Which?".into(), options: vec!["A".into()] }],
            resume: Some(ResumeHandle {
                home_session: SessionId::new(),
                runtime_session_id: "r1".into(),
                branch: "wh/task/x".into(),
                conflicts: vec!["a.rs".into()],
            }),
        }
    }

    fn answered() -> TaskEvent {
        TaskEvent::UserAnswered {
            stage: "impl".into(),
            answers: vec![Answer { option: Some("A".into()), text: None }],
            user: "u".into(),
        }
    }

    #[test]
    fn question_events_round_trip() {
        for e in [asked(), answered()] {
            let v = serde_json::to_value(&e).unwrap();
            assert_eq!(v["type"], e.event_type());
            assert_eq!(serde_json::from_value::<TaskEvent>(v).unwrap(), e);
        }
    }

    #[test]
    fn question_fold_waits_then_runs_again() {
        let entered = || TaskEvent::StageEntered { stage: "impl".into(), feedback: None };
        let mut s = TaskState::fold([(1, entered())]);
        s.apply(2, asked());
        assert_eq!(s.status, TaskStatus::AwaitingInput { stage: "impl".into() });
        assert_eq!(s.pending_questions.as_ref().map(|p| p.questions.len()), Some(1));
        assert_eq!((s.question_rounds, s.last_stage_entered()), (1, Some("impl".into())));
        s.apply(3, answered());
        assert_eq!(s.status, TaskStatus::Running { stage: "impl".into() });
        assert_eq!(s.pending_questions, None);
        s.apply(4, asked());
        assert_eq!(s.question_rounds, 2);
        s.apply(5, TaskEvent::TaskCancelled { reason: "no".into() });
        assert_eq!((&s.status, &s.pending_questions), (&TaskStatus::Cancelled, &None));
        s.apply(6, answered());
        assert_eq!(s.status, TaskStatus::Cancelled, "only acts while awaiting");
        s.apply(7, entered());
        assert_eq!(s.question_rounds, 0);
    }

    #[test]
    fn question_event_without_resume_data_decodes() {
        let v = serde_json::json!({"type": "AgentAsked", "stage": "impl", "session": SessionId::new(), "questions": []});
        assert!(matches!(serde_json::from_value(v).unwrap(), TaskEvent::AgentAsked { resume: None, .. }));
    }

    #[test]
    fn question_pause_and_dialogue_follow_the_stage() {
        let entered = || TaskEvent::StageEntered { stage: "impl".into(), feedback: None };
        let resumed = || TaskEvent::StageResumed { stage: "impl".into() };
        let mut s = TaskState::fold([(1, entered()), (2, asked()), (3, answered()), (4, resumed())]);
        assert_eq!(s.status, TaskStatus::Running { stage: "impl".into() });
        let pause = s.paused.as_ref().unwrap();
        assert_eq!((pause.stage.as_str(), pause.resume.is_some()), ("impl", true));
        assert_eq!(s.dialogue.len(), 1);
        assert_eq!((s.dialogue[0].questions[0].text.as_str(), s.dialogue[0].user.as_str()), ("Which?", "u"));
        assert_eq!(s.dialogue[0].answers[0].option.as_deref(), Some("A"));
        s.apply(5, asked());
        s.apply(6, answered());
        assert_eq!(s.dialogue.len(), 2);
        s.apply(7, TaskEvent::TaskBlocked { stage: Some("impl".into()), reason: "x".into() });
        s.apply(8, TaskEvent::TaskResumed { stage: None, comment: None });
        assert_eq!((s.paused.is_some(), s.dialogue.len()), (true, 2));
        s.apply(9, entered());
        assert_eq!((&s.paused, s.dialogue.len()), (&None, 0));

        s.apply(10, asked());
        s.apply(11, answered());
        let outcome = StageOutcome::Succeeded;
        s.apply(12, TaskEvent::StageExited { stage: "impl".into(), outcome, summary: None, session: None });
        assert_eq!((&s.paused, s.dialogue.len()), (&None, 1));
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
