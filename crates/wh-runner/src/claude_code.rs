//! Adapter that runs Claude Code headless:
//! `claude -p <prompt> --output-format stream-json --verbose ...`
//! and turns its JSON line stream into [`Signal`]s and a final [`Outcome`].

use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc::UnboundedSender,
};
use wh_core::{FailureClass, session::TokenUsage};

use crate::{
    docker,
    runtime::{AgentRequest, AgentRuntime, Outcome, OutcomeStatus, SessionMode, Signal},
};

pub struct ClaudeCode {
    pub bin: PathBuf,
    /// `docker` binary, used for sessions with a container.
    pub docker_bin: PathBuf,
    /// Claude Code binary name/path inside agent images.
    pub container_bin: String,
}

impl ClaudeCode {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into(), docker_bin: "docker".into(), container_bin: "claude".into() }
    }

    /// Claude Code arguments (without the binary). Pure.
    pub fn args(req: &AgentRequest) -> Vec<OsString> {
        let mut a: Vec<OsString> = Vec::new();
        let mut push = |s: &str| a.push(s.into());
        push("-p");
        push(&req.prompt);
        for s in ["--output-format", "stream-json", "--verbose"] {
            push(s);
        }
        match &req.session {
            SessionMode::Ephemeral => push("--no-session-persistence"),
            SessionMode::Persist => {}
            SessionMode::Resume(id) => {
                push("--resume");
                push(id);
            }
        }
        if let Some(m) = &req.model {
            push("--model");
            push(m);
        }
        if let Some(b) = req.budget_usd {
            push("--max-budget-usd");
            push(&b.to_string());
        }
        if let Some(p) = &req.permission_mode {
            push("--permission-mode");
            push(p);
        }
        if !req.allowed_tools.is_empty() {
            push("--allowed-tools");
            for t in &req.allowed_tools {
                push(t);
            }
        }
        match req.tools.as_deref() {
            Some([]) => {
                push("--tools");
                push("");
            }
            Some(tools) => {
                push("--tools");
                push(&tools.join(","));
            }
            None => {}
        }
        if !req.with_user_mcp {
            push("--strict-mcp-config");
        }
        a
    }

    fn command(&self, req: &AgentRequest) -> Command {
        self.command_inheriting(req, std::env::vars_os().map(|(name, _)| name))
    }

    /// `inherited` names the variables the child would get from this process.
    fn command_inheriting(&self, req: &AgentRequest, inherited: impl IntoIterator<Item = OsString>) -> Command {
        let mut cmd = match &req.container {
            None => {
                let mut c = Command::new(&self.bin);
                c.args(Self::args(req)).current_dir(&req.workdir);
                c
            }
            Some(container) => {
                let mut inner: Vec<OsString> = vec![self.container_bin.clone().into()];
                inner.extend(Self::args(req));
                let mut container = container.clone();
                for name in req.env.keys() {
                    if !container.config.env.contains(name) {
                        container.config.env.push(name.clone());
                    }
                }
                let mut c = Command::new(&self.docker_bin);
                c.args(docker::run_args(&container, &inner));
                c
            }
        };
        // Before `envs`, so a factory `env:` can still set DATABASE_URL explicitly.
        strip_names(&mut cmd, inherited);
        cmd.envs(&req.env)
            .env_remove("CLAUDECODE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    fn launcher(&self, req: &AgentRequest) -> &Path {
        if req.container.is_some() { &self.docker_bin } else { &self.bin }
    }
}

/// Whether an inherited variable must never reach agents or agent-written code:
/// `DATABASE_URL` is the database the running WorkHub uses, `WH_AGENT_DB_*` can
/// create and drop databases on that server.
fn is_withheld(name: &str) -> bool {
    name == "DATABASE_URL" || name.starts_with(wh_core::AGENT_DB_ENV_PREFIX)
}

/// Remove the withheld variables from what `cmd` inherits from this process. Call
/// it before setting the command's own variables.
pub fn strip_live_env(cmd: &mut Command) {
    strip_names(cmd, std::env::vars_os().map(|(name, _)| name));
}

fn strip_names(cmd: &mut Command, inherited: impl IntoIterator<Item = OsString>) {
    cmd.env_remove("DATABASE_URL");
    for name in inherited {
        if let Some(name) = name.to_str()
            && is_withheld(name)
        {
            cmd.env_remove(name);
        }
    }
}

impl AgentRuntime for ClaudeCode {
    fn name(&self) -> &str {
        "claude-code"
    }

