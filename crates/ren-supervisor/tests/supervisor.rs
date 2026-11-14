//! The supervisor with an in-memory host: each "process" is a scripted agent
//! that speaks Codex's app-server protocol, or Claude Code's stream-json, over
//! in-memory pipes. No real client, network, or credentials.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    reason = "integration tests are a shell: helpers outside #[test] fail loudly, and scripted agents are spawned tasks"
)]

use ren_depot::toolset::Generation;
use ren_harness::{
    Harness,
    drive::{ApprovalId, Decision, Prompt},
    endpoint::GatewayEndpoint,
    launch::LaunchPlan,
    lines::LineLimit,
};
use ren_protocol::label::Stored;
use ren_recorder::{
    CaptureOptions, Recorder, RecorderSetup,
    capture::RunEnd,
    id::SequentialIds,
    retention::RetentionPolicy,
    secrets::SecretScanner,
    time::{ManualClock, UnixMillis},
};
use ren_store::{Reader, Store, query::PageLimit, view::RunView};
use ren_supervisor::{
    AgentHost, AgentProcess, CommandError, Conversation, FileQuery, FoundPath, KillSignal,
    KillSwitch, Look, PathKind, ProcessExit, RunCommand, RunRefused, RunState, RunTools, RunUpdate,
    StartError, Supervisor, SupervisorOptions, WalkLimits, Workdir,
};
use serde_json::{Value, json};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    sync::{broadcast, oneshot},
};

const THREAD: &str = "0190f3a0-0000-7000-8000-000000000001";
const TURN: &str = "0190f3a0-0000-7000-8000-000000000002";

/// How the scripted agent behaves.
#[derive(Clone, Copy)]
enum Script {
    /// A well-behaved Codex: opens a thread, runs turns, asks one approval.
    Codex,
    /// Refuses `thread/start`.
    RefusesOpening,
    /// Exits with code 3 right after `initialize`.
    ExitsEarly,
    /// A well-behaved Claude Code in stream-json print mode: echoes each
    /// prompt, asks to run one command, and ends the turn once answered.
    Claude,
    /// A well-behaved Pi in RPC mode: reports its session, runs each prompt
    /// to a reply, and settles.
    Pi,
    /// OpenCode in ACP mode: opens a session whose default model is a free
    /// one, and answers every other request.
    OpenCode,
}

/// The arguments and working directory of one spawned agent.
type Spawned = (Vec<String>, PathBuf);

#[derive(Clone)]
struct FakeHost {
    script: Script,
    spawn_fails: bool,
    spawned: Arc<Mutex<Vec<Spawned>>>,
    /// The tools each spawned agent was given, in order.
    tools_given: Arc<Mutex<Vec<RunTools>>>,
    /// Folders made, scratch runs' marked as repositories.
    made: Arc<Mutex<Vec<(PathBuf, bool)>>>,
    /// Folders walked for a file search, in order.
    walked: Arc<Mutex<Vec<PathBuf>>>,
}

impl FakeHost {
    fn new(script: Script) -> Self {
        Self {
            script,
            spawn_fails: false,
            spawned: Arc::default(),
            tools_given: Arc::default(),
            made: Arc::default(),
            walked: Arc::default(),
        }
    }
}

