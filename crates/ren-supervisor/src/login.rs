//! Running a client's own sign-in commands, off the task's loop: its status
//! commands, read once they exit ([`StatusCheck`]), and its login, talked to
//! until it ends ([`LoginTalk`]). The task keeps what each client's sign-in
//! is and who waits on it ([`SignIns`]).

use crate::{
    host::AgentProcess,
    sign_in::{Awaiting, Login, LoginEnd, LoginInput, LoginOutcome, SignInState},
};
use futures_util::{FutureExt, future::BoxFuture, stream::FuturesUnordered};
use ren_harness::{
    Harness,
    lines::{Line, LineLimit, Lines},
    sign_in::{
        Console, LocalRedirect, LoginEvent, LoginReader, RedirectRefused, SignInAnswer, SignInFlow,
        SignInRefused,
    },
};
use std::{collections::HashMap, io, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot, watch},
};

/// Why a sign-in request was not carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignInError {
    #[error(transparent)]
    Refused(#[from] SignInRefused),
    #[error("{0} is not installed here")]
    NotInstalled(Harness),
    #[error("no run root to run the client in")]
    NoRoot,
    #[error("cannot start the client; check it is installed on PATH")]
    Spawn,
    #[error("no sign-in is running for {0}")]
    NoLogin(Harness),
    #[error("the sign-in is not waiting for an answer")]
    NotAwaitingAnswer,
    #[error(transparent)]
    Redirect(#[from] RedirectRefused),
    #[error("the supervisor has stopped")]
    Gone,
}

pub(crate) type CheckReply = oneshot::Sender<Result<SignInState, SignInError>>;
pub(crate) type OutcomeReply = oneshot::Sender<Result<LoginOutcome, SignInError>>;
/// Opening a loopback address on the host, for a login waiting on it.
pub(crate) type Opening = BoxFuture<'static, io::Result<()>>;

/// A wait on a login: going, with its state now and its outcome to come, or
/// already ended.
pub(crate) enum WaitOn {
    Going {
        login: Login,
        outcome: oneshot::Receiver<Result<LoginOutcome, SignInError>>,
    },
    Done(LoginOutcome),
}

/// A client's status commands, each run to its end and read.
pub(crate) struct StatusCheck;

impl StatusCheck {
    /// How long a status command may take: well under the UI proxy's 30
    /// seconds, with a login's end before it.
    const TIMEOUT: Duration = Duration::from_secs(10);
    /// The most output read; a status is short.
    const MAX: usize = 256 * 1024;

    /// Read `processes`, the client's status commands, all at once.
    pub(crate) async fn run(harness: Harness, processes: Vec<AgentProcess>) -> SignInState {
        let read = futures_util::future::join_all(processes.into_iter().map(Self::one)).await;
        let outputs: Option<Vec<Vec<u8>>> = read.into_iter().collect();
        match outputs {
            None => SignInState::Unknown {
                reason: format!("{harness} did not say whether it is signed in in time"),
            },
            Some(outputs) => match harness.read_sign_in(&outputs) {
                Ok(standing) => standing.into(),
                Err(unread) => SignInState::Unknown {
                    reason: unread.to_string(),
                },
            },
        }
    }

    /// One command's output; none when it took too long.
    async fn one(process: AgentProcess) -> Option<Vec<u8>> {
        let AgentProcess {
            stdin,
            mut stdout,
            exit,
            kill,
        } = process;
        drop(stdin);
        let read = tokio::time::timeout(Self::TIMEOUT, Self::read(&mut stdout)).await;
        kill.fire();
        let _ = exit.await;
        read.ok()
    }

    async fn read(stdout: &mut (dyn AsyncRead + Send + Unpin)) -> Vec<u8> {
        let mut output = Vec::new();
        let mut buffer = vec![0; 8 * 1024];
        while let Ok(read @ 1..) = stdout.read(&mut buffer).await {
            output.extend_from_slice(buffer.get(..read).unwrap_or_default());
            if output.len() > Self::MAX {
                break;
            }
        }
        output
    }
}

/// An operator's answer on its way to a login: typed into it, or an address
/// opened on the host for it. `secret` is left out of anything it prints.
pub(crate) enum Answer {
    Typed { line: Line, secret: String },
    Opened { opening: Opening, secret: String },
}

/// One login, talked to: its output read, the operator's answer handed to
/// it, cancelled when asked or when its handle is dropped. Whatever it
/// prints is cleaned of every answer handed to it before it is shown.
pub(crate) struct LoginTalk {
    answers: mpsc::Receiver<Answer>,
    cancel: oneshot::Receiver<()>,
    state: watch::Sender<Login>,
    secrets: Vec<String>,
}

/// The task's side of a running login.
pub(crate) struct LoginHandle {
    attempt: u64,
    console: Console,
    pub(crate) state: watch::Receiver<Login>,
    answers: mpsc::Sender<Answer>,
    /// Dropping it cancels the login.
    _cancel: oneshot::Sender<()>,
    /// Who waits for its outcome.
    waiting: Vec<OutcomeReply>,
}

impl LoginTalk {
    /// How long a login may wait for its answer or approval: a device
    /// code's lifetime.
    const AWAITING: Duration = Duration::from_secs(15 * 60);
    /// How long a client may take to finish once it has its answer.
    const FINISHING: Duration = Duration::from_secs(15);
    /// How long opening a loopback address may take.
    const OPENING: Duration = Duration::from_secs(10);
    const CHUNK: usize = 8 * 1024;
    const HIDDEN: &'static str = "•••";

    fn new(attempt: u64, console: Console, flow: SignInFlow) -> (Self, LoginHandle) {
        let (answers, answer_receiver) = mpsc::channel(1);
        let (cancel, cancel_receiver) = oneshot::channel();
        let (state, state_receiver) = watch::channel(Login::start(flow));
        (
            Self {
                answers: answer_receiver,
                cancel: cancel_receiver,
                state,
                secrets: Vec::new(),
            },
            LoginHandle {
                attempt,
                console,
                state: state_receiver,
                answers,
                _cancel: cancel,
                waiting: Vec::new(),
            },
        )
    }

    fn step(&self, input: LoginInput) {
        let current = self.state.borrow().clone();
        if let Ok(next) = current.apply(input) {
            self.state.send_replace(next);
        }
    }

    /// What it printed, any answer it echoed left out.
    fn printed(&self, event: LoginEvent) {
        let event = match event {
            LoginEvent::Said(mut text) => {
                for secret in &self.secrets {
                    text = text.replace(secret.as_str(), Self::HIDDEN);
                }
                LoginEvent::Said(text)
            }
            event @ (LoginEvent::Link(_) | LoginEvent::DeviceCode(_)) => event,
        };
        self.step(LoginInput::Printed(event));
    }

    fn end(&self) -> LoginEnd {
        match &*self.state.borrow() {
            Login::Ended(end) => end.clone(),
            Login::Starting { .. }
            | Login::AwaitingCode { .. }
            | Login::AwaitingApproval { .. }
            | Login::AwaitingKey { .. }
            | Login::AwaitingRedirect { .. }
            | Login::Finishing { .. } => LoginEnd::Broken { said: None },
        }
    }

    async fn run(
        mut self,
        harness: Harness,
        flow: SignInFlow,
        process: AgentProcess,
        limit: LineLimit,
    ) -> LoginEnd {
        let AgentProcess {
            mut stdin,
            mut stdout,
            exit,
            kill,
        } = process;
        let mut reader = LoginReader::new(harness, flow);
        let mut lines = Lines::new(limit);
        let mut buffer = vec![0; Self::CHUNK];
        let deadline = tokio::time::sleep(Self::AWAITING);
        tokio::pin!(deadline);
        let mut exit = exit.fuse();
        let mut answers_open = true;
        // What stopped it, when Ren stops it; none when its output ended.
        let stopped = loop {
            tokio::select! {
                read = stdout.read(&mut buffer) => match read {
                    Ok(read @ 1..) => {
                        for frame in lines.push(buffer.get(..read).unwrap_or_default()) {
                            for event in reader.receive(frame) {
                                self.printed(event);
                            }
                        }
                    }
                    // A terminal reads as an error once its process exits.
                    Ok(_) | Err(_) => {
                        let last = std::mem::replace(&mut lines, Lines::new(limit)).finish();
                        for event in last.into_iter().flat_map(|frame| reader.receive(frame)) {
                            self.printed(event);
                        }
                        break None;
                    }
                },
                answer = self.answers.recv(), if answers_open => match answer {
                    Some(_) if self.state.borrow().awaiting().is_none() => {}
                    Some(Answer::Typed { line, secret }) => {
                        self.secrets.push(secret);
                        let written = async {
                            stdin.write_all(line.as_bytes()).await?;
                            stdin.flush().await
                        };
                        if written.await.is_err() {
                            break Some(LoginInput::Broken(None));
                        }
                        self.step(LoginInput::Answered);
                        deadline.set(tokio::time::sleep(Self::FINISHING));
                    }
                    Some(Answer::Opened { opening, secret }) => {
                        self.secrets.push(secret);
                        match tokio::time::timeout(Self::OPENING, opening).await {
                            Ok(Ok(())) => {
                                self.step(LoginInput::Answered);
                                deadline.set(tokio::time::sleep(Self::FINISHING));
                            }
                            Ok(Err(error)) => {
                                break Some(LoginInput::Broken(Some(format!(
                                    "the address could not be opened on the host: {error}"
                                ))));
                            }
                            Err(_) => {
                                break Some(LoginInput::Broken(Some(
                                    "the address did not answer on the host in time".into(),
                                )));
                            }
                        }
                    }
                    None => answers_open = false,
                },
                _ = &mut self.cancel => break Some(LoginInput::Cancelled),
                () = &mut deadline => break Some(LoginInput::TimedOut),
            }
        };
        drop(stdin);
        let input = match stopped {
            Some(input) => {
                kill.fire();
                let _ = exit.await;
                input
            }
            // Its output ended: it is exiting.
            None => tokio::select! {
                exited = &mut exit => LoginInput::Exited(exited),
                () = &mut deadline => {
                    kill.fire();
                    let _ = exit.await;
                    LoginInput::TimedOut
                }
            },
        };
        self.step(input);
        self.end()
    }
}

type Checked = (Harness, SignInState);
type Talked = (Harness, u64, LoginEnd);

/// Every client's sign-in as the task keeps it: what was last read, the
/// checks and logins running, and who waits on them.
#[derive(Default)]
pub(crate) struct SignIns {
    states: HashMap<Harness, SignInState>,
    pub(crate) checks: FuturesUnordered<BoxFuture<'static, Checked>>,
    checking: HashMap<Harness, Vec<CheckReply>>,
    /// Logins that ended, waiting on the check that follows them, with who
    /// waits on their outcome.
    after: HashMap<Harness, (LoginEnd, Vec<OutcomeReply>)>,
    /// Each client's last login's outcome, until another starts: for whoever
    /// asks after it ended.
    last: HashMap<Harness, LoginOutcome>,
    logins: HashMap<Harness, LoginHandle>,
    pub(crate) talks: FuturesUnordered<BoxFuture<'static, Talked>>,
    attempts: u64,
}

impl SignIns {
    /// What is known of `harness`'s sign-in.
    pub(crate) fn state(&self, harness: Harness) -> SignInState {
        match self.states.get(&harness) {
            Some(state) => state.clone(),
            None if self.checking.contains_key(&harness) => SignInState::Checking,
            None => SignInState::NotChecked,
        }
    }

    /// Whether a check is running for any client.
    pub(crate) fn checking(&self) -> bool {
        !self.checking.is_empty()
    }

    /// Run `check` for `harness`, answering `reply` when it ends; join one
    /// already running instead.
    pub(crate) fn check(
        &mut self,
        harness: Harness,
        reply: Option<CheckReply>,
        start: impl FnOnce() -> Result<BoxFuture<'static, SignInState>, SignInError>,
    ) {
        if let Some(waiting) = self.checking.get_mut(&harness) {
            waiting.extend(reply);
            return;
        }
        match start() {
            Ok(check) => {
                self.checking.insert(harness, reply.into_iter().collect());
                self.checks
                    .push(check.map(move |state| (harness, state)).boxed());
            }
            Err(error) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(error));
                }
            }
        }
    }

    /// A check ended: keep what it read, and answer who waited on it and on
    /// the login before it.
    pub(crate) fn checked(&mut self, harness: Harness, state: SignInState) {
        self.states.insert(harness, state.clone());
        for reply in self.checking.remove(&harness).unwrap_or_default() {
            let _ = reply.send(Ok(state.clone()));
        }
        if let Some((end, waiting)) = self.after.remove(&harness) {
            let outcome = LoginOutcome { end, state };
            for reply in waiting {
                let _ = reply.send(Ok(outcome.clone()));
            }
            self.last.insert(harness, outcome);
        }
    }

    /// Start a login for `harness` through `process`, replacing (and so
    /// cancelling) any running one, whose waiters learn it was cancelled.
    pub(crate) fn login(
        &mut self,
        harness: Harness,
        process: AgentProcess,
        console: Console,
        flow: SignInFlow,
        limit: LineLimit,
    ) -> watch::Receiver<Login> {
        self.attempts += 1;
        let attempt = self.attempts;
        let (talk, handle) = LoginTalk::new(attempt, console, flow);
        let state = handle.state.clone();
        self.last.remove(&harness);
        if let Some(replaced) = self.logins.insert(harness, handle) {
            let current = self.state(harness);
            for reply in replaced.waiting {
                let _ = reply.send(Ok(LoginOutcome {
                    end: LoginEnd::Cancelled,
                    state: current.clone(),
                }));
            }
        }
        self.talks.push(
            talk.run(harness, flow, process, limit)
                .map(move |end| (harness, attempt, end))
                .boxed(),
        );
        state
    }

    /// Hand `answer` to `harness`'s login, answering `reply` with its
    /// outcome: typed into it, or, for an address, opened on the host by
    /// `open` once it is the one the login waits on.
    pub(crate) fn answer(
        &mut self,
        harness: Harness,
        answer: SignInAnswer,
        open: impl FnOnce(&LocalRedirect) -> Opening,
        reply: OutcomeReply,
    ) {
        let Some(handle) = self.logins.get_mut(&harness) else {
            let _ = reply.send(Err(SignInError::NoLogin(harness)));
            return;
        };
        let awaiting = handle.state.borrow().awaiting();
        let secret = answer.text().to_owned();
        let sent = match awaiting {
            None => Err(SignInError::NotAwaitingAnswer),
            Some(Awaiting::Code | Awaiting::Key) => Ok(Answer::Typed {
                line: answer.line(handle.console),
                secret,
            }),
            Some(Awaiting::Redirect(waited)) => waited
                .accept(&answer)
                .map(|address| Answer::Opened {
                    opening: open(&address),
                    secret,
                })
                .map_err(SignInError::from),
        };
        let result = sent.and_then(|sent| {
            handle
                .answers
                .try_send(sent)
                .map_err(|_| SignInError::NotAwaitingAnswer)
        });
        match result {
            Ok(()) => handle.waiting.push(reply),
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Cancel `harness`'s login.
    pub(crate) fn cancel(&mut self, harness: Harness) -> Result<(), SignInError> {
        let handle = self
            .logins
            .remove(&harness)
            .ok_or(SignInError::NoLogin(harness))?;
        let outcome = LoginOutcome {
            end: LoginEnd::Cancelled,
            state: self.state(harness),
        };
        for reply in handle.waiting {
            let _ = reply.send(Ok(outcome.clone()));
        }
        self.last.insert(harness, outcome);
        Ok(())
    }

    /// Wait on `harness`'s login: running, ended and being checked, or
    /// ended; refused when there has been none.
    pub(crate) fn wait(&mut self, harness: Harness) -> Result<WaitOn, SignInError> {
        let (reply, outcome) = oneshot::channel();
        if let Some(handle) = self.logins.get_mut(&harness) {
            let login = handle.state.borrow().clone();
            handle.waiting.push(reply);
            return Ok(WaitOn::Going { login, outcome });
        }
        if let Some((end, waiting)) = self.after.get_mut(&harness) {
            let login = Login::Ended(end.clone());
            waiting.push(reply);
            return Ok(WaitOn::Going { login, outcome });
        }
        self.last
            .get(&harness)
            .cloned()
            .map(WaitOn::Done)
            .ok_or(SignInError::NoLogin(harness))
    }

    /// A login ended. Whether it was the current one: then a check follows,
    /// and who waited on the login is answered after it.
    pub(crate) fn talked(&mut self, harness: Harness, attempt: u64, end: LoginEnd) -> bool {
        let current = self
            .logins
            .get(&harness)
            .is_some_and(|handle| handle.attempt == attempt);
        if !current {
            return false;
        }
        let waiting = self
            .logins
            .remove(&harness)
            .map(|handle| handle.waiting)
            .unwrap_or_default();
        self.after.insert(harness, (end, waiting));
        true
    }

    /// Answer who waits on a login whose check could not start.
    pub(crate) fn unchecked(&mut self, harness: Harness, error: &SignInError) {
        for reply in self
            .after
            .remove(&harness)
            .map(|(_, waiting)| waiting)
            .unwrap_or_default()
        {
            let _ = reply.send(Err(error.clone()));
        }
    }
}
