//! Runs: agent processes Ren starts in a folder and drives through their
//! client's structured mode. The primary type is [`Supervisor`], the handle
//! the gateway's routes use; [`SupervisorTask`] runs every run's I/O and is
//! returned to the caller to run, because components do not spawn.
//!
//! A run's lifecycle is the pure [`RunState`] machine (`run`); where a run may
//! work is [`Workdir`] (`workdir`); starting processes is the [`AgentHost`]
//! port (`host`), which the daemon implements. Clients keep their own sign-in:
//! the supervisor starts the official program and never handles credentials.
//! It can run a client's own sign-in commands (`sign_in`, run by `login`):
//! its status, and its login, whose link and code it relays.

pub mod client;
mod entries;
pub mod files;
pub mod host;
mod login;
mod models;
pub mod project;
#[cfg(test)]
mod purity;
pub mod run;
pub mod session;
pub mod sign_in;
mod task;
pub mod workdir;

pub use client::{Client, ClientStatus};
pub use files::{FileListing, FileMatch, FileQuery, FoundPath, PathKind, WalkLimits};
pub use host::{AgentHost, AgentProcess, KillSignal, KillSwitch, RunTools};
pub use login::SignInError;
pub use models::ModelsError;
pub use project::{Look, PlaceKind, Project, ProjectGit, Search};
pub use run::{Approvals, Delivery, ProcessExit, RunCommand, RunRefused, RunState, Waiting};
pub use session::{
    SessionAction, SessionInput, SessionPhase, SessionRefused, SessionState, Shelving,
};
pub use sign_in::{Login, LoginEnd, LoginOutcome, SignInState, Waited};
pub use task::SupervisorTask;
pub use workdir::{Bounds, Roots, Workdir, WorkdirError};

use ren_harness::{
    Harness,
    commands::ClientCommand,
    drive::{CommandRefused, DriveEvent, Prompt},
    endpoint::GatewayEndpoint,
    launch::NotDrivable,
    lines::LineLimit,
    models::Model,
    settings::RunSettings,
    sign_in::{SignInAnswer, SignInMethod},
};
use ren_protocol::native::NativeThreadId;
use ren_recorder::{
    Recorder, RunUnrecorded,
    id::{IdSource, RunId, SessionId},
};
use std::{io, path::PathBuf, sync::Arc};
use tokio::sync::{broadcast, mpsc, oneshot};

/// What a supervisor is set up with.
pub struct SupervisorOptions {
    /// The folders runs may work in, as configured; the host resolves them.
    pub roots: Vec<PathBuf>,
    /// Other places runs may start in (`~/.config`), offered only when
    /// asked for: as configured, resolved by the host.
    pub places: Vec<PathBuf>,
    /// Where scratch runs each get a new folder; created when missing.
    /// None: runs cannot start in scratch.
    pub scratch: Option<PathBuf>,
    /// The operator's home, for showing paths as `~/…`.
    pub home: Option<PathBuf>,
    /// When each client's models are first listed.
    pub warmup: Warmup,
    /// Where driven clients send their model traffic.
    pub gateway: GatewayEndpoint,
    /// Ren's version, reported to clients as theirs.
    pub version: String,
    pub line_limit: LineLimit,
    /// When a client sitting idle between turns is stopped.
    pub idle: IdleStop,
}

/// When a client sitting idle between turns is stopped. Its session goes
/// on: the next prompt starts a client that resumes the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleStop {
    Never,
    After(std::time::Duration),
}

impl IdleStop {
    /// Ten minutes: a client held for a turn soon after, and given back
    /// when the operator has moved on.
    pub const DEFAULT: Self = Self::After(std::time::Duration::from_secs(600));
}

/// The handle to every run: start one, command it, list them, and watch them.
/// Clones share the same runs.
#[derive(Clone)]
pub struct Supervisor {
    requests: mpsc::Sender<Request>,
    updates: broadcast::Sender<RunUpdate>,
    sessions: broadcast::Sender<SessionSnapshot>,
}

/// A session as the supervisor serves it: what it is, where it stands,
/// and its newest process, whose state the run feed carries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionSnapshot {
    pub session: SessionId,
    pub harness: Harness,
    pub workdir: PathBuf,
    /// What its next turn runs on; each left out is the client's own.
    pub settings: RunSettings,
    pub state: SessionState,
    pub process: Option<RunId>,
}

