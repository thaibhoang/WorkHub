use serde::{Deserialize, Serialize};

/// Where an agent session runs. Every session gets a git worktree on the host;
/// `Docker` additionally runs the agent process inside a container that only
/// sees that worktree (plus the git metadata it needs and declared volumes).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Sandbox {
    #[default]
    Worktree,
    Docker(DockerSandbox),
}

impl Sandbox {
    /// Short label for read models and UIs: `worktree` or `docker:<image>`.
    pub fn label(&self) -> String {
        match self {
            Self::Worktree => "worktree".into(),
            Self::Docker(d) => format!("docker:{}", d.image),
        }
    }

    pub fn docker(&self) -> Option<&DockerSandbox> {
        match self {
            Self::Worktree => None,
            Self::Docker(d) => Some(d),
        }
    }
}

/// Container settings for the docker sandbox (`sandbox:` in factory.yaml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DockerSandbox {
    pub image: String,
    /// `docker run --network` (`bridge`, `host`, a named network). The agent needs
    /// to reach the Anthropic API, so `none` only works behind a proxy.
    pub network: String,
    pub cpus: Option<f64>,
    /// `docker run --memory`, e.g. `4g`.
    pub memory: Option<String>,
    pub pids_limit: Option<u32>,
    /// Host environment variables passed through by name (values never appear in
    /// the command line or events). Auth variables are always passed.
    #[serde(default)]
    pub env: Vec<String>,
    /// Extra bind mounts `host:container[:ro]`, host path absolute.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Copy the host's `~/.claude/.credentials.json` into the container's
    /// throwaway home when no auth variable is set.
    pub copy_credentials: bool,
}

impl DockerSandbox {
    pub const DEFAULT_IMAGE: &'static str = "workhub-agent:latest";
    pub const DEFAULT_NETWORK: &'static str = "bridge";

    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            network: Self::DEFAULT_NETWORK.into(),
            cpus: None,
            memory: None,
            pids_limit: None,
            env: Vec::new(),
            volumes: Vec::new(),
            copy_credentials: true,
        }
    }
}
