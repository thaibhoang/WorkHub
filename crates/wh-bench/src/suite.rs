//! Benchmark suite files: a fixed set of tasks, each with an optional
//! deterministic check, run against a factory variant.
//!
//! ```yaml
//! name: smoke
//! factory: ../factory/factory.yaml   # relative to this file
//! max_cost_usd: 5.0                  # stop starting cases once spent
//! check_timeout_min: 10
//! cases:
//!   - id: greet-flag
//!     title: Add a --name flag to greet.sh
//!     body: ...
//!     fixture: fixtures/greet        # directory copied into a fresh git repo per case run
//!     check: sh "$WH_BENCH_SUITE_DIR/checks/greet.sh"   # exit 0 on the finished branch = pass
//! ```
//!
//! The check runs in a checkout of the task branch with `WH_BENCH_SUITE_DIR`
//! (this file's directory) and `WH_BENCH_CASE` set, so its tests can live in
//! `checks/` where the agents cannot see (or edit) them. `checks/` is hashed
//! into the suite version.
//!
//! Suites live outside the factory directory on purpose: the improver may edit
//! the factory but must never edit the yardstick it is measured with.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const DEFAULT_CHECK_TIMEOUT_MIN: u64 = 10;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteFile {
    name: String,
    description: Option<String>,
    factory: PathBuf,
    max_cost_usd: Option<f64>,
    check_timeout_min: Option<u64>,
    cases: Vec<CaseFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    id: String,
    title: String,
    body: Option<String>,
    fixture: Option<PathBuf>,
    repo: Option<PathBuf>,
    base_ref: Option<String>,
    check: Option<String>,
    check_timeout_min: Option<u64>,
}

/// Where a case's agents work.
#[derive(Debug, Clone, PartialEq)]
pub enum CaseRepo {
    /// The factory's own repo (optionally at `Case::base_ref`).
    Factory,
    /// An existing git repo.
    Repo(PathBuf),
    /// A directory copied into a fresh single-commit git repo for every case run.
    Fixture(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Case {
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub repo: CaseRepo,
    pub base_ref: Option<String>,
    /// Shell command run (`sh -c`) in a checkout of the finished task branch.
    pub check: Option<String>,
    pub check_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Suite {
    /// Canonical path of the suite file.
    pub path: PathBuf,
    pub name: String,
    pub description: Option<String>,
    /// Canonical path of the factory file the suite runs against.
    pub factory_file: PathBuf,
    pub max_cost_usd: Option<f64>,
    pub cases: Vec<Case>,
    /// Content hash of the suite file + fixtures (12 hex chars).
    pub version: String,
}

impl Suite {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let file: SuiteFile = serde_yaml_ng::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        let mut hasher = Sha256::new();
        hasher.update(&raw);

        ensure!(valid_id(&file.name), "suite name '{}' must be [a-z0-9_-]+", file.name);
        ensure!(!file.cases.is_empty(), "suite '{}' has no cases", file.name);
        if let Some(c) = file.max_cost_usd {
            ensure!(c > 0.0, "max_cost_usd must be > 0");
        }
        let factory_path = base.join(&file.factory);
        let factory_file = std::fs::canonicalize(&factory_path)
            .with_context(|| format!("factory {} not found", factory_path.display()))?;
        let default_timeout = file.check_timeout_min.unwrap_or(DEFAULT_CHECK_TIMEOUT_MIN);

        let mut ids = HashSet::new();
        let mut cases = Vec::new();
        for c in file.cases {
            let id = &c.id;
            ensure!(valid_id(id), "case id '{id}' must be [a-z0-9_-]+");
            ensure!(ids.insert(id.clone()), "duplicate case id '{id}'");
            ensure!(!c.title.trim().is_empty(), "case '{id}': title is empty");
            let repo = match (&c.fixture, &c.repo) {
                (Some(_), Some(_)) => bail!("case '{id}': use either `fixture` or `repo`, not both"),
                (Some(f), None) => {
                    ensure!(c.base_ref.is_none(), "case '{id}': `base_ref` cannot be used with `fixture`");
                    let dir = std::fs::canonicalize(base.join(f))
                        .with_context(|| format!("case '{id}': fixture {} not found", f.display()))?;
                    ensure!(dir.is_dir(), "case '{id}': fixture {} is not a directory", dir.display());
                    hasher.update(format!("fixture:{id}").as_bytes());
                    hash_dir(&mut hasher, &dir, &dir)?;
                    CaseRepo::Fixture(dir)
                }
                (None, Some(r)) => CaseRepo::Repo(
                    std::fs::canonicalize(base.join(r))
                        .with_context(|| format!("case '{id}': repo {} not found", r.display()))?,
                ),
                (None, None) => CaseRepo::Factory,
            };
            if let Some(check) = &c.check {
                ensure!(!check.trim().is_empty(), "case '{id}': check is empty");
            }
            let timeout = c.check_timeout_min.unwrap_or(default_timeout);
            ensure!(timeout > 0, "case '{id}': check_timeout_min must be > 0");
            cases.push(Case {
                id: c.id,
                title: c.title,
                body: c.body,
                repo,
                base_ref: c.base_ref,
                check: c.check,
                check_timeout: Duration::from_secs(60 * timeout),
            });
        }

        // Hidden check scripts (outside the agents' repo) live in `checks/` next to
        // the suite; they are part of the yardstick, so they are part of the version.
        let checks = base.join("checks");
        if checks.is_dir() {
            hasher.update(b"checks");
            hash_dir(&mut hasher, &checks, &checks)?;
        }

        Ok(Self {
            path: std::fs::canonicalize(path)?,
            name: file.name,
            description: file.description,
            factory_file,
            max_cost_usd: file.max_cost_usd,
            cases,
            version: hasher.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect(),
        })
    }

    /// Case runs in execution order: each selected case, `repeat` times
    /// (case-major so a cost limit still covers whole cases first).
    pub fn plan(&self, only: &[String], repeat: u32) -> anyhow::Result<Vec<(&Case, u32)>> {
        for id in only {
            ensure!(self.cases.iter().any(|c| &c.id == id), "suite '{}' has no case '{id}'", self.name);
        }
        ensure!(repeat >= 1, "repeat must be >= 1");
        Ok(self
            .cases
            .iter()
            .filter(|c| only.is_empty() || only.contains(&c.id))
            .flat_map(|c| (1..=repeat).map(move |r| (c, r)))
            .collect())
    }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Hash relative paths + contents of every file under `dir`, in sorted order.
fn hash_dir(h: &mut Sha256, root: &Path, dir: &Path) -> anyhow::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if e.file_type()?.is_dir() {
            if e.file_name() == ".git" {
                continue;
            }
            hash_dir(h, root, &p)?;
        } else {
            h.update(p.strip_prefix(root).unwrap_or(&p).to_string_lossy().as_bytes());
            h.update(std::fs::read(&p)?);
        }
    }
    Ok(())
}

/// Every `*.yaml` / `*.yml` suite in `dir` (unloadable ones as errors).
pub fn discover(dir: &Path) -> Vec<(PathBuf, anyhow::Result<Suite>)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml")))
        .collect();
    paths.sort();
    paths.into_iter().map(|p| (p.clone(), Suite::load(&p))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, yaml: &str) -> PathBuf {
        std::fs::create_dir_all(dir.join("fixtures/a")).unwrap();
        std::fs::write(dir.join("fixtures/a/x.txt"), "x").unwrap();
        std::fs::write(dir.join("factory.yaml"), "stub").unwrap();
        let p = dir.join("suite.yaml");
        std::fs::write(&p, yaml).unwrap();
        p
    }

