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

const DEFAULT_BUDGET_USD: f64 = 2.0;
const DEFAULT_TIMEOUT_MIN: u64 = 30;
const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_PERMISSION_MODE: &str = "acceptEdits";
const DEFAULT_MAX_REWORK_LOOPS: u32 = 3;

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
}

#[derive(Debug, Clone)]
pub struct Factory {
    pub name: String,
    /// Content hash of factory.yaml + agent prompts (12 hex chars).
    pub version: String,
    pub repo: PathBuf,
    pub base_ref: String,
    pub max_rework_loops: u32,
    pub stages: Vec<Stage>,
    pub agents: BTreeMap<String, Agent>,
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
        let mut agents = BTreeMap::new();
        for (name, a) in &file.agents {
            let prompt_path = base.join(&a.prompt);
            let prompt = std::fs::read_to_string(&prompt_path)
                .with_context(|| format!("agent '{name}': read prompt {}", prompt_path.display()))?;
            hasher.update(name.as_bytes());
            hasher.update(prompt.as_bytes());
            let s = &a.settings;
            let budget_usd = s.budget_usd.or(d.budget_usd).unwrap_or(DEFAULT_BUDGET_USD);
            ensure!(budget_usd > 0.0, "agent '{name}': budget_usd must be > 0");
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

        let repo_path = base.join(&file.repo);
        let repo = std::fs::canonicalize(&repo_path)
            .with_context(|| format!("repo {} not found", repo_path.display()))?;
        let version = hasher.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();

        Ok(Self {
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
        })
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
    }

    #[test]
    fn version_changes_when_a_prompt_changes() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), OK);
        let v1 = Factory::load(&p).unwrap().version;
        assert_eq!(Factory::load(&p).unwrap().version, v1);
        std::fs::write(dir.path().join("agents/b.md"), "changed").unwrap();
        assert_ne!(Factory::load(&p).unwrap().version, v1);
    }

    #[test]
    fn rejects_invalid_definitions() {
        let cases = [
            ("unknown agent", OK.replace("agent: b, on_reject", "agent: zz, on_reject")),
            ("not a stage", OK.replace("on_reject: impl", "on_reject: nope")),
            ("duplicate", OK.replace("id: impl", "id: spec")),
            ("unknown field", OK.replace("gate: spec-review", "gaet: x")),
        ];
        for (needle, yaml) in cases {
            let dir = tempfile::tempdir().unwrap();
            let err = format!("{:#}", Factory::load(&write(dir.path(), &yaml)).unwrap_err());
            assert!(err.contains(needle), "expected '{needle}' in: {err}");
        }
    }
}
