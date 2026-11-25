//! `wh bench ...`: benchmark suites, runs and comparisons.

use std::{path::{Path, PathBuf}, sync::Arc};

use anyhow::{Context, Result};
use wh_api::commands::local_user;
use wh_bench::{BenchConfig, BenchService, Request, RunResult, suite};
use wh_core::{BenchmarkRunId, benchmark::BenchmarkEvent};
use wh_eventstore::{EventStore, Subscription};
use wh_metrics::benchmarks;
use wh_runner::claude_code::ClaudeCode;

use super::{BenchCmd, ctrl_c, data_dir, first_line, fmt_ms};

/// A suite file path, or the name of a suite in `dir`.
fn resolve_suite(arg: &str, dir: &Path) -> Result<PathBuf> {
    let p = PathBuf::from(arg);
    if p.is_file() {
        return Ok(p);
    }
    for ext in ["yaml", "yml"] {
        let c = dir.join(format!("{arg}.{ext}"));
        if c.is_file() {
            return Ok(c);
        }
    }
    anyhow::bail!("no suite '{arg}' (not a file, and not in {})", dir.display())
}

fn service(store: &EventStore, claude_bin: PathBuf) -> BenchService<ClaudeCode> {
    let cfg = BenchConfig { data_dir: data_dir() };
    BenchService::new(store.clone(), move || ClaudeCode::new(claude_bin.clone()), cfg, wh_bench::Dispatch::Inline)
}

fn print_progress(ev: &BenchmarkEvent) {
    match ev {
        BenchmarkEvent::BenchmarkCaseStarted { case, repeat, task_id } => println!("  ▶ {case}#{repeat}  task {task_id}"),
        BenchmarkEvent::BenchmarkCaseFinished { case, repeat, passed, task_status, cost_usd, duration_ms, message, .. } => {
            println!(
                "  {} {case}#{repeat}  {task_status}  ${cost_usd:.4}  {}{}",
                if *passed { "✓" } else { "✗" },
                fmt_ms(*duration_ms as f64),
                message.as_deref().map(|m| format!("  {}", first_line(m))).unwrap_or_default()
            )
        }
        _ => {}
    }
}

fn pct(x: Option<f64>) -> String {
    x.map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|| "-".into())
}

fn variant(r: &benchmarks::BenchRun) -> String {
    let mut v = r.factory_version.clone().unwrap_or_else(|| "?".into());
    if let Some(l) = &r.label {
        v = format!("{l} {v}");
    }
    match &r.git_ref {
        Some(g) => format!("{v} @{g}"),
        None => format!("{v} (working tree)"),
    }
}

pub async fn bench_cmd(store: &EventStore, cmd: BenchCmd) -> Result<()> {
    let pool = store.pool();
    match cmd {
        BenchCmd::Suites { dir } => {
            let found = suite::discover(&dir);
            if found.is_empty() {
                println!("no suites in {}", dir.display());
            }
            for (path, s) in found {
                match s {
                    Ok(s) => println!(
                        "{:<16} {:>2} case(s)  version {}  factory {}{}",
                        s.name,
                        s.cases.len(),
                        s.version,
                        s.factory_file.display(),
                        s.description.map(|d| format!("\n                 {d}")).unwrap_or_default()
                    ),
                    Err(e) => println!("{}: INVALID {e:#}", path.display()),
                }
            }
        }
        BenchCmd::Run { suite, dir, git_ref, proposal, repeat, cases, label, claude_bin } => {
            let suite_file = resolve_suite(&suite, &dir)?;
            let svc = service(store, claude_bin).with_progress(Arc::new(print_progress));
            let mut variants = Vec::new();
            match proposal {
                Some(p) => {
                    let (base, candidate) = wh_bench::commands::proposal_refs(store, p).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                    variants.push((Some(base), Some("base".to_owned())));
                    variants.push((Some(candidate), Some("proposal".to_owned())));
                }
                None => variants.push((git_ref, label)),
            }
            let mut ids = Vec::new();
            for (git_ref, label) in variants {
                let req = Request { suite_file: suite_file.clone(), git_ref: git_ref.clone(), proposal, label: label.clone(), repeat, cases: cases.clone() };
                println!(
                    "benchmark {}{}{}",
                    suite_file.display(),
                    label.map(|l| format!("  [{l}]")).unwrap_or_default(),
                    git_ref.map(|g| format!("  @{g}")).unwrap_or_default()
                );
                let (id, r) = svc.run_now(local_user(), &req).await?;
                match r {
                    RunResult::Completed { cases, passed, cost_usd, aborted } => println!(
                        "run {id}: {passed}/{cases} passed  ${cost_usd:.4}{}\n",
                        aborted.map(|a| format!("  (stopped: {a})")).unwrap_or_default()
                    ),
                    RunResult::Failed { message } => println!("run {id} failed: {message}\n"),
                    RunResult::NotPending => println!("run {id} was taken by another process\n"),
                }
                ids.push(id);
            }
            if let [a, b] = ids[..] {
                compare(store, a, b).await?;
            }
        }
        BenchCmd::Runs { suite, limit } => {
            wh_metrics::catch_up_all(store).await?;
            println!(
                "{:<36}  {:<12}  {:<10}  {:>7}  {:>6}  {:>9}  {:>9}  variant",
                "run", "suite", "status", "passed", "pass%", "cost$", "p50 case"
            );
            for r in benchmarks::runs(pool, suite.as_deref(), None, limit).await? {
                println!(
                    "{}  {:<12}  {:<10}  {:>7}  {:>6}  {:>9.4}  {:>9}  {}",
                    r.run_id,
                    r.suite,
                    r.status,
                    format!("{}/{}", r.passed, r.planned.unwrap_or(r.cases_done)),
                    pct(r.pass_rate),
                    r.cost_usd,
                    r.p50_case_duration_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
                    variant(&r),
                );
            }
        }
        BenchCmd::Show { run } => {
            wh_metrics::catch_up_all(store).await?;
            let r = benchmarks::run(pool, run.0).await?.context("benchmark run not found")?;
            println!("{} [{}]  suite {} (version {})", r.run_id, r.status, r.suite, r.suite_version.as_deref().unwrap_or("-"));
            println!("variant: {}  factory {}", variant(&r), r.factory.as_deref().unwrap_or("-"));
            println!(
                "passed {}/{} ({})  cost ${:.4}  avg/case ${:.4}  p50 case {}",
                r.passed,
                r.cases_done,
                pct(r.pass_rate),
                r.cost_usd,
                r.avg_case_cost_usd.unwrap_or(0.0),
                r.p50_case_duration_ms.map(fmt_ms).unwrap_or_else(|| "-".into())
            );
            if let Some(m) = &r.message {
                println!("note: {m}");
            }
            println!("\n{:<20}  {:>3}  {:<9}  {:<6}  {:>8}  {:>8}  {:>6}  task / message", "case", "#", "status", "result", "cost$", "time", "rework");
            for c in benchmarks::cases(pool, run.0).await? {
                println!(
                    "{:<20}  {:>3}  {:<9}  {:<6}  {:>8.4}  {:>8}  {:>6}  {}{}",
                    c.case_id,
                    c.repeat,
                    c.status,
                    match c.passed {
                        Some(true) => "pass",
                        Some(false) => "FAIL",
                        None => "-",
                    },
                    c.cost_usd,
                    c.duration_ms.map(|d| fmt_ms(d as f64)).unwrap_or_else(|| "-".into()),
                    c.rework_loops.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
                    c.task_id,
                    c.message.as_deref().map(|m| format!("  {}", first_line(m))).unwrap_or_default()
                );
            }
        }
        BenchCmd::Compare { a, b } => {
            wh_metrics::catch_up_all(store).await?;
            compare(store, a, b).await?;
        }
        BenchCmd::Worker { claude_bin } => {
            println!("benchmark worker, Ctrl+C to stop");
            let (a, b) = tokio::join!(run_bench_worker(store, claude_bin, true), super::run_projections(store));
            a.and(b)?;
        }
    }
    Ok(())
}

