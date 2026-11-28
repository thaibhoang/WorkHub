//! `wh sandbox`: build the agent image, check the Docker setup, and list or
//! clean up agent containers.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use tokio::process::Command;
use wh_core::{DockerSandbox, Sandbox, SessionId};
use wh_eventstore::EventStore;
use wh_factory::Factory;
use wh_metrics::queries;
use wh_runner::{
    AgentRequest, AgentRuntime, OutcomeStatus, SessionMode, Signal,
    claude_code::ClaudeCode,
    docker::{self, AUTH_ENV},
    worktree::Worktree,
};

use crate::data_dir;

#[derive(Subcommand)]
pub enum SandboxCmd {
    /// Build the agent image (Claude Code pinned to the host's version by default)
    Build {
        #[arg(long, default_value = "docker/agent.Dockerfile")]
        file: PathBuf,
        #[arg(long, default_value = DockerSandbox::DEFAULT_IMAGE)]
        tag: String,
        /// Claude Code version to install (default: the host's `claude --version`)
        #[arg(long)]
        claude_version: Option<String>,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
    },
    /// Check docker, the image(s), and auth; --smoke also runs a tiny real session (~$0.01)
    Check {
        /// Check the images of this factory's docker agents
        #[arg(long)]
        factory: Option<PathBuf>,
        /// Check this image (default: the factory's images, or workhub-agent:latest)
        #[arg(long)]
        image: Option<String>,
        #[arg(long)]
        network: Option<String>,
        /// Run a real Claude Code session in the container (Haiku, budget $0.05)
        #[arg(long)]
        smoke: bool,
    },
    /// List WorkHub agent containers and the state of their sessions
    Ps,
    /// Remove containers (and sandbox homes) of sessions that are no longer running
    Prune {
        /// Also remove containers whose session still looks running
        #[arg(long)]
        force: bool,
    },
}

pub async fn sandbox_cmd(store: &EventStore, cmd: SandboxCmd) -> Result<()> {
    let docker_bin = Path::new("docker");
    match cmd {
        SandboxCmd::Build { file, tag, claude_version, claude_bin } => {
            let version = match claude_version {
                Some(v) => v,
                None => host_claude_version(&claude_bin).await.unwrap_or_else(|e| {
                    eprintln!("could not read the host's Claude Code version ({e:#}); installing latest");
                    "latest".into()
                }),
            };
            let context = file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            println!("building {tag} from {} (Claude Code {version})", file.display());
            let status = Command::new(docker_bin)
                .arg("build")
                .arg("-f")
                .arg(&file)
                .args(["--build-arg", &format!("CLAUDE_CODE_VERSION={version}"), "-t", &tag])
                .arg(context)
                .status()
                .await
                .context("failed to run docker")?;
            if !status.success() {
                bail!("docker build failed ({status})");
            }
            println!("built {tag}");
        }
        SandboxCmd::Check { factory, image, network, smoke } => {
            let mut configs: Vec<(String, DockerSandbox)> = Vec::new();
            if let Some(path) = &factory {
                let f = Factory::load(path)?;
                for a in f.agents.values() {
                    match &a.sandbox {
                        Sandbox::Worktree => println!("  agent {:<15} worktree (no container)", a.name),
                        Sandbox::Docker(d) => configs.push((format!("agent {}", a.name), d.clone())),
                    }
                }
            }
            if image.is_some() || configs.is_empty() {
                let mut d = DockerSandbox::new(image.unwrap_or_else(|| DockerSandbox::DEFAULT_IMAGE.into()));
                if let Some(n) = network {
                    d.network = n;
                }
                configs.push(("image".into(), d));
            }
            check(docker_bin, &configs, smoke).await?;
        }
        SandboxCmd::Ps => {
            let containers = docker::list(docker_bin).await?;
            if containers.is_empty() {
                println!("no WorkHub agent containers");
                return Ok(());
            }
            wh_metrics::catch_up_all(store).await?;
            println!("{:<44} {:<12} {:<28} docker status", "container", "session", "image");
            for c in containers {
                let state = session_status(store, &c.session).await.unwrap_or_else(|| "unknown".into());
                println!("{:<44} {:<12} {:<28} {}", c.name, state, c.image, c.status);
            }
        }
        SandboxCmd::Prune { force } => {
            wh_metrics::catch_up_all(store).await?;
            let mut removed = 0;
            for c in docker::list(docker_bin).await? {
                let state = session_status(store, &c.session).await;
                if force || !matches!(state.as_deref(), Some("dispatched" | "running")) {
                    docker::remove(docker_bin, &c.name).await;
                    println!("removed {} (session {})", c.name, state.as_deref().unwrap_or("unknown"));
                    removed += 1;
                }
            }
            let homes = data_dir().join("sandbox-home");
            if let Ok(mut dir) = tokio::fs::read_dir(&homes).await {
                while let Some(entry) = dir.next_entry().await? {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let state = session_status(store, &name).await;
                    if force || !matches!(state.as_deref(), Some("dispatched" | "running")) {
                        tokio::fs::remove_dir_all(entry.path()).await.ok();
                        println!("removed sandbox home {name}");
                        removed += 1;
                    }
                }
            }
            println!("{removed} removed");
        }
    }
    Ok(())
}

