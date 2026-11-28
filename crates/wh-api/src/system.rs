//! Self-update for a local `wh serve`: pull the repository the server was built
//! from, rebuild the server and the UI, migrate, wait for running stages to
//! finish ([`wh_core::Drain`]), then replace the process with the new binary
//! (`exec`: same pid, same arguments, same working directory).
//!
//! Only offered without `--auth` (production updates with `deploy/deploy.sh`),
//! and only when the running binary lives in `<repo>/target/`.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use wh_core::Drain;

/// Changes on every start; the UI reloads when it sees a new one.
pub static BOOT_ID: LazyLock<String> = LazyLock::new(|| uuid::Uuid::now_v7().to_string());

/// Kept per step, so a long build log does not grow without bound.
const OUTPUT_TAIL: usize = 48 * 1024;

#[derive(Clone)]
pub enum SelfUpdate {
    Enabled(Arc<Updater>),
    /// Why the UI offers no update button.
    Disabled(String),
}

impl Default for SelfUpdate {
    fn default() -> Self {
        Self::Disabled("self-update is not enabled for this server".into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub id: &'static str,
    pub label: &'static str,
    pub status: StepStatus,
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Failed,
    Restarting,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub state: JobState,
    pub steps: Vec<Step>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// Commit before and after the pull.
    pub from: Option<String>,
    pub to: Option<String>,
    /// Restart without waiting for running stages.
    pub force: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
}

/// What an update would bring in (`git fetch` first).
#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheck {
    pub branch: String,
    pub expected_branch: String,
    pub dirty: bool,
    pub head: String,
    pub commits: Vec<Commit>,
    /// Why an update cannot run now, if it cannot.
    pub blocked: Option<String>,
}

pub struct Updater {
    repo: PathBuf,
    exe: PathBuf,
    args: Vec<OsString>,
    release: bool,
    remote: String,
    branch: String,
    drain: Arc<Drain>,
    /// Commit the repository was at when this process started.
    pub started_commit: String,
    job: Mutex<Option<Job>>,
}

const STEPS: [(&str, &str); 7] = [
    ("check", "Check the repository"),
    ("pull", "Pull the latest code"),
    ("server", "Build the server"),
    ("web", "Build the UI"),
    ("migrate", "Migrate the database"),
    ("drain", "Wait for running stages"),
    ("restart", "Restart"),
];

impl Updater {
    /// Self-update for the binary running now, built from `repo`, following
    /// `remote`/`branch`. `Err` says why it cannot be offered.
    pub fn detect(repo: &Path, remote: &str, branch: &str, drain: Arc<Drain>) -> Result<Self, String> {
        let repo = std::fs::canonicalize(repo).map_err(|e| format!("{}: {e}", repo.display()))?;
        if !repo.join(".git").exists() {
            return Err(format!("{} is not a git checkout", repo.display()));
        }
        let exe = std::env::current_exe().and_then(std::fs::canonicalize).map_err(|e| format!("current executable: {e}"))?;
        if !exe.starts_with(repo.join("target")) {
            return Err(format!("the running binary ({}) was not built in {}", exe.display(), repo.display()));
        }
        let release = exe.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "release");
        let started_commit = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .ok_or_else(|| format!("git rev-parse HEAD failed in {}", repo.display()))?;
        Ok(Self {
            repo,
            exe,
            args: std::env::args_os().skip(1).collect(),
            release,
            remote: remote.into(),
            branch: branch.into(),
            drain,
            started_commit,
            job: Mutex::new(None),
        })
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn job(&self) -> Option<Job> {
        self.job.lock().unwrap().clone()
    }

    /// Stage runs (and foreman events) still in flight.
    pub fn busy(&self) -> usize {
        self.drain.busy()
    }

