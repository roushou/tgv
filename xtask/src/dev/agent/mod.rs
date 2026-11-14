//! `cargo xtask dev agent`: a fake `codex app-server`, `claude -p`,
//! `pi --mode rpc`, or `opencode acp` for the dev slot's runs. The slot puts
//! a `codex`, a `claude`, a `pi`, and an `opencode` that run it first on the
//! dev daemon's PATH, so the daemon starts and drives them as it would the
//! real clients. Each speaks its client's protocol on stdio ([`script`] for
//! Codex, [`claude`] for Claude Code, [`pi`] for Pi, [`opencode`] for
//! OpenCode) and sends each turn's
//! model call through the dev gateway to the fake provider, as the client
//! would, so the run's traffic is recorded under its session. The fake
//! `claude` also answers `claude auth` ([`auth`]), for the sign-in sheet. It
//! never reads a real client's configuration or credentials.

mod auth;
mod claude;
mod opencode;
mod pi;
mod script;

use super::seed::{Call, Caller, Client, Run, Scenario, Seeder};
use clap::ValueEnum;
use ren_harness::host::HostEnv;
use ren_protocol::sse::{EventName, SseFramer, SseLimits, SseOutput};
use serde_json::Value;
use std::{io::BufRead as _, path::Path};

/// Why the fake agent stopped.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(
        "no gateway: expected a model_providers base_url argument (codex), ANTHROPIC_BASE_URL (claude), REN_PI_GATEWAY (pi), or OPENCODE_CONFIG_CONTENT (opencode)"
    )]
    NoGateway,
    #[error("no conversation: expected --session-id (or --resume for claude)")]
    NoSession,
    #[error("cannot start the agent's runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("stdio failed: {0}")]
    Stdio(#[source] std::io::Error),
    #[error(transparent)]
    Seed(#[from] super::seed::SeedError),
    #[error("Login failed: That code wasn't accepted. The dev slot's code is `acme`.")]
    LoginRefused,
    /// The fake `codex login status`, signed out: it exits 1, as Codex does.
    #[error("signed out")]
    SignedOut,
}

pub use auth::{FakeClaudeAuth, FakeCodexAuth};

/// The client a fake agent plays.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Played {
    Codex,
    Claude,
    Pi,
    #[value(name = "opencode")]
    OpenCode,
}

/// One client's script.
enum Script {
    Codex(script::AgentScript),
    Claude(claude::ClaudeScript),
    Pi(pi::PiScript),
    OpenCode(opencode::OpenCodeScript),
}

/// What a script asks of the shell, whichever client it plays.
enum Step {
    Send(Value),
    Call(Call),
    /// Keep a session of a folder for later processes (the fake opencode's).
    Remember {
        cwd: String,
        session: String,
    },
}

pub struct FakeAgent {
    /// The gateway's root, from where the daemon pointed the client.
    gateway: String,
    script: Script,
    /// Where the fake opencode keeps the sessions it made, by folder.
    sessions: std::path::PathBuf,
}

