//! Pure parts of the autopilot: the prompt it gets for a gate or a question set,
//! and parsing its answer. No I/O, so it is unit-tested directly.

use serde_json::Value;
use wh_core::{Answer, Question, check_answers};

const MAX_BODY: usize = 8_000;
const MAX_SUMMARY: usize = 6_000;
const MAX_DIFF: usize = 60_000;

/// Everything the autopilot sees about the task.
#[derive(Debug, Clone, Default)]
pub struct TaskContext {
    pub title: String,
    pub body: Option<String>,
    /// Pipeline stages of the factory, in order.
    pub stages: Vec<String>,
    /// Outputs of the stages that ran, oldest first: (stage, outcome, summary).
    pub results: Vec<(String, String, Option<String>)>,
    pub rework_loops: u32,
    pub max_rework_loops: u32,
    /// `git diff` of the task branch against its base (stat + patch).
    pub diff: Option<String>,
    /// Earlier questions and answers in the current stage.
    pub dialogue: Vec<(Vec<Question>, Vec<Answer>, String)>,
    /// Factory-specific policy (`autopilot.prompt`).
    pub guidelines: Option<String>,
}

/// What the autopilot decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The gate `gate` after `stage` (`ship` / `conflict` are pseudo stages).
    Gate { gate: String, stage: String, conflict_base: Option<String> },
    /// Questions the agent of `stage` asked.
    Questions { stage: String, questions: Vec<Question> },
}

pub fn build_prompt(ctx: &TaskContext, req: &Request) -> String {
    let mut p = String::from(
        "You are the autopilot of a software factory: AI agents take a task through a pipeline of stages, \
         and this task runs without a human. Wherever the pipeline would stop for a person (approving a gate, \
         answering an agent's question), you decide in their place. You have no tools: decide only from the \
         material below. Decide as a careful senior engineer and owner of the codebase would: keep the task \
         moving when the work is good enough, and push back when it is wrong, incomplete, risky or off-scope.\n",
    );
    if let Some(g) = ctx.guidelines.as_deref().filter(|g| !g.trim().is_empty()) {
        p.push_str(&format!("\n# Policy of this factory\n\n{}\n", g.trim()));
    }
    p.push_str(&format!("\n# Task\n\nTitle: {}\n", ctx.title));
    if let Some(body) = ctx.body.as_deref().filter(|b| !b.trim().is_empty()) {
        p.push_str(&format!("\n{}\n", head(body.trim(), MAX_BODY)));
    }
    if !ctx.stages.is_empty() {
        p.push_str(&format!("\nPipeline: {}\n", ctx.stages.join(" → ")));
    }
    p.push_str(&format!("Rework loops so far: {} of at most {}\n", ctx.rework_loops, ctx.max_rework_loops));
    if !ctx.results.is_empty() {
        p.push_str("\n# Stage outputs so far (oldest first)\n");
        // Older outputs are context; the latest one is what is being decided on.
        let n = ctx.results.len();
        for (i, (stage, outcome, summary)) in ctx.results.iter().enumerate() {
            let max = if i + 1 == n { MAX_SUMMARY } else { 1_500 };
            let text = summary.as_deref().map(|s| head(s.trim(), max)).unwrap_or_else(|| "(no output)".into());
            p.push_str(&format!("\n## `{stage}` ({outcome})\n\n{text}\n"));
        }
    }
    match ctx.diff.as_deref().filter(|d| !d.trim().is_empty()) {
        Some(d) => p.push_str(&format!("\n# Changes on the task branch so far\n\n```diff\n{}\n```\n", head(d, MAX_DIFF))),
        None => p.push_str("\n# Changes on the task branch so far\n\n(none)\n"),
    }
    if !ctx.dialogue.is_empty() {
        p.push_str("\n# Questions already answered in this stage\n");
        for (questions, answers, by) in &ctx.dialogue {
            p.push_str(&format!("\n(answered by {by})\n{}\n", wh_core::format_answers(questions, answers)));
        }
    }
    match req {
        Request::Gate { gate, stage, conflict_base } => {
            let what = match (stage.as_str(), conflict_base) {
                (_, Some(base)) => format!(
                    "The task's pull request conflicts with its base branch `{base}`. Approving lets an agent merge \
                     `{base}` into the task branch and resolve the conflicts; rejecting blocks the task for a human."
                ),
                (wh_core::task::SHIP_STAGE, _) => "All stages are done. Approving pushes the branch and opens the pull \
                     request; rejecting sends the work back for rework with your comment."
                    .to_owned(),
                _ => format!(
                    "Stage `{stage}` finished. Approving moves the task to the next stage; rejecting re-runs \
                     `{stage}` with your comment as feedback."
                ),
            };
            p.push_str(&format!(
                "\n# Your decision: gate `{gate}`\n\n{what}\n\nReject only for concrete problems the agent can fix, \
                 and say exactly what to change. Mind the rework limit: if it is nearly used up, approve work that is \
                 acceptable even if not perfect.\n\n# Answer format\n\nThink briefly, then end your answer with exactly one \
                 JSON object on its own line:\n{{\"decision\": \"approve\" | \"reject\", \"comment\": \"<for the agent: what to \
                 change (required to reject), or a short note>\", \"rationale\": \"<1-3 sentences: why>\"}}\n"
            ));
        }
        Request::Questions { stage, questions } => {
            p.push_str(&format!(
                "\n# Your decision: answer the agent of stage `{stage}`\n\nThe agent stopped to ask the user the questions \
                 below. Answer each one as the task owner would, so it can continue. Prefer one of the offered options when \
                 one fits; otherwise answer in free text. When the material does not settle a question, pick the simplest \
                 reasonable choice that fits the task and say it is an assumption.\n\n"
            ));
            for (i, q) in questions.iter().enumerate() {
                p.push_str(&format!("{}. {}\n", i + 1, q.text));
                for o in &q.options {
                    p.push_str(&format!("   - option: {o}\n"));
                }
            }
            p.push_str(
                "\n# Answer format\n\nThink briefly, then end your answer with exactly one JSON object on its own line, \
                 with one entry in `answers` per question, in order. Each entry is either {\"option\": \"<the exact text \
                 of an offered option>\"} or {\"text\": \"<free-text answer>\"}:\n\
                 {\"answers\": [...], \"rationale\": \"<1-3 sentences: why>\"}\n",
            );
        }
    }
    p
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDecision {
    pub approved: bool,
    /// For the agent; always set when rejecting.
    pub comment: Option<String>,
    pub rationale: String,
}

/// The last JSON object with a `decision` in the autopilot's answer.
pub fn parse_gate_decision(text: &str) -> Result<GateDecision, String> {
    let v = last_object_with(text, "decision").ok_or("answer has no JSON object with a \"decision\"")?;
    let approved = match v["decision"].as_str().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("approve" | "approved") => true,
        Some("reject" | "rejected") => false,
        other => return Err(format!("decision must be \"approve\" or \"reject\", got {other:?}")),
    };
    let comment = str_field(&v, "comment");
    let rationale = str_field(&v, "rationale").unwrap_or_default();
    if !approved && comment.is_none() {
        return Err("a rejection needs a comment for the agent".into());
    }
    Ok(GateDecision { approved, comment, rationale })
}

