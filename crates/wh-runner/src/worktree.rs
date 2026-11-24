use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tokio::process::Command;

/// A git worktree used as the sandbox for one agent session.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
}

async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .context("failed to run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

impl Worktree {
    /// `git worktree add -b <branch> <path> <base_ref>` inside `repo`.
    pub async fn create(repo: &Path, base_ref: &str, path: &Path, branch: &str) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let path_str = path.to_str().context("worktree path is not valid UTF-8")?;
        git(repo, &["worktree", "add", "-b", branch, path_str, base_ref]).await?;
        Ok(Self { repo: repo.to_owned(), path: path.to_owned(), branch: branch.to_owned() })
    }

    /// Remove the worktree directory. The branch is kept so any commits the agent made survive.
    pub async fn remove(&self) -> anyhow::Result<()> {
        let path_str = self.path.to_str().context("worktree path is not valid UTF-8")?;
        git(&self.repo, &["worktree", "remove", "--force", path_str]).await?;
        Ok(())
    }
}