/// A new session: its client, where it works, and its settings. Its client
/// starts at once, so its commands are there before the first prompt.
pub struct Create {
    pub harness: Harness,
    pub place: StartPlace,
    pub settings: RunSettings,
    /// A client conversation to go on with (one begun elsewhere), instead
    /// of a new one.
    pub resume: Option<NativeThreadId>,
}

/// A session the store holds that this supervisor has not served since it
/// started, to serve it again.
pub struct SessionSeed {
    pub session: SessionId,
    pub harness: Harness,
    pub workdir: String,
    pub settings: RunSettings,
    /// The client's conversation, when one was opened: its next process
    /// resumes it.
    pub thread: Option<NativeThreadId>,
    pub state: SessionState,
}

/// Why a session request was not carried out.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no session {0}")]
    NoSuchSession(SessionId),
    #[error(transparent)]
    Refused(SessionRefused),
    #[error("no process serves the session")]
    NoProcess,
    #[error("a process still serves the session")]
    Running,
    #[error("cannot read the session's folder: {0}")]
    Unreadable(io::Error),
    #[error("the recorder has no room to record the session")]
    Unrecorded,
    #[error(transparent)]
    Start(StartError),
    #[error(transparent)]
    Command(CommandError),
    #[error("the supervisor has stopped")]
    Gone,
}

/// A change to a run, as it happens.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunUpdate {
    State { run: RunId, state: RunState },
    Event { run: RunId, event: DriveEvent },
}

/// When each client's models are first listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warmup {
    /// As the task starts, for every client installed here, so the first
    /// form that offers them has them at once.
    AtStart,
    /// The first time they are asked for.
    WhenAsked,
}

/// Where a new run works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartPlace {
    /// A new folder of its own in the scratch folder.
    Scratch,
    /// This folder, which must lie inside a root or another place.
    Folder(String),
}

/// Where runs can start: every place, and the folder scratch runs get
/// theirs in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Places {
    pub projects: Vec<Project>,
    /// None when runs cannot start in scratch here.
    pub scratch: Option<String>,
}

/// Which conversation a new run opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversation {
    New,
    /// Continue the client's own conversation, by its thread id: a run that
    /// ended, or was recovered after a restart, picks up where it left off.
    Resume(NativeThreadId),
}

/// One run as the supervisor holds it now.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunSnapshot {
    pub run: RunId,
    /// The session the run serves.
    pub session: SessionId,
    pub harness: Harness,
    pub workdir: PathBuf,
    pub state: RunState,
    /// What it was started with; each left out is the client's own.
    pub settings: RunSettings,
}

/// Why the supervisor could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("cannot resolve a run root")]
    Root(#[source] io::Error),
    #[error(transparent)]
    Roots(WorkdirError),
}

/// Why a run did not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Workdir(WorkdirError),
    #[error("cannot resolve the directory")]
    Resolve(#[source] io::Error),
    #[error("runs cannot start in scratch here: its folder could not be set up")]
    NoScratch,
    #[error("cannot create the scratch run's folder")]
    Scratch(#[source] io::Error),
    #[error(transparent)]
    NotDrivable(NotDrivable),
    #[error("{0} does not take that approval policy")]
    NoApprovals(Harness),
    #[error("the recorder has no room to record the run")]
    Unrecorded(RunUnrecorded),
    #[error("cannot start the client; check it is installed on PATH")]
    Spawn {
        run: RunId,
        #[source]
        source: io::Error,
    },
    #[error("the supervisor has stopped")]
    Gone,
    /// The ID source gave the nil UUID, which names no conversation.
    #[error("cannot name the new conversation: the ID source gave a nil UUID")]
    NilId,
}

/// Why a command was not carried out.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("no run {0}")]
    NoSuchRun(RunId),
    #[error(transparent)]
    Refused(RunRefused),
    #[error(transparent)]
    Protocol(CommandRefused),
    #[error("the supervisor has stopped")]
    Gone,
}

/// What is asked of the sessions.
pub(crate) enum SessionRequest {
    Create(Create),
    Adopt(SessionSeed),
    Send {
        session: SessionId,
        command: RunCommand,
    },
    ChooseClient {
        session: SessionId,
        harness: Harness,
    },
    Shelve {
        session: SessionId,
        shelving: Shelving,
    },
    /// Stop serving a session that is being deleted.
    Forget(SessionId),
    List,
    Commands(SessionId),
    Files {
        session: SessionId,
        query: FileQuery,
        limit: usize,
    },
}