async fn session_status(store: &EventStore, session: &str) -> Option<String> {
    let id: SessionId = session.parse().ok()?;
    queries::session(store.pool(), id.0).await.ok().flatten().map(|s| s.status)
}

async fn host_claude_version(bin: &Path) -> Result<String> {
    let out = Command::new(bin).arg("--version").output().await.context("run claude --version")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = text.split_whitespace().next().context("empty version output")?;
    if !v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        bail!("unexpected version output: {text}");
    }
    Ok(v.to_owned())
}

async fn output(cmd: &mut Command) -> Result<(bool, String)> {
    let out = cmd.output().await?;
    let text = if out.status.success() { &out.stdout } else { &out.stderr };
    Ok((out.status.success(), String::from_utf8_lossy(text).trim().to_owned()))
}

fn mark(ok: bool) -> &'static str {
    if ok { "ok  " } else { "FAIL" }
}

async fn check(docker_bin: &Path, configs: &[(String, DockerSandbox)], smoke: bool) -> Result<()> {
    let mut failed = false;
    let (ok, server) =
        output(Command::new(docker_bin).args(["version", "--format", "{{.Server.Version}}"])).await.unwrap_or_else(
            |e| (false, format!("{e}")),
        );
    println!("{} docker daemon {server}", mark(ok));
    if !ok {
        bail!("docker is not available");
    }

    let auth: Vec<&str> = AUTH_ENV[..3].iter().copied().filter(|n| std::env::var_os(n).is_some()).collect();
    let creds = std::env::var_os("HOME").map(|h| Path::new(&h).join(".claude/.credentials.json"));
    let has_creds = creds.as_ref().is_some_and(|c| c.exists());
    match (auth.is_empty(), has_creds) {
        (false, _) => println!("ok   auth from env: {}", auth.join(", ")),
        (true, true) => println!(
            "ok   auth: ~/.claude/.credentials.json is copied into each container's home \
             (a long-lived `claude setup-token` in CLAUDE_CODE_OAUTH_TOKEN is preferred)"
        ),
        (true, false) => {
            failed = true;
            println!("FAIL auth: set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN (`claude setup-token`)");
        }
    }

    let mut seen = std::collections::HashSet::new();
    for (who, d) in configs {
        if !seen.insert(d.image.clone()) {
            continue;
        }
        let (ok, _) = output(Command::new(docker_bin).args(["image", "inspect", "--format", "{{.Id}}", &d.image])).await?;
        println!("{} image {} ({who})", mark(ok), d.image);
        if !ok {
            failed = true;
            println!("     build it with `wh sandbox build --tag {}` (or your own Dockerfile)", d.image);
            continue;
        }
        let (ok, v) =
            output(Command::new(docker_bin).args(["run", "--rm", "--pull", "never", &d.image, "claude", "--version"]))
                .await?;
        println!("{} claude in image: {v}", mark(ok));
        failed |= !ok;
        for v in &d.volumes {
            let host = v.split(':').next().unwrap_or_default();
            let ok = Path::new(host).exists();
            println!("{} volume {v}", mark(ok));
            failed |= !ok;
        }
    }
    if failed {
        bail!("sandbox check failed");
    }
    if smoke {
        let (_, d) = &configs[0];
        smoke_session(d).await?;
    }
    Ok(())
}