    const OK: &str = r#"
name: smoke
factory: factory.yaml
max_cost_usd: 3
cases:
  - { id: one, title: First, fixture: fixtures/a, check: "true" }
  - { id: two, title: Second, base_ref: main, check_timeout_min: 2 }
"#;

    #[test]
    fn loads_and_plans() {
        let dir = tempfile::tempdir().unwrap();
        let s = Suite::load(&write(dir.path(), OK)).unwrap();
        assert_eq!((s.name.as_str(), s.version.len(), s.max_cost_usd), ("smoke", 12, Some(3.0)));
        assert!(matches!(s.cases[0].repo, CaseRepo::Fixture(_)));
        assert_eq!(s.cases[1].repo, CaseRepo::Factory);
        assert_eq!(s.cases[0].check_timeout, Duration::from_secs(600));
        assert_eq!(s.cases[1].check_timeout, Duration::from_secs(120));
        let plan: Vec<_> = s.plan(&[], 2).unwrap().into_iter().map(|(c, r)| format!("{}#{r}", c.id)).collect();
        assert_eq!(plan, ["one#1", "one#2", "two#1", "two#2"]);
        assert_eq!(s.plan(&["two".into()], 1).unwrap().len(), 1);
        assert!(s.plan(&["nope".into()], 1).is_err());
    }

    #[test]
    fn version_tracks_fixture_and_check_contents() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), OK);
        let v1 = Suite::load(&p).unwrap().version;
        std::fs::write(dir.path().join("fixtures/a/x.txt"), "changed").unwrap();
        let v2 = Suite::load(&p).unwrap().version;
        assert_ne!(v2, v1);
        std::fs::create_dir_all(dir.path().join("checks")).unwrap();
        std::fs::write(dir.path().join("checks/c.sh"), "exit 0").unwrap();
        assert_ne!(Suite::load(&p).unwrap().version, v2);
    }

    #[test]
    fn rejects_invalid_suites() {
        let cases = [
            ("duplicate case id", OK.replace("id: two", "id: one")),
            ("either `fixture` or `repo`", OK.replace("fixture: fixtures/a", "fixture: fixtures/a, repo: .")),
            ("cannot be used with `fixture`", OK.replace("fixture: fixtures/a", "fixture: fixtures/a, base_ref: x")),
            ("must be [a-z0-9_-]+", OK.replace("id: one", "id: One")),
            ("fixture", OK.replace("fixtures/a", "fixtures/missing")),
            ("unknown field", OK.replace("check_timeout_min: 2", "timeout: 2")),
            ("factory", OK.replace("factory: factory.yaml", "factory: nope.yaml")),
        ];
        for (needle, yaml) in cases {
            let dir = tempfile::tempdir().unwrap();
            let err = format!("{:#}", Suite::load(&write(dir.path(), &yaml)).unwrap_err());
            assert!(err.contains(needle), "expected '{needle}' in: {err}");
        }
    }
}
