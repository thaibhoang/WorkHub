use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tokio::process::Command;

/// Cap on the conflict hunks quoted in the prompt (every file is always listed).
const MAX_HUNKS: usize = 20_000;

const IDENTITY: [&str; 4] = ["-c", "user.name=WorkHub", "-c", "user.email=workhub@localhost"];

/// A file git could not merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflicted {
    pub path: String,
    pub kind: &'static str,
}

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

    /// Reopen a worktree kept from an earlier session. Errors if the directory is
    /// gone or is not a worktree on `branch`.
    pub async fn open(repo: &Path, path: &Path, branch: &str) -> anyhow::Result<Self> {
        let real = tokio::fs::canonicalize(path).await.with_context(|| format!("worktree {} is gone", path.display()))?;
        // Compare the top level too: a plain directory inside some repo is not a worktree.
        let top = git(path, &["rev-parse", "--show-toplevel"]).await?;
        if tokio::fs::canonicalize(&top).await? != real {
            bail!("{} is not a git worktree", path.display());
        }
        let head = git(path, &["symbolic-ref", "--short", "HEAD"]).await?;
        if head != branch {
            bail!("worktree {} is on {head}, not {branch}", path.display());
        }
        Ok(Self { repo: repo.to_owned(), path: path.to_owned(), branch: branch.to_owned() })
    }

    /// Stage and commit everything the agent changed. Returns the new commit sha,
    /// or `None` when there was nothing to commit.
    pub async fn commit_all(&self, message: &str) -> anyhow::Result<Option<String>> {
        git(&self.path, &["add", "-A"]).await?;
        // A merge must be committed even if its resolution equals our side.
        let merging = git(&self.path, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).await.is_ok();
        if !merging && git(&self.path, &["status", "--porcelain"]).await?.is_empty() {
            return Ok(None);
        }
        git(&self.path, &[&IDENTITY[..], &["commit", "-q", "-m", message]].concat()).await?;
        Ok(Some(git(&self.path, &["rev-parse", "HEAD"]).await?))
    }

    /// Fetch `<remote> <base>` and merge it without committing. Returns the
    /// conflicted files (empty = merged cleanly, or already up to date).
    pub async fn merge(&self, remote: &str, base: &str) -> anyhow::Result<Vec<Conflicted>> {
        git(&self.path, &["fetch", "--quiet", remote, base]).await?;
        let args = [&IDENTITY[..], &["merge", "--no-ff", "--no-commit", "FETCH_HEAD"]].concat();
        let merged = git(&self.path, &args).await;
        let unmerged = git(&self.path, &["diff", "--name-only", "--diff-filter=U", "-z"]).await?;
        let mut files = Vec::new();
        for path in unmerged.split('\0').filter(|p| !p.is_empty()) {
            // Index stages present: 1 = common ancestor, 2 = ours, 3 = theirs.
            let index = git(&self.path, &["ls-files", "-u", "--", path]).await?;
            let has = |n: &str| index.lines().any(|l| l.split_whitespace().nth(2) == Some(n));
            let kind = match (has("1"), has("2"), has("3")) {
                (true, true, true) => "both modified",
                (false, true, true) => "both added",
                (true, false, true) => "deleted by us",
                (true, true, false) => "deleted by them",
                _ => "unmerged",
            };
            files.push(Conflicted { path: path.to_owned(), kind });
        }
        match merged {
            Err(e) if files.is_empty() => Err(e),
            _ => Ok(files),
        }
    }

    /// What the agent is told about the merge, including the conflict hunks.
    pub async fn merge_note(&self, base: &str, files: &[Conflicted]) -> String {
        if files.is_empty() {
            return format!(
                "## Merge of `{base}`\nWorkHub merged `{base}` into this branch without textual conflicts. \
                 Check that the combined code works and fix what does not.\n"
            );
        }
        let mut s = format!(
            "## Merge conflicts with `{base}`\nWorkHub started merging `{base}` into this branch. Resolve these \
             files and remove every conflict marker (WorkHub commits the merge):\n"
        );
        for f in files {
            s.push_str(&format!("- {} ({})\n", f.path, f.kind));
        }
        let mut left = MAX_HUNKS;
        for f in files {
            let Ok(bytes) = tokio::fs::read(self.path.join(&f.path)).await else { continue };
            let hunks = conflict_hunks(&String::from_utf8_lossy(&bytes));
            if hunks.is_empty() || left == 0 {
                continue;
            }
            let cut: String = hunks.chars().take(left).collect();
            left -= cut.chars().count();
            let more = if cut.len() < hunks.len() { "\n[truncated: read the file]" } else { "" };
            s.push_str(&format!("\n### {}\n```\n{}{more}\n```\n", f.path, cut.trim_end()));
        }
        s
    }

    /// Which of `files` still contain conflict markers.
    pub async fn with_markers(&self, files: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for f in files {
            let Ok(bytes) = tokio::fs::read(self.path.join(f)).await else { continue };
            if String::from_utf8_lossy(&bytes).lines().any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> ")) {
                out.push(f.clone());
            }
        }
        out
    }

    /// Remove the worktree directory. The branch is kept so any commits the agent made survive.
    pub async fn remove(&self) -> anyhow::Result<()> {
        let path_str = self.path.to_str().context("worktree path is not valid UTF-8")?;
        git(&self.repo, &["worktree", "remove", "--force", path_str]).await?;
        Ok(())
    }
}

/// The `<<<<<<<` … `>>>>>>>` blocks of a file, each with its starting line number.
fn conflict_hunks(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for (n, line) in text.lines().enumerate() {
        if line.starts_with("<<<<<<< ") {
            inside = true;
            out.push_str(&format!("(line {})\n", n + 1));
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
        if line.starts_with(">>>>>>> ") {
            inside = false;
        }
    }
    out
}
