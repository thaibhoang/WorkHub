//! Factory-as-code: load and validate `factory.yaml` (stages, gates, agents,
//! policies) and compute the `factory_version` stamped on every event.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
pub use wh_core::{DockerSandbox, Sandbox};

const DEFAULT_BUDGET_USD: f64 = 2.0;
const DEFAULT_TIMEOUT_MIN: u64 = 30;
const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_PERMISSION_MODE: &str = "acceptEdits";
const DEFAULT_MAX_REWORK_LOOPS: u32 = 3;
const DEFAULT_SCORER_MODEL: &str = "claude-haiku-5-5";
const DEFAULT_SCORER_BUDGET_USD: f64 = 0.25;
const DEFAULT_SCORER_TIMEOUT_MIN: u64 = 5;
const DEFAULT_PASS_THRESHOLD: f64 = 0.7;
const DEFAULT_IMPROVER_MODEL: &str = "claude-sonnet-5-5";
const DEFAULT_IMPROVER_BUDGET_USD: f64 = 2.0;
const DEFAULT_IMPROVER_TIMEOUT_MIN: u64 = 20;
const DEFAULT_WINDOW_DAYS: u32 = 14;
const DEFAULT_MIN_SESSIONS: u64 = 5;
const DEFAULT_MAX_OPEN_PROPOSALS: u32 = 3;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FactoryFile {
    name: String,
    #[serde(default = "dot")]
    repo: PathBuf,
    #[serde(default = "head")]
    base_ref: String,
    #[serde(default)]
    defaults: Settings,
    max_rework_loops: Option<u32>,
    stages: Vec<StageFile>,
    agents: BTreeMap<String, AgentFile>,
    #[serde(default)]
    scorers: Vec<ScorerFile>,
    improvement: Option<ImprovementFile>,
    github: Option<GithubFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubFile {
    repo: Option<String>,
    remote: Option<String>,
    base: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    labels: Vec<String>,
    poll_interval_sec: Option<u64>,
    ship_gate: Option<String>,
    conflict_gate: Option<String>,
    rework_stage: Option<String>,
    issues: Option<IssuesFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuesFile {
    label: Option<String>,
    #[serde(default)]
    comment: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImprovementFile {
    prompt: PathBuf,
    model: Option<String>,
    budget_usd: Option<f64>,
    timeout_min: Option<u64>,
    window_days: Option<u32>,
    min_sessions: Option<u64>,
    interval_hours: Option<u64>,
    max_open_proposals: Option<u32>,
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    triggers: Vec<TriggerFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TriggerFile {
    metric: TriggerMetric,
    stage: Option<String>,
    scorer: Option<String>,
    above: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScorerFile {
    id: String,
    /// Agent whose sessions this scorer grades.
    target: String,
    prompt: PathBuf,
    description: Option<String>,
    model: Option<String>,
    pass_threshold: Option<f64>,
    sampling_rate: Option<f64>,
    budget_usd: Option<f64>,
    timeout_min: Option<u64>,
    #[serde(default)]
    include_failed: bool,
}

fn dot() -> PathBuf {
    ".".into()
}
fn head() -> String {
    "HEAD".into()
}

/// Settings that can be set in `defaults` and overridden per agent.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    model: Option<String>,
    budget_usd: Option<f64>,
    timeout_min: Option<u64>,
    max_retries: Option<u32>,
    permission_mode: Option<String>,
    allowed_tools: Option<Vec<String>>,
    /// Built-in tools the agent gets at all (Claude Code `--tools`). Unset = all
    /// of them. Every tool definition is resent on every turn, so a short list
    /// is the cheapest way to shrink each session's context.
    tools: Option<Vec<String>>,
    sandbox: Option<SandboxFile>,
    /// Environment variables set for the agent process (merged by key: an agent's
    /// entries override the defaults'). A leading `~/` in a value is `$HOME/`.
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SandboxKind {
    Worktree,
    Docker,
}

/// `sandbox:` in `defaults` or on an agent. An agent's block replaces the default
/// block as a whole (fields are not merged).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxFile {
    kind: SandboxKind,
    image: Option<String>,
    network: Option<String>,
    cpus: Option<f64>,
    memory: Option<String>,
    pids_limit: Option<u32>,
    env: Option<Vec<String>>,
    volumes: Option<Vec<String>>,
    copy_credentials: Option<bool>,
}

impl SandboxFile {
    fn resolve(&self, base: &Path) -> anyhow::Result<Sandbox> {
        if self.kind == SandboxKind::Worktree {
            let docker_only = self.image.is_some()
                || self.network.is_some()
                || self.cpus.is_some()
                || self.memory.is_some()
                || self.pids_limit.is_some()
                || self.env.is_some()
                || self.volumes.is_some()
                || self.copy_credentials.is_some();
            ensure!(!docker_only, "container settings need `kind: docker`");
            return Ok(Sandbox::Worktree);
        }
        let mut d = DockerSandbox::new(self.image.clone().unwrap_or_else(|| DockerSandbox::DEFAULT_IMAGE.into()));
        ensure!(!d.image.trim().is_empty(), "image is empty");
        if let Some(n) = &self.network {
            ensure!(!n.trim().is_empty(), "network is empty");
            d.network = n.clone();
        }
        if let Some(c) = self.cpus {
            ensure!(c > 0.0, "cpus must be > 0");
        }
        d.cpus = self.cpus;
        d.memory = self.memory.clone();
        d.pids_limit = self.pids_limit;
        for name in self.env.iter().flatten() {
            ensure!(is_var_name(name), "env: '{name}' is not a variable name (values are passed through from the host)");
        }
        d.env = self.env.clone().unwrap_or_default();
        d.volumes = self.volumes.iter().flatten().map(|v| resolve_volume(v, base)).collect::<anyhow::Result<_>>()?;
        d.copy_credentials = self.copy_credentials.unwrap_or(true);
        Ok(Sandbox::Docker(d))
    }
}

fn is_var_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Merge `env:` maps in order (later entries win) and expand a leading `~/`.
fn resolve_env<'a>(entries: impl Iterator<Item = (&'a String, &'a String)>) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for (name, value) in entries {
        ensure!(is_var_name(name), "'{name}' is not a variable name");
        let value = match value.strip_prefix("~/") {
            Some(rest) => {
                let home = std::env::var_os("HOME").context("value uses ~ but HOME is not set")?;
                PathBuf::from(home).join(rest).to_str().context("value is not UTF-8")?.to_owned()
            }
            None => value.clone(),
        };
        env.insert(name.clone(), value);
    }
    Ok(env)
}

/// `host:container[:ro|rw]` with the host path made absolute (`~/` = $HOME,
/// relative = relative to the factory file).
fn resolve_volume(spec: &str, base: &Path) -> anyhow::Result<String> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (host, container, mode) = match parts.as_slice() {
        [h, c] => (*h, *c, None),
        [h, c, m] => (*h, *c, Some(*m)),
        _ => bail!("volume '{spec}': expected host:container[:ro|rw]"),
    };
    ensure!(!host.is_empty(), "volume '{spec}': host path is empty");
    ensure!(container.starts_with('/'), "volume '{spec}': container path must be absolute");
    ensure!(matches!(mode, None | Some("ro") | Some("rw")), "volume '{spec}': mode must be ro or rw");
    let host = if let Some(rest) = host.strip_prefix("~/") {
        let home = std::env::var_os("HOME").context("volume uses ~ but HOME is not set")?;
        PathBuf::from(home).join(rest)
    } else {
        let p = Path::new(host);
        if p.is_absolute() { p.to_owned() } else { absolute(&base.join(p)) }
    };
    let host = host.to_str().with_context(|| format!("volume '{spec}': host path is not UTF-8"))?.to_owned();
    ensure!(!host.contains(':'), "volume '{spec}': host path contains ':'");
    Ok(match mode {
        Some(m) => format!("{host}:{container}:{m}"),
        None => format!("{host}:{container}"),
    })
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageFile {
    id: String,
    agent: String,
    gate: Option<String>,
    on_reject: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFile {
    prompt: PathBuf,
    description: Option<String>,
    #[serde(flatten)]
    settings: Settings,
}

#[derive(Debug, Clone)]
pub struct Stage {
    pub id: String,
    pub agent: String,
    /// Human gate required after the stage succeeds.
    pub gate: Option<String>,
    /// Stage to send the task back to when this stage's agent returns `VERDICT: REJECT`.
    /// Its presence makes a verdict mandatory.
    pub on_reject: Option<String>,
}

impl Stage {
    pub fn requires_verdict(&self) -> bool {
        self.on_reject.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub description: Option<String>,
    /// Prompt file contents.
    pub prompt: String,
    pub model: Option<String>,
    pub budget_usd: f64,
    pub timeout: Duration,
    pub max_retries: u32,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
    /// Built-in tool set (`None` = the runtime's default, all tools).
    pub tools: Option<Vec<String>>,
    /// Where the agent's sessions run (worktree on the host, or a container).
    pub sandbox: Sandbox,
    /// Extra environment for the agent process (also passed into its container).
    pub env: BTreeMap<String, String>,
}

/// LLM-as-judge definition: grades sessions of `target` agent with `prompt`.
#[derive(Debug, Clone)]
pub struct Scorer {
    pub id: String,
    pub target: String,
    pub description: Option<String>,
    /// Prompt file contents (the grading rubric).
    pub prompt: String,
    pub model: String,
    /// `score >= pass_threshold` passes.
    pub pass_threshold: f64,
    /// Fraction of matching sessions graded (deterministic per session).
    pub sampling_rate: f64,
    pub budget_usd: f64,
    pub timeout: Duration,
    /// Also grade sessions that failed (default: only completed ones).
    pub include_failed: bool,
}

/// Metric a trigger watches. Rates are 0..=1 over finished sessions of the
/// current factory version within the improvement window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerMetric {
    /// Failed / finished sessions of `stage`.
    StageFailureRate,
    /// Sessions of `stage` whose verdict was REJECT / finished sessions.
    StageRejectRate,
    /// Average session cost of `stage` in USD.
    StageAvgCostUsd,
    /// Failing / scored results of `scorer`.
    ScorerFailRate,
    /// Shipped tasks whose PR got a `CHANGES_REQUESTED` review / shipped tasks
    /// (needs `github:`; no stage or scorer).
    PrChangesRequestedRate,
}

impl TriggerMetric {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StageFailureRate => "stage_failure_rate",
            Self::StageRejectRate => "stage_reject_rate",
            Self::StageAvgCostUsd => "stage_avg_cost_usd",
            Self::ScorerFailRate => "scorer_fail_rate",
            Self::PrChangesRequestedRate => "pr_changes_requested_rate",
        }
    }
}

/// Trigger subject of PR-level metrics.
pub const PR_SUBJECT: &str = "pr";

/// Run the improver when `metric` (for `subject`: a stage or scorer id) exceeds `above`.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub metric: TriggerMetric,
    pub subject: String,
    pub above: f64,
}

