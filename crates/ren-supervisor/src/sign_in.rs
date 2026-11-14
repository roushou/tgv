//! A client's sign-in on this host, as Ren reads it through the client's
//! own commands: what its status command last said ([`SignInState`]), and
//! the pure lifecycle of one run of its login ([`Login`]), which prints a
//! link, reads back the code the page shows, and exits. Whether a login
//! signed the client in is never taken from the login: a status check
//! after it says so.

use crate::run::ProcessExit;
use ren_harness::sign_in::{
    DeviceCode, LocalRedirect, LoginEvent, SignInFlow, SignInLink, Standing, StatedMethod,
};
use serde::Serialize;

/// What Ren knows of a client's sign-in here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignInState {
    /// Ren cannot check this client's sign-in: it offers no command Ren
    /// reads, or it is not installed.
    NotChecked,
    /// Its status command is running, and nothing was read before.
    Checking,
    SignedIn {
        method: StatedMethod,
    },
    SignedOut,
    /// Its status command could not be run or read: whether it is signed in
    /// is unknown.
    Unknown {
        reason: String,
    },
}

impl From<Standing> for SignInState {
    fn from(standing: Standing) -> Self {
        match standing {
            Standing::SignedIn { method } => Self::SignedIn { method },
            Standing::SignedOut => Self::SignedOut,
        }
    }
}

/// One run of a client's login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Login {
    /// Started; its link has not been printed yet.
    Starting {
        /// How it goes once it has: for the machine, not shown.
        #[serde(skip)]
        flow: SignInFlow,
        said: Option<String>,
    },
    /// Waiting for the code the page at `link` shows.
    AwaitingCode {
        link: SignInLink,
        said: Option<String>,
    },
    /// Waiting for the operator to enter `code` on the page at `link` and
    /// approve (a device login); `code` is none until it is printed.
    AwaitingApproval {
        link: SignInLink,
        code: Option<DeviceCode>,
        said: Option<String>,
    },
    /// Waiting for an API key.
    AwaitingKey {
        said: Option<String>,
    },
    /// Waiting for the address the browser could not open after approving
    /// at `link`, to open it on the host.
    AwaitingRedirect {
        link: SignInLink,
        said: Option<String>,
    },
    /// The answer was handed to the client; waiting for it to finish.
    Finishing {
        link: Option<SignInLink>,
        said: Option<String>,
    },
    Ended(LoginEnd),
}

/// What a login waits for from the operator now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Awaiting {
    /// The code the page showed, typed into the login.
    Code,
    /// An API key, typed into the login.
    Key,
    /// The address the browser could not open, opened on the host when it
    /// is `redirect`.
    Redirect(LocalRedirect),
}

/// How a login ended. Whether the client is signed in after it is what the
/// status check that follows says, not this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "end", rename_all = "snake_case")]
pub enum LoginEnd {
    /// The login exited by itself, having last said `said`.
    Exited {
        exit: ProcessExit,
        said: Option<String>,
    },
    /// The operator cancelled it, or a newer login replaced it.
    Cancelled,
    /// No answer or approval came, or the client did not finish, in time.
    TimedOut,
    /// Its output ended unreadably, its input closed, or the address could
    /// not be opened: it was stopped, having last said `said`.
    Broken { said: Option<String> },
}

/// What happens to a login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginInput {
    Printed(LoginEvent),
    /// The operator's answer was handed to it.
    Answered,
    Exited(ProcessExit),
    Cancelled,
    TimedOut,
    Broken(Option<String>),
}

/// A login input that its state does not take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LoginRefused {
    #[error("the sign-in is not waiting for an answer")]
    NotAwaitingAnswer,
    #[error("the sign-in has ended")]
    Ended,
}

impl Login {
    /// A login just started, going as `flow` says: a key login asks for its
    /// key at once.
    pub fn start(flow: SignInFlow) -> Self {
        match flow {
            SignInFlow::EnterKey => Self::AwaitingKey { said: None },
            SignInFlow::PasteCode | SignInFlow::DeviceCode | SignInFlow::PasteRedirect => {
                Self::Starting { flow, said: None }
            }
        }
    }

    /// What the login waits for from the operator now, if anything.
    pub fn awaiting(&self) -> Option<Awaiting> {
        match self {
            Self::AwaitingCode { .. } => Some(Awaiting::Code),
            Self::AwaitingKey { .. } => Some(Awaiting::Key),
            Self::AwaitingRedirect { link, .. } => link.redirect().map(Awaiting::Redirect),
            Self::Starting { .. }
            | Self::AwaitingApproval { .. }
            | Self::Finishing { .. }
            | Self::Ended(_) => None,
        }
    }

