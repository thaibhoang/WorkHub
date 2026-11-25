//! Pure part of PR tracking: what has already been recorded for a task (folded
//! from its stream) and what a fresh `gh pr view` adds to it.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use wh_core::task::TaskEvent;

/// Fields requested from `gh pr view --json`.
pub const PR_VIEW_FIELDS: &str = "state,mergedAt,mergedBy,closedAt,headRefOid,reviews,statusCheckRollup";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrView {
    /// `OPEN` | `CLOSED` | `MERGED`.
    pub state: String,
    #[serde(default)]
    pub merged_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub merged_by: Option<Login>,
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub head_ref_oid: String,
    #[serde(default)]
    pub reviews: Vec<Review>,
    #[serde(default)]
    pub status_check_rollup: Vec<Check>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Login {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub id: String,
    #[serde(default)]
    pub author: Option<Login>,
    pub state: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub submitted_at: Option<DateTime<Utc>>,
}

/// One entry of `statusCheckRollup`: a check run (Actions etc.) or a commit status.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    #[serde(default, rename = "__typename")]
    pub typename: String,
    // CheckRun
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub details_url: Option<String>,
    // StatusContext
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub target_url: Option<String>,
}

impl Check {
    fn label(&self) -> String {
        self.name.clone().or_else(|| self.context.clone()).unwrap_or_else(|| "check".into())
    }

    fn url(&self) -> Option<String> {
        self.details_url.clone().or_else(|| self.target_url.clone()).filter(|u| !u.is_empty())
    }

    fn is_status_context(&self) -> bool {
        self.typename == "StatusContext" || (self.context.is_some() && self.name.is_none())
    }

    /// None while still running.
    fn passed(&self) -> Option<bool> {
        if self.is_status_context() {
            return match self.state.as_deref().unwrap_or("").to_ascii_uppercase().as_str() {
                "SUCCESS" => Some(true),
                "FAILURE" | "ERROR" => Some(false),
                _ => None, // PENDING, EXPECTED
            };
        }
        if !self.status.as_deref().unwrap_or("").eq_ignore_ascii_case("COMPLETED") {
            return None;
        }
        Some(!matches!(
            self.conclusion.as_deref().unwrap_or("").to_ascii_uppercase().as_str(),
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        ))
    }
}

/// What the task stream already says about its PR.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub reviews: HashSet<String>,
    pub checked_commits: HashSet<String>,
    pub closed: bool,
}

impl Seen {
    pub fn fold<'a>(events: impl IntoIterator<Item = &'a TaskEvent>) -> Self {
        let mut s = Self::default();
        for e in events {
            match e {
                TaskEvent::PrReviewed { review_id, .. } => {
                    s.reviews.insert(review_id.clone());
                }
                TaskEvent::PrChecksCompleted { commit, .. } => {
                    s.checked_commits.insert(commit.clone());
                }
                TaskEvent::PrMerged { .. } | TaskEvent::PrClosed { .. } => s.closed = true,
                // A new PR (after a closed one) starts tracking again.
                TaskEvent::TaskShipped { created: true, .. } => s.closed = false,
                _ => {}
            }
        }
        s
    }
}

/// A failing check, before its log is fetched.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedCheckRef {
    pub name: String,
    pub url: Option<String>,
}

/// Something new on the PR, in the order it should be recorded.
#[derive(Debug, Clone, PartialEq)]
pub enum Observed {
    Review { id: String, reviewer: String, state: String, body: String, submitted_at: Option<DateTime<Utc>> },
    Checks { commit: String, passed: bool, failed: Vec<FailedCheckRef> },
    Merged { at: Option<DateTime<Utc>>, by: Option<String> },
    Closed { at: Option<DateTime<Utc>> },
}