    /// Fetch and report what an update would pull, and whether it can run.
    pub async fn check(&self) -> anyhow::Result<UpdateCheck> {
        let branch = git(&self.repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        let dirty = !git(&self.repo, &["status", "--porcelain", "--untracked-files=no"]).await?.is_empty();
        let head = git(&self.repo, &["rev-parse", "HEAD"]).await?;
        git(&self.repo, &["fetch", "--quiet", &self.remote, &self.branch]).await?;
        let range = format!("HEAD..{}/{}", self.remote, self.branch);
        let log = git(&self.repo, &["log", "--format=%h%x09%s", &range]).await?;
        let commits = log
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(sha, subject)| Commit { sha: sha.into(), subject: subject.into() })
            .collect();
        let blocked = self.blocked(&branch, dirty);
        Ok(UpdateCheck { branch, expected_branch: self.branch.clone(), dirty, head, commits, blocked })
    }

    fn blocked(&self, branch: &str, dirty: bool) -> Option<String> {
        if branch != self.branch {
            Some(format!(
                "the repository is on branch '{branch}', not '{}': switch to it (git switch {}) to update from the UI",
                self.branch, self.branch
            ))
        } else if dirty {
            Some("the repository has uncommitted changes: commit or stash them first".into())
        } else {
            None
        }
    }