    async fn run(&self, req: &AgentRequest, signals: UnboundedSender<Signal>) -> Outcome {
        let started = Instant::now();
        let mut child = match self.command(req).spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("failed to start {}: {e}", self.launcher(req).display());
                return Outcome::failed(FailureClass::EnvSetup, msg, false, started.elapsed());
            }
        };
        let mut guard =
            docker::Guard { docker_bin: self.docker_bin.clone(), name: req.container.as_ref().map(|c| c.name.clone()) };
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut buf).await;
            buf
        });

        let mut transcript = match File::create(&req.transcript_path).await {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!(path = %req.transcript_path.display(), "cannot write transcript: {e}");
                None
            }
        };
        let mut parser = StreamParser::default();
        let read = async {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(f) = transcript.as_mut() {
                    let _ = f.write_all(line.as_bytes()).await;
                    let _ = f.write_all(b"\n").await;
                }
                for s in parser.feed(&line) {
                    let _ = signals.send(s);
                }
            }
        };
        let timed_out = tokio::time::timeout(req.timeout, read).await.is_err();
        if let Some(f) = transcript.as_mut() {
            let _ = f.flush().await;
        }
        if timed_out {
            let _ = child.kill().await;
            if let Some(name) = guard.name.take() {
                docker::remove(&self.docker_bin, &name).await;
            }
            let mut o = parser.outcome().unwrap_or_else(|| Outcome::failed(FailureClass::Timeout, "", true, Duration::ZERO));
            o.status = OutcomeStatus::Failed {
                class: FailureClass::Timeout,
                message: format!("timed out after {}s", req.timeout.as_secs()),
                retryable: true,
            };
            o.duration_ms = started.elapsed().as_millis() as u64;
            return o;
        }

        let status = child.wait().await;
        guard.name = None; // `--rm` removes a container whose process exited
        let stderr = stderr_task.await.unwrap_or_default();
        let code = status.as_ref().ok().and_then(|s| s.code());
        if req.container.is_some() && parser.outcome().is_none() && matches!(code, Some(125..=127)) {
            // 125: docker itself failed (no such image, bad flag, daemon down);
            // 126/127: the agent binary is missing or not executable in the image.
            let msg = format!("container failed to start (exit {}): {}", code.unwrap_or(0), tail(&stderr, 2000));
            return Outcome::failed(FailureClass::EnvSetup, msg, false, started.elapsed());
        }
        parser.outcome().unwrap_or_else(|| {
            let msg = format!(
                "claude exited ({}) without a result. stderr: {}",
                status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()),
                tail(&stderr, 2000)
            );
            if is_usage_limit(&stderr) {
                return Outcome::failed(FailureClass::UsageLimit, msg, false, started.elapsed());
            }
            // Usually transient (API overload, crashed process): worth a retry.
            Outcome::failed(FailureClass::Unknown, msg, true, started.elapsed())
        })
    }
}

/// Claude Code's message when the account's usage window is exhausted, e.g.
/// "You've hit your 5-hour limit · resets 12:40am". Retrying right away only
/// burns attempts, so these failures are not retryable.
pub fn is_usage_limit(message: &str) -> bool {
    let m = message.to_lowercase();
    (m.contains("hit your") && m.contains("limit")) || m.contains("usage limit")
}

/// Incremental parser for Claude Code's `stream-json` output. Pure, so it can be
/// unit-tested with recorded lines.
#[derive(Default)]
pub struct StreamParser {
    pending_tools: HashMap<String, (String, Instant)>,
    result: Option<Value>,
}

