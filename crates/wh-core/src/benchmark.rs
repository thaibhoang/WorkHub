use serde::{Deserialize, Serialize};

use crate::{
    event::DomainEvent,
    ids::{BenchmarkRunId, ProposalId, TaskId},
};

/// Marks a task as a benchmark case. Benchmark tasks are driven by the
/// benchmark runner (not the live foreman) and are left out of production
/// metrics, so the improver never learns from them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkContext {
    pub run: BenchmarkRunId,
    pub case: String,
    /// Repo the agents work in (overrides the factory's `repo`).
    #[serde(default)]
    pub repo: Option<String>,
    /// Ref the task branch starts from (overrides the factory's `base_ref`).
    #[serde(default)]
    pub base_ref: Option<String>,
}

/// Result of a case's deterministic check, run on the finished task branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub passed: bool,
    /// None when the check timed out or could not start.
    pub exit_code: Option<i32>,
    /// Tail of stdout + stderr.
    pub output: String,
    pub duration_ms: u64,
}

/// Events on a `bench-<id>` stream: one run of a benchmark suite against one
/// factory variant (a factory file at a git ref). Requested by a human or the
/// API, claimed by the runner with `BenchmarkStarted` at `Exact(1)` so it runs
/// at most once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BenchmarkEvent {
    BenchmarkRequested {
        /// Suite name and the absolute path of its file.
        suite: String,
        suite_file: String,
        /// Factory variant: the suite's factory file as of this git ref
        /// (None = the working tree as it is now).
        #[serde(default)]
        git_ref: Option<String>,
        /// Proposal this run evaluates, if any.
        #[serde(default)]
        proposal: Option<ProposalId>,
        /// Free-form label, e.g. "base" / "proposal".
        #[serde(default)]
        label: Option<String>,
        /// Each case runs this many times (agents are not deterministic).
        repeat: u32,
        /// Only these case ids (empty = all).
        #[serde(default)]
        cases: Vec<String>,
    },
    BenchmarkStarted {
        factory: String,
        factory_version: String,
        /// Commit the factory was loaded from (None = working tree).
        factory_commit: Option<String>,
        /// Content hash of the suite file + fixtures: only runs with the same
        /// suite version are comparable.
        suite_version: String,
        /// Case runs planned (cases × repeat).
        planned: u32,
        /// Process running the benchmark: startup recovery leaves runs of a live
        /// process alone (a CLI `wh bench run` next to a daemon).
        #[serde(default)]
        pid: Option<u32>,
    },
    BenchmarkCaseStarted {
        case: String,
        repeat: u32,
        task_id: TaskId,
    },
    BenchmarkCaseFinished {
        case: String,
        repeat: u32,
        task_id: TaskId,
        /// done | blocked | cancelled | error
        task_status: String,
        /// Task finished and the check (if any) passed.
        passed: bool,
        check: Option<CheckResult>,
        cost_usd: f64,
        duration_ms: u64,
        sessions: u32,
        rework_loops: u32,
        /// Why the case did not pass (blocked reason, check failure, error).
        message: Option<String>,
    },
    BenchmarkCompleted {
        cases: u32,
        passed: u32,
        cost_usd: f64,
        duration_ms: u64,
        /// Set when the run stopped early (e.g. the suite's cost limit).
        #[serde(default)]
        aborted: Option<String>,
    },
    BenchmarkFailed {
        message: String,
    },
}

impl DomainEvent for BenchmarkEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::BenchmarkRequested { .. } => "BenchmarkRequested",
            Self::BenchmarkStarted { .. } => "BenchmarkStarted",
            Self::BenchmarkCaseStarted { .. } => "BenchmarkCaseStarted",
            Self::BenchmarkCaseFinished { .. } => "BenchmarkCaseFinished",
            Self::BenchmarkCompleted { .. } => "BenchmarkCompleted",
            Self::BenchmarkFailed { .. } => "BenchmarkFailed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tag_matches_event_type() {
        let e = BenchmarkEvent::BenchmarkCaseStarted { case: "c".into(), repeat: 1, task_id: TaskId::new() };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], e.event_type());
        assert_eq!(serde_json::from_value::<BenchmarkEvent>(v).unwrap(), e);
    }
}
