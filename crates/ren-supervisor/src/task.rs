//! The shell around the pure session and run machines: one task owns every
//! session and every run (a session's client process), reads each agent's
//! stdout, writes its stdin, and waits for its exit. Components do not
//! spawn, so the daemon runs this task; it ends once every [`Supervisor`]
//! handle is gone and every run has ended.
//!
//! Writes to an agent's stdin are awaited in the task. Ren's lines are small,
//! so this waits only if an agent stops reading its stdin altogether.
//!
//! [`Supervisor`]: crate::Supervisor

use crate::entries::Entries;
use crate::login::{SignInError, SignIns, StatusCheck};
use crate::models::{Listings, ModelProbe, ModelsError, ModelsReply};
use crate::{
    CommandError, Conversation, Create, IdleStop, Places, Request, RunSnapshot, RunUpdate,
    SessionAnswer, SessionError, SessionRequest, SessionSeed, SessionSnapshot, SignInRequest,
    Start, StartError, StartPlace, Warmup,
    client::{Client, ClientStatus},
    files::FileListing,
    host::{AgentHost, AgentProcess, KillSwitch},
    project::Search,
    run::{ProcessExit, RunAction, RunCommand, RunEvent, RunState},
    session::{SessionAction, SessionInput, SessionState, Shelving},
    workdir::{Bounds, RequestedDir, Workdir},
};
use futures_util::{
    FutureExt, StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream, FuturesUnordered, SelectAll},
};
use ren_harness::{
    Harness,
    drive::{DriveEvent, Driver, Opening, Prompt, TurnEnd},
    endpoint::GatewayEndpoint,
    lines::{Framed, Line, LineLimit, Lines},
    models::{DefaultChoice, ModelId, ModelListing},
    settings::{ApprovalPolicy, Midway, RunSettings},
};
use ren_protocol::label::Labeled;
use ren_protocol::native::NativeThreadId;
use ren_recorder::{
    Recorder, RunSession, RunTap,
    batch::SessionFact,
    capture::RunEnd,
    id::{IdSource, RunId, SessionId},
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{broadcast, mpsc, oneshot},
};

/// Runs every run's I/O. Returned by [`Supervisor::new`](crate::Supervisor::new).
#[must_use = "the supervisor does nothing until its task runs"]
pub struct SupervisorTask<H> {
    pub(crate) requests: mpsc::Receiver<Request>,
    pub(crate) updates: broadcast::Sender<RunUpdate>,
    pub(crate) sessions: broadcast::Sender<SessionSnapshot>,
    pub(crate) host: H,
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) recorder: Recorder,
    pub(crate) bounds: Bounds,
    pub(crate) gateway: GatewayEndpoint,
    pub(crate) warmup: Warmup,
    pub(crate) version: String,
    pub(crate) line_limit: LineLimit,
    pub(crate) idle: IdleStop,
}

/// One run the task knows of.
struct Live {
    /// The session the run serves.
    session: SessionId,
    harness: Harness,
    workdir: Workdir,
    settings: RunSettings,
    state: RunState,
    codec: Option<Driver>,
    stdin: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    kill: Option<KillSwitch>,
    /// Records the run's opening and end; taken when the end is recorded.
    tap: Option<RunTap>,
    /// The first prompt, sent once the conversation opens.
    first: Option<Prompt>,
    /// What the run's conversation keeps of its client's events.
    entries: Entries,
    /// How many times the run became idle: a timer set for an earlier time
    /// is stale.
    rested: u64,
    /// It became idle and its timer is not set yet.
    arm: bool,
}

/// One session the task serves.
struct Held {
    state: SessionState,
    harness: Harness,
    workdir: Workdir,
    /// What its next turn runs on.
    settings: RunSettings,
    /// The client's conversation, once a process opened it: the next
    /// process resumes it.
    thread: Option<NativeThreadId>,
    /// Its newest process.
    process: Option<RunId>,
    /// Its folder as walked for the last file search, until a turn ends:
    /// turns change files.
    files: Option<FileListing>,
}

impl Held {
    fn new(harness: Harness, workdir: Workdir, settings: RunSettings) -> Self {
        Self {
            state: SessionState::NEW,
            harness,
            workdir,
            settings,
            thread: None,
            process: None,
            files: None,
        }
    }

    fn snapshot(&self, session: SessionId) -> SessionSnapshot {
        SessionSnapshot {
            session,
            harness: self.harness,
            workdir: self.workdir.as_path().to_path_buf(),
            settings: self.settings.clone(),
            state: self.state,
            process: self.process,
        }
    }
}

type Outputs = SelectAll<BoxStream<'static, (RunId, Vec<Framed>)>>;
type Exits = FuturesUnordered<BoxFuture<'static, (RunId, ProcessExit)>>;

/// Everything the task serves: sessions, their runs, and the runs' output
/// and exits it waits on.
#[derive(Default)]
struct Served {
    sessions: BTreeMap<SessionId, Held>,
    runs: BTreeMap<RunId, Live>,
    outputs: Outputs,
    exits: Exits,
    /// Idle timers: the run, and which time it became idle.
    idles: FuturesUnordered<BoxFuture<'static, (RunId, u64)>>,
    /// The default model each client listed, for a client that is started
    /// on it by name when none is chosen.
    defaults: HashMap<Harness, ModelId>,
}