async fn compare(store: &EventStore, a: BenchmarkRunId, b: BenchmarkRunId) -> Result<()> {
    let c = benchmarks::compare(store.pool(), a.0, b.0).await?.context("benchmark run not found")?;
    println!("A {}  {}", c.a.run_id, variant(&c.a));
    println!("B {}  {}", c.b.run_id, variant(&c.b));
    if !c.comparable {
        println!("warning: the runs used different suite files/versions; numbers are not directly comparable");
    }
    let rate = |s: &Option<benchmarks::CaseStats>| {
        s.as_ref().map(|s| format!("{}/{}", s.passed, s.runs)).unwrap_or_else(|| "-".into())
    };
    let cost = |s: &Option<benchmarks::CaseStats>| {
        s.as_ref().and_then(|s| s.avg_cost_usd).map(|c| format!("{c:.4}")).unwrap_or_else(|| "-".into())
    };
    let time = |s: &Option<benchmarks::CaseStats>| {
        s.as_ref().and_then(|s| s.avg_duration_ms).map(fmt_ms).unwrap_or_else(|| "-".into())
    };
    println!("\n{:<20}  {:>7}  {:>7}  {:>9}  {:>9}  {:>8}  {:>8}", "case", "A pass", "B pass", "A avg$", "B avg$", "A time", "B time");
    for row in &c.cases {
        println!(
            "{:<20}  {:>7}  {:>7}  {:>9}  {:>9}  {:>8}  {:>8}",
            row.case_id,
            rate(&row.a),
            rate(&row.b),
            cost(&row.a),
            cost(&row.b),
            time(&row.a),
            time(&row.b)
        );
    }
    println!(
        "{:<20}  {:>7}  {:>7}  {:>9.4}  {:>9.4}  {:>8}  {:>8}",
        "total",
        pct(c.a.pass_rate),
        pct(c.b.pass_rate),
        c.a.avg_case_cost_usd.unwrap_or(0.0),
        c.b.avg_case_cost_usd.unwrap_or(0.0),
        c.a.p50_case_duration_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
        c.b.p50_case_duration_ms.map(fmt_ms).unwrap_or_else(|| "-".into()),
    );
    Ok(())
}

/// Benchmark worker until Ctrl+C: recover, then run queued requests one at a time.
pub async fn run_bench_worker(store: &EventStore, claude_bin: PathBuf, enabled: bool) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let cfg = BenchConfig { data_dir: data_dir() };
    // One run at a time: runs are long and expensive, and parallel runs would skew durations.
    let dispatch = wh_bench::Dispatch::Spawn(Arc::new(tokio::sync::Semaphore::new(1)));
    let mut svc = BenchService::new(store.clone(), move || ClaudeCode::new(claude_bin.clone()), cfg, dispatch);
    let (failed, pending) = svc.recover().await?;
    if failed > 0 {
        println!("{failed} interrupted benchmark run(s) marked failed");
    }
    println!("benchmarks: running queued runs ({} pending)", pending.len());
    for id in pending {
        svc.dispatch(id).await;
    }
    Subscription::new(store.clone()).run(&mut svc, ctrl_c()).await
}
