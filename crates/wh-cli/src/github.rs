//! `wh github …` and the GitHub worker run by `wh foreman` / `wh serve --foreman`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use wh_eventstore::EventStore;
use wh_factory::Factory;
use wh_github::{Gh, GithubService, commands};

#[derive(Subcommand)]
pub enum GithubCmd {
    /// Check the setup: gh installed and logged in, remote, repo access, base branch
    Check {
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        #[arg(long, env = "WH_GH_BIN", default_value = "gh")]
        gh_bin: PathBuf,
    },
    /// Poll once now: create tasks from labelled issues, record PR reviews / CI / merges
    Sync {
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        #[arg(long, env = "WH_GH_BIN", default_value = "gh")]
        gh_bin: PathBuf,
    },
}

pub async fn github_cmd(store: &EventStore, cmd: GithubCmd) -> Result<()> {
    match cmd {
        GithubCmd::Check { factory, gh_bin } => {
            let f = Factory::load(&factory)?;
            let lines = commands::check(&f, &Gh::new(gh_bin)).await;
            let mut ok = true;
            for l in &lines {
                ok &= l.ok;
                println!("{} {:<7} {}", if l.ok { "ok  " } else { "FAIL" }, l.label, l.detail);
            }
            anyhow::ensure!(ok, "GitHub setup is incomplete");
        }
        GithubCmd::Sync { factory, gh_bin } => {
            let svc = GithubService::new(store.clone(), Factory::load(&factory)?, Gh::new(gh_bin))?;
            let r = svc.sync_once().await?;
            for (task, issue) in &r.created {
                println!("issue #{issue} → task {task}");
            }
            for (task, evs) in &r.recorded {
                println!("task {task}: {}", evs.join(", "));
            }
            for e in &r.errors {
                eprintln!("warning: {e}");
            }
            if r.created.is_empty() && r.recorded.is_empty() {
                println!("nothing new");
            }
        }
    }
    Ok(())
}

/// PR tracking + issue intake until Ctrl+C (only for factories with `github:`).
pub async fn run_github(store: &EventStore, f: Factory, gh_bin: PathBuf, enabled: bool) -> Result<()> {
    let Some(g) = f.github.clone().filter(|_| enabled) else { return Ok(()) };
    let issues = g.issues.as_ref().map(|i| format!(", tasks from issues labelled '{}'", i.label)).unwrap_or_default();
    println!(
        "github: {} via {}, polling every {}s{issues}",
        g.repo.as_deref().unwrap_or("(repo from remote)"),
        g.remote,
        g.poll_interval.as_secs()
    );
    GithubService::new(store.clone(), f, Gh::new(gh_bin))?.run(crate::ctrl_c()).await
}
