mod bench;
mod github;
mod sandbox;

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;
use wh_api::commands::{self, NewTask, local_user};
use wh_core::{
    RecordedEvent, TaskId,
    task::{TaskEvent, TaskSource},
};
use wh_eventstore::{EventStore, Subscription};
use wh_factory::Factory;
use wh_foreman::{Dispatch, Foreman};
use wh_core::{BenchmarkRunId, ProposalId, SessionId};
use wh_improver::{ImproverConfig, ImproverService, RunResult};
use wh_metrics::{
    benchmarks::BenchmarksProjection,
    proposals::{self, ProposalsProjection},
    scores::{self, ScoreFilter, ScoreResultsProjection},
    sessions::SessionFactsProjection,
    tasks::TasksProjection,
};
use bench::{bench_cmd, run_bench_worker};
use github::{GithubCmd, github_cmd, run_github};
use sandbox::{SandboxCmd, sandbox_cmd};
use wh_scorer::{Graded, ScorerConfig, ScorerService};
use wh_runner::{OutcomeStatus, RunSpec, Runner, RunnerConfig, Signal, claude_code::ClaudeCode};

#[derive(Parser)]
#[command(name = "wh", about = "WorkHub agent factory CLI")]
struct Cli {
    #[arg(long, env = "DATABASE_URL", global = true, hide_env_values = true)]
    database_url: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Database administration
    Db {
        #[command(subcommand)]
        cmd: DbCmd,
    },
    /// Create and list tasks
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Run one agent session on a repo (in a fresh git worktree)
    Run(RunArgs),
    /// Docker sandbox: build the agent image, check the setup, list/clean containers
    Sandbox {
        #[command(subcommand)]
        cmd: SandboxCmd,
    },
    /// Drive factory tasks through the pipeline (plus projections) until Ctrl+C
    Foreman {
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        /// Max stages running at the same time
        #[arg(long, default_value_t = 2)]
        max_parallel: usize,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
        /// Do not run the factory's scorers alongside the pipeline
        #[arg(long)]
        no_scorers: bool,
        /// Do not run the self-improvement agent (requests, triggers, schedule)
        #[arg(long)]
        no_improver: bool,
        /// Do not run queued benchmark runs
        #[arg(long)]
        no_bench: bool,
        /// Do not poll GitHub (PR reviews / CI / merges, issue intake); shipping still happens
        #[arg(long)]
        no_github: bool,
        #[arg(long, env = "WH_GH_BIN", default_value = "gh")]
        gh_bin: PathBuf,
    },
    /// Grade finished agent sessions with the factory's scorers (LLM-as-judge) until Ctrl+C
    Scorer {
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        /// Max gradings running at the same time
        #[arg(long, default_value_t = 2)]
        max_parallel: usize,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
    },
    /// Grade one session now (ignores sampling), e.g. an old session or after a rubric change
    Score {
        #[command(subcommand)]
        cmd: ScoreCmd,
    },
    /// List scorer results, newest first
    Scores {
        #[arg(long)]
        scorer: Option<String>,
        /// Only results that did not pass
        #[arg(long)]
        failing: bool,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Serve the HTTP API + live event stream (and the built UI); runs projections,
    /// and with --foreman also drives the pipeline (everything in one process)
    Serve {
        #[arg(long, env = "WH_ADDR", default_value = "127.0.0.1:7070")]
        addr: std::net::SocketAddr,
        /// Factory for tasks created in the UI (and for --foreman)
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        /// Also run the foreman (pipeline daemon) in this process
        #[arg(long)]
        foreman: bool,
        #[arg(long, default_value_t = 2)]
        max_parallel: usize,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
        /// With --foreman: do not run the factory's scorers
        #[arg(long)]
        no_scorers: bool,
        /// With --foreman: do not run the self-improvement agent
        #[arg(long)]
        no_improver: bool,
        /// With --foreman: do not run queued benchmark runs
        #[arg(long)]
        no_bench: bool,
        /// With --foreman: do not poll GitHub (shipping still happens)
        #[arg(long)]
        no_github: bool,
        #[arg(long, env = "WH_GH_BIN", default_value = "gh")]
        gh_bin: PathBuf,
        /// Directory of benchmark suites offered in the UI
        #[arg(long, env = "WH_BENCH_DIR", default_value = "benchmarks")]
        bench_dir: PathBuf,
        /// Built UI to serve at / (skipped if missing)
        #[arg(long, env = "WH_WEB_DIR", default_value = "web/dist")]
        web_dir: PathBuf,
    },
    /// Run the self-improvement agent once now (or --dry-run to print what it would see)
    Improve {
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        /// Hint for the improver, e.g. "why does code review reject so often?"
        #[arg(long)]
        focus: Option<String>,
        /// Print the observation report only (no agent run, no cost)
        #[arg(long)]
        dry_run: bool,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
    },
    /// Self-improvement daemon: run requested improvements, evaluate triggers and
    /// the schedule, detect rollouts, until Ctrl+C
    Improver {
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
    },
    /// Benchmarks: run a fixed suite of tasks against a factory variant and compare runs
    Bench {
        #[command(subcommand)]
        cmd: BenchCmd,
    },
    /// List improvement proposals, newest first
    Proposals {
        /// requested | running | open | no_proposal | failed | approved | rejected | rolled_out | inbox
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Review a proposal: show, approve, reject, apply
    Proposal {
        #[command(subcommand)]
        cmd: ProposalCmd,
    },
    /// Pipeline metrics per factory_version (before/after a workflow change)
    Versions {
        /// Only tasks of this factory (name)
        #[arg(long)]
        factory: Option<String>,
    },
    /// Approve or reject a task waiting at a human gate
    Gate {
        #[command(subcommand)]
        cmd: GateCmd,
    },
    /// GitHub (factories with `github:`): check the setup, poll once
    Github {
        #[command(subcommand)]
        cmd: GithubCmd,
    },
    /// Inspect a factory definition
    Factory {
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        file: PathBuf,
    },
    /// List recent agent sessions
    Sessions {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Cost / duration / failure statistics per agent
    Stats,
    /// Inspect the raw event log
    Events {
        #[command(subcommand)]
        cmd: EventsCmd,
    },
    /// Run or rebuild read-model projections
    Projections {
        #[command(subcommand)]
        cmd: ProjCmd,
    },
}

#[derive(Subcommand)]
enum DbCmd {
    /// Apply pending migrations
    Migrate,
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Create a task (appends TaskCreated)
    Create {
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        /// Factory file whose pipeline should run this task (picked up by `wh foreman`)
        #[arg(long)]
        factory: Option<PathBuf>,
    },
    /// List tasks from the tasks_view read model
    List,
    /// Show a task's timeline and its agent sessions
    Show { task: TaskId },
    /// Restart a blocked task (optionally at another stage, with guidance for the agent)
    Resume {
        task: TaskId,
        #[arg(long)]
        stage: Option<String>,
        #[arg(long)]
        comment: Option<String>,
    },
    Cancel {
        task: TaskId,
        #[arg(long)]
        reason: String,
    },
    /// Push the task branch and open / update its PR now (retry a failed ship, or
    /// ship a task that finished before `github:` was configured)
    Ship {
        task: TaskId,
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        #[arg(long, env = "WH_GH_BIN", default_value = "gh")]
        gh_bin: PathBuf,
    },
}

#[derive(Subcommand)]
enum ScoreCmd {
    Session {
        session: SessionId,
        /// Only this scorer (default: every scorer targeting the session's agent)
        #[arg(long)]
        scorer: Option<String>,
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
    },
}

#[derive(Subcommand)]
enum ProposalCmd {
    Show { id: ProposalId },
    /// Approve; with --apply also cherry-pick it onto the factory repo right away
    Approve {
        id: ProposalId,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long)]
        apply: bool,
    },
    /// Reject with a comment the improver will read next time
    Reject {
        id: ProposalId,
        #[arg(long)]
        comment: String,
    },
    /// Cherry-pick an approved proposal onto the factory repo's current branch
    Apply { id: ProposalId },
}

#[derive(Subcommand)]
enum BenchCmd {
    /// List (and validate) the suites in a directory
    Suites {
        #[arg(long, env = "WH_BENCH_DIR", default_value = "benchmarks")]
        dir: PathBuf,
    },
    /// Run a suite now, in this process
    Run {
        /// Suite file, or a suite name in --dir
        suite: String,
        #[arg(long, env = "WH_BENCH_DIR", default_value = "benchmarks")]
        dir: PathBuf,
        /// Benchmark the suite's factory as of this git ref (default: the working tree)
        #[arg(long = "ref", conflicts_with = "proposal")]
        git_ref: Option<String>,
        /// Benchmark a proposal: runs its base (parent commit) and its commit, then compares
        #[arg(long)]
        proposal: Option<ProposalId>,
        /// Run every case this many times
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Only this case (repeatable)
        #[arg(long = "case")]
        cases: Vec<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
    },
    /// Benchmark runs, newest first
    Runs {
        #[arg(long)]
        suite: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// One run and its case results
    Show { run: BenchmarkRunId },
    /// Compare two runs case by case (A = baseline, B = candidate)
    Compare { a: BenchmarkRunId, b: BenchmarkRunId },
    /// Run queued benchmark runs (from the UI / API) until Ctrl+C
    Worker {
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
    },
}

#[derive(Subcommand)]
enum GateCmd {
    Approve {
        task: TaskId,
        #[arg(long)]
        comment: Option<String>,
    },
    /// Send the stage back to its agent with your comment as feedback
    Reject {
        task: TaskId,
        #[arg(long)]
        comment: String,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Instructions for the agent
    prompt: String,
    /// Git repository the agent works on
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Commit/branch the worktree starts from
    #[arg(long, default_value = "HEAD")]
    base_ref: String,
    /// Attach to an existing task instead of creating one
    #[arg(long)]
    task: Option<TaskId>,
    #[arg(long, default_value = "implementation")]
    agent: String,
    #[arg(long, default_value = "implement")]
    stage: String,
    #[arg(long)]
    model: Option<String>,
    /// Hard spending cap for the session (USD)
    #[arg(long, default_value_t = 2.0)]
    budget: f64,
    /// Wall-clock limit in minutes
    #[arg(long, default_value_t = 30)]
    timeout_min: u64,
    /// Claude Code permission mode (acceptEdits, auto, bypassPermissions, ...)
    #[arg(long, default_value = "acceptEdits")]
    permission_mode: String,
    /// Extra tools allowed without prompting, e.g. "Bash(cargo test:*)"
    #[arg(long = "allow")]
    allowed_tools: Vec<String>,
    /// Also load your personal MCP servers (costs context tokens)
    #[arg(long)]
    with_user_mcp: bool,
    /// Retries after a retryable failure (timeout / unknown); 2 = at most 3 runs
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
    /// Base wait before a retry (multiplied by the attempt number)
    #[arg(long, default_value_t = 10)]
    retry_backoff_secs: u64,
    /// Remove the worktree after the run (the branch is kept)
    #[arg(long)]
    cleanup: bool,
    #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
    claude_bin: PathBuf,
    /// Run the agent in a Docker container over the worktree (see `wh sandbox`)
    #[arg(long)]
    docker: bool,
    /// Image for --docker
    #[arg(long, default_value = wh_core::DockerSandbox::DEFAULT_IMAGE, requires = "docker")]
    image: String,
    /// Container network for --docker (the agent must reach the Anthropic API)
    #[arg(long, default_value = wh_core::DockerSandbox::DEFAULT_NETWORK, requires = "docker")]
    network: String,
}

#[derive(Subcommand)]
enum EventsCmd {
    /// Print events from the global log
    Tail {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Print every event of one stream, e.g. task-<uuid>
    Stream { stream_id: String },
}

#[derive(Subcommand)]
enum ProjCmd {
    /// Catch projections up; with --follow keep running until Ctrl+C
    Run {
        #[arg(long)]
        follow: bool,
    },
    /// Truncate projections and replay the entire log
    Rebuild,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Die quietly on a closed pipe (`wh task list | head`) instead of panicking.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let url = cli
        .database_url
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .ok_or_else(|| anyhow::anyhow!("DATABASE_URL is not set (see .env.example)"))?;
    // Every following consumer (5 projections, SSE, foreman, scorer, improver, bench) holds a
    // LISTEN connection for its lifetime, so leave plenty of room for queries.
    let pool = PgPoolOptions::new().max_connections(20).connect(&url).await?;
    let store = EventStore::new(pool.clone());

