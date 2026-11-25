//! Pure parts of LLM-as-judge: the grading prompt, transcript condensing and
//! parsing the judge's answer. No I/O, so it is unit-tested directly.

use serde_json::Value;
use wh_factory::Scorer;

const MAX_AGENT_PROMPT: usize = 6_000;
const MAX_SUMMARY: usize = 6_000;
const MAX_DIFF: usize = 40_000;
const MAX_TRANSCRIPT: usize = 20_000;

/// What the judge gets to see about the graded session.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub task_title: String,
    pub task_body: Option<String>,
    pub agent: String,
    pub stage: String,
    /// Instructions the agent was given.
    pub agent_prompt: String,
    /// "succeeded" or "failed [class]: message".
    pub outcome: String,
    /// The agent's final message.
    pub summary: Option<String>,
    /// Patch of the commit WorkHub made for the session (if any).
    pub diff: Option<String>,
    /// Condensed transcript (see [`condense_transcript`]).
    pub transcript: Option<String>,
}

pub fn build_prompt(scorer: &Scorer, ctx: &SessionContext) -> String {
    let mut p = String::new();
    p.push_str(
        "You are grading the work of an AI agent in a software factory. You have no tools: \
         judge only from the material below, and do not try to fix anything.\n\n",
    );
    p.push_str(&format!("# Rubric: {}\n\n{}\n\n", scorer.id, scorer.prompt.trim()));
    p.push_str(&format!("# Task\n\nTitle: {}\n", ctx.task_title));
    if let Some(body) = ctx.task_body.as_deref().filter(|b| !b.trim().is_empty()) {
        p.push_str(&format!("\n{}\n", body.trim()));
    }
    p.push_str(&format!(
        "\n# Graded session\n\nAgent `{}` at stage `{}`. Outcome: {}\n",
        ctx.agent, ctx.stage, ctx.outcome
    ));
    p.push_str(&format!(
        "\n## Instructions the agent was given\n\n{}\n",
        head(&ctx.agent_prompt, MAX_AGENT_PROMPT)
    ));
    p.push_str(&format!(
        "\n## Agent's final message\n\n{}\n",
        ctx.summary.as_deref().map(|s| head(s, MAX_SUMMARY)).unwrap_or_else(|| "(none)".into())
    ));
    match ctx.diff.as_deref().filter(|d| !d.trim().is_empty()) {
        Some(d) => p.push_str(&format!("\n## Changes the agent committed\n\n```diff\n{}\n```\n", head(d, MAX_DIFF))),
        None => p.push_str("\n## Changes the agent committed\n\n(no file changes)\n"),
    }
    if let Some(t) = ctx.transcript.as_deref().filter(|t| !t.trim().is_empty()) {
        p.push_str(&format!("\n## Transcript excerpt (tool calls and messages)\n\n{}\n", tail(t, MAX_TRANSCRIPT)));
    }
    p.push_str(
        "\n# Answer format\n\nThink briefly, then end your answer with exactly one JSON object on its own line:\n\
         {\"score\": <number from 0.0 (fails the rubric) to 1.0 (fully meets it)>, \
         \"classification\": \"<short_snake_case_label for the main finding>\", \
         \"rationale\": \"<1-3 sentences citing concrete evidence>\"}\n",
    );
    p
}

#[derive(Debug, Clone, PartialEq)]
pub struct Judgement {
    pub score: f64,
    pub classification: Option<String>,
    pub rationale: String,
}

/// Find the last JSON object with a numeric `score` in the judge's answer
/// (tolerates code fences and text around it).
pub fn parse_judgement(text: &str) -> Result<Judgement, String> {
    for (i, _) in text.match_indices('{').collect::<Vec<_>>().into_iter().rev() {
        let mut it = serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>();
        let Some(Ok(v)) = it.next() else { continue };
        let Some(score) = v.get("score").and_then(score_value) else { continue };
        if !(0.0..=1.0).contains(&score) {
            return Err(format!("score {score} is outside 0..=1"));
        }
        let classification = v
            .get("classification")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let rationale = v.get("rationale").and_then(Value::as_str).unwrap_or("").trim().to_owned();
        return Ok(Judgement { score, classification, rationale });
    }
    Err("judge answer has no JSON object with a \"score\"".into())
}

fn score_value(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// Turn a Claude Code `stream-json` transcript into a compact, readable log:
/// assistant text, tool calls with a short input preview, and tool results
/// (errors kept longer, since they are usually what a rubric cares about).
pub fn condense_transcript(jsonl: &str) -> String {
    let mut out = String::new();
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let blocks = v["message"]["content"].as_array().cloned().unwrap_or_default();
        match v["type"].as_str() {
            Some("assistant") => {
                for b in &blocks {
                    match b["type"].as_str() {
                        Some("text") => {
                            let t = b["text"].as_str().unwrap_or("").trim();
                            if !t.is_empty() {
                                out.push_str(&format!("assistant: {}\n", head(t, 1_500)));
                            }
                        }
                        Some("tool_use") => {
                            let name = b["name"].as_str().unwrap_or("?");
                            out.push_str(&format!("→ {name} {}\n", one_line(&b["input"].to_string(), 300)));
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                    let is_error = b["is_error"].as_bool().unwrap_or(false);
                    let content = match &b["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(parts) => {
                            parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
                        }
                        _ => String::new(),
                    };
                    let (tag, max) = if is_error { ("← ERROR", 800) } else { ("←", 300) };
                    out.push_str(&format!("{tag} {}\n", one_line(&content, max)));
                }
            }
            _ => {}
        }
    }
    out
}

fn one_line(s: &str, max: usize) -> String {
    head(&s.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn head(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}\n… [truncated]", &s[..i]),
        None => s.to_owned(),
    }
}

fn tail(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_owned();
    }
    let skip = s.char_indices().nth(n - max_chars).map(|(i, _)| i).unwrap_or(0);
    format!("[… earlier transcript truncated]\n{}", &s[skip..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_last_score_object() {
        let text = "Reasoning {not json}.\n```json\n{\"score\": 0.4, \"classification\": \"scope_creep\", \"rationale\": \"edited CI\"}\n```\nthanks";
        let j = parse_judgement(text).unwrap();
        assert_eq!(j.score, 0.4);
        assert_eq!(j.classification.as_deref(), Some("scope_creep"));
        assert_eq!(j.rationale, "edited CI");

        let two = "{\"score\": 0.1}\nrevised: {\"score\": \"0.9\", \"rationale\": \"ok\"}";
        assert_eq!(parse_judgement(two).unwrap().score, 0.9);
    }

    #[test]
    fn rejects_missing_or_out_of_range_scores() {
        assert!(parse_judgement("no json here").is_err());
        assert!(parse_judgement("{\"verdict\": \"good\"}").is_err());
        assert!(parse_judgement("{\"score\": 7}").unwrap_err().contains("outside"));
    }

    #[test]
    fn condenses_transcript() {
        let jsonl = [
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo   test"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":[{"type":"text","text":"permission denied"}]}]}}"#,
            r#"{"type":"result","subtype":"success"}"#,
        ]
        .join("\n");
        let c = condense_transcript(&jsonl);
        assert_eq!(
            c,
            "assistant: Let me look.\n→ Bash {\"command\":\"cargo test\"}\n← ERROR permission denied\n"
        );
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(head("ééé", 2), "éé\n… [truncated]");
        assert!(tail("ééé", 2).ends_with("éé"));
        assert_eq!(tail("ab", 5), "ab");
    }
}
