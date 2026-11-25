//! Docker sandbox: runs the agent process inside a container that only sees the
//! session's worktree (same path as on the host), the git metadata it needs, a
//! throwaway home directory and the volumes the factory declares.
//!
//! The git worktree is still created and committed on the host, so the
//! pipeline (shared task branch, commit on success) works unchanged. Inside the
//! container the repository's object store and refs are mounted read-only:
//! the agent can read history and diff, but only WorkHub commits.

use std::{
    ffi::OsString,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail, ensure};
use tokio::process::Command;
use wh_core::{DockerSandbox, SessionId};

/// Auth variables always passed through (by name) when set on the host.
pub const AUTH_ENV: &[&str] = &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"];
/// Home directory inside the container (a per-session host dir mounted read-write).
pub const CONTAINER_HOME: &str = "/home/wh";
/// Label put on every agent container (`workhub.session=<id>` identifies the session).
pub const LABEL: &str = "workhub.session";

/// A fully resolved container launch for one session.
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub name: String,
    pub session: SessionId,
    pub config: DockerSandbox,
    /// Working directory (same path inside and outside).
    pub workdir: PathBuf,
    /// Bind mounts computed by [`prepare`] (worktree, git dirs, home), then `config.volumes`.
    pub mounts: Vec<String>,
    /// `uid:gid` of the worktree owner, so files the agent writes stay owned by the user.
    pub user: Option<String>,
    /// Host directory mounted as the container's home; removed after the session.
    pub home_dir: PathBuf,
}

pub fn container_name(session: SessionId) -> String {
    format!("wh-{session}")
}

/// Arguments for `docker` that run `inner` (the agent command, starting with the
/// binary name inside the image) in the container. Pure, so it is unit-tested.
pub fn run_args(c: &Container, inner: &[OsString]) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |s: &str| a.push(s.into());
    for s in ["run", "--rm", "--init", "--pull", "never", "--name", &c.name] {
        push(s);
    }
    push("--label");
    push(&format!("{LABEL}={}", c.session));
    push("--network");
    push(&c.config.network);
    push("--cap-drop");
    push("ALL");
    push("--security-opt");
    push("no-new-privileges");
    if let Some(cpus) = c.config.cpus {
        push("--cpus");
        push(&cpus.to_string());
    }
    if let Some(m) = &c.config.memory {
        push("--memory");
        push(m);
    }
    if let Some(p) = c.config.pids_limit {
        push("--pids-limit");
        push(&p.to_string());
    }
    if let Some(u) = &c.user {
        push("--user");
        push(u);
    }
    for m in c.mounts.iter().chain(&c.config.volumes) {
        push("--volume");
        push(m);
    }
    push("--env");
    push(&format!("HOME={CONTAINER_HOME}"));
    // Bind mounts make docker create missing parent dirs as root (a repo under
    // /tmp/claude-<uid>/... yields a root-owned /tmp/claude-<uid>, which Claude
    // Code refuses as its temp dir), so keep temp files in the session's home.
    for var in ["TMPDIR", "CLAUDE_CODE_TMPDIR"] {
        push("--env");
        push(&format!("{var}={CONTAINER_HOME}/.tmp"));
    }
    let extra = c.config.env.iter().map(String::as_str).filter(|n| !AUTH_ENV.contains(n));
    for name in AUTH_ENV.iter().copied().chain(extra) {
        // `-e NAME` without a value: docker copies it from its own environment
        // when set, so secrets never show up in the command line.
        push("--env");
        push(name);
    }
    push("--workdir");
    a.push(c.workdir.clone().into_os_string());
    a.push(c.config.image.clone().into());
    a.extend(inner.iter().cloned());
    a
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().await.context("failed to run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn bind(path: &Path, mode: &str) -> anyhow::Result<String> {
    let p = path.to_str().with_context(|| format!("path is not UTF-8: {}", path.display()))?;
    ensure!(!p.contains(':'), "path contains ':' and cannot be bind-mounted: {p}");
    Ok(format!("{p}:{p}:{mode}"))
}