impl FakeAgent {
    /// `arguments` are those the daemon gave the client.
    pub fn new(
        played: Played,
        env: &HostEnv,
        data_dir: &Path,
        arguments: &[String],
    ) -> Result<Self, AgentError> {
        let own = Run(Seeder::next_run(data_dir)?);
        let sessions = data_dir.join("opencode-sessions.tsv");
        match played {
            Played::Codex => {
                let base = arguments
                    .iter()
                    .find_map(|argument| {
                        let rest = argument.split_once("base_url=\"")?.1;
                        Some(rest.split_once('"')?.0.to_owned())
                    })
                    .ok_or(AgentError::NoGateway)?;
                Ok(Self {
                    gateway: Self::root(&base, "/codex"),
                    script: Script::Codex(script::AgentScript::new(own)),
                    sessions,
                })
            }
            Played::Claude => {
                let base = env.get("ANTHROPIC_BASE_URL").ok_or(AgentError::NoGateway)?;
                let cwd = std::env::current_dir()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default();
                let script =
                    claude::ClaudeScript::new(own, arguments, cwd).ok_or(AgentError::NoSession)?;
                Ok(Self {
                    gateway: Self::root(base, "/anthropic"),
                    script: Script::Claude(script),
                    sessions,
                })
            }
            Played::Pi => {
                let base = env.get("REN_PI_GATEWAY").ok_or(AgentError::NoGateway)?;
                let script = pi::PiScript::new(own, arguments).ok_or(AgentError::NoSession)?;
                Ok(Self {
                    gateway: base.trim_end_matches('/').to_owned(),
                    script: Script::Pi(script),
                    sessions,
                })
            }
            Played::OpenCode => {
                let config = env
                    .get("OPENCODE_CONFIG_CONTENT")
                    .ok_or(AgentError::NoGateway)?;
                let base = serde_json::from_str::<Value>(config)
                    .ok()
                    .and_then(|config| {
                        config["provider"]["openai"]["options"]["baseURL"]
                            .as_str()
                            .map(str::to_owned)
                    })
                    .ok_or(AgentError::NoGateway)?;
                // The folder's sessions earlier processes made: one per line.
                let known = std::fs::read_to_string(&sessions)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| line.split_once('\t'))
                    .map(|(cwd, session)| (cwd.to_owned(), session.to_owned()))
                    .collect();
                Ok(Self {
                    gateway: Self::root(&base, "/openai/v1"),
                    script: Script::OpenCode(opencode::OpenCodeScript::new(
                        own,
                        known,
                        Some(config),
                    )),
                    sessions,
                })
            }
        }
    }

    fn root(base: &str, mount: &str) -> String {
        base.trim_end_matches('/')
            .strip_suffix(mount)
            .unwrap_or(base)
            .trim_end_matches('/')
            .to_owned()
    }

    /// Serve stdin until it closes.
    pub fn run(mut self) -> Result<(), AgentError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(AgentError::Runtime)?;
        let http = reqwest::Client::new();
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line.map_err(AgentError::Stdio)?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let mut pending = self.receive(&message);
            while !pending.is_empty() {
                let mut next = Vec::new();
                for step in pending {
                    match step {
                        Step::Send(value) => Self::write(&mut stdout, &value)?,
                        Step::Remember { cwd, session } => self.remember(&cwd, &session)?,
                        Step::Call(call) => {
                            let reply = runtime.block_on(self.call(&http, &mut stdout, call));
                            next.extend(self.replied(reply));
                        }
                    }
                }
                pending = next;
            }
        }
        Ok(())
    }

    fn receive(&mut self, message: &Value) -> Vec<Step> {
        match &mut self.script {
            Script::Codex(script) => script
                .receive(message)
                .into_iter()
                .map(Self::codex_step)
                .collect(),
            Script::Claude(script) => script
                .receive(message)
                .into_iter()
                .map(Self::claude_step)
                .collect(),
            Script::Pi(script) => script
                .receive(message)
                .into_iter()
                .map(Self::pi_step)
                .collect(),
            Script::OpenCode(script) => script
                .receive(message)
                .into_iter()
                .map(Self::opencode_step)
                .collect(),
        }
    }

    fn replied(&mut self, reply: Result<String, String>) -> Vec<Step> {
        match &mut self.script {
            Script::Codex(script) => script
                .replied(reply)
                .into_iter()
                .map(Self::codex_step)
                .collect(),
            Script::Claude(script) => script
                .replied(reply)
                .into_iter()
                .map(Self::claude_step)
                .collect(),
            Script::Pi(script) => script
                .replied(reply)
                .into_iter()
                .map(Self::pi_step)
                .collect(),
            Script::OpenCode(script) => script
                .replied(reply)
                .into_iter()
                .map(Self::opencode_step)
                .collect(),
        }
    }

    /// Keep a session the fake opencode made, for later processes.
    fn remember(&self, cwd: &str, session: &str) -> Result<(), AgentError> {
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.sessions)
            .and_then(|mut file| writeln!(file, "{cwd}\t{session}"))
            .map_err(AgentError::Stdio)
    }

    fn codex_step(out: script::Out) -> Step {
        match out {
            script::Out::Send(value) => Step::Send(value),
            script::Out::Call {
                session,
                numbers,
                prompt,
            } => Step::Call(Scenario::step_call(
                Client::Codex,
                &Caller::Unidentified,
                session,
                numbers,
                "gpt-5.5-codex",
                &prompt,
                0,
            )),
        }
    }

    fn claude_step(out: claude::Out) -> Step {
        match out {
            claude::Out::Send(value) => Step::Send(value),
            claude::Out::Call(call) => Step::Call(call),
        }
    }

    fn pi_step(out: pi::Out) -> Step {
        match out {
            pi::Out::Send(value) => Step::Send(value),
            pi::Out::Call(call) => Step::Call(call),
        }
    }

    fn opencode_step(out: opencode::Out) -> Step {
        match out {
            opencode::Out::Send(value) => Step::Send(value),
            opencode::Out::Call(call) => Step::Call(call),
            opencode::Out::Remember { cwd, session } => Step::Remember { cwd, session },
        }
    }

    fn write(stdout: &mut impl std::io::Write, value: &Value) -> Result<(), AgentError> {
        let mut line = value.to_string();
        line.push('\n');
        stdout
            .write_all(line.as_bytes())
            .and_then(|()| stdout.flush())
            .map_err(AgentError::Stdio)
    }

    /// The text a reply's SSE event adds, in the wire the client speaks.
    fn text(&self, kind: &str, event: &Value) -> Option<String> {
        let text = match (&self.script, kind) {
            (Script::Codex(_) | Script::OpenCode(_), "response.output_text.delta") => {
                event["delta"].as_str()
            }
            (Script::Claude(_) | Script::Pi(_), "content_block_delta") => {
                event["delta"]["text"].as_str()
            }
            (Script::Codex(_) | Script::Claude(_) | Script::Pi(_) | Script::OpenCode(_), _) => None,
        };
        text.map(str::to_owned)
    }

    fn delta(&self, text: &str) -> Value {
        match &self.script {
            Script::Codex(script) => script.delta(text),
            Script::Claude(script) => script.delta(text),
            Script::Pi(script) => script.delta(text),
            Script::OpenCode(script) => script.delta(text),
        }
    }

    /// Send the turn's model call as the client would and stream its reply text.
    async fn call(
        &self,
        http: &reqwest::Client,
        stdout: &mut impl std::io::Write,
        call: Call,
    ) -> Result<String, String> {
        let mut request = http
            .post(format!("{}{}", self.gateway, call.path))
            .body(call.body);
        for (name, value) in &call.headers {
            request = request.header(name, value);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| "cannot reach the dev gateway".to_owned())?;
        if !response.status().is_success() {
            return Err(format!("the model call answered {}", response.status()));
        }
        let mut framer = SseFramer::new(SseLimits::default());
        let mut reply = String::new();
        let mut written = Ok(());
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "the reply stream broke".to_owned())?
        {
            framer.feed(&chunk, |output| {
                let SseOutput::Event { name, data } = output else {
                    return;
                };
                let Ok(event) = serde_json::from_slice::<Value>(data) else {
                    return;
                };
                let kind = match name {
                    EventName::Present(name) => name,
                    EventName::Absent | EventName::Invalid => {
                        event["type"].as_str().unwrap_or_default()
                    }
                };
                if let Some(text) = self.text(kind, &event) {
                    reply.push_str(&text);
                    if written.is_ok() {
                        written = Self::write(stdout, &self.delta(&text));
                    }
                }
            });
        }
        written.map_err(|error| error.to_string())?;
        Ok(reply)
    }
}