/// The last JSON object with `answers`, checked against `questions` (one valid
/// answer per question) and the rationale.
pub fn parse_answers(text: &str, questions: &[Question]) -> Result<(Vec<Answer>, String), String> {
    let v = last_object_with(text, "answers").ok_or("answer has no JSON object with \"answers\"")?;
    let items = v["answers"].as_array().ok_or("\"answers\" must be a list")?;
    let answers = items
        .iter()
        .map(|a| match a {
            Value::String(s) => Answer { option: None, text: Some(s.clone()) },
            _ => Answer { option: str_field(a, "option"), text: str_field(a, "text") },
        })
        .zip(questions)
        .map(|(mut a, q)| {
            // An option the model reworded is kept as free text rather than refused.
            if let Some(o) = a.option.take() {
                match q.options.iter().find(|x| x.trim().eq_ignore_ascii_case(o.trim())) {
                    Some(x) => a.option = Some(x.clone()),
                    None => a.text = Some(a.text.map_or(o.clone(), |t| format!("{o}: {t}"))),
                }
            }
            // Several questions: an option and text together are refused, keep the option.
            if questions.len() > 1 && a.option.is_some() {
                a.text = None;
            }
            a
        })
        .collect::<Vec<_>>();
    if items.len() != questions.len() {
        return Err(format!("expected {} answer(s), got {}", questions.len(), items.len()));
    }
    let answers = check_answers(questions, answers)?;
    Ok((answers, str_field(&v, "rationale").unwrap_or_default()))
}

