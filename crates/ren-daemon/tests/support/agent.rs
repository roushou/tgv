//! A scripted agent host: each "process" is an in-memory Codex app server
//! that opens a thread, asks one file-change approval per turn, and finishes
//! the turn once it is answered; or, for Claude Code's own `auth` commands,
//! a scripted status (always signed out) and login (a link, then a code:
//! `good` exits 0, anything else fails as Claude Code does); or, for Codex's
//! `login`, a status (signed out) and a device login approved at once. No
//! real client, network, or credentials.

use ren_harness::launch::LaunchPlan;
use ren_supervisor::{
    AgentHost, AgentProcess, FoundPath, KillSignal, KillSwitch, Look, PathKind, ProcessExit,
    RunTools, WalkLimits, Workdir,
};
use serde_json::{Value, json};
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    sync::oneshot,
};

pub const THREAD: &str = "0190f3a0-0000-7000-8000-0000000000aa";
const TURN: &str = "0190f3a0-0000-7000-8000-0000000000bb";

/// Resolves every path to itself and starts scripted agents.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScriptedHost;

impl AgentHost for ScriptedHost {
    fn finds(&self, _program: &str) -> bool {
        true
    }

    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }

    fn look(&self, dir: &Path) -> io::Result<Look> {
        Ok(match dir.to_str() {
            Some("/work") => Look {
                folders: vec!["acme".into(), "web".into()],
                git: None,
            },
            Some("/work/acme") => Look {
                folders: vec![],
                git: Some(Some("ref: refs/heads/main".into())),
            },
            Some(_) | None => Look::default(),
        })
    }

    fn files(&self, _dir: &Path, _limits: WalkLimits) -> io::Result<Vec<FoundPath>> {
        Ok(vec![
            FoundPath {
                path: "src".into(),
                kind: PathKind::Directory,
            },
            FoundPath {
                path: "src/upload.rs".into(),
                kind: PathKind::File,
            },
        ])
    }

    fn make_dir(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn make_scratch(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn tools(&self) -> RunTools {
        RunTools::Empty
    }

    fn spawn(
        &self,
        plan: &LaunchPlan,
        _workdir: &Workdir,
        _tools: &RunTools,
    ) -> io::Result<AgentProcess> {
        let (stdin, agent_in) = tokio::io::duplex(64 * 1024);
        let (agent_out, stdout) = tokio::io::duplex(64 * 1024);
        let (exit_tx, exit_rx) = oneshot::channel();
        let (kill, signal) = KillSwitch::pair();
        let arguments: Vec<String> = plan
            .command(None, &[])
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        match arguments.first().map(String::as_str) {
            Some("auth") => tokio::spawn(auth(arguments, agent_in, agent_out, signal, exit_tx)),
            Some("login") => tokio::spawn(login(arguments, agent_out, signal, exit_tx)),
            _ => tokio::spawn(serve(agent_in, agent_out, signal, exit_tx)),
        };
        Ok(AgentProcess {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            exit: Box::pin(async move { exit_rx.await.unwrap_or(ProcessExit::Unknown) }),
            kill,
        })
    }

    fn spawn_with_stderr(
        &self,
        plan: &LaunchPlan,
        workdir: &Workdir,
        tools: &RunTools,
    ) -> io::Result<AgentProcess> {
        // Scripted processes write everything to the one stream.
        self.spawn(plan, workdir, tools)
    }
}

/// Claude Code's `auth status --json` (signed out) or `auth login`.
async fn auth(
    arguments: Vec<String>,
    input: DuplexStream,
    mut out: DuplexStream,
    kill: KillSignal,
    exit: oneshot::Sender<ProcessExit>,
) {
    let script = async {
        if arguments.get(1).map(String::as_str) == Some("status") {
            out.write_all(br#"{"loggedIn": false, "authMethod": "none"}"#)
                .await
                .unwrap();
            return ProcessExit::Code(1);
        }
        out.write_all(
            b"Opening browser to sign in\nIf the browser didn't open, visit: https://login.example.test/authorize?state=acme\nPaste code here if prompted > ",
        )
        .await
        .unwrap();
        let mut lines = BufReader::new(input).lines();
        match lines.next_line().await {
            Ok(Some(code)) if code == "good" => {
                out.write_all(b"Login successful.\n").await.unwrap();
                ProcessExit::Code(0)
            }
            Ok(Some(_)) => {
                out.write_all(b"Login failed: That code wasn't accepted.\n")
                    .await
                    .unwrap();
                ProcessExit::Code(1)
            }
            Ok(None) | Err(_) => ProcessExit::Code(1),
        }
    };
    let status = tokio::select! {
        status = script => status,
        () = kill.fired() => ProcessExit::Signal(9),
    };
    drop(out);
    let _ = exit.send(status);
}

/// Codex's `login status` (always signed out) or `login --device-auth`,
/// which shows its link and code and is approved a moment later.
async fn login(
    arguments: Vec<String>,
    mut out: DuplexStream,
    kill: KillSignal,
    exit: oneshot::Sender<ProcessExit>,
) {
    let script = async {
        if arguments.get(1).map(String::as_str) == Some("status") {
            out.write_all(b"Not logged in\n").await.unwrap();
            return ProcessExit::Code(1);
        }
        out.write_all(
            b"Follow these steps to sign in with ChatGPT using device code authorization:\n\n1. Open this link in your browser and sign in to your account\n   \x1b[94mhttps://login.example.test/codex/device\x1b[0m\n\n2. Enter this one-time code \x1b[90m(expires in 15 minutes)\x1b[0m\n   \x1b[94mACME-12345\x1b[0m\n",
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        out.write_all(b"Successfully logged in\n").await.unwrap();
        ProcessExit::Code(0)
    };
    let status = tokio::select! {
        status = script => status,
        () = kill.fired() => ProcessExit::Signal(9),
    };
    drop(out);
    let _ = exit.send(status);
}

async fn send(out: &mut DuplexStream, message: Value) {
    let mut line = message.to_string().into_bytes();
    line.push(b'\n');
    out.write_all(&line).await.unwrap();
}

async fn serve(
    input: DuplexStream,
    mut out: DuplexStream,
    kill: KillSignal,
    exit: oneshot::Sender<ProcessExit>,
) {
    let mut lines = BufReader::new(input).lines();
    let script = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).unwrap();
            let id = message["id"].clone();
            match message["method"].as_str() {
                Some("initialize") => send(&mut out, json!({ "id": id, "result": {} })).await,
                Some("model/list") => {
                    send(
                        &mut out,
                        json!({ "id": id, "result": { "data": [
                            { "id": "gpt-fast", "model": "gpt-fast", "displayName": "GPT Fast",
                              "description": "Quick.", "isDefault": true, "hidden": false },
                        ] } }),
                    )
                    .await;
                }
                Some("thread/resume") => {
                    let thread = message["params"]["threadId"].clone();
                    send(
                        &mut out,
                        json!({ "id": id, "result": { "thread": { "id": thread } } }),
                    )
                    .await;
                }
                Some("thread/start") => {
                    send(
                        &mut out,
                        json!({ "id": id, "result": { "thread": { "id": THREAD } } }),
                    )
                    .await;
                }
                Some("turn/start") => {
                    let turn = json!({ "id": TURN, "status": "inProgress" });
                    send(&mut out, json!({ "id": id, "result": { "turn": turn } })).await;
                    send(
                        &mut out,
                        json!({ "id": "ap-1", "method": "item/fileChange/requestApproval",
                            "params": { "threadId": THREAD, "turnId": TURN, "itemId": "fc",
                                        "startedAtMs": 1, "reason": "edit src" } }),
                    )
                    .await;
                }
                Some(_) => {}
                None if id == "ap-1" => {
                    send(
                        &mut out,
                        json!({ "method": "item/agentMessage/delta", "params": { "threadId": THREAD,
                            "turnId": TURN, "itemId": "msg", "delta": "Edited." } }),
                    )
                    .await;
                    send(
                        &mut out,
                        json!({ "method": "turn/completed", "params": { "threadId": THREAD,
                            "turn": { "id": TURN, "status": "completed" } } }),
                    )
                    .await;
                }
                None => {}
            }
        }
        ProcessExit::Code(0)
    };
    let status = tokio::select! {
        status = script => status,
        () = kill.fired() => ProcessExit::Signal(9),
    };
    let _ = exit.send(status);
}