/// New facts in `view` that `seen` does not have yet: reviews (oldest first),
/// then the head commit's checks once all of them finished, then merge / close.
pub fn observe(seen: &Seen, view: &PrView) -> Vec<Observed> {
    if seen.closed {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut reviews: Vec<&Review> = view
        .reviews
        .iter()
        .filter(|r| !seen.reviews.contains(&r.id) && !r.state.eq_ignore_ascii_case("PENDING"))
        .collect();
    reviews.sort_by_key(|r| r.submitted_at);
    for r in reviews {
        out.push(Observed::Review {
            id: r.id.clone(),
            reviewer: r.author.as_ref().map_or_else(|| "unknown".into(), |a| a.login.clone()),
            state: r.state.to_ascii_uppercase(),
            body: r.body.clone(),
            submitted_at: r.submitted_at,
        });
    }
    let commit = &view.head_ref_oid;
    if !commit.is_empty() && !seen.checked_commits.contains(commit) && !view.status_check_rollup.is_empty() {
        let results: Vec<_> = view.status_check_rollup.iter().map(|c| (c, c.passed())).collect();
        if results.iter().all(|(_, p)| p.is_some()) {
            let failed: Vec<_> = results
                .iter()
                .filter(|(_, p)| *p == Some(false))
                .map(|(c, _)| FailedCheckRef { name: c.label(), url: c.url() })
                .collect();
            out.push(Observed::Checks { commit: commit.clone(), passed: failed.is_empty(), failed });
        }
    }
    match view.state.to_ascii_uppercase().as_str() {
        "MERGED" => out.push(Observed::Merged { at: view.merged_at, by: view.merged_by.as_ref().map(|l| l.login.clone()) }),
        "CLOSED" => out.push(Observed::Closed { at: view.closed_at }),
        _ => {}
    }
    out
}

/// `(run id)` of a GitHub Actions job URL (`…/actions/runs/<run>/job/<job>`).
pub fn actions_run_id(url: &str) -> Option<u64> {
    let rest = url.split("/actions/runs/").nth(1)?;
    rest.split('/').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(json: serde_json::Value) -> PrView {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn records_new_reviews_finished_checks_and_merge_once() {
        let v = view(serde_json::json!({
            "state": "OPEN", "headRefOid": "abc",
            "reviews": [
                {"id": "R2", "author": {"login": "bob"}, "state": "CHANGES_REQUESTED", "body": "fix", "submittedAt": "2026-11-25T10:00:00Z"},
                {"id": "R1", "author": {"login": "amy"}, "state": "COMMENTED", "body": "hm", "submittedAt": "2026-11-25T09:00:00Z"},
                {"id": "R3", "author": {"login": "amy"}, "state": "PENDING", "body": ""}
            ],
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "FAILURE",
                 "detailsUrl": "https://github.com/a/b/actions/runs/123/job/456"},
                {"__typename": "StatusContext", "context": "ci/legacy", "state": "SUCCESS"}
            ]
        }));
        let obs = observe(&Seen::default(), &v);
        let ids: Vec<_> = obs.iter().filter_map(|o| match o { Observed::Review { id, .. } => Some(id.as_str()), _ => None }).collect();
        assert_eq!(ids, ["R1", "R2"]);
        assert_eq!(obs[2], Observed::Checks {
            commit: "abc".into(),
            passed: false,
            failed: vec![FailedCheckRef { name: "test".into(), url: Some("https://github.com/a/b/actions/runs/123/job/456".into()) }],
        });

        let seen = Seen { reviews: ["R1".into(), "R2".into()].into(), checked_commits: ["abc".into()].into(), closed: false };
        assert!(observe(&seen, &v).is_empty());
        let merged = PrView { state: "MERGED".into(), merged_by: Some(Login { login: "bob".into() }), ..v.clone() };
        assert!(matches!(observe(&seen, &merged)[..], [Observed::Merged { by: Some(ref b), .. }] if b == "bob"));
        assert!(observe(&Seen { closed: true, ..seen }, &merged).is_empty());
        assert_eq!(actions_run_id("https://github.com/a/b/actions/runs/123/job/456"), Some(123));
    }

    #[test]
    fn checks_wait_until_all_finished() {
        let v = view(serde_json::json!({
            "state": "OPEN", "headRefOid": "abc",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "a", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "CheckRun", "name": "b", "status": "IN_PROGRESS", "conclusion": ""}
            ]
        }));
        assert!(observe(&Seen::default(), &v).is_empty());
        let mut done = v.clone();
        done.status_check_rollup[1].status = Some("COMPLETED".into());
        done.status_check_rollup[1].conclusion = Some("SKIPPED".into());
        assert_eq!(observe(&Seen::default(), &done), vec![Observed::Checks { commit: "abc".into(), passed: true, failed: vec![] }]);
        // No checks configured: nothing to record.
        assert!(observe(&Seen::default(), &view(serde_json::json!({"state": "OPEN", "headRefOid": "abc"}))).is_empty());
    }
}