    /// Whether the operator has what they need to go on: the link, a device
    /// login's code, or the key prompt.
    pub fn shown(&self) -> bool {
        match self {
            Self::Starting { .. } | Self::AwaitingApproval { code: None, .. } => false,
            Self::AwaitingCode { .. }
            | Self::AwaitingApproval { code: Some(_), .. }
            | Self::AwaitingKey { .. }
            | Self::AwaitingRedirect { .. }
            | Self::Finishing { .. }
            | Self::Ended(_) => true,
        }
    }

    pub fn ended(&self) -> bool {
        matches!(self, Self::Ended(_))
    }

    /// The state after `input`. Ending inputs end any state but an ended
    /// one; an answer is taken only while one is awaited.
    pub fn apply(self, input: LoginInput) -> Result<Self, LoginRefused> {
        if self.ended() {
            return Err(LoginRefused::Ended);
        }
        Ok(match (self, input) {
            (state, LoginInput::Printed(LoginEvent::Said(text))) => state.saying(text),
            (Self::Starting { flow, said }, LoginInput::Printed(LoginEvent::Link(link))) => {
                match flow {
                    SignInFlow::PasteCode => Self::AwaitingCode { link, said },
                    SignInFlow::DeviceCode => Self::AwaitingApproval {
                        link,
                        code: None,
                        said,
                    },
                    SignInFlow::PasteRedirect => Self::AwaitingRedirect { link, said },
                    SignInFlow::EnterKey => Self::AwaitingKey { said },
                }
            }
            // A second link (the reader yields one) changes nothing.
            (state, LoginInput::Printed(LoginEvent::Link(_))) => state,
            (
                Self::AwaitingApproval {
                    link,
                    code: None,
                    said,
                },
                LoginInput::Printed(LoginEvent::DeviceCode(code)),
            ) => Self::AwaitingApproval {
                link,
                code: Some(code),
                said,
            },
            // A code before its link, or a second one, changes nothing.
            (state, LoginInput::Printed(LoginEvent::DeviceCode(_))) => state,
            (
                Self::AwaitingCode { link, said } | Self::AwaitingRedirect { link, said },
                LoginInput::Answered,
            ) => Self::Finishing {
                link: Some(link),
                said,
            },
            (Self::AwaitingKey { said }, LoginInput::Answered) => {
                Self::Finishing { link: None, said }
            }
            (
                Self::Starting { .. }
                | Self::AwaitingApproval { .. }
                | Self::Finishing { .. }
                | Self::Ended(_),
                LoginInput::Answered,
            ) => return Err(LoginRefused::NotAwaitingAnswer),
            (state, LoginInput::Exited(exit)) => Self::Ended(LoginEnd::Exited {
                exit,
                said: state.said().map(str::to_owned),
            }),
            (_, LoginInput::Cancelled) => Self::Ended(LoginEnd::Cancelled),
            (_, LoginInput::TimedOut) => Self::Ended(LoginEnd::TimedOut),
            (state, LoginInput::Broken(reason)) => Self::Ended(LoginEnd::Broken {
                said: reason.or_else(|| state.said().map(str::to_owned)),
            }),
        })
    }

    fn said(&self) -> Option<&str> {
        match self {
            Self::Starting { said, .. }
            | Self::AwaitingCode { said, .. }
            | Self::AwaitingApproval { said, .. }
            | Self::AwaitingKey { said }
            | Self::AwaitingRedirect { said, .. }
            | Self::Finishing { said, .. } => said.as_deref(),
            Self::Ended(_) => None,
        }
    }

    fn saying(self, text: String) -> Self {
        let said = Some(text);
        match self {
            Self::Starting { flow, .. } => Self::Starting { flow, said },
            Self::AwaitingCode { link, .. } => Self::AwaitingCode { link, said },
            Self::AwaitingApproval { link, code, .. } => {
                Self::AwaitingApproval { link, code, said }
            }
            Self::AwaitingKey { .. } => Self::AwaitingKey { said },
            Self::AwaitingRedirect { link, .. } => Self::AwaitingRedirect { link, said },
            Self::Finishing { link, .. } => Self::Finishing { link, said },
            Self::Ended(end) => Self::Ended(end),
        }
    }
}

/// Where a waited-on login stands: still going, or ended and checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Waited {
    Going { login: Login },
    Done { outcome: LoginOutcome },
}

/// A login's end, and the client's sign-in as checked after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoginOutcome {
    pub end: LoginEnd,
    pub state: SignInState,
}

#[cfg(test)]
mod tests {
    use super::{Awaiting, Login, LoginEnd, LoginInput, LoginRefused};
    use crate::run::ProcessExit;
    use ren_harness::{
        Harness,
        lines::Framed,
        sign_in::{LoginEvent, LoginReader, SignInFlow},
    };