impl AgentHost for FakeHost {
    fn finds(&self, program: &str) -> bool {
        // Codex is installed here; Claude Code is not.
        program == "codex"
    }

    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        match path.to_str() {
            // A link that leads out of the roots.
            Some("/work/escape") => Ok(PathBuf::from("/etc")),
            Some(path)
                if ["/work", "/scratch", "/home/op/.config"]
                    .iter()
                    .any(|bound| path.starts_with(bound))
                    || path == "/elsewhere" =>
            {
                Ok(path.into())
            }
            Some(_) | None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn look(&self, dir: &Path) -> io::Result<Look> {
        Ok(match dir.to_str() {
            Some("/work") => Look {
                folders: vec!["acme".into(), "notes".into()],
                git: None,
            },
            Some("/work/acme") => Look {
                folders: vec!["src".into()],
                git: Some(Some("ref: refs/heads/main".into())),
            },
            Some(_) | None => Look::default(),
        })
    }

    fn files(&self, dir: &Path, _: WalkLimits) -> io::Result<Vec<FoundPath>> {
        self.walked.lock().unwrap().push(dir.to_path_buf());
        Ok(["src", "src/upload.rs", "src/lease.rs", "README.md"]
            .iter()
            .map(|path| FoundPath {
                path: (*path).into(),
                kind: if path.contains('.') {
                    PathKind::File
                } else {
                    PathKind::Directory
                },
            })
            .collect())
    }

    fn make_dir(&self, dir: &Path) -> io::Result<()> {
        self.made.lock().unwrap().push((dir.to_path_buf(), false));
        Ok(())
    }

    fn make_scratch(&self, dir: &Path) -> io::Result<()> {
        self.made.lock().unwrap().push((dir.to_path_buf(), true));
        Ok(())
    }

    fn tools(&self) -> RunTools {
        RunTools::Toolset {
            generation: Generation::from(4),
            bin: "/data/tools/generations/4/bin".into(),
        }
    }

    fn spawn(
        &self,
        plan: &LaunchPlan,
        workdir: &Workdir,
        tools: &RunTools,
    ) -> io::Result<AgentProcess> {
        if self.spawn_fails {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        self.tools_given.lock().unwrap().push(tools.clone());
        let command = plan.command(None, &[]);
        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        self.spawned
            .lock()
            .unwrap()
            .push((arguments.clone(), workdir.as_path().to_path_buf()));
        let (stdin, agent_in) = tokio::io::duplex(64 * 1024);
        let (agent_out, stdout) = tokio::io::duplex(64 * 1024);
        let (exit_tx, exit_rx) = oneshot::channel();
        let (kill, signal) = KillSwitch::pair();
        match self.script {
            Script::Claude => tokio::spawn(claude(agent_in, agent_out, signal, exit_tx)),
            Script::Pi => {
                let session = arguments
                    .iter()
                    .position(|argument| argument == "--session-id")
                    .and_then(|at| arguments.get(at + 1))
                    .cloned()
                    .unwrap_or_default();
                tokio::spawn(pi(session, agent_in, agent_out, signal, exit_tx))
            }
            Script::OpenCode => tokio::spawn(opencode(agent_in, agent_out, signal, exit_tx)),
            Script::Codex | Script::RefusesOpening | Script::ExitsEarly => {
                tokio::spawn(agent(self.script, agent_in, agent_out, signal, exit_tx))
            }
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
        let command = plan.command(None, &[]);
        let login = command
            .get_args()
            .next()
            .is_some_and(|argument| argument == "login");
        if !login {
            return self.spawn(plan, workdir, tools);
        }
        // Codex's `login status`, signed out; not counted as an agent.
        let (stdin, _agent_in) = tokio::io::duplex(1024);
        let (mut agent_out, stdout) = tokio::io::duplex(1024);
        let (kill, _signal) = KillSwitch::pair();
        tokio::spawn(async move {
            agent_out.write_all(b"Not logged in\n").await.unwrap();
        });
        Ok(AgentProcess {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            exit: Box::pin(async { ProcessExit::Code(1) }),
            kill,
        })
    }

    fn spawn_in_terminal(
        &self,
        plan: &LaunchPlan,
        workdir: &Workdir,
        tools: &RunTools,
    ) -> io::Result<AgentProcess> {
        self.spawn_with_stderr(plan, workdir, tools)
    }

    fn open_local(
        &self,
        _address: &ren_harness::sign_in::LocalRedirect,
    ) -> futures_util::future::BoxFuture<'static, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

async fn send(out: &mut DuplexStream, message: Value) {
    let mut line = message.to_string().into_bytes();
    line.push(b'\n');
    out.write_all(&line).await.unwrap();
}

/// A scripted Codex app server.
async fn agent(
    script: Script,
    input: DuplexStream,
    mut out: DuplexStream,
    kill: KillSignal,
    exit: oneshot::Sender<ProcessExit>,
) {
    let mut lines = BufReader::new(input).lines();
    let serve = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).unwrap();
            let id = message["id"].clone();
            match (message["method"].as_str(), script) {
                (Some("initialize"), Script::ExitsEarly) => return ProcessExit::Code(3),
                (Some("initialize"), _) => send(&mut out, json!({ "id": id, "result": {} })).await,
                (Some("model/list"), _) => {
                    send(
                        &mut out,
                        json!({ "id": id, "result": { "data": [
                            { "id": "gpt-fast", "model": "gpt-fast", "displayName": "GPT Fast",
                              "description": "Quick.", "isDefault": true, "hidden": false },
                        ] } }),
                    )
                    .await;
                }
                (Some("thread/start"), Script::RefusesOpening) => {
                    send(
                        &mut out,
                        json!({ "id": id, "error": { "code": -1, "message": "no" } }),
                    )
                    .await;
                }
                (Some("skills/list"), _) => {
                    send(
                        &mut out,
                        json!({ "id": id, "result": { "data": [{ "cwd": "/work/acme",
                            "errors": [], "skills": [{ "name": "release-notes",
                                "description": "Write release notes", "enabled": true,
                                "path": "/work/acme/.codex/skills/release-notes/SKILL.md",
                                "scope": "repo" }] }] } }),
                    )
                    .await;
                }
                (Some("thread/resume"), _) => {
                    let thread = message["params"]["threadId"].clone();
                    send(
                        &mut out,
                        json!({ "id": id, "result": { "thread": { "id": thread } } }),
                    )
                    .await;
                }
                (Some("thread/start"), _) => {
                    send(
                        &mut out,
                        json!({ "id": id, "result": { "thread": { "id": THREAD } } }),
                    )
                    .await;
                }
                (Some("turn/start"), _) => {
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
                (Some("turn/interrupt"), _) => {
                    send(&mut out, json!({ "id": id, "result": {} })).await;
                    send(
                        &mut out,
                        json!({ "method": "turn/completed", "params": { "threadId": THREAD,
                            "turn": { "id": TURN, "status": "interrupted" } } }),
                    )
                    .await;
                }
                (None, _) if id == "ap-1" => {
                    assert_eq!(message["result"]["decision"], "accept");
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
                (Some(_) | None, _) => {}
            }
        }
        ProcessExit::Code(0)
    };
    let status = tokio::select! {
        status = serve => status,
        () = kill.fired() => ProcessExit::Signal(9),
    };
    let _ = exit.send(status);
}

fn options() -> SupervisorOptions {
    SupervisorOptions {
        roots: vec![PathBuf::from("/work")],
        places: vec![PathBuf::from("/home/op/.config")],
        scratch: Some(PathBuf::from("/scratch")),
        home: Some(PathBuf::from("/home/op")),
        warmup: ren_supervisor::Warmup::WhenAsked,
        gateway: GatewayEndpoint::parse("http://127.0.0.1:8787").unwrap(),
        version: "0.1.0".into(),
        line_limit: LineLimit::DEFAULT,
        idle: ren_supervisor::IdleStop::Never,
    }
}

/// A supervisor recording into a real store, with its recorder running.
/// A scripted Claude Code in stream-json print mode.
async fn claude(
    input: DuplexStream,
    mut out: DuplexStream,
    kill: KillSignal,
    exit: oneshot::Sender<ProcessExit>,
) {
    let mut lines = BufReader::new(input).lines();
    let serve = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).unwrap();
            match message["type"].as_str() {
                Some("control_request") => {
                    let id = message["request_id"].clone();
                    send(
                        &mut out,
                        json!({ "type": "control_response", "response": {
                            "subtype": "success", "request_id": id, "response": {} } }),
                    )
                    .await;
                }
                Some("user") => {
                    let mut echo = message.clone();
                    echo["uuid"] = TURN.into();
                    echo["isReplay"] = true.into();
                    send(&mut out, echo).await;
                    send(
                        &mut out,
                        json!({ "type": "control_request", "request_id": "perm-1", "request": {
                            "subtype": "can_use_tool", "tool_name": "Bash",
                            "input": { "command": "cargo test" }, "description": "Run tests" } }),
                    )
                    .await;
                }
                Some("control_response") => {
                    assert_eq!(message["response"]["request_id"], "perm-1");
                    assert_eq!(message["response"]["response"]["behavior"], "allow");
                    send(
                        &mut out,
                        json!({ "type": "result", "subtype": "success", "is_error": false,
                                "result": "Tests pass." }),
                    )
                    .await;
                }
                Some(_) | None => {}
            }
        }
        ProcessExit::Code(0)
    };
    let status = tokio::select! {
        status = serve => status,
        () = kill.fired() => ProcessExit::Signal(9),
    };
    let _ = exit.send(status);
}