    match cli.cmd {
        Cmd::Db { cmd: DbCmd::Migrate } => {
            wh_eventstore::MIGRATOR.run(&pool).await?;
            println!("migrations applied");
        }
        Cmd::Task { cmd } => match cmd {
            TaskCmd::Create { title, body, factory } => {
                let factory = factory.map(|p| Factory::load(&p)).transpose()?.map(|f| f.name);
                let t = NewTask { title, body, source: TaskSource::Cli, factory };
                println!("{}", commands::create_task(&store, local_user(), t).await?);
            }
            TaskCmd::List => {
                wh_metrics::catch_up_all(&store).await?;
                println!("{:<36}  {:<13}  {:<12}  {:>6}  title", "task", "status", "stage", "rework");
                for t in TasksProjection::list(&pool).await? {
                    println!(
                        "{}  {:<13}  {:<12}  {:>6}  {}{}",
                        t.task_id,
                        t.status,
                        t.current_stage.as_deref().unwrap_or("-"),
                        t.rework_loops,
                        t.title,
                        t.blocked_reason.map(|r| format!("  [blocked: {}]", first_line(&r))).unwrap_or_default()
                            + &t.pr_url.map(|u| format!("  [PR {}: {u}]", t.pr_state.as_deref().unwrap_or("?"))).unwrap_or_default()
                    );
                }
            }
            TaskCmd::Show { task } => show_task(&store, task).await?,
            TaskCmd::Resume { task, stage, comment } => {
                commands::resume(&store, local_user(), task, stage, comment).await?;
                println!("resumed; `wh foreman` will pick it up");
            }
            TaskCmd::Cancel { task, reason } => {
                commands::cancel(&store, local_user(), task, reason).await?;
                println!("cancelled");
            }
            TaskCmd::Ship { task, factory, gh_bin } => {
                let f = Factory::load(&factory)?;
                let ev = wh_github::commands::ship_task(&store, &f, &wh_github::Gh::new(gh_bin), local_user(), task).await?;
                if let TaskEvent::TaskShipped { pr_url, created, .. } = ev {
                    println!("{} {pr_url}", if created { "opened" } else { "pushed to" });
                }
            }
        },
        Cmd::Gate { cmd } => {
            let (task, approved, comment) = match cmd {
                GateCmd::Approve { task, comment } => (task, true, comment),
                GateCmd::Reject { task, comment } => (task, false, Some(comment)),
            };
            let gate = commands::decide_gate(&store, local_user(), task, approved, comment).await?;
            println!("gate '{gate}' {}", if approved { "approved" } else { "rejected" });
        }
        Cmd::Factory { file } => {
            let f = Factory::load(&file)?;
            println!("factory {} (version {})  repo {}  base {}", f.name, f.version, f.repo.display(), f.base_ref);
            println!("max rework loops: {}\n", f.max_rework_loops);
            for st in &f.stages {
                let a = f.agent(st);
                println!(
                    "  {:<12} agent {:<15} model {:<18} budget ${:<5} retries {}  {}{}{}",
                    st.id,
                    a.name,
                    a.model.as_deref().unwrap_or("(default)"),
                    a.budget_usd,
                    a.max_retries,
                    a.sandbox.label(),
                    st.gate.as_ref().map(|g| format!("  gate: {g}")).unwrap_or_default(),
                    st.on_reject.as_ref().map(|t| format!("  on_reject → {t}")).unwrap_or_default(),
                );
            }
            if let Some(im) = &f.improvement {
                println!(
                    "\nimprovement: model {}  budget ${}  window {}d  min sessions {}  schedule {}  max open {}",
                    im.model,
                    im.budget_usd,
                    im.window_days,
                    im.min_sessions,
                    im.interval_hours.map(|h| format!("every {h}h")).unwrap_or_else(|| "off".into()),
                    im.max_open_proposals
                );
                for t in &im.triggers {
                    println!("  trigger {} for {} > {}", t.metric.as_str(), t.subject, t.above);
                }
            }
            if let Some(g) = &f.github {
                println!(
                    "\ngithub: {} via {}  base {}  rework → {}  poll {}s{}{}{}",
                    g.repo.as_deref().unwrap_or("(repo from remote)"),
                    g.remote,
                    g.base.as_deref().unwrap_or("(default branch)"),
                    g.rework_stage,
                    g.poll_interval.as_secs(),
                    if g.draft { "  draft" } else { "" },
                    g.ship_gate.as_ref().map(|s| format!("  ship gate: {s}")).unwrap_or_default(),
                    g.issues.as_ref().map(|i| format!("  issues labelled '{}'", i.label)).unwrap_or_default(),
                );
            }
            if !f.scorers.is_empty() {
                println!("\nscorers:");
                for sc in &f.scorers {
                    println!(
                        "  {:<18} target {:<15} model {:<18} pass ≥ {:<4} sample {:>3.0}%  budget ${}",
                        sc.id,
                        sc.target,
                        sc.model,
                        sc.pass_threshold,
                        sc.sampling_rate * 100.0,
                        sc.budget_usd
                    );
                }
            }
        }
        Cmd::Foreman { factory, max_parallel, claude_bin, no_scorers, no_improver, no_bench, no_github, gh_bin } => {
            let f = Factory::load(&factory)?;
            println!("foreman for factory '{}' (version {}), Ctrl+C to stop", f.name, f.version);
            let scorer = run_scorer(&store, f.clone(), max_parallel, claude_bin.clone(), !no_scorers);
            let improver = run_improver(&store, f.clone(), claude_bin.clone(), !no_improver);
            let bench = run_bench_worker(&store, claude_bin.clone(), !no_bench);
            let gh = run_github(&store, f.clone(), gh_bin.clone(), !no_github);
            let (a, b, c, d, e, g) = tokio::join!(
                run_foreman(&store, f, max_parallel, claude_bin, gh_bin),
                run_projections(&store),
                scorer,
                improver,
                bench,
                gh
            );
            a.and(b).and(c).and(d).and(e).and(g)?;
        }
        Cmd::Bench { cmd } => bench_cmd(&store, cmd).await?,
        Cmd::Github { cmd } => github_cmd(&store, cmd).await?,
        Cmd::Improver { factory, claude_bin } => {
            let f = Factory::load(&factory)?;
            anyhow::ensure!(f.improvement.is_some(), "factory '{}' has no `improvement:` section", f.name);
            println!("improver for factory '{}' (version {}), Ctrl+C to stop", f.name, f.version);
            let (a, b) = tokio::join!(run_improver(&store, f, claude_bin, true), run_projections(&store));
            a?;
            b?;
        }
        Cmd::Improve { factory, focus, dry_run, claude_bin } => {
            let f = Factory::load(&factory)?;
            wh_metrics::catch_up_all(&store).await?;
            let cfg = ImproverConfig { data_dir: data_dir() };
            let svc = ImproverService::new(store.clone(), f, ClaudeCode::new(claude_bin), cfg, wh_improver::Dispatch::Inline)?;
            if dry_run {
                println!("{}", wh_improver::render(&svc.observe().await?));
                return Ok(());
            }
            println!("running the improver for factory '{}' (version {})…", svc.factory().name, svc.factory().version);
            let (id, r) = svc.run_now(local_user(), focus).await?;
            match r {
                RunResult::Proposed { title, files, factory_version, cost_usd } => println!(
                    "proposal {id}: {title}\n  files: {}\n  → factory version {factory_version}  (${cost_usd:.4})\n\
                     review it: wh proposal show {id}",
                    files.join(", ")
                ),
                RunResult::NoProposal { reason, cost_usd } => println!("no proposal: {reason}  (${cost_usd:.4})"),
                RunResult::Failed { message, cost_usd } => println!("improver failed: {message}  (${cost_usd:.4})"),
                RunResult::NotPending => println!("{id} was already handled"),
            }
        }
        Cmd::Proposals { status, limit } => {
            wh_metrics::catch_up_all(&store).await?;
            println!("{:<36}  {:<11}  {:>8}  {:<20}  title / note", "proposal", "status", "cost$", "trigger");
            for p in proposals::proposals(&pool, None, status.as_deref(), limit).await? {
                println!(
                    "{}  {:<11}  {:>8.4}  {:<20}  {}",
                    p.proposal_id,
                    p.status,
                    p.cost_usd,
                    truncate(&p.trigger, 20),
                    p.title.as_deref().or(p.message.as_deref()).map(first_line).unwrap_or("-"),
                );
            }
        }
        Cmd::Proposal { cmd } => match cmd {
            ProposalCmd::Show { id } => show_proposal(&store, id).await?,
            ProposalCmd::Approve { id, comment, apply } => {
                wh_improver::commands::decide(&store, local_user(), id, true, comment).await?;
                println!("approved");
                if apply {
                    let v = wh_improver::commands::apply(&store, local_user(), id).await?;
                    println!("applied: factory version is now {v}; restart the foreman to use it");
                } else {
                    println!("roll it out with: wh proposal apply {id}");
                }
            }
            ProposalCmd::Reject { id, comment } => {
                wh_improver::commands::decide(&store, local_user(), id, false, Some(comment)).await?;
                println!("rejected");
            }
            ProposalCmd::Apply { id } => {
                let v = wh_improver::commands::apply(&store, local_user(), id).await?;
                println!("applied: factory version is now {v}; restart the foreman to use it");
            }
        },
        Cmd::Versions { factory } => {
            wh_metrics::catch_up_all(&store).await?;
            println!(
                "{:<12}  {:>5}  {:>8}  {:>6}  {:>7}  {:>7}  {:>8}  {:>8}  {:>9}  {:>11}  first seen",
                "version", "tasks", "sessions", "fail%", "reject%", "retries", "avg$", "p50", "tool err%", "scorer pass"
            );
            let pct = |x: Option<f64>| x.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "-".into());
            for v in proposals::version_stats(&pool, factory.as_deref()).await? {
                println!(
                    "{:<12}  {:>5}  {:>8}  {:>6}  {:>7}  {:>7}  {:>8.4}  {:>8}  {:>9}  {:>11}  {}",
                    v.factory_version,
                    v.tasks,
                    v.sessions,
                    pct(v.fail_rate),
                    pct(v.reject_rate),
                    v.retries,
                    v.avg_cost_usd.unwrap_or(0.0),
                    v.p50_duration_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
                    pct(v.tool_error_rate),
                    if v.scored > 0 { format!("{} n={}", pct(v.score_pass_rate), v.scored) } else { "-".into() },
                    v.first_at.format("%Y-%m-%d %H:%M"),
                );
            }
        }
        Cmd::Scorer { factory, max_parallel, claude_bin } => {
            let f = Factory::load(&factory)?;
            anyhow::ensure!(!f.scorers.is_empty(), "factory '{}' defines no scorers", f.name);
            println!("scorers for factory '{}' (version {}), Ctrl+C to stop", f.name, f.version);
            let (a, b) = tokio::join!(run_scorer(&store, f, max_parallel, claude_bin, true), run_projections(&store));
            a?;
            b?;
        }
        Cmd::Score { cmd: ScoreCmd::Session { session, scorer, factory, claude_bin } } => {
            let f = Factory::load(&factory)?;
            let cfg = ScorerConfig { data_dir: data_dir() };
            let svc = ScorerService::new(store.clone(), f, ClaudeCode::new(claude_bin), cfg, wh_scorer::Dispatch::Inline);
            for (id, g) in svc.grade_session(session, scorer.as_deref()).await? {
                match g {
                    Graded::Scored { score, pass, classification, rationale, cost_usd } => println!(
                        "{id}: {} score {score:.2}{}  (${cost_usd:.4})\n  {rationale}",
                        if pass { "PASS" } else { "FAIL" },
                        classification.map(|c| format!(" [{c}]")).unwrap_or_default()
                    ),
                    Graded::Failed { message, cost_usd } => println!("{id}: ERROR {message}  (${cost_usd:.4})"),
                    Graded::Duplicate => println!("{id}: already graded"),
                }
            }
            wh_metrics::catch_up_all(&store).await?;
        }
        Cmd::Scores { scorer, failing, limit } => {
            wh_metrics::catch_up_all(&store).await?;
            println!(
                "{:<36}  {:<18}  {:<15}  {:<7}  {:>5}  {:<22}  {:>8}  rationale",
                "session", "scorer", "agent", "result", "score", "classification", "cost$"
            );
            for r in scores::scores(&pool, &ScoreFilter { scorer, failing, ..Default::default() }, limit).await? {
                let result = match (r.status.as_str(), r.pass) {
                    ("scored", Some(true)) => "pass",
                    ("scored", _) => "FAIL",
                    ("failed", _) => "error",
                    _ => "running",
                };
                println!(
                    "{}  {:<18}  {:<15}  {:<7}  {:>5}  {:<22}  {:>8.4}  {}",
                    r.target_session,
                    r.scorer,
                    r.agent,
                    result,
                    r.score.map(|s| format!("{s:.2}")).unwrap_or_else(|| "-".into()),
                    r.classification.as_deref().unwrap_or("-"),
                    r.cost_usd,
                    first_line(r.rationale.as_deref().or(r.error.as_deref()).unwrap_or("")),
                );
            }
        }
        Cmd::Serve {
            addr,
            factory,
            foreman,
            max_parallel,
            claude_bin,
            no_scorers,
            no_improver,
            no_bench,
            no_github,
            gh_bin,
            bench_dir,
            web_dir,
        } => {
            let f = match Factory::load(&factory) {
                Ok(f) => Some(f),
                Err(e) if !foreman => {
                    eprintln!("warning: no factory ({e:#}); UI tasks will not run through a pipeline");
                    None
                }
                Err(e) => return Err(e),
            };
            let web_dir = web_dir.join("index.html").is_file().then_some(web_dir);
            match &web_dir {
                Some(d) => println!("serving UI from {}", d.display()),
                None => println!("no built UI found (run `npm run build` in web/); API only"),
            }
            let state = wh_api::AppState {
                store: store.clone(),
                factory: f.clone().map(std::sync::Arc::new),
                live: wh_api::Live::start(store.clone()).await?,
                bench_dir: std::fs::canonicalize(&bench_dir).unwrap_or(bench_dir),
            };
            println!("WorkHub on http://{addr}  (Ctrl+C to stop)");
            let api = wh_api::serve(addr, state, web_dir, ctrl_c());
            let pipeline = async {
                match (foreman, f) {
                    (true, Some(f)) => {
                        println!("foreman for factory '{}' (version {})", f.name, f.version);
                        let scorer = run_scorer(&store, f.clone(), max_parallel, claude_bin.clone(), !no_scorers);
                        let improver = run_improver(&store, f.clone(), claude_bin.clone(), !no_improver);
                        let bench = run_bench_worker(&store, claude_bin.clone(), !no_bench);
                        let gh = run_github(&store, f.clone(), gh_bin.clone(), !no_github);
                        let (a, b, c, d, e) = tokio::join!(
                            run_foreman(&store, f, max_parallel, claude_bin, gh_bin),
                            scorer,
                            improver,
                            bench,
                            gh
                        );
                        a.and(b).and(c).and(d).and(e)
                    }
                    _ => Ok(()),
                }
            };
            let (a, b, c) = tokio::join!(api, pipeline, run_projections(&store));
            a?;
            b?;
            c?;
        }
        Cmd::Run(args) => run_agent(&store, args).await?,
        Cmd::Sandbox { cmd } => sandbox_cmd(&store, cmd).await?,
        Cmd::Sessions { limit } => {
            println!(
                "{:<36}  {:<14}  {:>3}  {:<9}  {:<15}  {:>8}  {:>5}  {:>9}  {:>8}",
                "session", "agent", "try", "status", "failure", "cost$", "turns", "tools/err", "time"
            );
            for r in SessionFactsProjection::recent(&pool, limit).await? {
                println!(
                    "{:<36}  {:<14}  {:>3}  {:<9}  {:<15}  {:>8.4}  {:>5}  {:>9}  {:>8}",
                    r.session_id,
                    r.agent,
                    r.attempt,
                    r.status,
                    r.failure_class.as_deref().unwrap_or("-"),
                    r.cost_usd,
                    r.num_turns.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
                    format!("{}/{}", r.tool_calls, r.tool_errors),
                    r.duration_ms.map(|d| fmt_ms(d as f64)).unwrap_or_else(|| "-".into()),
                );
            }
        }
        Cmd::Stats => {
            println!(
                "{:<14}  {:>5}  {:>7}  {:>6}  {:>10}  {:>9}  {:>8}  {:>8}  {:>6}  {:>9}",
                "agent", "runs", "retries", "fail%", "total$", "avg$", "p50", "p95", "turns", "tool err%"
            );
            for s in SessionFactsProjection::stats_by_agent(&pool).await? {
                let tool_err = if s.tool_calls == 0 { 0.0 } else { s.tool_errors as f64 / s.tool_calls as f64 };
                println!(
                    "{:<14}  {:>5}  {:>7}  {:>5.1}%  {:>10.4}  {:>9.4}  {:>8}  {:>8}  {:>6.1}  {:>8.1}%",
                    s.agent,
                    s.runs,
                    s.retries,
                    s.failure_rate() * 100.0,
                    s.total_cost_usd,
                    s.avg_cost_usd,
                    s.p50_duration_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
                    s.p95_duration_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
                    s.avg_turns.unwrap_or(0.0),
                    tool_err * 100.0,
                );
            }
            let scorer_stats = scores::scorer_stats(&pool).await?;
            if !scorer_stats.is_empty() {
                println!(
                    "\n{:<18}  {:<15}  {:>6}  {:>6}  {:>7}  {:>6}  {:>9}",
                    "scorer", "target", "scored", "pass%", "pass%7d", "errors", "cost$"
                );
                let pct = |x: Option<f64>| x.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "-".into());
                for s in scorer_stats {
                    println!(
                        "{:<18}  {:<15}  {:>6}  {:>6}  {:>7}  {:>6}  {:>9.4}",
                        s.scorer,
                        s.agent,
                        s.scored,
                        pct(s.pass_rate),
                        pct(s.pass_rate_7d),
                        s.errors,
                        s.cost_usd
                    );
                }
            }
            let failures = SessionFactsProjection::failures_by_class(&pool).await?;
            if !failures.is_empty() {
                println!("\nfailures by class:");
                for (class, n) in failures {
                    println!("  {class:<16} {n}");
                }
            }
        }
        Cmd::Events { cmd } => match cmd {
            EventsCmd::Tail { after, limit } => store.read_all(after, limit).await?.iter().for_each(print_event),
            EventsCmd::Stream { stream_id } => store.read_stream(&stream_id).await?.iter().for_each(print_event),
        },
        Cmd::Projections { cmd } => match cmd {
            ProjCmd::Run { follow: false } => {
                for (name, n) in wh_metrics::catch_up_all(&store).await? {
                    println!("{name}: {n} events applied");
                }
            }
            ProjCmd::Run { follow: true } => {
                println!("following events, Ctrl+C to stop");
                run_projections(&store).await?;
            }
            ProjCmd::Rebuild => {
                for (name, n) in wh_metrics::rebuild_all(&store).await? {
                    println!("{name}: rebuilt from {n} events");
                }
            }
        },
    }
    Ok(())
}

pub(crate) async fn ctrl_c() {
    tokio::signal::ctrl_c().await.ok();
}

/// Foreman daemon until Ctrl+C (recovers interrupted stages first).
async fn run_foreman(store: &EventStore, f: Factory, max_parallel: usize, claude_bin: PathBuf, gh_bin: PathBuf) -> Result<()> {
    let runner = Runner::new(store.clone(), ClaudeCode::new(claude_bin), RunnerConfig { data_dir: data_dir() });
    let dispatch = Dispatch::Spawn(std::sync::Arc::new(tokio::sync::Semaphore::new(max_parallel)));
    let mut foreman = Foreman::new(store.clone(), f, runner, dispatch).with_gh(wh_github::Gh::new(gh_bin));
    for task in foreman.recover_interrupted().await? {
        println!("task {task} was interrupted mid-stage → blocked. Restart it with: wh task resume {task}");
    }
    Subscription::new(store.clone()).run(&mut foreman, ctrl_c()).await
}

/// Scorer consumer until Ctrl+C. New consumers start at the head of the log, so
/// enabling scorers does not grade every past session.
async fn run_scorer(store: &EventStore, f: Factory, max_parallel: usize, claude_bin: PathBuf, enabled: bool) -> Result<()> {
    if !enabled || f.scorers.is_empty() {
        return Ok(());
    }
    let ids: Vec<_> = f.scorers.iter().map(|s| format!("{} → {}", s.id, s.target)).collect();
    println!("scorers: {}", ids.join(", "));
    let dispatch = wh_scorer::Dispatch::Spawn(std::sync::Arc::new(tokio::sync::Semaphore::new(max_parallel)));
    let cfg = ScorerConfig { data_dir: data_dir() };
    let mut svc = ScorerService::new(store.clone(), f, ClaudeCode::new(claude_bin), cfg, dispatch);
    let n = svc.recover_interrupted().await?;
    if n > 0 {
        println!("{n} interrupted grading(s) marked failed");
    }
    if svc.start_from_head_if_new().await? {
        println!("scorers start from now; grade older sessions with `wh score session <id>`");
    }
    Subscription::new(store.clone()).run(&mut svc, ctrl_c()).await
}

/// Improver until Ctrl+C: recover, then run requests as they arrive and evaluate
/// triggers / schedule every few minutes.
async fn run_improver(store: &EventStore, f: Factory, claude_bin: PathBuf, enabled: bool) -> Result<()> {
    if !enabled || f.improvement.is_none() {
        return Ok(());
    }
    let im = f.improvement.as_ref().expect("checked");
    let mut how = vec!["on request".to_owned()];
    if !im.triggers.is_empty() {
        how.push(format!("{} trigger(s)", im.triggers.len()));
    }
    if let Some(h) = im.interval_hours {
        how.push(format!("every {h}h with new sessions"));
    }
    println!("improver: {} (model {})", how.join(", "), im.model);
    // One improvement at a time: each one reads the outcome of the previous.
    let dispatch = wh_improver::Dispatch::Spawn(std::sync::Arc::new(tokio::sync::Semaphore::new(1)));
    let cfg = ImproverConfig { data_dir: data_dir() };
    let mut svc = ImproverService::new(store.clone(), f, ClaudeCode::new(claude_bin), cfg, dispatch)?;
    let (failed, pending) = svc.recover().await?;
    if failed > 0 {
        println!("{failed} interrupted improver run(s) marked failed");
    }
    wh_metrics::catch_up_all(store).await?;
    for id in svc.detect_rollouts().await? {
        println!("proposal {id} is live in factory version {}", svc.factory().version);
    }
    for id in pending {
        svc.dispatch(id).await;
    }
    let ticker = svc.clone();
    let ticks = async move {
        let mut every = tokio::time::interval(Duration::from_secs(5 * 60));
        let stop = ctrl_c();
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = &mut stop => return Ok::<(), anyhow::Error>(()),
                _ = every.tick() => match ticker.tick().await {
                    Ok(reqs) => reqs.iter().for_each(|(id, why)| println!("improver requested {id}: {why}")),
                    Err(e) => eprintln!("improver tick failed: {e:#}"),
                },
            }
        }
    };
    let sub = Subscription::new(store.clone());
    let (a, b) = tokio::join!(sub.run(&mut svc, ctrl_c()), ticks);
    a.and(b)
}

