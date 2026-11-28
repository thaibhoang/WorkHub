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

/// `UserAnswered.user` of answers the foreman gives itself (question limit,
/// benchmarks). Not a human intervention.
pub const AUTO_ANSWER_USER: &str = "foreman:auto";

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

/// The text handed back to the agent: each numbered question followed by its
/// answer (`answers[i]` answers `questions[i]`). Pure.
pub fn format_answers(questions: &[Question], answers: &[Answer]) -> String {
    let blocks: Vec<_> = questions
        .iter()
        .zip(answers)
        .enumerate()
        .map(|(i, (q, a))| {
            let reply: Vec<_> = [&a.option, &a.text].into_iter().flatten().map(String::as_str).collect();
            format!("{}. {}\nAnswer: {}", i + 1, q.text, reply.join("\n"))
        })
        .collect();
    blocks.join("\n\n")
}

/// Validate answers to `questions`: one per question, each with an offered option
/// and/or free text (both only when there is a single question). Blank text counts
/// as none. Returns the cleaned answers or a message for the one who answered. Pure.
pub fn check_answers(questions: &[Question], answers: Vec<Answer>) -> Result<Vec<Answer>, String> {
    if answers.len() != questions.len() {
        return Err(format!("expected {} answer(s), got {}", questions.len(), answers.len()));
    }
    let mut checked = Vec::with_capacity(answers.len());
    for (i, (q, mut a)) in questions.iter().zip(answers).enumerate() {
        let n = i + 1;
        a.text = a.text.filter(|t| !t.trim().is_empty());
        if a.option.is_none() && a.text.is_none() {
            return Err(format!("answer {n} is empty"));
        }
        if let Some(o) = &a.option
            && !q.options.contains(o)
        {
            return Err(format!("answer {n}: {o:?} is not one of the offered options"));
        }
        if questions.len() > 1 && a.option.is_some() && a.text.is_some() {
            return Err(format!("answer {n}: give an option or text, not both, when there are several questions"));
        }
        checked.push(a);
    }
    Ok(checked)
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
    fn question_answers_are_formatted_with_option_text_or_both() {
        let questions = [q("Which?", &["A"]), q("Name?", &[]), q("Colour?", &["red"])];
        let answer = |option: Option<&str>, text: Option<&str>| Answer {
            option: option.map(Into::into),
            text: text.map(Into::into),
        };
        let answers = [answer(Some("A"), None), answer(None, Some("wh")), answer(Some("red"), Some("dark"))];
        assert_eq!(
            format_answers(&questions, &answers),
            "1. Which?\nAnswer: A\n\n2. Name?\nAnswer: wh\n\n3. Colour?\nAnswer: red\ndark"
        );
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
