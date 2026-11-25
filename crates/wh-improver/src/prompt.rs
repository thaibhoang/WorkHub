//! Pure parts of the improver: its prompt, parsing its answer, and deciding
//! which triggers fire. No I/O, so it is unit-tested directly.

use serde_json::Value;
use uuid::Uuid;
use wh_core::SessionId;
use wh_factory::{Improvement, Trigger, TriggerMetric};

/// Where the factory lives inside the improver's worktree.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// factory.yaml path relative to the repo root, e.g. `factory/factory.yaml`.
    pub factory_file: String,
    /// Directory the improver may edit, e.g. `factory` (empty = repo root).
    pub factory_dir: String,
}

pub fn build_prompt(im: &Improvement, ws: &Workspace, observation: &str, focus: Option<&str>) -> String {
    let dir = if ws.factory_dir.is_empty() { ".".to_owned() } else { format!("{}/", ws.factory_dir) };
    let mut p = String::new();
    p.push_str(im.prompt.trim());
    p.push_str(&format!(
        "\n\n# Your workspace\n\n\
         The current directory is a git worktree of the repository that holds the factory definition. \
         The factory is `{}`: factory.yaml plus the agent prompts and scorer rubrics it references. \
         You may only edit files inside `{dir}`; a change anywhere else invalidates the proposal. \
         Do not commit: WorkHub validates factory.yaml, commits your change on its own branch and sends it to a human for review.\n",
        ws.factory_file
    ));
    if let Some(f) = focus.map(str::trim).filter(|f| !f.is_empty()) {
        p.push_str(&format!("\n# Focus requested by a human\n\n{f}\n"));
    }
    p.push_str(&format!("\n# Observation\n\n{}\n", observation.trim()));
    p.push_str(
        "\n# Answer format\n\n\
         End your answer with exactly one JSON object on its own line.\n\
         If you edited files:\n\
         {\"title\": \"<imperative summary, <= 80 chars>\", \"rationale\": \"<the pattern, its evidence and why this change addresses it>\", \
         \"expected_impact\": \"<metric, current value, expected value>\", \"evidence\": [\"<session id>\", ...]}\n\
         If no change is warranted (not enough evidence, or the problem is outside the factory):\n\
         {\"no_proposal\": \"<why>\"}\n",
    );
    p
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Proposal { title: String, rationale: String, expected_impact: Option<String>, evidence: Vec<SessionId> },
    NoProposal { reason: String },
}

/// Find the last JSON object with `title` or `no_proposal` in the improver's answer.
pub fn parse_answer(text: &str) -> Result<Answer, String> {
    for (i, _) in text.match_indices('{').collect::<Vec<_>>().into_iter().rev() {
        let mut it = serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>();
        let Some(Ok(v)) = it.next() else { continue };
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned);
        if let Some(reason) = s("no_proposal") {
            return Ok(Answer::NoProposal { reason });
        }
        let Some(title) = s("title") else { continue };
        let evidence = v
            .get("evidence")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).filter_map(parse_session).collect())
            .unwrap_or_default();
        return Ok(Answer::Proposal {
            title: title.chars().take(200).collect(),
            rationale: s("rationale").unwrap_or_default(),
            expected_impact: s("expected_impact"),
            evidence,
        });
    }
    Err("improver answer has no JSON object with \"title\" or \"no_proposal\"".into())
}

/// Accept `<uuid>` or `session-<uuid>`.
fn parse_session(s: &str) -> Option<SessionId> {
    let s = s.trim();
    SessionId::from_stream_id(s).or_else(|| s.parse::<Uuid>().ok().map(SessionId))
}

/// Metrics of one stage for the current factory version within the window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StageMetric {
    pub stage: String,
    pub finished: i64,
    pub failed: i64,
    pub rejected: i64,
    pub avg_cost_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScorerMetric {
    pub scorer: String,
    pub scored: i64,
    pub failed: i64,
}

/// Pull requests of the current factory version within the window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrMetric {
    pub shipped: i64,
    pub changes_requested: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fired {
    pub trigger: Trigger,
    pub value: f64,
    pub samples: i64,
}

impl Fired {
    pub fn describe(&self) -> String {
        let t = &self.trigger;
        format!("{} for {} is {:.3} > {} (n={})", t.metric.as_str(), t.subject, self.value, t.above, self.samples)
    }
}

/// Triggers whose metric exceeds its threshold, over at least `min_samples` data points.
pub fn evaluate_triggers(
    triggers: &[Trigger],
    stages: &[StageMetric],
    scorers: &[ScorerMetric],
    pr: &PrMetric,
    min_samples: i64,
) -> Vec<Fired> {
    let mut out = Vec::new();
    for t in triggers {
        let (value, samples) = match t.metric {
            TriggerMetric::PrChangesRequestedRate if pr.shipped > 0 => {
                (pr.changes_requested as f64 / pr.shipped as f64, pr.shipped)
            }
            TriggerMetric::PrChangesRequestedRate => continue,
            TriggerMetric::ScorerFailRate => match scorers.iter().find(|s| s.scorer == t.subject) {
                Some(s) if s.scored > 0 => (s.failed as f64 / s.scored as f64, s.scored),
                _ => continue,
            },
            m => match stages.iter().find(|s| s.stage == t.subject) {
                Some(s) if s.finished > 0 => {
                    let v = match m {
                        TriggerMetric::StageFailureRate => s.failed as f64 / s.finished as f64,
                        TriggerMetric::StageRejectRate => s.rejected as f64 / s.finished as f64,
                        _ => s.avg_cost_usd,
                    };
                    (v, s.finished)
                }
                _ => continue,
            },
        };
        if samples >= min_samples && value > t.above {
            out.push(Fired { trigger: t.clone(), value, samples });
        }
    }
    out
}