async fn show_proposal(store: &EventStore, id: ProposalId) -> Result<()> {
    wh_metrics::catch_up_all(store).await?;
    let pool = store.pool();
    let p = proposals::proposal(pool, id.0).await?.context("proposal not found")?;
    println!("{}  [{}]", p.title.as_deref().unwrap_or("(no proposal)"), p.status);
    println!(
        "trigger: {}  requested by {} {}  cost ${:.4}",
        p.trigger,
        p.requested_by,
        p.requested_at.format("%Y-%m-%d %H:%M"),
        p.cost_usd
    );
    if let Some(f) = &p.focus {
        println!("focus: {f}");
    }
    if let Some(v) = &p.base_factory_version {
        println!(
            "factory version: {v} → {}{}",
            p.proposed_factory_version.as_deref().unwrap_or("-"),
            p.branch.as_deref().map(|b| format!("  (branch {b})")).unwrap_or_default()
        );
    }
    for (label, text) in [
        ("rationale", &p.rationale),
        ("expected impact", &p.expected_impact),
        ("note", &p.message),
        ("reviewer", &p.decision_comment),
    ] {
        if let Some(t) = text {
            println!("\n{label}:\n  {}", t.replace('\n', "\n  "));
        }
    }
    if let Some(ev) = p.evidence.as_array().filter(|a| !a.is_empty()) {
        let ids: Vec<_> = ev.iter().filter_map(|v| v.as_str()).collect();
        println!("\nevidence sessions: {}", ids.join(", "));
    }
    if let Some(diff) = &p.diff {
        println!("\n{diff}");
    }
    let versions = proposals::version_stats(pool, Some(&p.factory)).await?;
    let find = |v: &Option<String>| versions.iter().find(|x| Some(&x.factory_version) == v.as_ref());
    if let (Some(b), Some(a)) = (find(&p.base_factory_version), find(&p.rolled_out_version)) {
        let pct = |x: Option<f64>| x.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "-".into());
        println!("\nbefore → after ({} → {}):", b.factory_version, a.factory_version);
        println!("  sessions     {} → {}", b.sessions, a.sessions);
        println!("  fail rate    {} → {}", pct(b.fail_rate), pct(a.fail_rate));
        println!("  reject rate  {} → {}", pct(b.reject_rate), pct(a.reject_rate));
        println!("  avg cost     ${:.4} → ${:.4}", b.avg_cost_usd.unwrap_or(0.0), a.avg_cost_usd.unwrap_or(0.0));
        println!("  scorer pass  {} → {}", pct(b.score_pass_rate), pct(a.score_pass_rate));
    }
    match p.status.as_str() {
        "open" => println!("\napprove: wh proposal approve {id} [--apply]   reject: wh proposal reject {id} --comment ..."),
        "approved" => println!("\nroll out: wh proposal apply {id}"),
        _ => {}
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i.saturating_sub(1)]),
        None => s.to_owned(),
    }
}

