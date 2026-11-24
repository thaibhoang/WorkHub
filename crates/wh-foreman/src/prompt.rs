use wh_core::task::{StageOutcome, TaskState};
use wh_factory::{Agent, Stage};

/// Assemble the prompt for a stage: the agent's role prompt, the task, what
/// earlier stages produced, any feedback to address, and the output contract.
pub fn build_prompt(agent: &Agent, stage: &Stage, state: &TaskState, feedback: Option<&str>) -> String {
    let mut p = String::new();
    p.push_str(agent.prompt.trim());
    p.push_str("\n\n## Task\n\n");
    p.push_str(&format!("**{}**\n", state.title));
    if let Some(body) = state.body.as_deref().filter(|b| *b != state.title) {
        p.push_str(&format!("\n{body}\n"));
    }

    let previous: Vec<_> = state.results.iter().filter(|r| r.summary.is_some()).collect();
    if !previous.is_empty() {
        p.push_str("\n## Earlier stages (oldest first)\n");
        for r in previous {
            let tag = match r.outcome {
                StageOutcome::Succeeded => "",
                StageOutcome::Rejected => " — REJECTED",
                StageOutcome::Failed => " — FAILED",
                StageOutcome::Skipped => " — skipped",
            };
            p.push_str(&format!("\n### {}{tag}\n\n{}\n", r.stage, r.summary.as_deref().unwrap_or("").trim()));
        }
    }

    if let Some(fb) = feedback {
        p.push_str("\n## Feedback you must address\n\nThis stage is being re-run. Fix every point below:\n\n");
        p.push_str(fb.trim());
        p.push('\n');
    }

    p.push_str(&format!(
        "\n## Rules\n\n- You are the `{}` stage. Work in the current directory, on the current git branch.\n\
         - Do not create branches, push, or open PRs. WorkHub commits your changes when you finish.\n\
         - End with a concise summary of what you did; later stages only see that summary and the code.\n",
        stage.id
    ));
    if stage.requires_verdict() {
        p.push_str(
            "- Your final message MUST end with exactly one line: `VERDICT: APPROVE` or `VERDICT: REJECT`. \
             When rejecting, list each concrete problem above that line.\n",
        );
    }
    p
}
