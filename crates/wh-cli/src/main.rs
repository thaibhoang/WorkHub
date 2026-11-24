use anyhow::Result;
use clap::{Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;
use wh_core::{
    Actor, EventMeta, NewEvent, RecordedEvent, TaskId,
    task::{TaskEvent, TaskSource},
};
use wh_eventstore::{EventStore, ExpectedVersion, Subscription};
use wh_metrics::tasks::TasksProjection;

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
    },
    /// List tasks from the tasks_view read model
    List,
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
            TaskCmd::Create { title, body } => {
                let id = TaskId::new();
                let meta = EventMeta::new(id.0, current_user());
                let ev = TaskEvent::TaskCreated { title, body, source: TaskSource::Cli };
                store
                    .append(&id.stream_id(), ExpectedVersion::NoStream, vec![NewEvent::from_domain(&ev, meta)?])
                    .await?;
                // Keep the read model fresh for immediate `task list`.
                Subscription::new(store.clone()).catch_up(&mut TasksProjection::new(pool.clone())).await?;
                println!("{id}");
            }
            TaskCmd::List => {
                for t in TasksProjection::list(&pool).await? {
                    println!(
                        "{}  {:<11}  {:<10}  {}  ({})",
                        t.task_id,
                        t.status,
                        t.current_stage.as_deref().unwrap_or("-"),
                        t.title,
                        t.created_at.format("%Y-%m-%d %H:%M")
                    );
                }
            }
        },
        Cmd::Events { cmd } => match cmd {
            EventsCmd::Tail { after, limit } => store.read_all(after, limit).await?.iter().for_each(print_event),
            EventsCmd::Stream { stream_id } => store.read_stream(&stream_id).await?.iter().for_each(print_event),
        },
        Cmd::Projections { cmd } => {
            let mut tasks = TasksProjection::new(pool.clone());
            match cmd {
                ProjCmd::Run { follow: false } => {
                    let n = Subscription::new(store.clone()).catch_up(&mut tasks).await?;
                    println!("{}: {n} events applied", TasksProjection::NAME);
                }
                ProjCmd::Run { follow: true } => {
                    println!("following events, Ctrl+C to stop");
                    Subscription::new(store.clone())
                        .run(&mut tasks, async {
                            tokio::signal::ctrl_c().await.ok();
                        })
                        .await?;
                }
                ProjCmd::Rebuild => {
                    let n = wh_metrics::rebuild(&store, &mut tasks).await?;
                    println!("{}: rebuilt from {n} events", TasksProjection::NAME);
                }
            }
        }
    }
    Ok(())
}

fn current_user() -> Actor {
    Actor::Human {
        user: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
    }
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