/// A scripted Pi in RPC mode, on the session `session`.
async fn pi(
    session: String,
    input: DuplexStream,
    mut out: DuplexStream,
    kill: KillSignal,
    exit: oneshot::Sender<ProcessExit>,
) {
    let mut lines = BufReader::new(input).lines();
    let serve = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).unwrap();
            let id = message["id"].clone();
            match message["type"].as_str() {
                Some("get_state") => {
                    send(
                        &mut out,
                        json!({ "id": id, "type": "response", "command": "get_state",
                                "success": true, "data": { "sessionId": session } }),
                    )
                    .await;
                }
                Some("prompt") => {
                    let text = message["message"].clone();
                    for line in [
                        json!({ "id": id, "type": "response", "command": "prompt",
                                "success": true, "data": { "disposition": "started" } }),
                        json!({ "type": "agent_start" }),
                        json!({ "type": "message_start",
                                "message": { "role": "user", "content": text, "timestamp": 1 } }),
                        json!({ "type": "message_start",
                                "message": { "role": "assistant", "content": [] } }),
                        json!({ "type": "message_end", "message": { "role": "assistant",
                                "stopReason": "stop",
                                "content": [{ "type": "text", "text": "Tests pass." }] } }),
                        json!({ "type": "agent_end", "messages": [] }),
                        json!({ "type": "agent_settled", "aborted": false }),
                    ] {
                        send(&mut out, line).await;
                    }
                }
                Some(_) | None => {}
            }
        }
        ProcessExit::Code(0)
    };
    let status = tokio::select! {
        status = serve => status,
        () = kill.fired() => ProcessExit::Signal(9),
    };
    let _ = exit.send(status);
}

async fn opencode(
    input: DuplexStream,
    mut out: DuplexStream,
    kill: KillSignal,
    exit: oneshot::Sender<ProcessExit>,
) {
    let mut lines = BufReader::new(input).lines();
    let serve = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).unwrap();
            let id = message["id"].clone();
            if id.is_null() {
                continue;
            }
            let result = match message["method"].as_str() {
                Some("session/new") => json!({ "sessionId": "ses_free1", "configOptions": [
                    { "id": "model", "currentValue": "opencode/free", "options": [
                        { "value": "opencode/free", "name": "opencode/Free" },
                        { "value": "opencode/other", "name": "opencode/Other" },
                    ] },
                ] }),
                Some(_) | None => json!({}),
            };
            send(
                &mut out,
                json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            )
            .await;
        }
        ProcessExit::Code(0)
    };
    let status = tokio::select! {
        status = serve => status,
        () = kill.fired() => ProcessExit::Signal(9),
    };
    let _ = exit.send(status);
}

struct Supervised {
    supervisor: Supervisor,
    task: tokio::task::JoinHandle<()>,
    recorder: tokio::task::JoinHandle<()>,
    dir: tempfile::TempDir,
}

async fn supervise(host: FakeHost) -> Supervised {
    supervise_with(host, options()).await
}

async fn supervise_with(host: FakeHost, options: SupervisorOptions) -> Supervised {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::at(UnixMillis::from_millis(1_000)));
    let ids = Arc::new(SequentialIds::default());
    let (store, recovery) = Store::open(dir.path(), clock.as_ref(), ids.clone())
        .await
        .unwrap();
    let setup = RecorderSetup {
        options: CaptureOptions::default(),
        retention: RetentionPolicy::default(),
        max_calls: 16,
        live_buffer: 64,
        scanner: Arc::new(SecretScanner::new().unwrap()),
    };
    let (recorder, recording) =
        Recorder::new(store, recovery.calls, setup, clock, ids.clone()).unwrap();
    let (supervisor, task) = Supervisor::new(options, host, ids, recorder).unwrap();
    Supervised {
        supervisor,
        task: tokio::spawn(task.run()),
        recorder: tokio::spawn(async move { recording.run().await.unwrap() }),
        dir,
    }
}

impl Supervised {
    /// Drop the supervisor, let its runs drain and the recorder commit, and
    /// read what was stored.
    async fn stored_sessions(self) -> Vec<ren_store::view::SessionView> {
        drop(self.supervisor);
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("the supervisor drains")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.recorder)
            .await
            .expect("the recorder drains")
            .unwrap();
        let reader = Reader::open(self.dir.path(), 1).await.unwrap();
        let mut sessions = Vec::new();
        for shelf in [
            ren_store::query::Shelf::Current,
            ren_store::query::Shelf::Archived,
        ] {
            sessions.extend(reader.sessions(PageLimit::DEFAULT, shelf).await.unwrap());
        }
        reader.close().await;
        sessions
    }

    async fn stored(self) -> (Vec<RunView>, tempfile::TempDir) {
        drop(self.supervisor);
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("the supervisor drains")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.recorder)
            .await
            .expect("the recorder drains")
            .unwrap();
        let reader = Reader::open(self.dir.path(), 1).await.unwrap();
        let runs = reader
            .runs(PageLimit::DEFAULT, ren_store::query::Shelf::Current)
            .await
            .unwrap();
        reader.close().await;
        (runs, self.dir)
    }
}

