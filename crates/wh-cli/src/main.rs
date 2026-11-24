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
use wh_metrics::{sessions::SessionFactsProjection, tasks::TasksProjection};
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
    /// Drive factory tasks through the pipeline (plus projections) until Ctrl+C
    Foreman {
        #[arg(long, env = "WH_FACTORY", default_value = "factory/factory.yaml")]
        factory: PathBuf,
        /// Max stages running at the same time
        #[arg(long, default_value_t = 2)]
        max_parallel: usize,
        #[arg(long, env = "WH_CLAUDE_BIN", default_value = "claude")]
        claude_bin: PathBuf,
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
        /// Built UI to serve at / (skipped if missing)
        #[arg(long, env = "WH_WEB_DIR", default_value = "web/dist")]
        web_dir: PathBuf,
    },
    /// Approve or reject a task waiting at a human gate
    Gate {
        #[command(subcommand)]
        cmd: GateCmd,
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
    let pool = PgPoolOptions::new().max_connections(5).connect(&url).await?;
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
                    "  {:<12} agent {:<15} model {:<18} budget ${:<5} retries {}{}{}",
                    st.id,
                    a.name,
                    a.model.as_deref().unwrap_or("(default)"),
                    a.budget_usd,
                    a.max_retries,
                    st.gate.as_ref().map(|g| format!("  gate: {g}")).unwrap_or_default(),
                    st.on_reject.as_ref().map(|t| format!("  on_reject → {t}")).unwrap_or_default(),
                );
            }
        }
        Cmd::Foreman { factory, max_parallel, claude_bin } => {
            let f = Factory::load(&factory)?;
            println!("foreman for factory '{}' (version {}), Ctrl+C to stop", f.name, f.version);
            let (a, b) = tokio::join!(run_foreman(&store, f, max_parallel, claude_bin), run_projections(&store));
            a?;
            b?;
        }
        Cmd::Serve { addr, factory, foreman, max_parallel, claude_bin, web_dir } => {
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
            };
            println!("WorkHub on http://{addr}  (Ctrl+C to stop)");
            let api = wh_api::serve(addr, state, web_dir, ctrl_c());
            let pipeline = async {
                match (foreman, f) {
                    (true, Some(f)) => {
                        println!("foreman for factory '{}' (version {})", f.name, f.version);
                        run_foreman(&store, f, max_parallel, claude_bin).await
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

async fn ctrl_c() {
    tokio::signal::ctrl_c().await.ok();
}

/// Foreman daemon until Ctrl+C (recovers interrupted stages first).
async fn run_foreman(store: &EventStore, f: Factory, max_parallel: usize, claude_bin: PathBuf) -> Result<()> {
    let runner = Runner::new(store.clone(), ClaudeCode::new(claude_bin), RunnerConfig { data_dir: data_dir() });
    let dispatch = Dispatch::Spawn(std::sync::Arc::new(tokio::sync::Semaphore::new(max_parallel)));
    let mut foreman = Foreman::new(store.clone(), f, runner, dispatch);
    for task in foreman.recover_interrupted().await? {
        println!("task {task} was interrupted mid-stage → blocked. Restart it with: wh task resume {task}");
    }
    Subscription::new(store.clone()).run(&mut foreman, ctrl_c()).await
}

/// Keep every projection following the log until Ctrl+C.
async fn run_projections(store: &EventStore) -> Result<()> {
    let pool = store.pool().clone();
    let (sa, sb) = (Subscription::new(store.clone()), Subscription::new(store.clone()));
    let (mut tasks, mut sessions) = (TasksProjection::new(pool.clone()), SessionFactsProjection::new(pool));
    let (a, b) = tokio::join!(sa.run(&mut tasks, ctrl_c()), sb.run(&mut sessions, ctrl_c()));
    a?;
    b
}

async fn show_task(store: &EventStore, task: TaskId) -> Result<()> {
    let state = commands::load_task(store, task).await?;
    println!("{}  [{:?}]  rework loops: {}", state.title, state.status, state.rework_loops);
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
            _ => String::new(),
        };
        println!("  {}  {:<14} {detail}", e.meta.occurred_at.format("%H:%M:%S"), e.event_type);
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
fn data_dir() -> PathBuf {
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