/// What a session request answers.
pub(crate) enum SessionAnswer {
    Created(SessionId),
    Done,
    Listed(Vec<SessionSnapshot>),
    Commands(Vec<ClientCommand>),
    Files(Vec<FileMatch>),
}

/// The supervisor has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the supervisor has stopped")]
pub struct Gone;

/// What a new run starts: which client, where, which conversation, with which
/// settings, and its first prompt.
pub struct Start {
    pub harness: Harness,
    pub place: StartPlace,
    pub conversation: Conversation,
    /// Each left out is the client's own.
    pub settings: RunSettings,
    /// Sent once the conversation is open.
    pub first: Option<Prompt>,
}

pub(crate) enum Request {
    Start {
        start: Start,
        reply: oneshot::Sender<Result<RunId, StartError>>,
    },
    Command {
        run: RunId,
        command: RunCommand,
        reply: oneshot::Sender<Result<(), CommandError>>,
    },
    List {
        reply: oneshot::Sender<Vec<RunSnapshot>>,
    },
    Projects {
        reply: oneshot::Sender<Places>,
    },
    Clients {
        reply: oneshot::Sender<Vec<Client>>,
    },
    Models {
        harness: Harness,
        reply: models::ModelsReply,
    },
    Session {
        request: SessionRequest,
        reply: oneshot::Sender<Result<SessionAnswer, SessionError>>,
    },
    SignIn(SignInRequest),
}

/// A request about a client's sign-in, answered by the task's loop.
pub(crate) enum SignInRequest {
    Check {
        harness: Harness,
        reply: login::CheckReply,
    },
    Start {
        harness: Harness,
        method: SignInMethod,
        reply: oneshot::Sender<Result<tokio::sync::watch::Receiver<Login>, SignInError>>,
    },
    Answer {
        harness: Harness,
        answer: SignInAnswer,
        reply: login::OutcomeReply,
    },
    Cancel {
        harness: Harness,
        reply: oneshot::Sender<Result<(), SignInError>>,
    },
    Wait {
        harness: Harness,
        reply: oneshot::Sender<Result<login::WaitOn, SignInError>>,
    },
}

impl Supervisor {
    /// Requests that wait for the task beyond this are refused at once.
    const QUEUE: usize = 64;
    /// Updates a slow subscriber may fall behind by before it lags.
    const UPDATES: usize = 1024;
    /// How long starting a sign-in waits for the client's link: under the
    /// UI proxy's 30 seconds.
    pub const LINK_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

    /// A supervisor and the task that runs it. Roots are resolved by `host`
    /// now, so a link cannot change what they cover later. Every run is
    /// recorded through `recorder`.
    pub fn new<H: AgentHost>(
        options: SupervisorOptions,
        host: H,
        ids: Arc<dyn IdSource>,
        recorder: Recorder,
    ) -> Result<(Self, SupervisorTask<H>), SupervisorError> {
        let resolved = options
            .roots
            .iter()
            .map(|root| host.resolve(root))
            .collect::<Result<Vec<_>, _>>()
            .map_err(SupervisorError::Root)?;
        let roots = Roots::new(resolved).map_err(SupervisorError::Roots)?;
        // Other places that cannot be resolved are left out, not fatal.
        let places = options
            .places
            .iter()
            .filter_map(|place| {
                host.resolve(place)
                    .inspect_err(
                        |error| tracing::warn!(?place, %error, "a run place is unreadable"),
                    )
                    .ok()
            })
            .collect();
        let scratch = options.scratch.as_deref().and_then(|scratch| {
            host.make_dir(scratch)
                .and_then(|()| host.resolve(scratch))
                .inspect_err(|error| tracing::warn!(?scratch, %error, "no scratch folder for runs"))
                .ok()
        });
        let bounds =
            Bounds::new(roots, places, scratch, options.home).map_err(SupervisorError::Roots)?;
        let (requests, receiver) = mpsc::channel(Self::QUEUE);
        let (updates, _) = broadcast::channel(Self::UPDATES);
        let (sessions, _) = broadcast::channel(Self::UPDATES);
        let task = SupervisorTask {
            requests: receiver,
            updates: updates.clone(),
            sessions: sessions.clone(),
            host,
            ids,
            recorder,
            bounds,
            gateway: options.gateway,
            warmup: options.warmup,
            version: options.version,
            line_limit: options.line_limit,
            idle: options.idle,
        };
        Ok((
            Self {
                requests,
                updates,
                sessions,
            },
            task,
        ))
    }