    /// Feed `lines` as `harness`'s login printed them.
    fn printed(harness: Harness, flow: SignInFlow, lines: &[&str]) -> Vec<LoginInput> {
        let mut reader = LoginReader::new(harness, flow);
        lines
            .iter()
            .flat_map(|text| reader.receive(Framed::Line(text.as_bytes().to_vec())))
            .map(LoginInput::Printed)
            .collect()
    }

    fn run(mut login: Login, inputs: Vec<LoginInput>) -> Login {
        for input in inputs {
            login = login.apply(input).unwrap();
        }
        login
    }

    #[test]
    fn a_paste_code_login_waits_for_its_code_then_ends_with_what_it_said() {
        let flow = SignInFlow::PasteCode;
        let login = run(
            Login::start(flow),
            printed(
                Harness::ClaudeCode,
                flow,
                &[
                    "Opening browser to sign in",
                    "visit: https://example.com/authorize",
                ],
            ),
        );
        assert_eq!(login.awaiting(), Some(Awaiting::Code));
        let mut inputs = vec![LoginInput::Answered];
        inputs.extend(printed(
            Harness::ClaudeCode,
            flow,
            &["Login failed: That code wasn't accepted."],
        ));
        inputs.push(LoginInput::Exited(ProcessExit::Code(1)));
        assert_eq!(
            run(login, inputs),
            Login::Ended(LoginEnd::Exited {
                exit: ProcessExit::Code(1),
                said: Some("Login failed: That code wasn't accepted.".into()),
            })
        );
    }

    #[test]
    fn an_answer_is_refused_unless_awaited_and_an_ended_login_takes_nothing() {
        let flow = SignInFlow::PasteCode;
        assert_eq!(
            Login::start(flow).apply(LoginInput::Answered),
            Err(LoginRefused::NotAwaitingAnswer)
        );
        let ended = Login::start(flow).apply(LoginInput::Cancelled).unwrap();
        assert_eq!(ended, Login::Ended(LoginEnd::Cancelled));
        assert_eq!(
            ended.apply(LoginInput::Exited(ProcessExit::Code(0))),
            Err(LoginRefused::Ended)
        );
    }

    #[test]
    fn a_device_login_waits_for_approval_once_its_code_is_shown() {
        let flow = SignInFlow::DeviceCode;
        let login = run(
            Login::start(flow),
            printed(Harness::Codex, flow, &["https://auth.example.test/device"]),
        );
        assert!(!login.shown());
        let login = run(login, printed(Harness::Codex, flow, &["ACME-12345"]));
        assert!(login.shown());
        assert_eq!(login.awaiting(), None);
        let Login::AwaitingApproval {
            code: Some(code), ..
        } = &login
        else {
            panic!("no code: {login:?}");
        };
        assert_eq!(code.as_str(), "ACME-12345");
        // Nothing is typed into a device login.
        assert_eq!(
            login.clone().apply(LoginInput::Answered),
            Err(LoginRefused::NotAwaitingAnswer)
        );
        let ended = login
            .apply(LoginInput::Exited(ProcessExit::Code(0)))
            .unwrap();
        assert!(matches!(ended, Login::Ended(LoginEnd::Exited { .. })));
    }

    #[test]
    fn a_key_login_asks_at_once_and_finishes_with_no_link() {
        let login = Login::start(SignInFlow::EnterKey);
        assert!(login.shown());
        assert_eq!(login.awaiting(), Some(Awaiting::Key));
        let login = login.apply(LoginInput::Answered).unwrap();
        assert_eq!(
            login,
            Login::Finishing {
                link: None,
                said: None
            }
        );
    }

    #[test]
    fn a_redirect_login_waits_on_the_address_its_link_sends_to() {
        let flow = SignInFlow::PasteRedirect;
        let login = run(
            Login::start(flow),
            printed(
                Harness::OpenCode,
                flow,
                &[
                    "https://auth.example.test/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A39547%2Fauth%2Fcallback&state=s",
                ],
            ),
        );
        let Some(Awaiting::Redirect(redirect)) = login.awaiting() else {
            panic!("not awaiting a redirect: {login:?}");
        };
        assert_eq!(redirect.as_str(), "http://127.0.0.1:39547/auth/callback");
        // Stopped because the address could not be opened, it says why.
        let broken = login
            .apply(LoginInput::Broken(Some(
                "the address could not be opened".into(),
            )))
            .unwrap();
        assert_eq!(
            broken,
            Login::Ended(LoginEnd::Broken {
                said: Some("the address could not be opened".into())
            })
        );
        assert!(matches!(
            printed(Harness::OpenCode, flow, &["Done"]).as_slice(),
            [LoginInput::Printed(LoginEvent::Said(_))]
        ));
    }
}
