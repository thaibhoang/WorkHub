//! Thin wrappers around the `gh` and `git` command lines. Every call has a
//! timeout, and failures carry a [`FailureClass`] so a blocked task says why.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde::de::DeserializeOwned;
use tokio::{io::AsyncWriteExt, process::Command};
use wh_core::FailureClass;

const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct CliError {
    pub class: FailureClass,
    pub message: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.class.as_str(), self.message)
    }
}

impl std::error::Error for CliError {}

pub type CliResult<T> = Result<T, CliError>;

fn err(class: FailureClass, message: impl Into<String>) -> CliError {
    CliError { class, message: message.into() }
}

async fn exec(bin: &Path, cwd: Option<&Path>, args: &[&str], stdin: Option<&str>) -> CliResult<String> {
    let name = bin.display().to_string();
    let mut cmd = Command::new(bin);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    // Never wait for a human at a prompt: gh must fail instead of asking.
    cmd.env("GH_PROMPT_DISABLED", "1").env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => err(FailureClass::EnvSetup, format!("`{name}` not found on PATH")),
        _ => err(FailureClass::EnvSetup, format!("cannot run `{name}`: {e}")),
    })?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).await.map_err(|e| err(FailureClass::Infra, format!("{name} stdin: {e}")))?;
    }
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| err(FailureClass::Timeout, format!("`{name} {}` timed out", args.join(" "))))?
        .map_err(|e| err(FailureClass::Infra, format!("{name}: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = format!("`{name} {}` failed: {}", short(args), stderr.trim());
        let lower = stderr.to_ascii_lowercase();
        let class = if lower.contains("auth login") || lower.contains("not logged") || lower.contains("authentication") {
            FailureClass::EnvSetup
        } else {
            FailureClass::Infra
        };
        return Err(err(class, msg));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Command line for an error message (long bodies elided).
fn short(args: &[&str]) -> String {
    args.iter().map(|a| if a.len() > 60 { "…" } else { a }).collect::<Vec<_>>().join(" ")
}

/// The GitHub CLI.
#[derive(Debug, Clone)]
pub struct Gh {
    pub bin: PathBuf,
}

impl Default for Gh {
    fn default() -> Self {
        Self { bin: "gh".into() }
    }
}

impl Gh {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }

    pub async fn run(&self, cwd: Option<&Path>, args: &[&str]) -> CliResult<String> {
        exec(&self.bin, cwd, args, None).await
    }

    pub async fn run_stdin(&self, cwd: Option<&Path>, args: &[&str], stdin: &str) -> CliResult<String> {
        exec(&self.bin, cwd, args, Some(stdin)).await
    }

    pub async fn json<T: DeserializeOwned>(&self, cwd: Option<&Path>, args: &[&str]) -> CliResult<T> {
        let out = self.run(cwd, args).await?;
        serde_json::from_str(&out)
            .map_err(|e| err(FailureClass::Infra, format!("`gh {}`: unexpected output ({e}): {}", short(args), head(&out))))
    }
}

fn head(s: &str) -> String {
    s.chars().take(200).collect()
}

pub async fn git(repo: &Path, args: &[&str]) -> CliResult<String> {
    let mut full = vec!["-C", repo.to_str().unwrap_or(".")];
    full.extend_from_slice(args);
    Ok(exec(Path::new("git"), None, &full, None).await?.trim().to_owned())
}