    /// Start a run.
    pub async fn start(&self, start: Start) -> Result<RunId, StartError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Start { start, reply })
            .await
            .map_err(|_| StartError::Gone)?;
        answer.await.map_err(|_| StartError::Gone)?
    }

    pub async fn command(&self, run: RunId, command: RunCommand) -> Result<(), CommandError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Command {
                run,
                command,
                reply,
            })
            .await
            .map_err(|_| CommandError::Gone)?;
        answer.await.map_err(|_| CommandError::Gone)?
    }

    /// Every run the supervisor knows of, ended ones included.
    pub async fn runs(&self) -> Result<Vec<RunSnapshot>, Gone> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::List { reply })
            .await
            .map_err(|_| Gone)?;
        answer.await.map_err(|_| Gone)
    }

    /// The projects a run can start in: the git repositories under the
    /// roots, and each root's folders with none below them.
    pub async fn projects(&self) -> Result<Places, Gone> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Projects { reply })
            .await
            .map_err(|_| Gone)?;
        answer.await.map_err(|_| Gone)
    }

    /// Every client, whether a run can start with it here, and its sign-in.
    /// Waits for sign-in checks running now, each bounded.
    pub async fn clients(&self) -> Result<Vec<Client>, Gone> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Clients { reply })
            .await
            .map_err(|_| Gone)?;
        answer.await.map_err(|_| Gone)
    }

    /// The models `harness` offers, as it lists them itself. The first
    /// non-empty answer is kept for as long as the supervisor runs.
    pub async fn models(&self, harness: Harness) -> Result<Vec<Model>, ModelsError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Models { harness, reply })
            .await
            .map_err(|_| ModelsError::Gone)?;
        answer.await.map_err(|_| ModelsError::Gone)?
    }

    async fn sign_in_request<T>(
        &self,
        request: impl FnOnce(oneshot::Sender<Result<T, SignInError>>) -> SignInRequest,
    ) -> Result<T, SignInError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::SignIn(request(reply)))
            .await
            .map_err(|_| SignInError::Gone)?;
        answer.await.map_err(|_| SignInError::Gone)?
    }

    /// Check `harness`'s sign-in now with its own status command.
    pub async fn check_sign_in(&self, harness: Harness) -> Result<SignInState, SignInError> {
        self.sign_in_request(|reply| SignInRequest::Check { harness, reply })
            .await
    }

    /// Start `harness`'s own login with `method`, replacing any running
    /// one. Answers once it shows what the operator needs (its link, a
    /// device login's code, or its key prompt),
    /// or with how it stands after [`Self::LINK_WAIT`].
    pub async fn sign_in(
        &self,
        harness: Harness,
        method: SignInMethod,
    ) -> Result<Login, SignInError> {
        let mut login = self
            .sign_in_request(|reply| SignInRequest::Start {
                harness,
                method,
                reply,
            })
            .await?;
        let shown = tokio::time::timeout(Self::LINK_WAIT, login.wait_for(Login::shown))
            .await
            .map(|shown| shown.map(|state| state.clone()));
        Ok(match shown {
            Ok(Ok(state)) => state,
            // The login ended and its state went with it: it was replaced.
            Ok(Err(_)) => Login::Ended(LoginEnd::Cancelled),
            Err(_) => login.borrow().clone(),
        })
    }

    /// Hand `answer` to `harness`'s login: the code the page showed, an API
    /// key, or the address the browser could not open (opened on this host
    /// when it is the one the login waits on). Answers once the login has
    /// ended and the client's sign-in was checked.
    pub async fn answer_sign_in(
        &self,
        harness: Harness,
        answer: SignInAnswer,
    ) -> Result<LoginOutcome, SignInError> {
        self.sign_in_request(|reply| SignInRequest::Answer {
            harness,
            answer,
            reply,
        })
        .await
    }

    /// Wait for `harness`'s login to end and its sign-in to be checked, at
    /// most `within`: a device login ends by itself once approved. Answers
    /// with how it stands when that time is up.
    pub async fn wait_sign_in(
        &self,
        harness: Harness,
        within: std::time::Duration,
    ) -> Result<Waited, SignInError> {
        match self
            .sign_in_request(|reply| SignInRequest::Wait { harness, reply })
            .await?
        {
            login::WaitOn::Done(outcome) => Ok(Waited::Done { outcome }),
            login::WaitOn::Going { login, outcome } => {
                match tokio::time::timeout(within, outcome).await {
                    Ok(Ok(outcome)) => outcome.map(|outcome| Waited::Done { outcome }),
                    Ok(Err(_)) => Err(SignInError::Gone),
                    Err(_) => Ok(Waited::Going { login }),
                }
            }
        }
    }

    /// Cancel `harness`'s running login.
    pub async fn cancel_sign_in(&self, harness: Harness) -> Result<(), SignInError> {
        self.sign_in_request(|reply| SignInRequest::Cancel { harness, reply })
            .await
    }

    async fn session(&self, request: SessionRequest) -> Result<SessionAnswer, SessionError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Session { request, reply })
            .await
            .map_err(|_| SessionError::Gone)?;
        answer.await.map_err(|_| SessionError::Gone)?
    }

    /// A new session, its client started in it.
    pub async fn create(&self, create: Create) -> Result<SessionId, SessionError> {
        match self.session(SessionRequest::Create(create)).await? {
            SessionAnswer::Created(session) => Ok(session),
            SessionAnswer::Done
            | SessionAnswer::Listed(_)
            | SessionAnswer::Commands(_)
            | SessionAnswer::Files(_) => Err(SessionError::Gone),
        }
    }

    /// Serve a session the store holds; nothing when it is served already.
    pub async fn adopt(&self, seed: SessionSeed) -> Result<(), SessionError> {
        self.session(SessionRequest::Adopt(seed)).await.map(|_| ())
    }

    /// Carry out `command` in `session`: through its process, or, for a
    /// prompt with none running, through a new one that resumes its
    /// conversation; settings with none running apply to the next.
    pub async fn send(&self, session: SessionId, command: RunCommand) -> Result<(), SessionError> {
        self.session(SessionRequest::Send { session, command })
            .await
            .map(|_| ())
    }

    /// Work with `harness` instead, while the session is a draft: its
    /// process is replaced, and its settings go back to the client's own.
    pub async fn choose_client(
        &self,
        session: SessionId,
        harness: Harness,
    ) -> Result<(), SessionError> {
        self.session(SessionRequest::ChooseClient { session, harness })
            .await
            .map(|_| ())
    }

    /// Archive a session (which stops its process) or unarchive it.
    pub async fn shelve(&self, session: SessionId, shelving: Shelving) -> Result<(), SessionError> {
        self.session(SessionRequest::Shelve { session, shelving })
            .await
            .map(|_| ())
    }

    /// Stop serving `session`, which is being deleted, with its ended runs:
    /// refused while a process serves it. Whether it was served.
    pub async fn forget(&self, session: SessionId) -> Result<bool, SessionError> {
        match self.session(SessionRequest::Forget(session)).await {
            Ok(_) => Ok(true),
            Err(SessionError::NoSuchSession(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Every session served since the supervisor started.
    pub async fn sessions(&self) -> Result<Vec<SessionSnapshot>, SessionError> {
        match self.session(SessionRequest::List).await? {
            SessionAnswer::Listed(sessions) => Ok(sessions),
            SessionAnswer::Created(_)
            | SessionAnswer::Done
            | SessionAnswer::Commands(_)
            | SessionAnswer::Files(_) => Err(SessionError::Gone),
        }
    }

    /// What the session's client offers a prompt: none while no process
    /// has opened its conversation.
    pub async fn session_commands(
        &self,
        session: SessionId,
    ) -> Result<Vec<ClientCommand>, SessionError> {
        match self.session(SessionRequest::Commands(session)).await? {
            SessionAnswer::Commands(commands) => Ok(commands),
            SessionAnswer::Created(_)
            | SessionAnswer::Done
            | SessionAnswer::Listed(_)
            | SessionAnswer::Files(_) => Err(SessionError::Gone),
        }
    }

    /// The `limit` paths of the session's folder that best match `query`,
    /// with or without a process.
    pub async fn session_files(
        &self,
        session: SessionId,
        query: FileQuery,
        limit: usize,
    ) -> Result<Vec<FileMatch>, SessionError> {
        match self
            .session(SessionRequest::Files {
                session,
                query,
                limit,
            })
            .await?
        {
            SessionAnswer::Files(files) => Ok(files),
            SessionAnswer::Created(_)
            | SessionAnswer::Done
            | SessionAnswer::Listed(_)
            | SessionAnswer::Commands(_) => Err(SessionError::Gone),
        }
    }

    /// Session changes from now on.
    pub fn subscribe_sessions(&self) -> broadcast::Receiver<SessionSnapshot> {
        self.sessions.subscribe()
    }

    /// Updates from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<RunUpdate> {
        self.updates.subscribe()
    }
}