fn last_object_with(text: &str, key: &str) -> Option<Value> {
    text.match_indices('{').collect::<Vec<_>>().into_iter().rev().find_map(|(i, _)| {
        let mut it = serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>();
        match it.next() {
            Some(Ok(v)) if v.get(key).is_some() => Some(v),
            _ => None,
        }
    })
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn head(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}\n… [truncated]", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, options: &[&str]) -> Question {
        Question { text: text.into(), options: options.iter().map(|o| o.to_string()).collect() }
    }

    #[test]
    fn gate_decisions() {
        let d = parse_gate_decision("Looks fine.\n{\"decision\": \"approve\", \"rationale\": \"tests pass\"}").unwrap();
        assert_eq!((d.approved, d.comment, d.rationale.as_str()), (true, None, "tests pass"));

        let text = "{\"decision\": \"approve\"}\nOn second thought:\n```json\n{\"decision\": \"REJECT\", \"comment\": \"add a test\"}\n```";
        let d = parse_gate_decision(text).unwrap();
        assert_eq!((d.approved, d.comment.as_deref()), (false, Some("add a test")), "the last object wins");

        assert!(parse_gate_decision("no json").is_err());
        assert!(parse_gate_decision("{\"decision\": \"maybe\"}").unwrap_err().contains("approve"));
        assert!(parse_gate_decision("{\"decision\": \"reject\", \"comment\": \"  \"}").unwrap_err().contains("comment"));
    }

    #[test]
    fn answers_are_matched_to_questions() {
        let qs = [q("Which db?", &["Postgres", "SQLite"]), q("Name?", &[])];
        let (a, why) =
            parse_answers("{\"answers\": [{\"option\": \"postgres\"}, {\"text\": \"wh\"}], \"rationale\": \"r\"}", &qs).unwrap();
        assert_eq!(a[0], Answer { option: Some("Postgres".into()), text: None }, "case-insensitive option match");
        assert_eq!(a[1], Answer { option: None, text: Some("wh".into()) });
        assert_eq!(why, "r");

        // A reworded option becomes free text; plain strings are free text.
        let (a, _) = parse_answers("{\"answers\": [{\"option\": \"MySQL\"}, \"wh\"]}", &qs).unwrap();
        assert_eq!(a[0], Answer { option: None, text: Some("MySQL".into()) });
        assert_eq!(a[1].text.as_deref(), Some("wh"));

        assert!(parse_answers("{\"answers\": [{\"text\": \"x\"}]}", &qs).unwrap_err().contains("expected 2"));
        assert!(parse_answers("{\"answers\": [{}, {\"text\": \"x\"}]}", &qs).unwrap_err().contains("empty"));
        assert!(parse_answers("nothing", &qs).is_err());
    }

    #[test]
    fn single_question_may_combine_option_and_text() {
        let qs = [q("Which?", &["A", "B"])];
        let (a, _) = parse_answers("{\"answers\": [{\"option\": \"B\", \"text\": \"because\"}]}", &qs).unwrap();
        assert_eq!(a[0], Answer { option: Some("B".into()), text: Some("because".into()) });
    }

    #[test]
    fn prompt_covers_task_request_and_policy() {
        let ctx = TaskContext {
            title: "Add login".into(),
            body: Some("Users sign in with email.".into()),
            stages: vec!["spec".into(), "implement".into()],
            results: vec![("spec".into(), "succeeded".into(), Some("SPEC TEXT".into()))],
            rework_loops: 1,
            max_rework_loops: 3,
            diff: Some("+fn login() {}".into()),
            dialogue: vec![],
            guidelines: Some("Never approve without tests.".into()),
        };
        let gate = build_prompt(&ctx, &Request::Gate { gate: "spec-review".into(), stage: "spec".into(), conflict_base: None });
        for needle in ["Add login", "Users sign in", "spec → implement", "SPEC TEXT", "+fn login", "Never approve without tests", "gate `spec-review`", "1 of at most 3", "\"decision\""] {
            assert!(gate.contains(needle), "missing {needle:?}");
        }
        let ship = build_prompt(&ctx, &Request::Gate { gate: "g".into(), stage: "ship".into(), conflict_base: None });
        assert!(ship.contains("pull request"));
        let conflict = build_prompt(&ctx, &Request::Gate { gate: "g".into(), stage: "conflict".into(), conflict_base: Some("main".into()) });
        assert!(conflict.contains("conflicts with its base branch `main`"));

        let ask = build_prompt(&ctx, &Request::Questions { stage: "implement".into(), questions: vec![q("Which?", &["A", "B"])] });
        for needle in ["1. Which?", "option: A", "option: B", "\"answers\""] {
            assert!(ask.contains(needle), "missing {needle:?}");
        }
    }
}
