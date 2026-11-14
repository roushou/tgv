//! Running a client's own sign-in commands, off the task's loop: its status
//! command, read once it exits ([`StatusCheck`]), and its login, talked to
//! until it ends ([`LoginTalk`]). The task keeps what each client's sign-in
//! is and who waits on it ([`SignIns`]).

use crate::{
    host::AgentProcess,
    sign_in::{Login, LoginEnd, LoginInput, LoginOutcome, SignInState},
};
use futures_util::{FutureExt, future::BoxFuture, stream::FuturesUnordered};
use ren_harness::{
    Harness,
    lines::{LineLimit, Lines},
    sign_in::{LoginReader, SignInCode, SignInFlow, SignInRefused},
};
use std::{collections::HashMap, time::Duration};
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
    #[error("the sign-in is not waiting for a code")]
    NotAwaitingCode,
    #[error("the supervisor has stopped")]
    Gone,
}

pub(crate) type CheckReply = oneshot::Sender<Result<SignInState, SignInError>>;
pub(crate) type OutcomeReply = oneshot::Sender<Result<LoginOutcome, SignInError>>;

/// A wait on a login: going, with its state now and its outcome to come, or
/// already ended.
pub(crate) enum WaitOn {
    Going {
        login: Login,
        outcome: oneshot::Receiver<Result<LoginOutcome, SignInError>>,
    },
    Done(LoginOutcome),
}

/// A client's status command, run to its end and read.
pub(crate) struct StatusCheck;

impl StatusCheck {
    /// How long a status command may take: well under the UI proxy's 30
    /// seconds, with a login's end before it.
    const TIMEOUT: Duration = Duration::from_secs(10);
    /// The most output read; a status is a short JSON object.
    const MAX: usize = 256 * 1024;

    pub(crate) async fn run(harness: Harness, process: AgentProcess) -> SignInState {
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
        match read {
            Err(_) => SignInState::Unknown {
                reason: format!("{harness} did not say whether it is signed in in time"),
            },
            Ok(output) => match harness.read_sign_in(&output) {
                Ok(standing) => standing.into(),
                Err(unread) => SignInState::Unknown {
                    reason: unread.to_string(),
                },
            },
        }
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

/// One login, talked to: its output read, the operator's code written,
/// cancelled when asked or when its handle is dropped.
pub(crate) struct LoginTalk {
    codes: mpsc::Receiver<SignInCode>,
    cancel: oneshot::Receiver<()>,
    state: watch::Sender<Login>,
}

/// The task's side of a running login.
pub(crate) struct LoginHandle {
    attempt: u64,
    pub(crate) state: watch::Receiver<Login>,
    codes: mpsc::Sender<SignInCode>,
    /// Dropping it cancels the login.
    _cancel: oneshot::Sender<()>,
    /// Who waits for its outcome.
    waiting: Vec<OutcomeReply>,
}

impl LoginTalk {
    /// How long a login may wait for its code or approval: a device
    /// code's lifetime.
    const AWAITING: Duration = Duration::from_secs(15 * 60);
    /// How long a client may take to finish once it has its code.
    const FINISHING: Duration = Duration::from_secs(15);
    const CHUNK: usize = 8 * 1024;

    fn new(attempt: u64, flow: SignInFlow) -> (Self, LoginHandle) {
        let (codes, code_receiver) = mpsc::channel(1);
        let (cancel, cancel_receiver) = oneshot::channel();
        let (state, state_receiver) = watch::channel(Login::start(flow));
        (
            Self {
                codes: code_receiver,
                cancel: cancel_receiver,
                state,
            },
            LoginHandle {
                attempt,
                state: state_receiver,
                codes,
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

    fn end(&self) -> LoginEnd {
        match &*self.state.borrow() {
            Login::Ended(end) => end.clone(),
            Login::Starting { .. }
            | Login::AwaitingCode { .. }
            | Login::AwaitingApproval { .. }
            | Login::Finishing { .. } => LoginEnd::Broken,
        }
    }

    async fn run(mut self, harness: Harness, process: AgentProcess, limit: LineLimit) -> LoginEnd {
        let AgentProcess {
            mut stdin,
            mut stdout,
            exit,
            kill,
        } = process;
        let mut reader = LoginReader::new(harness);
        let mut lines = Lines::new(limit);
        let mut buffer = vec![0; Self::CHUNK];
        let deadline = tokio::time::sleep(Self::AWAITING);
        tokio::pin!(deadline);
        let mut exit = exit.fuse();
        let mut codes_open = true;
        // What stopped it, when Ren stops it; none when its output ended.
        let stopped = loop {
            tokio::select! {
                read = stdout.read(&mut buffer) => match read {
                    Ok(read @ 1..) => {
                        for frame in lines.push(buffer.get(..read).unwrap_or_default()) {
                            if let Some(event) = reader.receive(frame) {
                                self.step(LoginInput::Printed(event));
                            }
                        }
                    }
                    Ok(_) | Err(_) => {
                        let last = std::mem::replace(&mut lines, Lines::new(limit)).finish();
                        if let Some(event) = last.and_then(|frame| reader.receive(frame)) {
                            self.step(LoginInput::Printed(event));
                        }
                        break None;
                    }
                },
                code = self.codes.recv(), if codes_open => match code {
                    Some(code) if self.state.borrow().awaits_code() => {
                        let line = code.line();
                        let written = async {
                            stdin.write_all(line.as_bytes()).await?;
                            stdin.flush().await
                        };
                        if written.await.is_err() {
                            break Some(LoginInput::Broken);
                        }
                        self.step(LoginInput::CodeSent);
                        deadline.set(tokio::time::sleep(Self::FINISHING));
                    }
                    Some(_) => {}
                    None => codes_open = false,
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
        limit: LineLimit,
    ) -> watch::Receiver<Login> {
        self.attempts += 1;
        let attempt = self.attempts;
        let (talk, handle) = LoginTalk::new(attempt, harness.sign_in_flow());
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
            talk.run(harness, process, limit)
                .map(move |end| (harness, attempt, end))
                .boxed(),
        );
        state
    }

    /// Hand `code` to `harness`'s login, answering `reply` with its outcome.
    pub(crate) fn code(&mut self, harness: Harness, code: SignInCode, reply: OutcomeReply) {
        let Some(handle) = self.logins.get_mut(&harness) else {
            let _ = reply.send(Err(SignInError::NoLogin(harness)));
            return;
        };
        if !handle.state.borrow().awaits_code() {
            let _ = reply.send(Err(SignInError::NotAwaitingCode));
            return;
        }
        match handle.codes.try_send(code) {
            Ok(()) => handle.waiting.push(reply),
            Err(_) => {
                let _ = reply.send(Err(SignInError::NotAwaitingCode));
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