/// Keep every projection following the log until Ctrl+C.
async fn run_projections(store: &EventStore) -> Result<()> {
    let pool = store.pool().clone();
    let sub = || Subscription::new(store.clone());
    let (sa, sb, sc, sd, se) = (sub(), sub(), sub(), sub(), sub());
    let mut tasks = TasksProjection::new(pool.clone());
    let mut sessions = SessionFactsProjection::new(pool.clone());
    let mut scores = ScoreResultsProjection::new(pool.clone());
    let mut proposals = ProposalsProjection::new(pool.clone());
    let mut benchmarks = BenchmarksProjection::new(pool);
    let (a, b, c, d, e) = tokio::join!(
        sa.run(&mut tasks, ctrl_c()),
        sb.run(&mut sessions, ctrl_c()),
        sc.run(&mut scores, ctrl_c()),
        sd.run(&mut proposals, ctrl_c()),
        se.run(&mut benchmarks, ctrl_c())
    );
    a.and(b).and(c).and(d).and(e)
}

async fn show_task(store: &EventStore, task: TaskId) -> Result<()> {
    let state = commands::load_task(store, task).await?;
    println!("{}  [{:?}]  rework loops: {}", state.title, state.status, state.rework_loops);
    if let Some(ext) = &state.external_ref {
        println!("from issue {}#{}  {}", ext.repo, ext.issue, ext.url);
    }
    if let Some(pr) = &state.pr {
        println!("PR #{} {}  ({} PR rework loop(s))", pr.number, pr.url, state.pr_rework_loops);
    }
    for e in store.read_stream(&task.stream_id()).await? {
        let detail = match e.decode::<TaskEvent>()? {
            TaskEvent::StageEntered { stage, feedback } => {
                format!("{stage}{}", feedback.map(|f| format!(" (rework: {})", first_line(&f))).unwrap_or_default())
            }
            TaskEvent::StageExited { stage, outcome, summary, .. } => format!(
                "{stage} → {}{}",
                outcome.as_str(),
                summary.map(|s| format!(": {}", first_line(&s))).unwrap_or_default()
            ),
            TaskEvent::GateRequested { gate, stage } => format!("{gate} after {stage}"),
            TaskEvent::GateDecided { gate, approved, reviewer, comment, .. } => format!(
                "{gate} {} by {reviewer}{}",
                if approved { "approved" } else { "rejected" },
                comment.map(|c| format!(": {c}")).unwrap_or_default()
            ),
            TaskEvent::TaskBlocked { reason, .. } => first_line(&reason).to_string(),
            TaskEvent::TaskCompleted { branch } => branch.unwrap_or_default(),
            TaskEvent::TaskShipped { pr_url, created, commit, .. } => {
                format!("{} {pr_url} @ {}", if created { "opened" } else { "pushed" }, &commit[..commit.len().min(10)])
            }
            TaskEvent::PrReviewed { reviewer, state, body, comments, .. } => format!(
                "{state} by {reviewer}{}{}",
                if body.trim().is_empty() { String::new() } else { format!(": {}", first_line(&body)) },
                if comments.is_empty() { String::new() } else { format!(" (+{} inline)", comments.len()) }
            ),
            TaskEvent::PrChecksCompleted { conclusion, failed, .. } => {
                let names: Vec<_> = failed.iter().map(|c| c.name.as_str()).collect();
                if names.is_empty() { conclusion } else { format!("{conclusion}: {}", names.join(", ")) }
            }
            TaskEvent::PrMerged { merged_by, .. } => merged_by.map(|b| format!("by {b}")).unwrap_or_default(),
            _ => String::new(),
        };
        println!("  {}  {:<17} {detail}", e.meta.occurred_at.format("%H:%M:%S"), e.event_type);
    }
    let pool = store.pool();
    wh_metrics::catch_up_all(store).await?;
    let sessions: Vec<_> =
        SessionFactsProjection::recent(pool, 1000).await?.into_iter().filter(|s| s.task_id == task.0).collect();
    if !sessions.is_empty() {
        let total: f64 = sessions.iter().map(|s| s.cost_usd).sum();
        println!("\nsessions ({}, total ${total:.4}):", sessions.len());
        for s in sessions.iter().rev() {
            println!(
                "  {:<15} try {}  {:<9}  ${:.4}  {}",
                s.agent,
                s.attempt,
                s.status,
                s.cost_usd,
                s.duration_ms.map(|d| fmt_ms(d as f64)).unwrap_or_else(|| "-".into())
            );
        }
    }
    Ok(())
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
}

