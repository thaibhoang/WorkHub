//! The question block an agent ends its final message with to ask the user:
//! one `QUESTION: {"text": "...", "options": ["A", "B"]}` line per question.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    /// Suggested answers; empty = open question.
    #[serde(default)]
    pub options: Vec<String>,
}

/// The user's reply to one `Question`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// Text of the chosen option (not its index).
    #[serde(default)]
    pub option: Option<String>,
    /// Free text.
    #[serde(default)]
    pub text: Option<String>,
}

/// Parse the `QUESTION:` lines at the end of an agent's final message (blank lines
/// and code fences may surround them). Malformed lines are ignored; an empty
/// result means the agent is not asking. Pure.
pub fn parse_questions(text: &str) -> Vec<Question> {
    let mut questions = Vec::new();
    for line in text.lines().rev() {
        let line = line.trim();
        if let Some(json) = question_json(line) {
            if let Ok(q) = serde_json::from_str::<Question>(json)
                && !q.text.trim().is_empty()
            {
                questions.push(q);
            }
        } else if !(line.is_empty() || line.starts_with("```")) {
            break;
        }
    }
    questions.reverse();
    questions
}

/// The part after `QUESTION:`, without surrounding markdown.
fn question_json(line: &str) -> Option<&str> {
    const PREFIX: &str = "QUESTION:";
    let markdown = |c: char| c == '*' || c == '`' || c == '#' || c.is_whitespace();
    let line = line.trim_matches(markdown);
    let head = line.get(..PREFIX.len())?;
    head.eq_ignore_ascii_case(PREFIX).then(|| line[PREFIX.len()..].trim_matches(markdown))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, options: &[&str]) -> Question {
        Question { text: text.into(), options: options.iter().map(|o| o.to_string()).collect() }
    }

    #[test]
    fn with_and_without_options() {
        assert_eq!(
            parse_questions("I need to know.\nQUESTION: {\"text\": \"Which?\", \"options\": [\"A\", \"B\"]}"),
            [q("Which?", &["A", "B"])]
        );
        assert_eq!(parse_questions(r#"QUESTION: {"text": "Name?"}"#), [q("Name?", &[])]);
        assert_eq!(parse_questions(r#"QUESTION: {"text": "Name?", "options": []}"#), [q("Name?", &[])]);
    }

    #[test]
    fn several_questions_keep_their_order() {
        let text = "QUESTION: {\"text\": \"first\"}\n\nQUESTION: {\"text\": \"second\"}\n";
        assert_eq!(parse_questions(text), [q("first", &[]), q("second", &[])]);
    }

    #[test]
    fn malformed_lines_are_ignored() {
        assert!(parse_questions(r#"QUESTION: {"text": "Which?""#).is_empty(), "broken JSON");
        assert!(parse_questions(r#"QUESTION: {"options": ["A"]}"#).is_empty(), "missing text");
        assert!(parse_questions(r#"QUESTION: {"text": "  "}"#).is_empty(), "blank text");
        let text = "QUESTION: {\"text\": \"ok\"}\nQUESTION: {nope}\nQUESTION: {\"text\": \"also\"}";
        assert_eq!(parse_questions(text), [q("ok", &[]), q("also", &[])]);
    }

    #[test]
    fn only_the_block_at_the_end_counts() {
        let text = "Ask like this:\nQUESTION: {\"text\": \"example\"}\nDone, nothing to ask.";
        assert!(parse_questions(text).is_empty());
        let text = "QUESTION: {\"text\": \"example\"}\nbut really:\nQUESTION: {\"text\": \"real\"}";
        assert_eq!(parse_questions(text), [q("real", &[])]);
        assert!(parse_questions("no question here").is_empty());
        assert!(parse_questions("").is_empty());
    }

    #[test]
    fn markdown_and_code_fences_are_tolerated() {
        let want = [q("Which?", &["A"])];
        let json = r#"{"text": "Which?", "options": ["A"]}"#;
        for text in [
            format!("**QUESTION:** {json}"),
            format!("`QUESTION: {json}`"),
            format!("**question: {json}**"),
            format!("```\nQUESTION: {json}\n```\n"),
            format!("```text\nQUESTION: {json}\n\n```\n\n"),
        ] {
            assert_eq!(parse_questions(&text), want, "{text}");
        }
    }
}