    /// Start an update in the background. While one waits for running stages,
    /// `force` makes it restart right away; otherwise a running update is an error.
    pub fn start(self: &Arc<Self>, force: bool) -> Result<(), String> {
        let mut job = self.job.lock().unwrap();
        if let Some(j) = job.as_mut().filter(|j| j.state != JobState::Failed) {
            if force && j.state == JobState::Running {
                j.force = true;
                return Ok(());
            }
            return Err("an update is already running".into());
        }
        *job = Some(Job {
            state: JobState::Running,
            steps: STEPS
                .iter()
                .map(|&(id, label)| Step { id, label, status: StepStatus::Pending, output: String::new() })
                .collect(),
            started_at: Utc::now(),
            finished_at: None,
            error: None,
            from: None,
            to: None,
            force,
        });
        drop(job);
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.run().await {
                tracing::warn!("self-update failed: {e}");
                this.drain.cancel();
                this.update(|j| {
                    j.state = JobState::Failed;
                    j.error = Some(e);
                    j.finished_at = Some(Utc::now());
                    for s in &mut j.steps {
                        if s.status == StepStatus::Running {
                            s.status = StepStatus::Failed;
                        }
                    }
                });
            }
        });
        Ok(())
    }

    fn update(&self, f: impl FnOnce(&mut Job)) {
        if let Some(j) = self.job.lock().unwrap().as_mut() {
            f(j);
        }
    }

    fn step(&self, id: &str, status: StepStatus) {
        self.update(|j| {
            if let Some(s) = j.steps.iter_mut().find(|s| s.id == id) {
                s.status = status;
            }
        });
    }

    fn say(&self, id: &str, line: &str) {
        self.update(|j| {
            if let Some(s) = j.steps.iter_mut().find(|s| s.id == id) {
                push_tail(&mut s.output, line);
            }
        });
    }

    async fn run(&self) -> Result<(), String> {
        let repo = self.repo.clone();
        let err = |e: anyhow::Error| format!("{e:#}");

        self.step("check", StepStatus::Running);
        let branch = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await.map_err(err)?;
        let dirty = !git(&repo, &["status", "--porcelain", "--untracked-files=no"]).await.map_err(err)?.is_empty();
        if let Some(why) = self.blocked(&branch, dirty) {
            self.say("check", &why);
            return Err(why);
        }
        let from = git(&repo, &["rev-parse", "HEAD"]).await.map_err(err)?;
        self.say("check", &format!("on {branch} at {}, clean", &from[..from.len().min(12)]));
        self.step("check", StepStatus::Done);

        self.step("pull", StepStatus::Running);
        self.cmd("pull", "git", &["fetch", &self.remote, &self.branch], &repo).await?;
        let target = format!("{}/{}", self.remote, self.branch);
        self.cmd("pull", "git", &["merge", "--ff-only", &target], &repo).await?;
        let to = git(&repo, &["rev-parse", "HEAD"]).await.map_err(err)?;
        self.update(|j| {
            j.from = Some(from.clone());
            j.to = Some(to.clone());
        });
        self.step("pull", StepStatus::Done);

        self.step("server", StepStatus::Running);
        let mut build = vec!["build", "--bin", "wh"];
        if self.release {
            build.push("--release");
        }
        self.cmd("server", "cargo", &build, &repo).await?;
        self.step("server", StepStatus::Done);

        let web = repo.join("web");
        if web.join("package.json").is_file() {
            self.step("web", StepStatus::Running);
            let lock_changed = from != to && git(&repo, &["diff", "--quiet", &from, &to, "--", "web/package-lock.json", "web/package.json"]).await.is_err();
            if lock_changed || !web.join("node_modules").is_dir() {
                self.cmd("web", "npm", &["install", "--no-audit", "--no-fund"], &web).await?;
            }
            self.cmd("web", "npm", &["run", "build"], &web).await?;
            self.step("web", StepStatus::Done);
        } else {
            self.step("web", StepStatus::Skipped);
        }

        self.step("migrate", StepStatus::Running);
        let exe = self.exe.display().to_string();
        self.cmd("migrate", &exe, &["db", "migrate"], &repo).await?;
        self.step("migrate", StepStatus::Done);

        self.step("drain", StepStatus::Running);
        self.drain.start();
        let mut last = usize::MAX;
        loop {
            let busy = self.drain.busy();
            let force = self.job().is_some_and(|j| j.force);
            if busy == 0 || force {
                self.say("drain", &if busy == 0 { "nothing running".to_string() } else { format!("restarting now; {busy} run(s) interrupted") });
                break;
            }
            if busy != last {
                self.say("drain", &format!("{busy} stage run(s) still running; new work is on hold"));
                last = busy;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.step("drain", StepStatus::Done);

        self.step("restart", StepStatus::Running);
        self.update(|j| j.state = JobState::Restarting);
        self.say("restart", &format!("exec {exe}"));
        // Let the UI read the last status before the server goes away.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Err(format!("restart failed: {}", self.exec()))
    }

    #[cfg(unix)]
    fn exec(&self) -> std::io::Error {
        use std::os::unix::process::CommandExt;
        tracing::info!(exe = %self.exe.display(), "self-update: restarting");
        std::process::Command::new(&self.exe).args(&self.args).exec()
    }

    #[cfg(not(unix))]
    fn exec(&self) -> std::io::Error {
        std::io::Error::other("restarting in place needs unix")
    }

    /// Run a command, streaming its output into the step.
    async fn cmd(&self, step: &str, program: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
        self.say(step, &format!("$ {program} {}", args.join(" ")));
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env("CARGO_TERM_COLOR", "never")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                let msg = format!("{program}: {e}");
                self.say(step, &msg);
                msg
            })?;
        let (out, err) = (child.stdout.take(), child.stderr.take());
        tokio::join!(self.pump(step, out), self.pump(step, err));
        let status = child.wait().await.map_err(|e| format!("{program}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("`{program} {}` failed ({status})", args.join(" ")))
        }
    }

    async fn pump(&self, step: &str, r: Option<impl AsyncRead + Unpin>) {
        let Some(r) = r else { return };
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            self.say(step, &line);
        }
    }
}

fn push_tail(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
    if out.len() > OUTPUT_TAIL {
        let mut cut = out.len() - OUTPUT_TAIL;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out.drain(..cut);
    }
}

async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git").args(args).current_dir(repo).output().await?;
    if !out.status.success() {
        anyhow::bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_keeps_the_tail_on_a_char_boundary() {
        let mut s = String::new();
        for i in 0..20_000 {
            push_tail(&mut s, &format!("dòng {i}"));
        }
        assert!(s.len() <= OUTPUT_TAIL);
        assert!(s.ends_with("dòng 19999\n"));
    }
}
