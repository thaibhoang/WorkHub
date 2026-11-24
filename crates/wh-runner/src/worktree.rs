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
    /// Check out `branch` into a new worktree at `path`, creating the branch from
    /// `base_ref` if it does not exist yet (so successive stages share a task branch).
    pub async fn create(repo: &Path, base_ref: &str, path: &Path, branch: &str) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let path_str = path.to_str().context("worktree path is not valid UTF-8")?;
        git(repo, &["worktree", "prune"]).await?;
        let exists = git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).await.is_ok();
        if exists {
            git(repo, &["worktree", "add", path_str, branch]).await?;
        } else {
            git(repo, &["worktree", "add", "-b", branch, path_str, base_ref]).await?;
        }
        Ok(Self { repo: repo.to_owned(), path: path.to_owned(), branch: branch.to_owned() })
    }

    /// Stage and commit everything the agent changed. Returns the new commit sha,
    /// or `None` when there was nothing to commit.
    pub async fn commit_all(&self, message: &str) -> anyhow::Result<Option<String>> {
        git(&self.path, &["add", "-A"]).await?;
        if git(&self.path, &["status", "--porcelain"]).await?.is_empty() {
            return Ok(None);
        }
        git(&self.path, &["-c", "user.name=WorkHub", "-c", "user.email=workhub@localhost", "commit", "-q", "-m", message])
            .await?;
        Ok(Some(git(&self.path, &["rev-parse", "HEAD"]).await?))
    }

    /// Remove the worktree directory. The branch is kept so any commits the agent made survive.
    pub async fn remove(&self) -> anyhow::Result<()> {
        let path_str = self.path.to_str().context("worktree path is not valid UTF-8")?;
        git(&self.repo, &["worktree", "remove", "--force", path_str]).await?;
        Ok(())
    }
}