/// The next state update matching `wanted`, skipping others.
async fn state(
    updates: &mut broadcast::Receiver<RunUpdate>,
    wanted: fn(&RunState) -> bool,
) -> RunState {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let RunUpdate::State { state, .. } = updates.recv().await.unwrap()
                && wanted(&state)
            {
                return state;
            }
        }
    })
    .await
    .expect("state update")
}

fn prompt() -> RunCommand {
    RunCommand::Prompt(Prompt::parse("edit src".into()).unwrap())
}

#[tokio::test]
async fn runs_a_turn_through_an_approval_and_stops() {
    let host = FakeHost::new(Script::Codex);
    let spawned = host.spawned.clone();
    let tools_given = host.tools_given.clone();
    let supervised = supervise(host).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();

    let run = supervisor
        .start(starting(
            Harness::Codex,
            "/work/acme",
            Conversation::New,
            None,
        ))
        .await
        .unwrap();
    let (arguments, workdir) = spawned.lock().unwrap()[0].clone();
    assert_eq!(arguments[..3], ["app-server", "--listen", "stdio://"]);
    assert_eq!(workdir, PathBuf::from("/work/acme"));
    // The run got the host's current toolset.
    assert_eq!(
        tools_given.lock().unwrap()[0].generation(),
        Some(Generation::from(4))
    );
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;

    supervisor.command(run, prompt()).await.unwrap();
    assert!(matches!(
        supervisor.command(run, prompt()).await,
        Err(CommandError::Refused(RunRefused::Busy))
    ));
    let waiting = state(
        &mut updates,
        |s| matches!(s, RunState::Working { approvals, .. } if !approvals.is_empty()),
    )
    .await;
    let RunState::Working { approvals, .. } = waiting else {
        unreachable!()
    };
    let waiting = approvals.first().unwrap();
    assert_eq!(
        waiting.ask,
        ren_harness::drive::ApprovalAsk::FileChange {
            reason: Some("edit src".into())
        }
    );
    let approval: ApprovalId = waiting.approval.clone();
    assert_eq!(approval.to_string(), "ap-1");
    supervisor
        .command(
            run,
            RunCommand::Answer {
                approval,
                decision: Decision::Accept,
            },
        )
        .await
        .unwrap();
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;

    supervisor.command(run, RunCommand::Stop).await.unwrap();
    let ended = state(&mut updates, RunState::is_ended).await;
    assert_eq!(
        ended,
        RunState::Ended {
            end: RunEnd::Stopped,
            exit: Some(ProcessExit::Signal(9))
        }
    );
    assert!(matches!(
        supervisor.command(run, prompt()).await,
        Err(CommandError::Refused(RunRefused::Ended))
    ));
    let runs = supervisor.runs().await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, ended);

    let (stored, _dir) = supervised.stored().await;
    let [stored] = stored.as_slice() else {
        panic!("one run is stored: {stored:?}");
    };
    assert_eq!(stored.id, run);
    assert_eq!(stored.harness, Stored::Known(Harness::Codex));
    assert_eq!(stored.workdir, "/work/acme");
    assert_eq!(stored.thread.as_deref(), Some(THREAD));
    assert_eq!(stored.end, Some(Stored::Known(RunEnd::Stopped)));
    assert_eq!(stored.exit, Some(ProcessExit::Signal(9)));
}