async fn run_agent(store: &EventStore, a: RunArgs) -> Result<()> {
    let repo = std::fs::canonicalize(&a.repo).with_context(|| format!("repo not found: {}", a.repo.display()))?;
    let task_id = match a.task {
        Some(t) => t,
        None => {
            let title: String = a.prompt.chars().take(80).collect();
            let t = NewTask { title, body: Some(a.prompt.clone()), source: TaskSource::Cli, factory: None };
            commands::create_task(store, local_user(), t).await?
        }
    };
    let spec = RunSpec {
        task_id,
        agent: a.agent,
        stage: a.stage,
        prompt: a.prompt,
        repo,
        base_ref: a.base_ref,
        model: a.model,
        budget_usd: Some(a.budget),
        permission_mode: Some(a.permission_mode),
        allowed_tools: a.allowed_tools,
        with_user_mcp: a.with_user_mcp,
        timeout: Duration::from_secs(a.timeout_min * 60),
        keep_worktree: !a.cleanup,
        max_retries: a.max_retries,
        retry_backoff: Duration::from_secs(a.retry_backoff_secs),
        branch: None,
        commit_on_success: false,
        verdict_required: false,
        task_expected_version: None,
        feedback: None,
        factory_version: None,
        benchmark: None,
        sandbox: if a.docker {
            let mut d = wh_core::DockerSandbox::new(a.image);
            d.network = a.network;
            wh_core::Sandbox::Docker(d)
        } else {
            wh_core::Sandbox::Worktree
        },
    };
    println!("task {task_id}");
    let runner = Runner::new(store.clone(), ClaudeCode::new(a.claude_bin), RunnerConfig { data_dir: data_dir() });
    let report = runner
        .run(spec, |sig| match sig {
            Signal::Ready { model, .. } => println!("  agent ready (model: {})", model.as_deref().unwrap_or("?")),
            Signal::ToolCalled { tool, ok, duration_ms } => {
                println!("  {} {tool} ({})", if *ok { "✓" } else { "✗" }, fmt_ms(*duration_ms as f64))
            }
        })
        .await?;
    wh_metrics::catch_up_all(store).await?;

    for a in &report.attempts {
        let status = match &a.outcome.status {
            OutcomeStatus::Succeeded => "succeeded".to_string(),
            OutcomeStatus::Failed { class, message, .. } => format!("FAILED [{}] {message}", class.as_str()),
        };
        println!("attempt {} · session {} · ${:.4} · {}", a.attempt, a.session_id, a.outcome.cost_usd, status);
    }
    let last = report.last();
    let o = &last.outcome;
    println!(
        "total cost ${:.4} over {} attempt(s) · last: {} turns · {} · tokens in {} / out {} / cache read {}",
        report.total_cost_usd(),
        report.attempts.len(),
        o.num_turns,
        fmt_ms(o.duration_ms as f64),
        o.usage.input_tokens,
        o.usage.output_tokens,
        o.usage.cache_read_tokens
    );
    if let Some(dir) = &last.workdir {
        println!("worktree: {}", dir.display());
    }
    if let Some(branch) = &last.branch {
        println!("branch:   {branch}");
    }
    if let Some(summary) = &o.summary {
        println!("\n{summary}");
    }
    Ok(())
}

/// `$WH_DATA_DIR`, else `$XDG_DATA_HOME/workhub`, else `~/.local/share/workhub`.
pub(crate) fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("WH_DATA_DIR") {
        return d.into();
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share"));
    base.join("workhub")
}

fn fmt_ms(ms: f64) -> String {
    let s = ms / 1000.0;
    if s < 60.0 { format!("{s:.1}s") } else { format!("{}m{:02}s", (s / 60.0) as u64, (s % 60.0) as u64) }
}

fn print_event(e: &RecordedEvent) {
    println!(
        "#{:<6} {}  {}@{}  {}  {}",
        e.global_seq,
        e.meta.occurred_at.format("%Y-%m-%d %H:%M:%S"),
        e.stream_id,
        e.stream_version,
        e.event_type,
        e.payload
    );
}