/// One real, cheap session through the same path the runner uses: a git worktree,
/// `docker::prepare`, and Claude Code in the container. Not recorded as events.
async fn smoke_session(d: &DockerSandbox) -> Result<()> {
    let data = data_dir();
    let root = data.join("sandbox-smoke");
    let _ = tokio::fs::remove_dir_all(&root).await;
    let repo = root.join("repo");
    tokio::fs::create_dir_all(&repo).await?;
    tokio::fs::write(repo.join("hello.txt"), "sandbox-ok-42\n").await?;
    let git = |args: &'static [&'static str]| {
        let repo = repo.clone();
        async move {
            let out = Command::new("git").arg("-C").arg(&repo).args(args).output().await?;
            anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            anyhow::Ok(())
        }
    };
    git(&["init", "-q", "-b", "main"]).await?;
    git(&["add", "."]).await?;
    git(&["-c", "user.name=wh", "-c", "user.email=wh@localhost", "commit", "-q", "-m", "init"]).await?;

    let session = SessionId::new();
    let wt = Worktree::create(&repo, "HEAD", &root.join("wt"), "wh/smoke").await?;
    let container = docker::prepare(session, d, &wt.path, &root).await?;
    let req = AgentRequest {
        prompt: "Use the Bash tool to run `cat hello.txt` and `git log --oneline`, then create a file \
                 written.txt containing `ok`. Reply with the contents of hello.txt only."
            .into(),
        workdir: wt.path.clone(),
        transcript_path: root.join("transcript.jsonl"),
        model: Some("claude-haiku-5-5".into()),
        budget_usd: Some(0.05),
        permission_mode: Some("acceptEdits".into()),
        allowed_tools: vec!["Bash(cat:*)".into(), "Bash(git log:*)".into()],
        with_user_mcp: false,
        timeout: Duration::from_secs(180),
        tools: None,
        container: Some(container),
        env: Default::default(),
        session: SessionMode::Ephemeral,
    };
    println!("smoke: running Claude Code (Haiku) in {} ...", d.image);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = ClaudeCode::new("claude").run(&req, tx).await;
    while let Ok(sig) = rx.try_recv() {
        if let Signal::ToolCalled { tool, ok, .. } = sig {
            println!("  {} {tool}", if ok { "✓" } else { "✗" });
        }
    }
    let written = tokio::fs::read_to_string(wt.path.join("written.txt")).await.ok();
    let owner_ok = std::fs::metadata(wt.path.join("written.txt"))
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m) == std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&repo).unwrap()))
        .unwrap_or(false);
    let summary = outcome.summary.clone().unwrap_or_default();
    let _ = wt.remove().await;
    let _ = tokio::fs::remove_dir_all(&root).await;
    match &outcome.status {
        OutcomeStatus::Succeeded => {
            println!("{} agent read the worktree (reply: {})", mark(summary.contains("sandbox-ok-42")), summary.trim());
            println!("{} agent wrote to the worktree, file owned by you", mark(written.is_some() && owner_ok));
            println!("smoke: ${:.4}, {} turns, {}ms", outcome.cost_usd, outcome.num_turns, outcome.duration_ms);
            if !summary.contains("sandbox-ok-42") || written.is_none() || !owner_ok {
                bail!("smoke session ran but did not behave as expected");
            }
        }
        OutcomeStatus::Failed { class, message, .. } => {
            bail!("smoke session failed [{}]: {message}", class.as_str())
        }
    }
    Ok(())
}