impl Served {
    /// The run serving `session` now, if one runs.
    fn process(&self, session: SessionId) -> Option<RunId> {
        let run = self.sessions.get(&session)?.process?;
        match self.runs.get(&run)?.state {
            RunState::Ended { .. } => None,
            RunState::Starting
            | RunState::Idle { .. }
            | RunState::Working { .. }
            | RunState::Stopping { .. } => Some(run),
        }
    }
}

impl<H: AgentHost> SupervisorTask<H> {
    pub async fn run(mut self) {
        let mut served = Served::default();
        let mut listings = Listings::default();
        let mut sign_ins = SignIns::default();
        // Who asked for the clients while sign-in checks were running.
        let mut clients_waiting: Vec<oneshot::Sender<Vec<Client>>> = Vec::new();
        match self.warmup {
            Warmup::AtStart => {
                self.warm(&mut listings);
                self.check_unchecked(&mut sign_ins);
            }
            Warmup::WhenAsked => {}
        }
        let mut accepting = true;
        loop {
            tokio::select! {
                request = self.requests.recv(), if accepting => match request {
                    Some(Request::Models { harness, reply }) => self.models(harness, reply, &mut listings),
                    Some(Request::Clients { reply }) => {
                        self.check_unchecked(&mut sign_ins);
                        if sign_ins.checking() {
                            clients_waiting.push(reply);
                        } else {
                            let _ = reply.send(self.clients(&sign_ins));
                        }
                    }
                    Some(Request::SignIn(request)) => self.sign_in(request, &mut sign_ins),
                    Some(request) => self.request(request, &mut served).await,
                    None => {
                        accepting = false;
                        for (run, live) in &mut served.runs {
                            self.apply(*run, live, RunEvent::Shutdown).await;
                        }
                    }
                },
                Some((run, frames)) = served.outputs.next(), if !served.outputs.is_empty() => {
                    if let Some(live) = served.runs.get_mut(&run) {
                        let mut held = served.sessions.get_mut(&live.session);
                        let mut failed = false;
                        for frame in frames {
                            failed |= self.frame(run, live, held.as_deref_mut(), frame).await;
                        }
                        // A failed turn may be a sign-in that ran out:
                        // read the client's sign-in again.
                        if failed {
                            let harness = live.harness;
                            sign_ins.check(harness, None, || self.status_check(harness));
                        }
                    }
                }
                Some((harness, outcome)) = listings.running.next(), if !listings.running.is_empty() => {
                    if let Ok(models) = &outcome
                        && let Some(listed) = models
                            .iter()
                            .find(|model| model.choice == DefaultChoice::Default)
                    {
                        served.defaults.insert(harness, listed.id.clone());
                    }
                    listings.finish(harness, outcome);
                }
                Some((harness, state)) = sign_ins.checks.next(), if !sign_ins.checks.is_empty() => {
                    sign_ins.checked(harness, state);
                    if !sign_ins.checking() {
                        let clients = self.clients(&sign_ins);
                        for reply in clients_waiting.drain(..) {
                            let _ = reply.send(clients.clone());
                        }
                    }
                }
                Some((harness, attempt, end)) = sign_ins.talks.next(), if !sign_ins.talks.is_empty() => {
                    tracing::debug!(%harness, ?end, "a client's sign-in ended");
                    if sign_ins.talked(harness, attempt, end) {
                        let mut unstarted = None;
                        sign_ins.check(harness, None, || {
                            self.status_check(harness).inspect_err(|error| unstarted = Some(error.clone()))
                        });
                        if let Some(error) = unstarted {
                            sign_ins.unchecked(harness, &error);
                        }
                    }
                }
                Some((run, rested)) = served.idles.next(), if !served.idles.is_empty() => {
                    if let Some(live) = served.runs.get_mut(&run)
                        && live.rested == rested
                    {
                        self.apply(run, live, RunEvent::IdleTimeout).await;
                    }
                }
                Some((run, exit)) = served.exits.next(), if !served.exits.is_empty() => {
                    if let Some(live) = served.runs.get_mut(&run) {
                        live.stdin = None;
                        live.kill = None;
                        self.apply(run, live, RunEvent::Exited(exit)).await;
                    }
                }
                else => break,
            }
            self.arm(&mut served);
            if !accepting && served.exits.is_empty() {
                break;
            }
        }
    }

    /// Set the timer of each run that became idle.
    fn arm(&self, served: &mut Served) {
        let IdleStop::After(after) = self.idle else {
            return;
        };
        for (run, live) in &mut served.runs {
            if std::mem::take(&mut live.arm) {
                let (run, rested) = (*run, live.rested);
                served.idles.push(
                    tokio::time::sleep(after)
                        .map(move |()| (run, rested))
                        .boxed(),
                );
            }
        }
    }