impl Trigger {
    /// Stable key, e.g. `stage_failure_rate:implement>0.25`.
    pub fn key(&self) -> String {
        format!("{}:{}>{}", self.metric.as_str(), self.subject, self.above)
    }
}

/// Self-improvement settings (`improvement:` in factory.yaml).
#[derive(Debug, Clone)]
pub struct Improvement {
    /// Improver instructions (prompt file contents).
    pub prompt: String,
    pub model: String,
    pub budget_usd: f64,
    pub timeout: Duration,
    /// Days of history the improver observes.
    pub window_days: u32,
    /// Don't run (automatically) with fewer finished sessions than this.
    pub min_sessions: u64,
    /// Run periodically when there is new data (None = triggers / manual only).
    pub interval_hours: Option<u64>,
    /// No automatic run while this many proposals are queued, running or awaiting review.
    pub max_open_proposals: u32,
    pub allowed_tools: Vec<String>,
    pub triggers: Vec<Trigger>,
}

/// GitHub integration (`github:` in factory.yaml). Its presence makes the task
/// ship (push + pull request) after the last stage instead of completing locally.
#[derive(Debug, Clone, PartialEq)]
pub struct Github {
    /// `owner/name`; None = derived from the remote's URL at run time.
    pub repo: Option<String>,
    pub remote: String,
    /// PR base branch; None = the repository's default branch.
    pub base: Option<String>,
    pub draft: bool,
    pub labels: Vec<String>,
    pub poll_interval: Duration,
    /// Human gate before pushing (pushing / opening a PR is visible to others).
    pub ship_gate: Option<String>,
    /// Human gate before WorkHub merges the base into a conflicting PR.
    pub conflict_gate: Option<String>,
    /// Stage that addresses PR feedback (changes requested, failed CI, conflicts).
    pub rework_stage: String,
    /// Create tasks from open issues carrying this label.
    pub issues: Option<Issues>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issues {
    pub label: String,
    /// Comment on the issue when a task is created from it.
    pub comment: bool,
}

pub const DEFAULT_ISSUE_LABEL: &str = "workhub";
const DEFAULT_POLL_INTERVAL_SEC: u64 = 60;

#[derive(Debug, Clone)]
pub struct Factory {
    /// Canonical path of factory.yaml.
    pub path: PathBuf,
    pub name: String,
    /// Content hash of factory.yaml + agent prompts (12 hex chars).
    pub version: String,
    pub repo: PathBuf,
    pub base_ref: String,
    pub max_rework_loops: u32,
    pub stages: Vec<Stage>,
    pub agents: BTreeMap<String, Agent>,
    pub scorers: Vec<Scorer>,
    pub improvement: Option<Improvement>,
    pub github: Option<Github>,
}

impl Factory {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let file: FactoryFile =
            serde_yaml_ng::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));

        let mut hasher = Sha256::new();
        hasher.update(&raw);

        let d = &file.defaults;
        ensure!(d.timeout_min != Some(0), "defaults: timeout_min must be > 0");
        let mut agents = BTreeMap::new();
        for (name, a) in &file.agents {
            let prompt_path = base.join(&a.prompt);
            let prompt = std::fs::read_to_string(&prompt_path)
                .with_context(|| format!("agent '{name}': read prompt {}", prompt_path.display()))?;
            hasher.update(name.as_bytes());
            hasher.update(prompt.as_bytes());
            let s = &a.settings;
            let sandbox = match s.sandbox.as_ref().or(d.sandbox.as_ref()) {
                None => Sandbox::Worktree,
                Some(sb) => sb.resolve(base).with_context(|| format!("agent '{name}': sandbox"))?,
            };
            let budget_usd = s.budget_usd.or(d.budget_usd).unwrap_or(DEFAULT_BUDGET_USD);
            ensure!(budget_usd > 0.0, "agent '{name}': budget_usd must be > 0");
            ensure!(s.timeout_min != Some(0), "agent '{name}': timeout_min must be > 0");
            let env = resolve_env(d.env.iter().flatten().chain(s.env.iter().flatten()))
                .with_context(|| format!("agent '{name}': env"))?;
            agents.insert(name.clone(), Agent {
                name: name.clone(),
                description: a.description.clone(),
                prompt,
                model: s.model.clone().or_else(|| d.model.clone()),
                budget_usd,
                timeout: Duration::from_secs(60 * s.timeout_min.or(d.timeout_min).unwrap_or(DEFAULT_TIMEOUT_MIN)),
                max_retries: s.max_retries.or(d.max_retries).unwrap_or(DEFAULT_MAX_RETRIES),
                permission_mode: s
                    .permission_mode
                    .clone()
                    .or_else(|| d.permission_mode.clone())
                    .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.into()),
                allowed_tools: s.allowed_tools.clone().or_else(|| d.allowed_tools.clone()).unwrap_or_default(),
                tools: s.tools.clone().or_else(|| d.tools.clone()),
                sandbox,
                env,
            });
        }

        ensure!(!file.stages.is_empty(), "factory must define at least one stage");
        let mut ids = HashSet::new();
        for st in &file.stages {
            ensure!(ids.insert(st.id.as_str()), "duplicate stage id '{}'", st.id);
            ensure!(agents.contains_key(&st.agent), "stage '{}': unknown agent '{}'", st.id, st.agent);
            if matches!(&st.gate, Some(g) if g.trim().is_empty()) {
                bail!("stage '{}': gate name is empty", st.id);
            }
        }
        for st in &file.stages {
            if let Some(target) = &st.on_reject {
                ensure!(ids.contains(target.as_str()), "stage '{}': on_reject '{target}' is not a stage", st.id);
            }
        }

        let mut scorers = Vec::new();
        let mut scorer_ids = HashSet::new();
        for sc in &file.scorers {
            let id = &sc.id;
            ensure!(scorer_ids.insert(id.as_str()), "duplicate scorer id '{id}'");
            ensure!(agents.contains_key(&sc.target), "scorer '{id}': unknown target agent '{}'", sc.target);
            let prompt_path = base.join(&sc.prompt);
            let prompt = std::fs::read_to_string(&prompt_path)
                .with_context(|| format!("scorer '{id}': read prompt {}", prompt_path.display()))?;
            hasher.update(b"scorer:");
            hasher.update(id.as_bytes());
            hasher.update(prompt.as_bytes());
            let pass_threshold = sc.pass_threshold.unwrap_or(DEFAULT_PASS_THRESHOLD);
            let sampling_rate = sc.sampling_rate.unwrap_or(1.0);
            let budget_usd = sc.budget_usd.unwrap_or(DEFAULT_SCORER_BUDGET_USD);
            ensure!((0.0..=1.0).contains(&pass_threshold), "scorer '{id}': pass_threshold must be within 0..=1");
            ensure!((0.0..=1.0).contains(&sampling_rate), "scorer '{id}': sampling_rate must be within 0..=1");
            ensure!(budget_usd > 0.0, "scorer '{id}': budget_usd must be > 0");
            ensure!(sc.timeout_min != Some(0), "scorer '{id}': timeout_min must be > 0");
            scorers.push(Scorer {
                id: id.clone(),
                target: sc.target.clone(),
                description: sc.description.clone(),
                prompt,
                model: sc.model.clone().unwrap_or_else(|| DEFAULT_SCORER_MODEL.into()),
                pass_threshold,
                sampling_rate,
                budget_usd,
                timeout: Duration::from_secs(60 * sc.timeout_min.unwrap_or(DEFAULT_SCORER_TIMEOUT_MIN)),
                include_failed: sc.include_failed,
            });
        }

        let improvement = match &file.improvement {
            None => None,
            Some(im) => {
                let prompt_path = base.join(&im.prompt);
                let prompt = std::fs::read_to_string(&prompt_path)
                    .with_context(|| format!("improvement: read prompt {}", prompt_path.display()))?;
                hasher.update(b"improver:");
                hasher.update(prompt.as_bytes());
                let budget_usd = im.budget_usd.unwrap_or(DEFAULT_IMPROVER_BUDGET_USD);
                ensure!(budget_usd > 0.0, "improvement: budget_usd must be > 0");
                ensure!(im.timeout_min != Some(0), "improvement: timeout_min must be > 0");
                ensure!(im.interval_hours != Some(0), "improvement: interval_hours must be > 0");
                let window_days = im.window_days.unwrap_or(DEFAULT_WINDOW_DAYS);
                ensure!(window_days > 0, "improvement: window_days must be > 0");
                let mut triggers = Vec::new();
                for t in &im.triggers {
                    let m = t.metric.as_str();
                    let subject = match t.metric {
                        TriggerMetric::PrChangesRequestedRate => {
                            ensure!(t.stage.is_none() && t.scorer.is_none(), "trigger {m}: takes no `stage` or `scorer`");
                            ensure!(file.github.is_some(), "trigger {m}: needs a `github:` section");
                            PR_SUBJECT.to_owned()
                        }
                        TriggerMetric::ScorerFailRate => {
                            ensure!(t.stage.is_none(), "trigger {m}: use `scorer`, not `stage`");
                            let id = t.scorer.clone().with_context(|| format!("trigger {m}: `scorer` is required"))?;
                            ensure!(scorer_ids.contains(id.as_str()), "trigger {m}: unknown scorer '{id}'");
                            id
                        }
                        _ => {
                            ensure!(t.scorer.is_none(), "trigger {m}: use `stage`, not `scorer`");
                            let id = t.stage.clone().with_context(|| format!("trigger {m}: `stage` is required"))?;
                            ensure!(ids.contains(id.as_str()), "trigger {m}: unknown stage '{id}'");
                            id
                        }
                    };
                    if m.ends_with("_rate") {
                        ensure!((0.0..1.0).contains(&t.above), "trigger {m}: `above` must be within 0..1");
                    }
                    triggers.push(Trigger { metric: t.metric, subject, above: t.above });
                }
                Some(Improvement {
                    prompt,
                    model: im.model.clone().or_else(|| d.model.clone()).unwrap_or_else(|| DEFAULT_IMPROVER_MODEL.into()),
                    budget_usd,
                    timeout: Duration::from_secs(60 * im.timeout_min.unwrap_or(DEFAULT_IMPROVER_TIMEOUT_MIN)),
                    window_days,
                    min_sessions: im.min_sessions.unwrap_or(DEFAULT_MIN_SESSIONS),
                    interval_hours: im.interval_hours,
                    max_open_proposals: im.max_open_proposals.unwrap_or(DEFAULT_MAX_OPEN_PROPOSALS),
                    allowed_tools: im.allowed_tools.clone().unwrap_or_default(),
                    triggers,
                })
            }
        };

        let github = match &file.github {
            None => None,
            Some(g) => {
                if let Some(r) = &g.repo {
                    let parts: Vec<&str> = r.split('/').collect();
                    ensure!(
                        parts.len() == 2 && parts.iter().all(|p| !p.trim().is_empty()),
                        "github: repo must be owner/name, got '{r}'"
                    );
                }
                let poll = g.poll_interval_sec.unwrap_or(DEFAULT_POLL_INTERVAL_SEC);
                ensure!(poll >= 5, "github: poll_interval_sec must be >= 5");
                if matches!(&g.ship_gate, Some(n) if n.trim().is_empty()) {
                    bail!("github: ship_gate name is empty");
                }
                if matches!(&g.conflict_gate, Some(n) if n.trim().is_empty()) {
                    bail!("github: conflict_gate name is empty");
                }
                // Default: the stage review sends rework to, else the first stage.
                let rework_stage = match &g.rework_stage {
                    Some(st) => {
                        ensure!(ids.contains(st.as_str()), "github: rework_stage '{st}' is not a stage");
                        st.clone()
                    }
                    None => file
                        .stages
                        .iter()
                        .find_map(|s| s.on_reject.clone())
                        .unwrap_or_else(|| file.stages[0].id.clone()),
                };
                ensure!(
                    !ids.contains(wh_core::task::SHIP_STAGE),
                    "stage id '{}' is reserved when `github:` is set",
                    wh_core::task::SHIP_STAGE
                );
                ensure!(
                    !ids.contains(wh_core::task::CONFLICT_STAGE),
                    "stage id '{}' is reserved when `github:` is set",
                    wh_core::task::CONFLICT_STAGE
                );
                Some(Github {
                    repo: g.repo.clone(),
                    remote: g.remote.clone().unwrap_or_else(|| "origin".into()),
                    base: g.base.clone(),
                    draft: g.draft,
                    labels: g.labels.clone(),
                    poll_interval: Duration::from_secs(poll),
                    ship_gate: g.ship_gate.clone(),
                    conflict_gate: g.conflict_gate.clone(),
                    rework_stage,
                    issues: g.issues.as_ref().map(|i| Issues {
                        label: i.label.clone().unwrap_or_else(|| DEFAULT_ISSUE_LABEL.into()),
                        comment: i.comment,
                    }),
                })
            }
        };

        let repo_path = base.join(&file.repo);
        let repo = std::fs::canonicalize(&repo_path)
            .with_context(|| format!("repo {} not found", repo_path.display()))?;
        let version = hasher.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();

        Ok(Self {
            path: std::fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))?,
            name: file.name,
            version,
            repo,
            base_ref: file.base_ref,
            max_rework_loops: file.max_rework_loops.unwrap_or(DEFAULT_MAX_REWORK_LOOPS),
            stages: file
                .stages
                .into_iter()
                .map(|s| Stage { id: s.id, agent: s.agent, gate: s.gate, on_reject: s.on_reject })
                .collect(),
            agents,
            scorers,
            improvement,
            github,
        })
    }

    pub fn scorer(&self, id: &str) -> Option<&Scorer> {
        self.scorers.iter().find(|s| s.id == id)
    }

    /// Scorers that grade sessions of `agent`.
    pub fn scorers_for<'a>(&'a self, agent: &'a str) -> impl Iterator<Item = &'a Scorer> + 'a {
        self.scorers.iter().filter(move |s| s.target == agent)
    }

    pub fn stage(&self, id: &str) -> Option<&Stage> {
        self.stages.iter().find(|s| s.id == id)
    }

    pub fn first_stage(&self) -> &Stage {
        &self.stages[0]
    }

    /// The stage after `id` in pipeline order.
    pub fn next_stage(&self, id: &str) -> Option<&Stage> {
        let i = self.stages.iter().position(|s| s.id == id)?;
        self.stages.get(i + 1)
    }

    pub fn agent(&self, stage: &Stage) -> &Agent {
        &self.agents[&stage.agent]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, yaml: &str) -> PathBuf {
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::create_dir_all(dir.join("scorers")).unwrap();
        std::fs::write(dir.join("scorers/s.md"), "rubric").unwrap();
        for a in ["a", "b"] {
            std::fs::write(dir.join(format!("agents/{a}.md")), format!("prompt {a}")).unwrap();
        }
        let p = dir.join("factory.yaml");
        std::fs::write(&p, yaml).unwrap();
        p
    }

    const OK: &str = r#"