impl StreamParser {
    pub fn feed(&mut self, line: &str) -> Vec<Signal> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => out.push(Signal::Ready {
                runtime_session_id: v["session_id"].as_str().map(str::to_owned),
                model: v["model"].as_str().map(str::to_owned),
            }),
            Some("assistant") => {
                for block in content_blocks(&v, "tool_use") {
                    if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) {
                        self.pending_tools.insert(id.to_owned(), (name.to_owned(), Instant::now()));
                    }
                }
            }
            Some("user") => {
                for block in content_blocks(&v, "tool_result") {
                    let Some(id) = block["tool_use_id"].as_str() else { continue };
                    if let Some((tool, at)) = self.pending_tools.remove(id) {
                        out.push(Signal::ToolCalled {
                            tool,
                            ok: !block["is_error"].as_bool().unwrap_or(false),
                            duration_ms: at.elapsed().as_millis() as u64,
                        });
                    }
                }
            }
            Some("result") => self.result = Some(v),
            _ => {}
        }
        out
    }

    /// The final outcome, available once the `result` message has been seen.
    pub fn outcome(&self) -> Option<Outcome> {
        let r = self.result.as_ref()?;
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let usage = TokenUsage {
            input_tokens: n(&r["usage"]["input_tokens"]),
            output_tokens: n(&r["usage"]["output_tokens"]),
            cache_read_tokens: n(&r["usage"]["cache_read_input_tokens"]),
            cache_write_tokens: n(&r["usage"]["cache_creation_input_tokens"]),
        };
        let subtype = r["subtype"].as_str().unwrap_or("");
        let summary = r["result"].as_str().map(|s| truncate(s, 4000));
        let status = if subtype == "success" && !r["is_error"].as_bool().unwrap_or(false) {
            OutcomeStatus::Succeeded
        } else if subtype.contains("budget") || subtype.contains("max_turns") {
            OutcomeStatus::Failed {
                class: FailureClass::BudgetExceeded,
                message: subtype.to_owned(),
                retryable: false,
            }
        } else {
            let message = summary.clone().unwrap_or_else(|| subtype.to_owned());
            if is_usage_limit(&message) {
                OutcomeStatus::Failed { class: FailureClass::UsageLimit, message, retryable: false }
            } else {
                OutcomeStatus::Failed { class: FailureClass::Unknown, message, retryable: true }
            }
        };
        Some(Outcome {
            status,
            usage,
            cost_usd: r["total_cost_usd"].as_f64().unwrap_or(0.0),
            num_turns: n(&r["num_turns"]) as u32,
            duration_ms: n(&r["duration_ms"]),
            summary,
        })
    }
}