    async fn request(&self, request: Request, served: &mut Served) {
        match request {
            Request::Start { start, reply } => {
                let started = self.start(start, served).await;
                let _ = reply.send(started);
            }
            Request::Command {
                run,
                command,
                reply,
            } => {
                let result = self.run_command(run, command, served).await;
                let _ = reply.send(result);
            }
            Request::Session { request, reply } => {
                let answer = self.session(request, served).await;
                let _ = reply.send(answer);
            }
            Request::Projects { reply } => {
                // Bounded by `Search`; the host reads only folder listings
                // and `HEAD` files.
                let projects = self.bounds.places(Search::DEFAULT, |folder| {
                    self.host
                        .look(folder)
                        .inspect_err(|error| tracing::debug!(%error, "cannot look in a folder"))
                        .ok()
                });
                let _ = reply.send(Places {
                    projects,
                    scratch: self
                        .bounds
                        .scratch()
                        .and_then(|scratch| scratch.to_str())
                        .map(str::to_owned),
                });
            }
            Request::Clients { reply } => {
                // Answered by the loop, which holds the sign-ins.
                drop(reply);
                tracing::debug!("a clients request reached the run handler");
            }
            Request::SignIn(_) => {
                // Answered by the loop, which holds the sign-ins.
                tracing::debug!("a sign-in request reached the run handler");
            }
            Request::Models { harness, reply } => {
                // Answered by the loop, which holds the listings.
                let _ = reply.send(Err(ModelsError::Gone));
                tracing::debug!(%harness, "a models request reached the run handler");
            }
            Request::List { reply } => {
                let _ = reply.send(
                    served
                        .runs
                        .iter()
                        .map(|(run, live)| RunSnapshot {
                            run: *run,
                            session: live.session,
                            harness: live.harness,
                            workdir: live.workdir.as_path().to_path_buf(),
                            state: live.state.clone(),
                            settings: live.settings.clone(),
                        })
                        .collect(),
                );
            }
        }
    }