name: demo
defaults: { budget_usd: 1.5, max_retries: 1 }
max_rework_loops: 2
stages:
  - { id: spec, agent: a, gate: spec-review }
  - { id: impl, agent: a }
  - { id: review, agent: b, on_reject: impl }
agents:
  a: { prompt: agents/a.md }
  b: { prompt: agents/b.md, budget_usd: 0.5, model: claude-haiku-5-5 }
scorers:
  - { id: s1, target: a, prompt: scorers/s.md, sampling_rate: 0.5 }
improvement:
  prompt: scorers/s.md
  interval_hours: 24
  triggers:
    - { metric: stage_failure_rate, stage: impl, above: 0.25 }
    - { metric: scorer_fail_rate, scorer: s1, above: 0.4 }
"#;

    #[test]
    fn loads_resolves_defaults_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let f = Factory::load(&write(dir.path(), OK)).unwrap();
        assert_eq!(f.name, "demo");
        assert_eq!(f.version.len(), 12);
        assert_eq!(f.max_rework_loops, 2);
        assert_eq!(f.next_stage("spec").unwrap().id, "impl");
        assert!(f.next_stage("review").is_none());
        assert!(f.stage("review").unwrap().requires_verdict());
        let a = &f.agents["a"];
        assert_eq!((a.budget_usd, a.max_retries, a.prompt.as_str()), (1.5, 1, "prompt a"));
        assert_eq!(a.timeout, Duration::from_secs(30 * 60));
        assert_eq!(f.agents["b"].budget_usd, 0.5);
        let sc = f.scorer("s1").unwrap();
        assert_eq!((sc.prompt.as_str(), sc.sampling_rate, sc.pass_threshold), ("rubric", 0.5, 0.7));
        assert_eq!(sc.model, "claude-haiku-5-5");
        assert_eq!(sc.timeout, Duration::from_secs(5 * 60));
        assert_eq!(f.scorers_for("a").count(), 1);
        assert_eq!(f.scorers_for("b").count(), 0);
        let im = f.improvement.as_ref().unwrap();
        assert_eq!((im.window_days, im.min_sessions, im.interval_hours), (14, 5, Some(24)));
        assert_eq!(im.model, "claude-sonnet-5-5");
        assert_eq!(im.timeout, Duration::from_secs(20 * 60));
        assert_eq!(im.triggers[0].key(), "stage_failure_rate:impl>0.25");
        assert_eq!(im.triggers[1].subject, "s1");
        assert!(f.path.is_absolute());
    }

    #[test]
    fn sandbox_defaults_and_agent_override() {
        let dir = tempfile::tempdir().unwrap();
        let f = Factory::load(&write(dir.path(), OK)).unwrap();
        assert_eq!(f.agents["a"].sandbox, Sandbox::Worktree);

        let yaml = OK
            .replace(
                "max_retries: 1 }",
                "max_retries: 1, sandbox: { kind: docker, network: host, memory: 4g, env: [GH_TOKEN], \
                 volumes: [\"cache:/cache:ro\", \"/abs:/abs\"] } }",
            )
            .replace("model: claude-haiku-5-5 }", "model: claude-haiku-5-5, sandbox: { kind: worktree } }");
        let f = Factory::load(&write(dir.path(), &yaml)).unwrap();
        let Sandbox::Docker(d) = &f.agents["a"].sandbox else { panic!("expected docker") };
        assert_eq!(d.image, DockerSandbox::DEFAULT_IMAGE);
        assert_eq!((d.network.as_str(), d.memory.as_deref()), ("host", Some("4g")));
        assert_eq!(d.env, vec!["GH_TOKEN".to_string()]);
        let cache = std::path::absolute(dir.path().join("cache")).unwrap();
        assert_eq!(d.volumes, vec![format!("{}:/cache:ro", cache.display()), "/abs:/abs".into()]);
        assert!(d.copy_credentials);
        assert_eq!(f.agents["b"].sandbox, Sandbox::Worktree);
        assert_eq!(f.agents["a"].sandbox.label(), "docker:workhub-agent:latest");
    }

    #[test]
    fn env_merges_defaults_and_expands_home() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = OK
            .replace("max_retries: 1 }", "max_retries: 1, env: { A: \"~/x\", B: one } }")
            .replace("model: claude-haiku-5-5 }", "model: claude-haiku-5-5, env: { B: two } }");
        let f = Factory::load(&write(dir.path(), &yaml)).unwrap();
        let home = std::env::var("HOME").unwrap();
        assert_eq!(f.agents["a"].env.get("A"), Some(&format!("{home}/x")));
        assert_eq!(f.agents["a"].env.get("B").map(String::as_str), Some("one"));
        assert_eq!(f.agents["b"].env.get("B").map(String::as_str), Some("two"));
        assert_eq!(f.agents["b"].env.len(), 2);
        let bad = OK.replace("max_retries: 1 }", "max_retries: 1, env: { \"A-B\": x } }");
        let err = format!("{:#}", Factory::load(&write(dir.path(), &bad)).unwrap_err());
        assert!(err.contains("not a variable name"), "{err}");
    }

    #[test]
    fn tools_default_to_all_and_agents_override() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Factory::load(&write(dir.path(), OK)).unwrap().agents["a"].tools, None);
        let yaml = OK
            .replace("max_retries: 1 }", "max_retries: 1, tools: [Bash, Read] }")
            .replace("model: claude-haiku-5-5 }", "model: claude-haiku-5-5, tools: [] }");
        let f = Factory::load(&write(dir.path(), &yaml)).unwrap();
        assert_eq!(f.agents["a"].tools, Some(vec!["Bash".to_string(), "Read".to_string()]));
        assert_eq!(f.agents["b"].tools, Some(vec![]));
    }

    #[test]
    fn github_block() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Factory::load(&write(dir.path(), OK)).unwrap().github.is_none());
        let yaml = format!("{OK}github: {{ repo: acme/demo, ship_gate: ship-review, issues: {{}} }}\n");
        let g = Factory::load(&write(dir.path(), &yaml)).unwrap().github.unwrap();
        assert_eq!((g.repo.as_deref(), g.remote.as_str(), g.base.as_deref()), (Some("acme/demo"), "origin", None));
        assert_eq!((g.rework_stage.as_str(), g.ship_gate.as_deref()), ("impl", Some("ship-review")));
        assert_eq!(g.poll_interval, Duration::from_secs(60));
        assert_eq!(g.conflict_gate, None);
        let with_gate = write(dir.path(), &format!("{OK}github: {{ conflict_gate: conflict-review }}\n"));
        let gate = Factory::load(&with_gate).unwrap().github.unwrap().conflict_gate;
        assert_eq!(gate.as_deref(), Some("conflict-review"));
        let reserved = format!("{}github: {{}}\n", OK.replace("impl", "conflict"));
        let err = format!("{:#}", Factory::load(&write(dir.path(), &reserved)).unwrap_err());
        assert!(err.contains("reserved"), "{err}");
        assert_eq!(g.issues, Some(Issues { label: "workhub".into(), comment: false }));
        for (needle, extra) in [
            ("owner/name", "github: { repo: nope }"),
            ("not a stage", "github: { rework_stage: zz }"),
            ("conflict_gate name is empty", "github: { conflict_gate: ' ' }"),
            ("unknown field", "github: { repoo: a/b }"),
        ] {
            let err = format!("{:#}", Factory::load(&write(dir.path(), &format!("{OK}{extra}\n"))).unwrap_err());
            assert!(err.contains(needle), "expected '{needle}' in: {err}");
        }
    }

    #[test]
    fn version_changes_when_a_prompt_changes() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), OK);
        let v1 = Factory::load(&p).unwrap().version;
        assert_eq!(Factory::load(&p).unwrap().version, v1);
        std::fs::write(dir.path().join("agents/b.md"), "changed").unwrap();
        let v2 = Factory::load(&p).unwrap().version;
        assert_ne!(v2, v1);
        std::fs::write(dir.path().join("scorers/s.md"), "new rubric").unwrap();
        assert_ne!(Factory::load(&p).unwrap().version, v2);
    }

    #[test]
    fn rejects_invalid_definitions() {
        let cases = [
            ("unknown agent", OK.replace("agent: b, on_reject", "agent: zz, on_reject")),
            ("not a stage", OK.replace("on_reject: impl", "on_reject: nope")),
            ("duplicate", OK.replace("id: impl", "id: spec")),
            ("unknown field", OK.replace("gate: spec-review", "gaet: x")),
            ("unknown target agent", OK.replace("target: a", "target: zz")),
            ("sampling_rate", OK.replace("sampling_rate: 0.5", "sampling_rate: 2")),
            ("unknown stage 'zz'", OK.replace("stage: impl, above", "stage: zz, above")),
            ("unknown scorer", OK.replace("scorer: s1, above", "scorer: nope, above")),
            ("use `scorer`, not `stage`", OK.replace("scorer: s1, above", "stage: impl, above")),
            ("within 0..1", OK.replace("above: 0.25", "above: 2")),
            ("unknown variant", OK.replace("metric: stage_failure_rate", "metric: vibes")),
            ("need `kind: docker`", OK.replace("max_retries: 1 }", "max_retries: 1, sandbox: { kind: worktree, image: x } }")),
            ("container path must be absolute", OK.replace("max_retries: 1 }", "max_retries: 1, sandbox: { kind: docker, volumes: [\"a:b\"] } }")),
            ("not a variable name", OK.replace("max_retries: 1 }", "max_retries: 1, sandbox: { kind: docker, env: [\"A=1\"] } }")),
            ("unknown field", OK.replace("max_retries: 1 }", "max_retries: 1, sandbox: { kind: docker, img: x } }")),
            ("defaults: timeout_min must be > 0", OK.replace("max_retries: 1 }", "max_retries: 1, timeout_min: 0 }")),
            ("agent 'a': timeout_min must be > 0", OK.replace("agents/a.md }", "agents/a.md, timeout_min: 0 }")),
            ("scorer 's1': timeout_min must be > 0", OK.replace("sampling_rate: 0.5", "sampling_rate: 0.5, timeout_min: 0")),
            ("improvement: timeout_min must be > 0", OK.replace("interval_hours: 24", "interval_hours: 24\n  timeout_min: 0")),
            // Rejected even when every agent overrides the default.
            (
                "defaults: timeout_min must be > 0",
                OK.replace("max_retries: 1 }", "max_retries: 1, timeout_min: 0 }")
                    .replace("agents/a.md }", "agents/a.md, timeout_min: 1 }")
                    .replace("model: claude-haiku-5-5 }", "model: claude-haiku-5-5, timeout_min: 1 }"),
            ),
        ];
        for (needle, yaml) in cases {
            let dir = tempfile::tempdir().unwrap();
            let err = format!("{:#}", Factory::load(&write(dir.path(), &yaml)).unwrap_err());
            assert!(err.contains(needle), "expected '{needle}' in: {err}");
        }
    }
}