#[tokio::test]
async fn refuses_directories_outside_the_roots() {
    let supervised = supervise(FakeHost::new(Script::Codex)).await;
    let supervisor = &supervised.supervisor;
    let refused = [
        ("relative/path", "Workdir(Relative)"),
        ("/work/../etc", "Workdir(Unnormalized)"),
        ("/elsewhere", "Workdir(OutsideRoots)"),
        ("/work/escape", "Workdir(OutsideRoots)"),
        ("/missing", "Resolve"),
    ];
    for (dir, expected) in refused {
        let error = supervisor
            .start(starting(Harness::Codex, dir, Conversation::New, None))
            .await
            .unwrap_err();
        assert!(
            format!("{error:?}").starts_with(expected),
            "{dir}: {error:?}"
        );
    }
    assert!(supervisor.runs().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_spawn_is_reported_and_kept() {
    let mut host = FakeHost::new(Script::Codex);
    host.spawn_fails = true;
    let supervised = supervise(host).await;
    let supervisor = &supervised.supervisor;
    let Err(StartError::Spawn { run, .. }) = supervisor
        .start(starting(
            Harness::Codex,
            "/work/acme",
            Conversation::New,
            None,
        ))
        .await
    else {
        panic!("expected a spawn failure");
    };
    let runs = supervisor.runs().await.unwrap();
    assert_eq!(runs[0].run, run);
    assert_eq!(
        runs[0].state,
        RunState::Ended {
            end: RunEnd::SpawnFailed,
            exit: None
        }
    );
    let (stored, _dir) = supervised.stored().await;
    assert_eq!(stored[0].end, Some(Stored::Known(RunEnd::SpawnFailed)));
    assert_eq!(stored[0].exit, None);
}

#[tokio::test]
async fn a_refused_opening_stops_the_run() {
    let supervised = supervise(FakeHost::new(Script::RefusesOpening)).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    supervisor
        .start(starting(
            Harness::Codex,
            "/work/acme",
            Conversation::New,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        state(&mut updates, RunState::is_ended).await,
        RunState::Ended {
            end: RunEnd::OpenRefused,
            exit: Some(ProcessExit::Signal(9))
        }
    );
}

#[tokio::test]
async fn an_agent_that_exits_on_its_own_ends_the_run() {
    let supervised = supervise(FakeHost::new(Script::ExitsEarly)).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    supervisor
        .start(starting(
            Harness::Codex,
            "/work/acme",
            Conversation::New,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        state(&mut updates, RunState::is_ended).await,
        RunState::Ended {
            end: RunEnd::Exited,
            exit: Some(ProcessExit::Code(3))
        }
    );
}

#[tokio::test]
async fn dropping_every_handle_drains_the_runs_and_ends_the_task() {
    let supervised = supervise(FakeHost::new(Script::Codex)).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    supervisor
        .start(starting(Harness::Codex, "/work/a", Conversation::New, None))
        .await
        .unwrap();
    supervisor
        .start(starting(Harness::Codex, "/work/b", Conversation::New, None))
        .await
        .unwrap();
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;
    let (stored, _dir) = supervised.stored().await;
    assert_eq!(stored.len(), 2);
    for run in &stored {
        assert_eq!(run.end, Some(Stored::Known(RunEnd::Drained)));
    }
    let mut drained = 0;
    while let Ok(update) = updates.try_recv() {
        if let RunUpdate::State {
            state: RunState::Ended { end, .. },
            ..
        } = update
        {
            assert_eq!(end, RunEnd::Drained);
            drained += 1;
        }
    }
    assert_eq!(drained, 2);
}

#[tokio::test]
async fn a_run_can_resume_the_clients_thread() {
    let supervised = supervise(FakeHost::new(Script::Codex)).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    let earlier = "0190f3a0-0000-7000-8000-0000000000ee";
    let thread =
        ren_protocol::native::NativeThreadId::new(uuid::Uuid::parse_str(earlier).unwrap()).unwrap();
    supervisor
        .start(starting(
            Harness::Codex,
            "/work/acme",
            Conversation::Resume(thread),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await,
        RunState::Idle { thread }
    );
    let (stored, _dir) = supervised.stored().await;
    assert_eq!(stored[0].thread.as_deref(), Some(earlier));
}

#[tokio::test]
async fn drives_claude_code_through_a_turn_in_the_conversation_ren_names() {
    let host = FakeHost::new(Script::Claude);
    let spawned = host.spawned.clone();
    let supervised = supervise(host).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();

    let run = supervisor
        .start(starting(
            Harness::ClaudeCode,
            "/work/acme",
            Conversation::New,
            None,
        ))
        .await
        .unwrap();
    let (arguments, _) = spawned.lock().unwrap()[0].clone();
    assert_eq!(arguments[0], "-p");
    let named = arguments
        .iter()
        .position(|argument| argument == "--session-id")
        .map(|at| arguments[at + 1].clone())
        .expect("Ren names the conversation");
    let RunState::Idle { thread } =
        state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(thread.to_string(), named);

    supervisor.command(run, prompt()).await.unwrap();
    let waiting = state(
        &mut updates,
        |s| matches!(s, RunState::Working { turn: Some(_), approvals, .. } if !approvals.is_empty()),
    )
    .await;
    let RunState::Working { approvals, .. } = waiting else {
        unreachable!()
    };
    let waiting = approvals.first().unwrap();
    assert_eq!(
        waiting.ask,
        ren_harness::drive::ApprovalAsk::Command {
            command: Some("cargo test".into()),
            cwd: None,
            reason: Some("Run tests".into()),
        }
    );
    supervisor
        .command(
            run,
            RunCommand::Answer {
                approval: waiting.approval.clone(),
                decision: Decision::Accept,
            },
        )
        .await
        .unwrap();
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;
    supervisor.command(run, RunCommand::Stop).await.unwrap();
    state(&mut updates, RunState::is_ended).await;

    let (stored, _dir) = supervised.stored().await;
    let [stored] = stored.as_slice() else {
        panic!("one run is stored: {stored:?}");
    };
    assert_eq!(stored.harness, Stored::Known(Harness::ClaudeCode));
    assert_eq!(stored.thread.as_deref(), Some(named.as_str()));
}

#[tokio::test]
async fn drives_pi_through_a_turn_in_the_session_ren_names() {
    let host = FakeHost::new(Script::Pi);
    let spawned = host.spawned.clone();
    let supervised = supervise(host).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();

    let run = supervisor
        .start(starting(Harness::Pi, "/work/acme", Conversation::New, None))
        .await
        .unwrap();
    let (arguments, workdir) = spawned.lock().unwrap()[0].clone();
    assert_eq!(workdir, PathBuf::from("/work/acme"));
    assert!(arguments.windows(2).any(|pair| pair == ["--mode", "rpc"]));
    let named = arguments
        .iter()
        .position(|argument| argument == "--session-id")
        .map(|at| arguments[at + 1].clone())
        .expect("Ren names the session");
    let RunState::Idle { thread } =
        state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(thread.to_string(), named);

    supervisor.command(run, prompt()).await.unwrap();
    state(&mut updates, |s| {
        matches!(s, RunState::Working { turn: Some(_), .. })
    })
    .await;
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;
    supervisor.command(run, RunCommand::Stop).await.unwrap();
    state(&mut updates, RunState::is_ended).await;

    let (stored, _dir) = supervised.stored().await;
    let [stored] = stored.as_slice() else {
        panic!("one run is stored: {stored:?}");
    };
    assert_eq!(stored.harness, Stored::Known(Harness::Pi));
    assert_eq!(stored.thread.as_deref(), Some(named.as_str()));
}

#[tokio::test]
async fn a_first_prompt_goes_out_once_the_conversation_opens() {
    let supervised = supervise(FakeHost::new(Script::Claude)).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    supervisor
        .start(starting(
            Harness::ClaudeCode,
            "/work/acme",
            Conversation::New,
            Some(prompt_text("run the tests")),
        ))
        .await
        .unwrap();
    // No prompt command: the run asks its first approval on its own.
    state(
        &mut updates,
        |s| matches!(s, RunState::Working { approvals, .. } if !approvals.is_empty()),
    )
    .await;
}

#[tokio::test]
async fn a_sessions_folder_is_walked_once_until_a_turn_ends() {
    use ren_supervisor::SessionError;
    let host = FakeHost::new(Script::Codex);
    let walked = host.walked.clone();
    let supervised = supervise(host).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    let session = supervisor
        .create(create(Harness::Codex, "/work/acme"))
        .await
        .unwrap();
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;
    let found = supervisor
        .session_files(session, FileQuery::parse("lease"), 5)
        .await
        .unwrap();
    assert_eq!(found[0].path, "src/lease.rs");
    let _ = supervisor
        .session_files(session, FileQuery::parse(""), 5)
        .await
        .unwrap();
    assert_eq!(*walked.lock().unwrap(), [PathBuf::from("/work/acme")]);
    // A turn changes files: the folder is walked again after it.
    supervisor.send(session, prompt()).await.unwrap();
    let waiting = state(&mut updates, working).await;
    let RunState::Working { approvals, .. } = waiting else {
        unreachable!()
    };
    let approval = approvals.iter().next().unwrap().approval.clone();
    supervisor
        .send(
            session,
            RunCommand::Answer {
                approval,
                decision: ren_harness::drive::Decision::Accept,
            },
        )
        .await
        .unwrap();
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;
    let _ = supervisor
        .session_files(session, FileQuery::parse("up"), 5)
        .await
        .unwrap();
    assert_eq!(walked.lock().unwrap().len(), 2);
    let unknown = ren_recorder::id::SessionId::restore(uuid::Uuid::from_u128(99));
    assert!(matches!(
        supervisor.session_commands(unknown).await,
        Err(SessionError::NoSuchSession(_))
    ));
}

#[tokio::test]
async fn projects_are_the_repositories_and_plain_folders_under_the_roots() {
    let supervised = supervise(FakeHost::new(Script::Codex)).await;
    let places = supervised.supervisor.projects().await.unwrap();
    // The scratch folder runs get theirs in comes with them.
    assert_eq!(places.scratch.as_deref(), Some("/scratch"));
    let listed: Vec<(&str, &str)> = places
        .projects
        .iter()
        .map(|project| (project.name.as_str(), project.path.as_str()))
        .collect();
    assert_eq!(listed, [("acme", "/work/acme"), ("notes", "/work/notes")]);
}

#[tokio::test]
async fn clients_say_whether_a_run_can_start_with_them_here() {
    use ren_supervisor::{Client, ClientStatus};
    let supervised = supervise(FakeHost::new(Script::Codex)).await;
    let clients = supervised.supervisor.clients().await.unwrap();
    let status = |harness: Harness| {
        clients
            .iter()
            .find(|client| client.harness == harness)
            .map(|client: &Client| client.status)
    };
    assert_eq!(status(Harness::Codex), Some(ClientStatus::Ready));
    assert_eq!(
        status(Harness::ClaudeCode),
        Some(ClientStatus::NotInstalled)
    );
    assert_eq!(status(Harness::Pi), Some(ClientStatus::NotInstalled));
    assert_eq!(status(Harness::OpenCode), Some(ClientStatus::NotInstalled));
}

#[tokio::test]
async fn a_clients_models_are_listed_once_and_kept() {
    let host = FakeHost::new(Script::Codex);
    let spawned = host.spawned.clone();
    let supervised = supervise(host).await;
    let supervisor = &supervised.supervisor;

    let (first, second) = tokio::join!(
        supervisor.models(Harness::Codex),
        supervisor.models(Harness::Codex)
    );
    let first = first.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id.as_str(), "gpt-fast");
    assert_eq!(first[0].choice, ren_harness::models::DefaultChoice::Default);
    assert_eq!(second.unwrap(), first);
    // Asked again, the kept answer: one listing process in all, in a root.
    assert_eq!(supervisor.models(Harness::Codex).await.unwrap(), first);
    let spawned = spawned.lock().unwrap().clone();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].1, PathBuf::from("/work"));
}

#[tokio::test]
async fn warmed_at_start_the_models_are_listed_before_anyone_asks() {
    let host = FakeHost::new(Script::Codex);
    let spawned = host.spawned.clone();
    let mut options = options();
    options.warmup = ren_supervisor::Warmup::AtStart;
    let supervised = supervise_with(host, options).await;
    // Only Codex is installed here: its listing starts with the task, before
    // anything asks for it.
    tokio::time::timeout(Duration::from_secs(5), async {
        while spawned.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the listing started on its own");
    let models = supervised.supervisor.models(Harness::Codex).await.unwrap();
    assert_eq!(models[0].id.as_str(), "gpt-fast");
    assert_eq!(spawned.lock().unwrap().len(), 1);
}

/// OpenCode under Ren's override would take a model of a provider it has no
/// key for, so a session that chose none starts on the default it listed on
/// its own, by name.
#[tokio::test]
async fn opencode_starts_on_the_default_it_listed_when_none_is_chosen() {
    let supervised = supervise(FakeHost::new(Script::OpenCode)).await;
    let supervisor = &supervised.supervisor;
    let models = supervisor.models(Harness::OpenCode).await.unwrap();
    assert_eq!(models[0].id.as_str(), "opencode/free");
    assert_eq!(
        models[0].choice,
        ren_harness::models::DefaultChoice::Default
    );
    let run = supervisor
        .start(starting(
            Harness::OpenCode,
            "/work/acme",
            Conversation::New,
            None,
        ))
        .await
        .unwrap();
    let runs = supervisor.runs().await.unwrap();
    let started = runs.iter().find(|entry| entry.run == run).unwrap();
    assert_eq!(
        started.settings.model.as_ref().map(|model| model.as_str()),
        Some("opencode/free")
    );
    // A model chosen is kept.
    let mut chosen = starting(Harness::OpenCode, "/work/acme", Conversation::New, None);
    chosen.settings.model = Some("opencode/other".parse().unwrap());
    let run = supervisor.start(chosen).await.unwrap();
    let runs = supervisor.runs().await.unwrap();
    let started = runs.iter().find(|entry| entry.run == run).unwrap();
    assert_eq!(
        started.settings.model.as_ref().map(|model| model.as_str()),
        Some("opencode/other")
    );
}

#[tokio::test]
async fn a_failed_listing_is_reported_and_asked_again() {
    let host = FakeHost::new(Script::ExitsEarly);
    let spawned = host.spawned.clone();
    let supervised = supervise(host).await;
    let supervisor = &supervised.supervisor;
    assert_eq!(
        supervisor.models(Harness::Codex).await,
        Err(ren_supervisor::ModelsError::Exited)
    );
    let _ = supervisor.models(Harness::Codex).await;
    assert_eq!(spawned.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_run_starts_with_its_chosen_settings() {
    let host = FakeHost::new(Script::Claude);
    let spawned = host.spawned.clone();
    let supervised = supervise(host).await;
    let mut start = starting(Harness::ClaudeCode, "/work/acme", Conversation::New, None);
    start.settings.model = Some("small".parse().unwrap());
    start.settings.approvals = Some(ren_harness::settings::ApprovalPolicy::Auto);
    supervised.supervisor.start(start).await.unwrap();
    let (arguments, _) = spawned.lock().unwrap()[0].clone();
    assert_eq!(
        arguments[arguments.len() - 4..],
        ["--model", "small", "--permission-mode", "auto"]
    );
    // The run says what it was started with.
    let runs = supervised.supervisor.runs().await.unwrap();
    assert_eq!(
        runs[0].settings.model.as_ref().map(|m| m.as_str()),
        Some("small")
    );
    // Pi asks for nothing, so it takes no approval policy.
    let mut pi = starting(Harness::Pi, "/work/acme", Conversation::New, None);
    pi.settings.approvals = Some(ren_harness::settings::ApprovalPolicy::Ask);
    assert!(matches!(
        supervised.supervisor.start(pi).await,
        Err(StartError::NoApprovals(Harness::Pi))
    ));
}

#[tokio::test]
async fn a_scratch_run_gets_a_new_repository_of_its_own() {
    let host = FakeHost::new(Script::Codex);
    let (spawned, made) = (host.spawned.clone(), host.made.clone());
    let supervised = supervise(host).await;
    let mut start = starting(Harness::Codex, "/unused", Conversation::New, None);
    start.place = ren_supervisor::StartPlace::Scratch;
    supervised.supervisor.start(start).await.unwrap();
    let (_, workdir) = spawned.lock().unwrap()[0].clone();
    assert!(
        workdir.to_str().unwrap().starts_with("/scratch/scratch-"),
        "{workdir:?}"
    );
    // The scratch folder was made at setup; the run's own as a repository.
    let made = made.lock().unwrap().clone();
    assert_eq!(made[0], (PathBuf::from("/scratch"), false));
    assert_eq!(made[1], (workdir, true));
    // A folder outside every root and place is refused, a home included.
    let outside = starting(Harness::Codex, "/home/op", Conversation::New, None);
    assert!(supervised.supervisor.start(outside).await.is_err());
}

fn prompt_text(text: &str) -> Prompt {
    Prompt::parse(text.into()).unwrap()
}

/// What a run starts with, on the client's own default model.
fn starting(
    harness: Harness,
    dir: &str,
    conversation: Conversation,
    first: Option<ren_harness::drive::Prompt>,
) -> ren_supervisor::Start {
    ren_supervisor::Start {
        harness,
        place: ren_supervisor::StartPlace::Folder(dir.into()),
        conversation,
        settings: ren_harness::settings::RunSettings::default(),
        first,
    }
}

fn create(harness: Harness, dir: &str) -> ren_supervisor::Create {
    ren_supervisor::Create {
        harness,
        place: ren_supervisor::StartPlace::Folder(dir.into()),
        settings: ren_harness::settings::RunSettings::default(),
        resume: None,
    }
}

async fn only_session(supervisor: &Supervisor) -> ren_supervisor::SessionSnapshot {
    let mut sessions = supervisor.sessions().await.unwrap();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    sessions.remove(0)
}

fn working(state: &RunState) -> bool {
    matches!(state, RunState::Working { approvals, .. } if !approvals.is_empty())
}

#[tokio::test]
async fn a_session_goes_on_across_its_processes() {
    use ren_supervisor::{
        FileQuery, SessionError, SessionPhase, SessionRefused, SessionState, Shelving,
    };
    let supervised = supervise(FakeHost::new(Script::Codex)).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    let mut changes = supervisor.subscribe_sessions();
    let session = supervisor
        .create(create(Harness::Codex, "/work/acme"))
        .await
        .unwrap();
    // Its client starts at once: a draft with a process, open, before any prompt.
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;
    let draft = only_session(supervisor).await;
    assert_eq!(draft.state, SessionState::Open(SessionPhase::Draft));
    let first = draft.process.unwrap();
    assert_eq!(changes.recv().await.unwrap().session, session);
    let commands = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let commands = supervisor.session_commands(session).await.unwrap();
            if !commands.is_empty() {
                return commands;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the skills are listed before the first prompt");
    assert_eq!(commands[0].invocation(), "$release-notes");

    // The first prompt goes to that process, and fixes the client.
    supervisor
        .send(session, RunCommand::Prompt(prompt_text("edit src")))
        .await
        .unwrap();
    state(&mut updates, working).await;
    let active = only_session(supervisor).await;
    assert_eq!(active.state, SessionState::Open(SessionPhase::Active));
    assert_eq!(active.process, Some(first));
    assert!(matches!(
        supervisor.choose_client(session, Harness::Pi).await,
        Err(SessionError::Refused(SessionRefused::ClientFixed))
    ));

    // Its process stops; the session goes on, and its folder is searched
    // without one.
    supervisor.send(session, RunCommand::Stop).await.unwrap();
    state(&mut updates, |s| matches!(s, RunState::Ended { .. })).await;
    assert!(matches!(
        supervisor.send(session, RunCommand::Interrupt).await,
        Err(SessionError::NoProcess)
    ));
    let found = supervisor
        .session_files(session, FileQuery::parse("lease"), 5)
        .await
        .unwrap();
    assert_eq!(found[0].path, "src/lease.rs");

    // The next prompt starts a process that resumes its conversation.
    supervisor
        .send(session, RunCommand::Prompt(prompt_text("and the docs")))
        .await
        .unwrap();
    let resumed = state(&mut updates, working).await;
    let RunState::Working { thread, .. } = resumed else {
        unreachable!()
    };
    assert_eq!(thread.to_string(), THREAD);
    let second = only_session(supervisor).await.process.unwrap();
    assert_ne!(second, first);

    // Archived, its process stops; written to, it comes back.
    supervisor.shelve(session, Shelving::Archive).await.unwrap();
    state(&mut updates, |s| matches!(s, RunState::Ended { .. })).await;
    assert_eq!(
        only_session(supervisor).await.state,
        SessionState::Archived(SessionPhase::Active)
    );
    assert!(matches!(
        supervisor.shelve(session, Shelving::Archive).await,
        Err(SessionError::Refused(SessionRefused::Archived))
    ));
    supervisor
        .shelve(session, Shelving::Unarchive)
        .await
        .unwrap();
    supervisor.shelve(session, Shelving::Archive).await.unwrap();

    // One session, both processes its runs, archived.
    let stored = supervised.stored_sessions().await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, session);
    assert_eq!(
        stored[0].runs.iter().map(|run| run.id).collect::<Vec<_>>(),
        [second, first]
    );
    assert!(stored[0].archived_at.is_some());
}

#[tokio::test]
async fn a_draft_changes_client_and_a_stored_session_is_adopted() {
    use ren_harness::settings::{Effort, RunSettings};
    use ren_supervisor::{SessionPhase, SessionSeed, SessionState};
    let host = FakeHost::new(Script::Codex);
    let spawned = host.spawned.clone();
    let supervised = supervise(host).await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    let session = supervisor
        .create(ren_supervisor::Create {
            settings: RunSettings {
                model: None,
                effort: Some(Effort::High),
                approvals: None,
            },
            ..create(Harness::Codex, "/work/acme")
        })
        .await
        .unwrap();
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;
    let first = only_session(supervisor).await.process.unwrap();
    supervisor
        .choose_client(session, Harness::Pi)
        .await
        .unwrap();
    let chosen = only_session(supervisor).await;
    assert_eq!(chosen.harness, Harness::Pi);
    assert_eq!(
        chosen.settings,
        RunSettings::default(),
        "its settings were Codex's"
    );
    assert_eq!(chosen.state, SessionState::Open(SessionPhase::Draft));
    assert_ne!(chosen.process, Some(first));
    assert_eq!(spawned.lock().unwrap().len(), 2);

    // A session the store holds is served again, resuming its thread.
    let stored = ren_recorder::id::SessionId::restore(uuid::Uuid::from_u128(0x77));
    let seed = |workdir: &str| SessionSeed {
        session: stored,
        harness: Harness::Codex,
        workdir: workdir.into(),
        settings: RunSettings::default(),
        thread: Some(
            ren_protocol::native::NativeThreadId::new(uuid::Uuid::parse_str(THREAD).unwrap())
                .unwrap(),
        ),
        state: SessionState::Open(SessionPhase::Active),
    };
    assert!(
        supervisor.adopt(seed("/etc")).await.is_err(),
        "outside the bounds"
    );
    supervisor.adopt(seed("/work/acme")).await.unwrap();
    supervisor.adopt(seed("/work/acme")).await.unwrap();
    assert_eq!(supervisor.sessions().await.unwrap().len(), 2);
    supervisor
        .send(stored, RunCommand::Prompt(prompt_text("go on")))
        .await
        .unwrap();
    let resumed = state(&mut updates, working).await;
    let RunState::Working { thread, .. } = resumed else {
        unreachable!()
    };
    assert_eq!(thread.to_string(), THREAD);
}

#[tokio::test]
async fn an_idle_client_is_stopped_and_its_session_goes_on() {
    use ren_supervisor::{IdleStop, SessionPhase, SessionState};
    let supervised = supervise_with(
        FakeHost::new(Script::Codex),
        SupervisorOptions {
            idle: IdleStop::After(Duration::from_millis(50)),
            ..options()
        },
    )
    .await;
    let supervisor = &supervised.supervisor;
    let mut updates = supervisor.subscribe();
    let session = supervisor
        .create(create(Harness::Codex, "/work/acme"))
        .await
        .unwrap();
    // Opened and idle, it is stopped once its timer runs out.
    state(&mut updates, |s| matches!(s, RunState::Idle { .. })).await;
    let ended = state(&mut updates, |s| matches!(s, RunState::Ended { .. })).await;
    assert!(
        matches!(
            ended,
            RunState::Ended {
                end: ren_recorder::capture::RunEnd::Idled,
                ..
            }
        ),
        "{ended:?}"
    );
    // The session goes on: a prompt starts its client again.
    assert_eq!(
        only_session(supervisor).await.state,
        SessionState::Open(SessionPhase::Draft)
    );
    supervisor
        .send(session, RunCommand::Prompt(prompt_text("go on")))
        .await
        .unwrap();
    // Working (waiting on its approval), it is not idle and is not stopped.
    state(&mut updates, working).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let run = only_session(supervisor).await.process.unwrap();
    let runs = supervisor.runs().await.unwrap();
    let held = runs.iter().find(|snapshot| snapshot.run == run).unwrap();
    assert!(working(&held.state), "{:?}", held.state);
}
