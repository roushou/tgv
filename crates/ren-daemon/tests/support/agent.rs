//! A scripted agent host: each "process" is an in-memory Codex app server
//! that opens a thread, asks one file-change approval per turn, and finishes
//! the turn once it is answered. No real client, network, or credentials.

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
        _plan: &LaunchPlan,
        _workdir: &Workdir,
        _tools: &RunTools,
    ) -> io::Result<AgentProcess> {
        let (stdin, agent_in) = tokio::io::duplex(64 * 1024);
        let (agent_out, stdout) = tokio::io::duplex(64 * 1024);
        let (exit_tx, exit_rx) = oneshot::channel();
        let (kill, signal) = KillSwitch::pair();
        tokio::spawn(serve(agent_in, agent_out, signal, exit_tx));
        Ok(AgentProcess {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            exit: Box::pin(async move { exit_rx.await.unwrap_or(ProcessExit::Unknown) }),
            kill,
        })
    }
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