/// Resolve the mounts and home directory for running a session in `config`.
/// Errors here are environment problems (missing volume, odd paths) and are
/// recorded as `env_setup` failures.
pub async fn prepare(
    session: SessionId,
    config: &DockerSandbox,
    workdir: &Path,
    data_dir: &Path,
) -> anyhow::Result<Container> {
    for v in &config.volumes {
        let host = v.split(':').next().unwrap_or_default();
        ensure!(Path::new(host).exists(), "sandbox volume source does not exist: {host}");
    }
    let dirs = git(workdir, &["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"]).await?;
    let mut dirs = dirs.lines().map(PathBuf::from);
    let (git_dir, common_dir) = (dirs.next().context("no git dir")?, dirs.next().context("no git common dir")?);

    let home_dir = data_dir.join("sandbox-home").join(session.to_string());
    tokio::fs::create_dir_all(home_dir.join(".tmp")).await.context("create sandbox home")?;
    let auth_in_env = AUTH_ENV[..3].iter().any(|n| std::env::var_os(n).is_some_and(|v| !v.is_empty()));
    let creds = std::env::var_os("HOME").map(|h| Path::new(&h).join(".claude/.credentials.json"));
    if let Some(creds) = creds.filter(|c| config.copy_credentials && !auth_in_env && c.exists()) {
        let dest = home_dir.join(".claude");
        tokio::fs::create_dir_all(&dest).await?;
        tokio::fs::copy(&creds, dest.join(".credentials.json")).await.context("copy credentials")?;
    }

    let mut mounts = vec![bind(workdir, "rw")?];
    if git_dir != workdir.join(".git") {
        // A linked worktree: objects/refs live in the main repo's git dir.
        mounts.push(bind(&common_dir, "ro")?);
        mounts.push(bind(&git_dir, "rw")?);
    }
    let home = home_dir.to_str().context("sandbox home is not UTF-8")?;
    mounts.push(format!("{home}:{CONTAINER_HOME}:rw"));

    let meta = std::fs::metadata(workdir).context("stat worktree")?;
    Ok(Container {
        name: container_name(session),
        session,
        config: config.clone(),
        workdir: workdir.to_owned(),
        mounts,
        user: Some(format!("{}:{}", meta.uid(), meta.gid())),
        home_dir,
    })
}

/// Force-remove a container (best effort, e.g. after a timeout: killing the
/// `docker run` client does not stop the container).
pub async fn remove(docker_bin: &Path, name: &str) {
    let _ = Command::new(docker_bin).args(["rm", "-f", name]).output().await;
}

/// Removes the container if dropped while armed (the session future was cancelled).
pub(crate) struct Guard {
    pub docker_bin: PathBuf,
    pub name: Option<String>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            let _ = std::process::Command::new(&self.docker_bin)
                .args(["rm", "-f", &name])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

/// A WorkHub agent container still present (`docker ps -a` filtered by label).
#[derive(Debug, Clone)]
pub struct Listed {
    pub name: String,
    pub session: String,
    pub image: String,
    pub status: String,
}

pub async fn list(docker_bin: &Path) -> anyhow::Result<Vec<Listed>> {
    let out = Command::new(docker_bin)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={LABEL}"),
            "--format",
            &format!("{{{{.Names}}}}\t{{{{.Label \"{LABEL}\"}}}}\t{{{{.Image}}}}\t{{{{.Status}}}}"),
        ])
        .output()
        .await
        .context("failed to run docker")?;
    ensure!(out.status.success(), "docker ps failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(Listed {
                name: f.next()?.into(),
                session: f.next()?.into(),
                image: f.next()?.into(),
                status: f.next()?.into(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(a: &[OsString]) -> Vec<String> {
        a.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn run_args_mount_worktree_and_pass_env_by_name() {
        let session = SessionId::new();
        let mut config = DockerSandbox::new("img:1");
        config.network = "host".into();
        config.memory = Some("2g".into());
        config.env = vec!["GH_TOKEN".into()];
        config.volumes = vec!["/cache:/cache:ro".into()];
        let c = Container {
            name: container_name(session),
            session,
            config,
            workdir: "/data/wt/s1".into(),
            mounts: vec!["/data/wt/s1:/data/wt/s1:rw".into()],
            user: Some("1000:1000".into()),
            home_dir: "/data/home/s1".into(),
        };
        let a = strs(&run_args(&c, &["claude".into(), "-p".into(), "do it".into()]));
        let joined = a.join(" ");
        assert!(joined.starts_with(&format!("run --rm --init --pull never --name wh-{session}")));
        assert!(joined.contains(&format!("--label workhub.session={session}")));
        assert!(joined.contains("--network host"));
        assert!(joined.contains("--memory 2g"));
        assert!(joined.contains("--user 1000:1000"));
        assert!(joined.contains("--volume /data/wt/s1:/data/wt/s1:rw --volume /cache:/cache:ro"));
        assert!(joined.contains("--env HOME=/home/wh"));
        assert!(joined.contains("--env CLAUDE_CODE_TMPDIR=/home/wh/.tmp"));
        assert!(joined.contains("--env ANTHROPIC_API_KEY"));
        assert!(joined.contains("--env GH_TOKEN"));
        assert!(!joined.contains("GH_TOKEN="), "values must not leak into args");
        assert!(joined.ends_with("--workdir /data/wt/s1 img:1 claude -p do it"));
        // The prompt stays a single argument.
        assert_eq!(a.last().unwrap(), "do it");
    }
}