/// Schedule slot for a periodic run: the same within each `interval_hours` window.
pub fn schedule_slot(unix_secs: i64, interval_hours: u64) -> i64 {
    unix_secs.div_euclid((interval_hours.max(1) * 3600) as i64)
}

/// Paths outside `dir` (relative to the repo root; empty dir = anything goes).
pub fn outside_dir<'a>(files: &'a [String], dir: &str) -> Vec<&'a str> {
    if dir.is_empty() {
        return Vec::new();
    }
    let prefix = format!("{}/", dir.trim_end_matches('/'));
    files.iter().map(String::as_str).filter(|f| !f.starts_with(&prefix)).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn trig(metric: TriggerMetric, subject: &str, above: f64) -> Trigger {
        Trigger { metric, subject: subject.into(), above }
    }

    #[test]
    fn parses_proposal_and_no_proposal() {
        let sid = SessionId::new();
        let text = format!(
            "I looked.\n```json\n{{\"title\": \"Allow chmod\", \"rationale\": \"denied 4x\", \"expected_impact\": \"fail 40% → 5%\", \
             \"evidence\": [\"{}\", \"session-{}\", \"nope\"]}}\n```",
            sid.0, sid.0
        );
        match parse_answer(&text).unwrap() {
            Answer::Proposal { title, rationale, expected_impact, evidence } => {
                assert_eq!((title.as_str(), rationale.as_str()), ("Allow chmod", "denied 4x"));
                assert_eq!(expected_impact.as_deref(), Some("fail 40% → 5%"));
                assert_eq!(evidence, vec![sid, sid]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            parse_answer("{\"title\": \"x\"} then changed mind {\"no_proposal\": \"too little data\"}").unwrap(),
            Answer::NoProposal { reason: "too little data".into() }
        );
        assert!(parse_answer("{\"score\": 1}").is_err());
        assert!(parse_answer("{\"title\": \"  \"}").is_err());
    }

    #[test]
    fn triggers_need_samples_and_threshold() {
        let triggers = [
            trig(TriggerMetric::StageFailureRate, "impl", 0.25),
            trig(TriggerMetric::StageRejectRate, "impl", 0.5),
            trig(TriggerMetric::StageAvgCostUsd, "review", 1.0),
            trig(TriggerMetric::ScorerFailRate, "s1", 0.3),
            trig(TriggerMetric::StageFailureRate, "missing", 0.0),
        ];
        let stages = [
            StageMetric { stage: "impl".into(), finished: 10, failed: 3, rejected: 2, avg_cost_usd: 0.5 },
            StageMetric { stage: "review".into(), finished: 2, failed: 0, rejected: 0, avg_cost_usd: 3.0 },
        ];
        let scorers = [ScorerMetric { scorer: "s1".into(), scored: 5, failed: 2 }];
        let fired = evaluate_triggers(&triggers, &stages, &scorers, &PrMetric::default(), 5);
        let keys: Vec<_> = fired.iter().map(|f| f.trigger.key()).collect();
        // review cost is high but has only 2 samples; reject rate 0.2 < 0.5.
        assert_eq!(keys, ["stage_failure_rate:impl>0.25", "scorer_fail_rate:s1>0.3"]);
        assert_eq!(fired[0].describe(), "stage_failure_rate for impl is 0.300 > 0.25 (n=10)");
        assert_eq!(evaluate_triggers(&triggers, &stages, &scorers, &PrMetric::default(), 1).len(), 3);
        let pr_trigger = [trig(TriggerMetric::PrChangesRequestedRate, "pr", 0.5)];
        let pr = PrMetric { shipped: 4, changes_requested: 3 };
        assert_eq!(evaluate_triggers(&pr_trigger, &[], &[], &pr, 4)[0].value, 0.75);
        assert!(evaluate_triggers(&pr_trigger, &[], &[], &pr, 5).is_empty());
    }

    #[test]
    fn schedule_slots_and_dir_checks() {
        assert_eq!(schedule_slot(0, 24), schedule_slot(86_399, 24));
        assert_ne!(schedule_slot(86_399, 24), schedule_slot(86_400, 24));
        let files = vec!["factory/agents/a.md".to_owned(), "src/main.rs".to_owned(), "factoryx/y".to_owned()];
        assert_eq!(outside_dir(&files, "factory"), ["src/main.rs", "factoryx/y"]);
        assert!(outside_dir(&files, "").is_empty());
    }

    #[test]
    fn prompt_has_workspace_focus_and_format() {
        let im = Improvement {
            prompt: "You improve factories.".into(),
            model: "m".into(),
            budget_usd: 1.0,
            timeout: Duration::from_secs(60),
            window_days: 14,
            min_sessions: 5,
            interval_hours: None,
            max_open_proposals: 3,
            allowed_tools: vec![],
            triggers: vec![],
        };
        let ws = Workspace { factory_file: "factory/factory.yaml".into(), factory_dir: "factory".into() };
        let p = build_prompt(&im, &ws, "## Stages\n...", Some("why so many rejects?"));
        assert!(p.starts_with("You improve factories."));
        assert!(p.contains("only edit files inside `factory/`"));
        assert!(p.contains("why so many rejects?"));
        assert!(p.contains("{\"no_proposal\""));
        assert!(!build_prompt(&im, &ws, "x", Some("  ")).contains("Focus requested"));
    }
}
