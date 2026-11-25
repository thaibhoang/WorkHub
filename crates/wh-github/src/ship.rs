//! Ship: push the task branch and open (or find) its pull request.

use std::path::Path;

use serde::Deserialize;
use wh_core::{
    FailureClass, TaskId,
    task::{StageOutcome, TaskEvent, TaskState, task_branch},
};
use wh_factory::{Factory, Github};

use crate::cli::{CliError, CliResult, Gh, git};

const MAX_SUMMARY: usize = 1_500;

/// `owner/name` from a GitHub remote URL (https, ssh or scp-like form).
pub fn parse_slug(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

/// PR number from the URL `gh pr create` prints (`…/pull/<n>`).
pub fn parse_pr_number(url: &str) -> Option<u64> {
    url.trim().trim_end_matches('/').rsplit_once("/pull/")?.1.parse().ok()
}

/// The configured repo, or the one the remote points at.
pub async fn resolve_repo(f: &Factory, g: &Github) -> CliResult<String> {
    if let Some(r) = &g.repo {
        return Ok(r.clone());
    }
    let url = git(&f.repo, &["remote", "get-url", &g.remote]).await?;
    parse_slug(&url).ok_or_else(|| CliError {
        class: FailureClass::EnvSetup,
        message: format!("remote '{}' ({url}) is not a GitHub URL; set `github.repo` in the factory", g.remote),
    })
}

/// Pull request description: the task, `Closes #N` for an issue of the same repo,
/// what each stage reported, and a footer pointing back at the task.
pub fn pr_body(task: TaskId, state: &TaskState, repo: &str) -> String {
    let mut s = String::new();
    if let Some(body) = state.body.as_deref().filter(|b| !b.trim().is_empty() && *b != state.title) {
        s.push_str(body.trim());
        s.push_str("\n\n");
    }
    if let Some(ext) = state.external_ref.as_ref().filter(|e| e.repo.eq_ignore_ascii_case(repo)) {
        s.push_str(&format!("Closes #{}\n\n", ext.issue));
    }
    // Latest successful summary per stage, in pipeline order of first appearance.
    let mut stages: Vec<(&str, &str)> = Vec::new();
    for r in state.results.iter().filter(|r| r.outcome == StageOutcome::Succeeded) {
        let Some(sum) = r.summary.as_deref().filter(|t| !t.trim().is_empty()) else { continue };
        match stages.iter_mut().find(|(st, _)| *st == r.stage) {
            Some(e) => e.1 = sum,
            None => stages.push((&r.stage, sum)),
        }
    }
    if !stages.is_empty() {
        s.push_str("## What the agents did\n");
        for (stage, sum) in stages {
            let sum = sum.trim();
            let cut: String = sum.chars().take(MAX_SUMMARY).collect();
            let more = if cut.len() < sum.len() { "\n…" } else { "" };
            s.push_str(&format!("\n<details><summary>{stage}</summary>\n\n{cut}{more}\n\n</details>\n"));
        }
        s.push('\n');
    }
    s.push_str(&format!("---\nOpened by WorkHub · task `{task}`\n"));
    s
}

#[derive(Deserialize)]
struct PrRef {
    number: u64,
    url: String,
}

/// Push the task branch and make sure an open PR exists for it. Idempotent: a
/// second call pushes again (a no-op or a fast-forward) and finds the same PR.
/// Returns the `TaskShipped` event to record.
pub async fn ship(gh: &Gh, f: &Factory, task: TaskId, state: &TaskState) -> CliResult<TaskEvent> {
    let g = f.github.as_ref().ok_or_else(|| CliError {
        class: FailureClass::EnvSetup,
        message: format!("factory '{}' has no `github:` section", f.name),
    })?;
    let repo = resolve_repo(f, g).await?;
    let branch = task_branch(task);
    let dir: &Path = &f.repo;
    let commit = git(dir, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]).await.map_err(|_| CliError {
        class: FailureClass::EnvSetup,
        message: format!("branch {branch} does not exist in {}", dir.display()),
    })?;
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    git(dir, &["push", "--quiet", &g.remote, &refspec]).await?;

    let existing: Vec<PrRef> = gh
        .json(Some(dir), &["pr", "list", "--repo", &repo, "--head", &branch, "--state", "open", "--json", "number,url"])
        .await?;
    let (number, url, created) = match existing.into_iter().next() {
        Some(pr) => (pr.number, pr.url, false),
        None => {
            let title = state.title.trim().to_owned();
            let mut args = vec!["pr", "create", "--repo", &repo, "--head", &branch, "--title", &title, "--body-file", "-"];
            if let Some(base) = &g.base {
                args.extend(["--base", base]);
            }
            if g.draft {
                args.push("--draft");
            }
            for l in &g.labels {
                args.extend(["--label", l]);
            }
            let out = gh.run_stdin(Some(dir), &args, &pr_body(task, state, &repo)).await?;
            let url = out.lines().rev().find(|l| l.contains("/pull/")).unwrap_or("").trim().to_owned();
            let number = parse_pr_number(&url).ok_or_else(|| CliError {
                class: FailureClass::Infra,
                message: format!("could not read the PR url from `gh pr create`: {}", out.trim()),
            })?;
            (number, url, true)
        }
    };
    Ok(TaskEvent::TaskShipped { repo, pr_number: number, pr_url: url, branch, commit, created })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wh_core::task::{ExternalRef, StageResult};

    #[test]
    fn slugs_and_numbers() {
        for u in [
            "https://github.com/acme/demo.git",
            "https://github.com/acme/demo",
            "git@github.com:acme/demo.git",
            "ssh://git@github.com/acme/demo/",
        ] {
            assert_eq!(parse_slug(u).as_deref(), Some("acme/demo"), "{u}");
        }
        assert_eq!(parse_slug("/tmp/remote.git"), None);
        assert_eq!(parse_slug("https://gitlab.com/a/b"), None);
        assert_eq!(parse_pr_number("https://github.com/acme/demo/pull/42\n"), Some(42));
        assert_eq!(parse_pr_number("nope"), None);
    }

    #[test]
    fn body_closes_issue_and_summarises_stages() {
        let task = TaskId::new();
        let mut state = TaskState { title: "Fix it".into(), body: Some("It is broken".into()), ..Default::default() };
        state.external_ref = Some(ExternalRef { repo: "Acme/Demo".into(), issue: 7, url: String::new() });
        let r = |stage: &str, outcome, summary: &str| StageResult { stage: stage.into(), outcome, summary: Some(summary.into()) };
        state.results = vec![
            r("implement", StageOutcome::Succeeded, "v1"),
            r("review", StageOutcome::Rejected, "bad"),
            r("implement", StageOutcome::Succeeded, "v2"),
        ];
        let b = pr_body(task, &state, "acme/demo");
        assert!(b.starts_with("It is broken\n\nCloses #7\n"), "{b}");
        assert!(b.contains("<summary>implement</summary>\n\nv2") && !b.contains("v1") && !b.contains("bad"), "{b}");
        assert!(b.contains(&task.to_string()));
        assert!(!pr_body(task, &state, "other/repo").contains("Closes"));
    }
}
