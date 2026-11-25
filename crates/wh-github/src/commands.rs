//! Human-facing commands: ship a task by hand, and check the GitHub setup.

use wh_core::{
    Actor, EventMeta, NewEvent, TaskId,
    task::{SHIP_STAGE, TaskEvent, TaskState, TaskStatus},
};
use wh_eventstore::{EventStore, ExpectedVersion};
use wh_factory::Factory;

use crate::{
    cli::{Gh, git},
    ship::{resolve_repo, ship},
};

/// Ship a task now (retry after a failed ship, or ship a task that completed
/// locally before `github:` was configured). Records `TaskShipped`.
pub async fn ship_task(store: &EventStore, f: &Factory, gh: &Gh, actor: Actor, task: TaskId) -> anyhow::Result<TaskEvent> {
    let events = store.read_stream(&task.stream_id()).await?;
    anyhow::ensure!(!events.is_empty(), "task {task} not found");
    let mut state = TaskState::default();
    for e in &events {
        state.apply(e.stream_version, e.decode()?);
    }
    anyhow::ensure!(state.factory.as_deref() == Some(f.name.as_str()), "task {task} does not belong to factory '{}'", f.name);
    anyhow::ensure!(state.benchmark.is_none(), "benchmark tasks are never shipped");
    let ok = match &state.status {
        TaskStatus::Blocked { .. } | TaskStatus::Done => true,
        TaskStatus::AwaitingGate { stage, .. } => stage == SHIP_STAGE,
        _ => false,
    };
    anyhow::ensure!(ok, "only blocked or done tasks can be shipped by hand (task is {:?})", state.status);
    let ev = ship(gh, f, task, &state).await?;
    let mut meta = EventMeta::new(task.0, actor);
    meta.factory_version = Some(f.version.clone());
    store.append(&task.stream_id(), ExpectedVersion::Exact(state.version), vec![NewEvent::from_domain(&ev, meta)?]).await?;
    wh_metrics::catch_up_all(store).await?;
    Ok(ev)
}

#[derive(Debug)]
pub struct CheckLine {
    pub ok: bool,
    pub label: String,
    pub detail: String,
}

/// Is `gh` installed and logged in, does the remote exist, can we see the repo
/// and its base branch? Every line is reported, even after a failure.
pub async fn check(f: &Factory, gh: &Gh) -> Vec<CheckLine> {
    let mut out = Vec::new();
    let mut line = |ok: bool, label: &str, detail: String| out.push(CheckLine { ok, label: label.into(), detail });
    let Some(g) = &f.github else {
        line(false, "factory", format!("'{}' has no `github:` section", f.name));
        return out;
    };
    match gh.run(None, &["--version"]).await {
        Ok(v) => line(true, "gh", v.lines().next().unwrap_or("").to_owned()),
        Err(e) => {
            line(false, "gh", format!("{e} (install the GitHub CLI: https://cli.github.com)"));
            return out;
        }
    }
    match gh.run(None, &["auth", "status"]).await {
        Ok(_) => line(true, "auth", "logged in".into()),
        Err(e) => line(false, "auth", format!("{e} (run `gh auth login`)")),
    }
    match git(&f.repo, &["remote", "get-url", &g.remote]).await {
        Ok(u) => line(true, "remote", format!("{} → {u}", g.remote)),
        Err(e) => line(false, "remote", e.to_string()),
    }
    let repo = match resolve_repo(f, g).await {
        Ok(r) => r,
        Err(e) => {
            line(false, "repo", e.to_string());
            return out;
        }
    };
    let view = gh
        .json::<serde_json::Value>(None, &["repo", "view", &repo, "--json", "nameWithOwner,defaultBranchRef,viewerPermission"])
        .await;
    let default_branch = match view {
        Ok(v) => {
            let perm = v["viewerPermission"].as_str().unwrap_or("?").to_owned();
            let can_push = matches!(perm.as_str(), "ADMIN" | "MAINTAIN" | "WRITE");
            line(can_push, "repo", format!("{repo} (permission {perm})"));
            v["defaultBranchRef"]["name"].as_str().map(str::to_owned)
        }
        Err(e) => {
            line(false, "repo", e.to_string());
            None
        }
    };
    if let Some(base) = g.base.clone().or(default_branch) {
        match git(&f.repo, &["ls-remote", "--heads", &g.remote, &base]).await {
            Ok(o) if !o.is_empty() => line(true, "base", format!("{base} exists on {}", g.remote)),
            Ok(_) => line(false, "base", format!("{base} not found on {}", g.remote)),
            Err(e) => line(false, "base", e.to_string()),
        }
    }
    if let Some(i) = &g.issues {
        line(true, "issues", format!("tasks from open issues labelled '{}'", i.label));
    }
    out
}