    /// The session's folder as last walked, walked now when it is not
    /// kept. Bounded by `FileListing::LIMITS`, so a walk is short.
    fn files<'a>(&self, held: &'a mut Held) -> Result<&'a FileListing, SessionError> {
        if held.files.is_none() {
            let found = self
                .host
                .files(held.workdir.as_path(), FileListing::LIMITS)
                .map_err(SessionError::Unreadable)?;
            held.files = Some(FileListing::new(found));
        }
        held.files.as_ref().ok_or(SessionError::Gone)
    }

    /// The folder a session works in: a folder inside the bounds, or a new
    /// scratch folder of its own.
    fn place(&self, place: StartPlace) -> Result<Workdir, StartError> {
        let requested = match place {
            StartPlace::Folder(dir) => RequestedDir::parse(&dir).map_err(StartError::Workdir)?,
            StartPlace::Scratch => {
                let scratch = self.bounds.scratch().ok_or(StartError::NoScratch)?;
                // Its last eight hex digits: random for real ids, and
                // distinct for sequential ones.
                let id = self.ids.next_uuid().simple().to_string();
                let folder = scratch.join(format!(
                    "scratch-{}",
                    id.get(id.len().saturating_sub(8)..).unwrap_or(&id)
                ));
                self.host
                    .make_scratch(&folder)
                    .map_err(StartError::Scratch)?;
                RequestedDir::parse(folder.to_str().ok_or(StartError::NoScratch)?)
                    .map_err(StartError::Workdir)?
            }
        };
        self.confirm(&requested)
    }

    fn confirm(&self, requested: &RequestedDir) -> Result<Workdir, StartError> {
        let resolved = self
            .host
            .resolve(requested.as_path())
            .map_err(StartError::Resolve)?;
        Workdir::confirm(&self.bounds, resolved).map_err(StartError::Workdir)
    }

    /// Whether `harness` can be driven, on `settings`.
    fn drivable(harness: Harness, settings: &RunSettings) -> Result<(), StartError> {
        harness.driven_program().map_err(StartError::NotDrivable)?;
        match settings.approvals {
            Some(policy) if !ApprovalPolicy::offered(harness).contains(&policy) => {
                Err(StartError::NoApprovals(harness))
            }
            Some(_) | None => Ok(()),
        }
    }

    /// A run: a session and its first process, sent `first` once open.
    async fn start(
        &self,
        Start {
            harness,
            place,
            conversation,
            settings,
            first,
        }: Start,
        served: &mut Served,
    ) -> Result<RunId, StartError> {
        let workdir = self.place(place)?;
        Self::drivable(harness, &settings)?;
        let session = SessionId::generate(self.ids.as_ref());
        let mut held = Held::new(harness, workdir, settings);
        if let Conversation::Resume(thread) = conversation {
            held.thread = Some(thread);
        }
        if first.is_some() {
            Self::advance(&mut held, SessionInput::Prompted);
        }
        let started = self
            .spawn(session, &mut held, RunSession::New(session), first, served)
            .await;
        // A run the recorder had no room for leaves no session.
        if !matches!(started, Err(StartError::Unrecorded(_))) {
            served.sessions.insert(session, held);
        }
        started
    }

    /// A new process for `session`, recorded as `recorded`: it resumes the
    /// session's conversation when one was opened, and is sent `first` once
    /// open. A process that cannot be spawned is kept, ended, as its run.
    async fn spawn(
        &self,
        session: SessionId,
        held: &mut Held,
        recorded: RunSession,
        first: Option<Prompt>,
        served: &mut Served,
    ) -> Result<RunId, StartError> {
        let harness = held.harness;
        // A client whose launch changes its own default starts on the one it
        // listed, by name, when the session chose none.
        if held.settings.model.is_none()
            && harness.names_its_default()
            && let Some(listed) = served.defaults.get(&harness)
        {
            held.settings.model = Some(listed.clone());
        }
        let settings = held.settings.clone();
        let workdir = held.workdir.clone();
        let opening = match held.thread {
            None => Opening::New {
                cwd: workdir.as_str().to_owned(),
                id: NativeThreadId::new(self.ids.next_uuid()).ok_or(StartError::NilId)?,
                settings: settings.clone(),
            },
            Some(thread) => Opening::Resume {
                thread,
                cwd: workdir.as_str().to_owned(),
                settings: settings.clone(),
            },
        };
        let plan = harness
            .drive(&self.gateway, &opening)
            .map_err(StartError::NotDrivable)?;
        let (codec, opening_lines) =
            Driver::open(harness, opening, &self.version, self.ids.next_uuid())
                .map_err(StartError::NotDrivable)?;
        let run = RunId::generate(self.ids.as_ref());
        let tools = self.host.tools();
        // Every run is recorded: without room, it does not start.
        let tap = self
            .recorder
            .run(
                run,
                recorded,
                harness,
                workdir.as_str().to_owned(),
                settings.clone(),
                tools.generation(),
            )
            .map_err(StartError::Unrecorded)?;
        held.process = Some(run);
        let process = match self.host.spawn(&plan, &workdir, &tools) {
            Ok(process) => process,
            Err(source) => {
                tap.end(RunEnd::SpawnFailed, None);
                let state = RunState::Ended {
                    end: RunEnd::SpawnFailed,
                    exit: None,
                };
                self.publish(RunUpdate::State {
                    run,
                    state: state.clone(),
                });
                served.runs.insert(
                    run,
                    Live {
                        session,
                        harness,
                        workdir,
                        settings,
                        state,
                        codec: None,
                        stdin: None,
                        kill: None,
                        tap: None,
                        first: None,
                        entries: Entries::default(),
                        rested: 0,
                        arm: false,
                    },
                );
                return Err(StartError::Spawn { run, source });
            }
        };
        let AgentProcess {
            stdin,
            stdout,
            exit,
            kill,
        } = process;
        let mut live = Live {
            session,
            harness,
            workdir,
            settings,
            state: RunState::Starting,
            codec: Some(codec),
            stdin: Some(stdin),
            kill: Some(kill),
            tap: Some(tap),
            first,
            entries: Entries::default(),
            rested: 0,
            arm: false,
        };
        self.publish(RunUpdate::State {
            run,
            state: RunState::Starting,
        });
        for line in &opening_lines {
            Self::write(run, &mut live, line).await;
        }
        served
            .outputs
            .push(Self::output(run, stdout, self.line_limit));
        served.exits.push(exit.map(move |exit| (run, exit)).boxed());
        served.runs.insert(run, live);
        Ok(run)
    }

    /// Move `held` on by an input its state takes: a prompt or a change
    /// the caller already checked. A refusal here changes nothing.
    fn advance(held: &mut Held, input: SessionInput) -> Vec<SessionAction> {
        match held.state.apply(input) {
            Ok((state, actions)) => {
                held.state = state;
                actions
            }
            Err(refused) => {
                tracing::debug!(%refused, "a session input was refused");
                Vec::new()
            }
        }
    }

    /// Record `fact` about `session`; without room it is counted lost by
    /// the caller's error.
    fn record(&self, session: SessionId, fact: SessionFact) -> Result<(), SessionError> {
        self.recorder
            .session_slot()
            .ok_or(SessionError::Unrecorded)?
            .record(session, fact);
        Ok(())
    }

    fn publish_session(&self, session: SessionId, held: &Held) {
        // No subscriber is not an error.
        let _ = self.sessions.send(held.snapshot(session));
    }

    async fn session(
        &self,
        request: SessionRequest,
        served: &mut Served,
    ) -> Result<SessionAnswer, SessionError> {
        match request {
            SessionRequest::Create(create) => self.create(create, served).await,
            SessionRequest::Adopt(seed) => self.adopt(seed, served),
            SessionRequest::Send { session, command } => self.send(session, command, served).await,
            SessionRequest::ChooseClient { session, harness } => {
                self.choose_client(session, harness, served).await
            }
            SessionRequest::Shelve { session, shelving } => {
                self.shelve(session, shelving, served).await
            }
            SessionRequest::Forget(session) => {
                if !served.sessions.contains_key(&session) {
                    return Err(SessionError::NoSuchSession(session));
                }
                if served.process(session).is_some() {
                    return Err(SessionError::Running);
                }
                served.sessions.remove(&session);
                served.runs.retain(|_, live| live.session != session);
                Ok(SessionAnswer::Done)
            }
            SessionRequest::List => Ok(SessionAnswer::Listed(
                served
                    .sessions
                    .iter()
                    .map(|(session, held)| held.snapshot(*session))
                    .collect(),
            )),
            SessionRequest::Commands(session) => {
                if !served.sessions.contains_key(&session) {
                    return Err(SessionError::NoSuchSession(session));
                }
                Ok(SessionAnswer::Commands(
                    served
                        .process(session)
                        .and_then(|run| served.runs.get(&run))
                        .and_then(|live| live.codec.as_ref())
                        .map(|codec| codec.commands().to_vec())
                        .unwrap_or_default(),
                ))
            }
            SessionRequest::Files {
                session,
                query,
                limit,
            } => {
                let held = served
                    .sessions
                    .get_mut(&session)
                    .ok_or(SessionError::NoSuchSession(session))?;
                let files = self.files(held)?;
                Ok(SessionAnswer::Files(files.search(&query, limit)))
            }
        }
    }

    async fn create(
        &self,
        Create {
            harness,
            place,
            settings,
            resume,
        }: Create,
        served: &mut Served,
    ) -> Result<SessionAnswer, SessionError> {
        let workdir = self.place(place).map_err(SessionError::Start)?;
        Self::drivable(harness, &settings).map_err(SessionError::Start)?;
        let session = SessionId::generate(self.ids.as_ref());
        self.record(
            session,
            SessionFact::Created {
                harness,
                workdir: workdir.as_str().to_owned(),
                settings: settings.clone(),
            },
        )?;
        let mut held = Held::new(harness, workdir, settings);
        held.thread = resume;
        // Its client starts now, so its commands are there before the first
        // prompt. One that cannot start is kept, ended, as its process.
        if let Err(error) = self
            .spawn(
                session,
                &mut held,
                RunSession::Existing(session),
                None,
                served,
            )
            .await
        {
            tracing::debug!(%session, %error, "a new session's client did not start");
        }
        self.publish_session(session, &held);
        served.sessions.insert(session, held);
        Ok(SessionAnswer::Created(session))
    }

    fn adopt(&self, seed: SessionSeed, served: &mut Served) -> Result<SessionAnswer, SessionError> {
        if served.sessions.contains_key(&seed.session) {
            return Ok(SessionAnswer::Done);
        }
        let requested = RequestedDir::parse(&seed.workdir)
            .map_err(|error| SessionError::Start(StartError::Workdir(error)))?;
        let workdir = self.confirm(&requested).map_err(SessionError::Start)?;
        let mut held = Held::new(seed.harness, workdir, seed.settings);
        held.state = seed.state;
        held.thread = seed.thread;
        self.publish_session(seed.session, &held);
        served.sessions.insert(seed.session, held);
        Ok(SessionAnswer::Done)
    }

    async fn send(
        &self,
        session: SessionId,
        command: RunCommand,
        served: &mut Served,
    ) -> Result<SessionAnswer, SessionError> {
        if !served.sessions.contains_key(&session) {
            return Err(SessionError::NoSuchSession(session));
        }
        let prompt = match &command {
            RunCommand::Prompt(prompt) | RunCommand::Queue(prompt) | RunCommand::Steer(prompt) => {
                Some(prompt.clone())
            }
            RunCommand::Unqueue
            | RunCommand::Configure(_)
            | RunCommand::Answer { .. }
            | RunCommand::Interrupt
            | RunCommand::Stop => None,
        };
        if let Some(run) = served.process(session) {
            let Some(live) = served.runs.get_mut(&run) else {
                return Err(SessionError::NoProcess);
            };
            match (&live.state, prompt.clone()) {
                // Still opening: the prompt waits for it, as a first one.
                (RunState::Starting, Some(prompt)) if live.first.is_none() => {
                    live.first = Some(prompt);
                }
                (
                    RunState::Starting
                    | RunState::Idle { .. }
                    | RunState::Working { .. }
                    | RunState::Stopping { .. }
                    | RunState::Ended { .. },
                    Some(_) | None,
                ) => self
                    .command(run, live, command)
                    .await
                    .map_err(SessionError::Command)?,
            }
            let settings = live.settings.clone();
            if let Some(held) = served.sessions.get_mut(&session) {
                held.settings = settings;
                self.prompted(session, held, prompt.is_some())?;
                self.publish_session(session, held);
            }
            return Ok(SessionAnswer::Done);
        }
        let Some(mut held) = served.sessions.remove(&session) else {
            return Err(SessionError::NoSuchSession(session));
        };
        let answer = match (command, prompt) {
            (RunCommand::Configure(settings), _) => self
                .record(
                    session,
                    SessionFact::Configured {
                        settings: settings.clone(),
                    },
                )
                .map(|()| {
                    held.settings = settings;
                    SessionAnswer::Done
                }),
            // No process: a new one resumes the conversation with it.
            (_, Some(prompt)) => match self.prompted(session, &mut held, true) {
                Ok(()) => self
                    .spawn(
                        session,
                        &mut held,
                        RunSession::Existing(session),
                        Some(prompt),
                        served,
                    )
                    .await
                    .map(|_| SessionAnswer::Done)
                    .map_err(SessionError::Start),
                Err(error) => Err(error),
            },
            (
                RunCommand::Prompt(_)
                | RunCommand::Queue(_)
                | RunCommand::Steer(_)
                | RunCommand::Unqueue
                | RunCommand::Answer { .. }
                | RunCommand::Interrupt
                | RunCommand::Stop,
                None,
            ) => Err(SessionError::NoProcess),
        };
        self.publish_session(session, &held);
        served.sessions.insert(session, held);
        answer
    }

    /// A prompt went out in the session (when `sent`): it is active, and
    /// current again if it was archived.
    fn prompted(
        &self,
        session: SessionId,
        held: &mut Held,
        sent: bool,
    ) -> Result<(), SessionError> {
        if !sent {
            return Ok(());
        }
        if let SessionState::Archived(_) = held.state {
            self.record(session, SessionFact::Unarchived)?;
        }
        Self::advance(held, SessionInput::Prompted);
        Ok(())
    }

    async fn choose_client(
        &self,
        session: SessionId,
        harness: Harness,
        served: &mut Served,
    ) -> Result<SessionAnswer, SessionError> {
        let held = served
            .sessions
            .get(&session)
            .ok_or(SessionError::NoSuchSession(session))?;
        held.state
            .apply(SessionInput::ChooseClient(harness))
            .map_err(SessionError::Refused)?;
        Self::drivable(harness, &RunSettings::default()).map_err(SessionError::Start)?;
        self.record(session, SessionFact::ClientChosen { harness })?;
        // The old client's process goes; its model and settings with it.
        self.stop_process(session, served).await;
        let Some(mut held) = served.sessions.remove(&session) else {
            return Err(SessionError::NoSuchSession(session));
        };
        Self::advance(&mut held, SessionInput::ChooseClient(harness));
        held.harness = harness;
        held.settings = RunSettings::default();
        held.thread = None;
        if let Err(error) = self
            .spawn(
                session,
                &mut held,
                RunSession::Existing(session),
                None,
                served,
            )
            .await
        {
            tracing::debug!(%session, %error, "the chosen client did not start");
        }
        self.publish_session(session, &held);
        served.sessions.insert(session, held);
        Ok(SessionAnswer::Done)
    }

    async fn shelve(
        &self,
        session: SessionId,
        shelving: Shelving,
        served: &mut Served,
    ) -> Result<SessionAnswer, SessionError> {
        let held = served
            .sessions
            .get(&session)
            .ok_or(SessionError::NoSuchSession(session))?;
        let (state, actions) = held
            .state
            .apply(shelving.input())
            .map_err(SessionError::Refused)?;
        let fact = match shelving {
            Shelving::Archive => SessionFact::Archived,
            Shelving::Unarchive => SessionFact::Unarchived,
        };
        self.record(session, fact)?;
        for action in actions {
            match action {
                SessionAction::StopProcess => self.stop_process(session, served).await,
                SessionAction::UseClient(_) => {}
            }
        }
        if let Some(held) = served.sessions.get_mut(&session) {
            held.state = state;
            self.publish_session(session, held);
        }
        Ok(SessionAnswer::Done)
    }

    /// Stop the process serving `session`, if one runs.
    async fn stop_process(&self, session: SessionId, served: &mut Served) {
        if let Some(run) = served.process(session)
            && let Some(live) = served.runs.get_mut(&run)
            && let Err(error) = self.command(run, live, RunCommand::Stop).await
        {
            tracing::debug!(%session, %error, "the session's process would not stop");
        }
    }

    /// An operator command to a run, and to the session it serves.
    async fn run_command(
        &self,
        run: RunId,
        command: RunCommand,
        served: &mut Served,
    ) -> Result<(), CommandError> {
        let live = served
            .runs
            .get_mut(&run)
            .ok_or(CommandError::NoSuchRun(run))?;
        let session = live.session;
        let prompted = matches!(
            command,
            RunCommand::Prompt(_) | RunCommand::Queue(_) | RunCommand::Steer(_)
        );
        self.command(run, live, command).await?;
        let settings = live.settings.clone();
        if let Some(held) = served.sessions.get_mut(&session) {
            held.settings = settings;
            if prompted && let Err(error) = self.prompted(session, held, true) {
                tracing::debug!(%session, %error, "the session's return was not recorded");
            }
            self.publish_session(session, held);
        }
        Ok(())
    }

    /// Whether `harness`'s program is on this host's path.
    fn installed(&self, harness: Harness) -> bool {
        harness
            .driven_program()
            .is_ok_and(|program| self.host.finds(program))
    }

    /// Every client as this host has it, with its sign-in as last read.
    fn clients(&self, sign_ins: &SignIns) -> Vec<Client> {
        Harness::ALL
            .iter()
            .map(|harness| Client {
                harness: *harness,
                status: match harness.driven_program() {
                    Ok(program) if self.host.finds(program) => ClientStatus::Ready,
                    Ok(_) => ClientStatus::NotInstalled,
                    Err(_) => ClientStatus::NotDrivable,
                },
                approvals: ApprovalPolicy::offered(*harness).to_vec(),
                midway: Midway::offered(*harness).to_vec(),
                sign_in: sign_ins.state(*harness),
                sign_in_methods: harness.sign_in_methods().to_vec(),
            })
            .collect()
    }

    /// Check the sign-in of each installed client that offers a status
    /// command and has not been checked.
    fn check_unchecked(&self, sign_ins: &mut SignIns) {
        for harness in Harness::ALL {
            let unchecked = matches!(
                sign_ins.state(*harness),
                crate::sign_in::SignInState::NotChecked
            );
            if unchecked && harness.sign_in_status().is_some() && self.installed(*harness) {
                sign_ins.check(*harness, None, || self.status_check(*harness));
            }
        }
    }

    /// The first root, where a client's own short commands run.
    fn root_workdir(&self) -> Result<Workdir, SignInError> {
        let root = self
            .bounds
            .roots()
            .iter()
            .next()
            .ok_or(SignInError::NoRoot)?;
        Workdir::confirm(&self.bounds, root.to_path_buf()).map_err(|_| SignInError::NoRoot)
    }

    /// Start `harness`'s status command, read when it ends.
    fn status_check(
        &self,
        harness: Harness,
    ) -> Result<BoxFuture<'static, crate::sign_in::SignInState>, SignInError> {
        let plan = harness
            .sign_in_status()
            .ok_or(ren_harness::sign_in::SignInRefused::NotOffered(harness))?;
        if !self.installed(harness) {
            return Err(SignInError::NotInstalled(harness));
        }
        let process = self
            .host
            .spawn_with_stderr(&plan, &self.root_workdir()?, &self.host.tools())
            .map_err(|error| {
                tracing::debug!(%harness, %error, "cannot start a client's status command");
                SignInError::Spawn
            })?;
        Ok(StatusCheck::run(harness, process).boxed())
    }

    /// Carry out a sign-in request.
    fn sign_in(&self, request: SignInRequest, sign_ins: &mut SignIns) {
        match request {
            SignInRequest::Check { harness, reply } => {
                sign_ins.check(harness, Some(reply), || self.status_check(harness));
            }
            SignInRequest::Start {
                harness,
                method,
                reply,
            } => {
                let started = harness
                    .sign_in(method)
                    .map_err(SignInError::from)
                    .and_then(|plan| {
                        if !self.installed(harness) {
                            return Err(SignInError::NotInstalled(harness));
                        }
                        self.host
                            .spawn_with_stderr(&plan, &self.root_workdir()?, &self.host.tools())
                            .map_err(|error| {
                                tracing::debug!(%harness, %error, "cannot start a client's login");
                                SignInError::Spawn
                            })
                    })
                    .map(|process| sign_ins.login(harness, process, self.line_limit));
                let _ = reply.send(started);
            }
            SignInRequest::Code {
                harness,
                code,
                reply,
            } => sign_ins.code(harness, code, reply),
            SignInRequest::Cancel { harness, reply } => {
                let _ = reply.send(sign_ins.cancel(harness));
            }
            SignInRequest::Wait { harness, reply } => {
                let _ = reply.send(sign_ins.wait(harness));
            }
        }
    }

    /// List the models of every client installed here, with no one waiting.
    fn warm(&self, listings: &mut Listings) {
        for harness in Harness::ALL {
            if !self.installed(*harness) {
                continue;
            }
            match self.probe(*harness) {
                Ok(listing) => listings.warm(*harness, listing),
                Err(error) => tracing::debug!(%harness, %error, "cannot list a client's models"),
            }
        }
    }

    /// Answer from the listings, or start the client briefly in the first
    /// root to list its models.
    fn models(&self, harness: Harness, reply: ModelsReply, listings: &mut Listings) {
        let Some(reply) = listings.ask(harness, reply) else {
            return;
        };
        match self.probe(harness) {
            Ok(listing) => listings.start(harness, reply, listing),
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn probe(
        &self,
        harness: Harness,
    ) -> Result<BoxFuture<'static, Result<Vec<ren_harness::models::Model>, ModelsError>>, ModelsError>
    {
        let plan = harness
            .list_models(&self.gateway)
            .map_err(ModelsError::NotDrivable)?;
        let root = self
            .bounds
            .roots()
            .iter()
            .next()
            .ok_or(ModelsError::NoRoot)?;
        let workdir =
            Workdir::confirm(&self.bounds, root.to_path_buf()).map_err(|_| ModelsError::NoRoot)?;
        let (listing, first) =
            ModelListing::open(harness, &self.version, &workdir.as_path().to_string_lossy())
                .map_err(ModelsError::NotDrivable)?;
        let process = self
            .host
            .spawn(&plan, &workdir, &self.host.tools())
            .map_err(|error| {
                tracing::debug!(%harness, %error, "cannot start a client to list its models");
                ModelsError::Spawn
            })?;
        Ok(ModelProbe::run(listing, first, process, self.line_limit).boxed())
    }

    async fn command(
        &self,
        run: RunId,
        live: &mut Live,
        command: RunCommand,
    ) -> Result<(), CommandError> {
        // Settings the run moves to, once the client takes them.
        let configuring = if let RunCommand::Configure(settings) = &command {
            Some(settings.clone())
        } else {
            None
        };
        let transition = live
            .state
            .apply(RunEvent::Operator(command))
            .map_err(CommandError::Refused)?;
        // Encode before committing, so a command the protocol refuses leaves
        // the run as it was.
        let mut lines = Vec::new();
        let mut kill = false;
        for action in transition.actions {
            match action {
                RunAction::Send(command) => {
                    let codec = live.codec.as_mut().ok_or(CommandError::NoSuchRun(run))?;
                    lines.extend(codec.command(command).map_err(CommandError::Protocol)?);
                }
                RunAction::Kill => kill = true,
            }
        }
        for line in &lines {
            Self::write(run, live, line).await;
        }
        if kill && let Some(switch) = live.kill.take() {
            switch.fire();
        }
        if let Some(settings) = configuring {
            if let Some(tap) = live.tap.as_ref() {
                tap.configured(settings.clone());
            }
            live.settings = settings;
            // The state may not change, but the run did: say so.
            self.publish(RunUpdate::State {
                run,
                state: transition.state.clone(),
            });
        }
        self.settle(run, live, transition.state);
        Ok(())
    }

    /// Read one frame of a run's output. Whether a turn failed in it.
    async fn frame(
        &self,
        run: RunId,
        live: &mut Live,
        held: Option<&mut Held>,
        frame: Framed,
    ) -> bool {
        let Some(codec) = live.codec.as_mut() else {
            return false;
        };
        let received = codec.receive(frame);
        for reply in &received.replies {
            Self::write(run, live, reply).await;
        }
        let mut held = held;
        let mut failed = false;
        for event in received.events {
            if let Some(held) = held.as_deref_mut() {
                match &event {
                    // Turns change files.
                    DriveEvent::TurnEnded { .. } => held.files = None,
                    DriveEvent::Ready { thread } => held.thread = Some(*thread),
                    DriveEvent::TurnStarted { .. }
                    | DriveEvent::Delta { .. }
                    | DriveEvent::Item { .. }
                    | DriveEvent::Approval { .. }
                    | DriveEvent::ApprovalResolved { .. }
                    | DriveEvent::Error { .. }
                    | DriveEvent::Refused { .. }
                    | DriveEvent::Unsupported { .. }
                    | DriveEvent::Unknown { .. }
                    | DriveEvent::Unreadable { .. } => {}
                }
            }
            if let (DriveEvent::Ready { thread }, Some(tap)) = (&event, live.tap.as_ref()) {
                tap.opened(*thread);
            }
            if let Some(tap) = live.tap.as_ref() {
                for (turn, entry) in live.entries.take(&event) {
                    tap.entry(turn, entry);
                }
            }
            failed |= matches!(
                event,
                DriveEvent::TurnEnded {
                    end: TurnEnd::Failed { .. },
                    ..
                }
            );
            self.publish(RunUpdate::Event {
                run,
                event: event.clone(),
            });
            self.apply(run, live, RunEvent::Agent(event)).await;
            // Open now: the first prompt goes out.
            if let RunState::Idle { .. } = live.state
                && let Some(prompt) = live.first.take()
                && let Err(error) = self.command(run, live, RunCommand::Prompt(prompt)).await
            {
                tracing::debug!(%run, %error, "the first prompt was refused");
            }
        }
        failed
    }

    /// Apply an event that is not the operator's. Refusals here mean the run
    /// already ended; they change nothing.
    async fn apply(&self, run: RunId, live: &mut Live, event: RunEvent) {
        let transition = match live.state.apply(event) {
            Ok(transition) => transition,
            Err(refused) => {
                tracing::debug!(%run, %refused, "event after the run's end");
                return;
            }
        };
        for action in transition.actions {
            match action {
                RunAction::Send(command) => {
                    let lines = match live.codec.as_mut().map(|codec| codec.command(command)) {
                        Some(Ok(lines)) => lines,
                        Some(Err(refused)) => {
                            // A queued prompt the client would not take.
                            tracing::debug!(%run, %refused, "the client refused a command");
                            Vec::new()
                        }
                        None => Vec::new(),
                    };
                    for line in &lines {
                        Self::write(run, live, line).await;
                    }
                }
                RunAction::Kill => {
                    if let Some(switch) = live.kill.take() {
                        switch.fire();
                    }
                }
            }
        }
        self.settle(run, live, transition.state);
    }

    fn settle(&self, run: RunId, live: &mut Live, state: RunState) {
        // Idle now, not before: its idle timer starts.
        if let (RunState::Idle { .. }, false) =
            (&state, matches!(live.state, RunState::Idle { .. }))
        {
            live.rested += 1;
            live.arm = true;
        }
        if let RunState::Ended { end, exit } = &state
            && let Some(tap) = live.tap.take()
        {
            for (turn, entry) in live.entries.finish() {
                tap.entry(turn, entry);
            }
            tap.end(*end, *exit);
        }
        if state != live.state {
            live.state = state.clone();
            self.publish(RunUpdate::State { run, state });
        }
    }

    fn publish(&self, update: RunUpdate) {
        // No subscriber is not an error.
        let _ = self.updates.send(update);
    }

    async fn write(run: RunId, live: &mut Live, line: &Line) {
        let Some(stdin) = live.stdin.as_mut() else {
            return;
        };
        let written = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await
        }
        .await;
        if let Err(error) = written {
            // The process is ending; its exit will say how.
            tracing::debug!(%run, %error, "agent stdin closed");
            live.stdin = None;
        }
    }

    fn output(
        run: RunId,
        stdout: Box<dyn AsyncRead + Send + Unpin>,
        limit: LineLimit,
    ) -> BoxStream<'static, (RunId, Vec<Framed>)> {
        const CHUNK: usize = 64 * 1024;
        stream::unfold(
            Some((stdout, Lines::new(limit), vec![0; CHUNK])),
            move |reading| async move {
                let (mut stdout, mut lines, mut buffer) = reading?;
                match stdout.read(&mut buffer).await {
                    Ok(0) | Err(_) => Some(((run, lines.finish().into_iter().collect()), None)),
                    Ok(read) => {
                        let frames = lines.push(buffer.get(..read).unwrap_or_default());
                        Some(((run, frames), Some((stdout, lines, buffer))))
                    }
                }
            },
        )
        .boxed()
    }
}