fn content_blocks<'a>(v: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    v["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |b| b["type"] == kind)
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn tail(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(max_chars)).collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsStr};

    use wh_core::{DockerSandbox, SessionId};

    use super::*;
    use crate::docker::Container;

    #[test]
    fn env_reaches_the_agent_and_its_container_by_name() {
        let mut req = AgentRequest {
            prompt: "p".into(),
            workdir: "/w".into(),
            transcript_path: "/t".into(),
            model: None,
            budget_usd: None,
            permission_mode: None,
            allowed_tools: vec![],
            with_user_mcp: false,
            timeout: Duration::from_secs(1),
            tools: None,
            container: None,
            env: BTreeMap::from([("CARGO_TARGET_DIR".to_string(), "/cache/target".to_string())]),
            session: SessionMode::Ephemeral,
        };
        let cc = ClaudeCode::new("claude");
        let has_env = |cmd: &Command| {
            cmd.as_std().get_envs().any(|(k, v)| k == OsStr::new("CARGO_TARGET_DIR") && v == Some(OsStr::new("/cache/target")))
        };
        assert!(has_env(&cc.command(&req)));

        req.container = Some(Container {
            name: "wh-x".into(),
            session: SessionId::new(),
            config: DockerSandbox::new("img"),
            workdir: "/w".into(),
            mounts: vec![],
            user: None,
            home_dir: "/h".into(),
        });
        let cmd = cc.command(&req);
        assert!(has_env(&cmd), "docker copies the value from its own environment");
        let args: Vec<String> = cmd.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.windows(2).any(|w| w == ["--env", "CARGO_TARGET_DIR"]), "{args:?}");
        assert!(!args.iter().any(|a| a.contains("/cache/target")), "values stay off the command line");
    }

    const STREAM: &[&str] = &[
        r#"{"type":"system","subtype":"init","session_id":"abc","model":"claude-haiku-5-5","tools":["Bash"]}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Bash","input":{}},{"type":"tool_use","id":"t2","name":"Write","input":{}}]}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"},{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":"denied"}]}}"#,
        "not json",
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":4200,"num_turns":3,"total_cost_usd":0.0123,"result":"done","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":300,"cache_creation_input_tokens":40}}"#,
    ];

    #[test]
    fn the_agent_never_inherits_the_live_database_url() {
        let mut req = AgentRequest {
            prompt: "p".into(),
            workdir: "/w".into(),
            transcript_path: "/t".into(),
            model: None,
            budget_usd: None,
            permission_mode: None,
            allowed_tools: vec![],
            with_user_mcp: false,
            timeout: Duration::from_secs(1),
            tools: None,
            container: None,
            env: BTreeMap::new(),
            session: SessionMode::Ephemeral,
        };
        let cc = ClaudeCode::new("claude");
        // `Some(None)` = removed from the inherited environment, `None` = inherited as is.
        let var = |cmd: &Command, name: &str| {
            cmd.as_std().get_envs().find(|(k, _)| *k == OsStr::new(name)).map(|(_, v)| v.map(OsStr::to_owned))
        };
        let database_url = |cmd: &Command| var(cmd, "DATABASE_URL");
        // Passed in rather than set on this process, which other tests share.
        let admin_url_is_stripped = |req: &AgentRequest| {
            let cmd = cc.command_inheriting(req, ["WH_AGENT_DB_ADMIN_URL", "PATH"].map(OsString::from));
            assert_eq!(var(&cmd, "WH_AGENT_DB_ADMIN_URL"), Some(None));
            assert_eq!(var(&cmd, "PATH"), None);
        };
        assert_eq!(database_url(&cc.command(&req)), Some(None));
        admin_url_is_stripped(&req);
        req.container = Some(Container {
            name: "wh-x".into(),
            session: SessionId::new(),
            config: DockerSandbox::new("img"),
            workdir: "/w".into(),
            mounts: vec![],
            user: None,
            home_dir: "/h".into(),
        });
        assert_eq!(database_url(&cc.command(&req)), Some(None), "docker copies from the client's environment");
        admin_url_is_stripped(&req);

        req.env.insert("DATABASE_URL".into(), "postgres://task".into());
        assert_eq!(database_url(&cc.command(&req)), Some(Some("postgres://task".into())));
    }

    #[test]
    fn parses_signals_and_outcome() {
        let mut p = StreamParser::default();
        let signals: Vec<Signal> = STREAM.iter().flat_map(|l| p.feed(l)).collect();
        assert_eq!(signals[0], Signal::Ready {
            runtime_session_id: Some("abc".into()),
            model: Some("claude-haiku-5-5".into())
        });
        let tools: Vec<(String, bool)> = signals[1..]
            .iter()
            .map(|s| match s {
                Signal::ToolCalled { tool, ok, .. } => (tool.clone(), *ok),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(tools, vec![("Bash".into(), true), ("Write".into(), false)]);

        let o = p.outcome().unwrap();
        assert_eq!(o.status, OutcomeStatus::Succeeded);
        assert_eq!(o.num_turns, 3);
        assert_eq!(o.duration_ms, 4200);
        assert_eq!(o.cost_usd, 0.0123);
        assert_eq!(o.usage.cache_read_tokens, 300);
        assert_eq!(o.usage.cache_write_tokens, 40);
    }

    #[test]
    fn budget_errors_are_classified() {
        let mut p = StreamParser::default();
        p.feed(r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"total_cost_usd":1.0}"#);
        assert!(matches!(
            p.outcome().unwrap().status,
            OutcomeStatus::Failed { class: FailureClass::BudgetExceeded, .. }
        ));
    }

    #[test]
    fn usage_limit_is_not_retried() {
        let mut p = StreamParser::default();
        p.feed(r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your 5-hour limit · resets 12:40am (Asia/Ho_Chi_Minh)","total_cost_usd":0}"#);
        assert!(matches!(
            p.outcome().unwrap().status,
            OutcomeStatus::Failed { class: FailureClass::UsageLimit, retryable: false, .. }
        ));
        assert!(!is_usage_limit("error: test failed"));
    }

    #[test]
    fn no_result_means_no_outcome() {
        let mut p = StreamParser::default();
        p.feed(STREAM[0]);
        assert!(p.outcome().is_none());
    }

    #[test]
    fn session_mode_selects_persistence_and_resume_flags() {
        let args = |session: SessionMode| -> Vec<String> {
            let req = AgentRequest {
                prompt: "p".into(),
                workdir: "/w".into(),
                transcript_path: "/t".into(),
                model: Some("m".into()),
                budget_usd: Some(1.0),
                permission_mode: None,
                allowed_tools: vec![],
                with_user_mcp: false,
                timeout: Duration::from_secs(1),
                tools: None,
                container: None,
                env: BTreeMap::new(),
                session,
            };
            ClaudeCode::args(&req).iter().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        const EPHEMERAL: &str = "--no-session-persistence";

        let ephemeral = args(SessionMode::default());
        assert!(ephemeral.iter().any(|a| a == EPHEMERAL) && !ephemeral.iter().any(|a| a == "--resume"));

        let persist = args(SessionMode::Persist);
        assert!(!persist.iter().any(|a| a == EPHEMERAL || a == "--resume"), "{persist:?}");

        let resume = args(SessionMode::Resume("abc".into()));
        assert!(resume.windows(2).any(|w| w == ["--resume", "abc"]), "{resume:?}");
        assert!(!resume.iter().any(|a| a == EPHEMERAL));
        let others: Vec<_> = resume.iter().filter(|a| *a != "--resume" && *a != "abc").collect();
        assert_eq!(others, persist.iter().collect::<Vec<_>>(), "other flags are unchanged");
    }
}
