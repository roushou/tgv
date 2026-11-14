//! A client's sign-in on this host, as Ren reads it through the client's
//! own commands: what its status command last said ([`SignInState`]), and
//! the pure lifecycle of one run of its login ([`Login`]), which prints a
//! link, reads back the code the page shows, and exits. Whether a login
//! signed the client in is never taken from the login: a status check
//! after it says so.

use crate::run::ProcessExit;
use ren_harness::sign_in::{
    DeviceCode, LoginEvent, SignInFlow, SignInLink, Standing, StatedMethod,
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
    /// Waiting for the code the page at `link` shows (a paste-code login).
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
    /// The code was handed to the client; waiting for it to finish.
    Finishing {
        link: SignInLink,
        said: Option<String>,
    },
    Ended(LoginEnd),
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
    /// No code or approval came, or the client did not finish, in time.
    TimedOut,
    /// Its output ended unreadably or its stdin closed: it was stopped.
    Broken,
}

/// What happens to a login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginInput {
    Printed(LoginEvent),
    /// The operator's code was written to its stdin.
    CodeSent,
    Exited(ProcessExit),
    Cancelled,
    TimedOut,
    Broken,
}

/// A login input that its state does not take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LoginRefused {
    #[error("the sign-in is not waiting for a code")]
    NotAwaitingCode,
    #[error("the sign-in has ended")]
    Ended,
}

impl Login {
    /// A login just started, going as `flow` says.
    pub fn start(flow: SignInFlow) -> Self {
        Self::Starting { flow, said: None }
    }

    /// Whether the code can be handed over now.
    pub fn awaits_code(&self) -> bool {
        matches!(self, Self::AwaitingCode { .. })
    }

    /// Whether the operator has what they need to go on: the link, and a
    /// device login's code.
    pub fn shown(&self) -> bool {
        match self {
            Self::Starting { .. } | Self::AwaitingApproval { code: None, .. } => false,
            Self::AwaitingCode { .. }
            | Self::AwaitingApproval { code: Some(_), .. }
            | Self::Finishing { .. }
            | Self::Ended(_) => true,
        }
    }

    pub fn ended(&self) -> bool {
        matches!(self, Self::Ended(_))
    }

    /// The state after `input`. Ending inputs end any state but an ended
    /// one; a code is taken only while one is awaited.
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
            (Self::AwaitingCode { link, said }, LoginInput::CodeSent) => {
                Self::Finishing { link, said }
            }
            (
                Self::Starting { .. }
                | Self::AwaitingApproval { .. }
                | Self::Finishing { .. }
                | Self::Ended(_),
                LoginInput::CodeSent,
            ) => return Err(LoginRefused::NotAwaitingCode),
            (state, LoginInput::Exited(exit)) => Self::Ended(LoginEnd::Exited {
                exit,
                said: state.said().map(str::to_owned),
            }),
            (_, LoginInput::Cancelled) => Self::Ended(LoginEnd::Cancelled),
            (_, LoginInput::TimedOut) => Self::Ended(LoginEnd::TimedOut),
            (_, LoginInput::Broken) => Self::Ended(LoginEnd::Broken),
        })
    }

    fn said(&self) -> Option<&str> {
        match self {
            Self::Starting { said, .. }
            | Self::AwaitingCode { said, .. }
            | Self::AwaitingApproval { said, .. }
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
    use super::{Login, LoginEnd, LoginInput, LoginRefused};
    use crate::run::ProcessExit;
    use ren_harness::sign_in::SignInFlow;
    use ren_harness::{
        lines::Framed,
        sign_in::{LoginEvent, LoginReader},
    };

    fn printed(text: &str) -> LoginInput {
        let event = LoginReader::new(ren_harness::Harness::ClaudeCode)
            .receive(Framed::Line(text.as_bytes().to_vec()))
            .unwrap();
        LoginInput::Printed(event)
    }

    #[test]
    fn a_login_waits_for_its_code_then_ends_with_what_it_said() {
        let login = Login::start(SignInFlow::PasteCode)
            .apply(printed("Opening browser to sign in"))
            .unwrap()
            .apply(printed("visit: https://example.com/authorize"))
            .unwrap();
        assert!(login.awaits_code());
        let login = login
            .apply(LoginInput::CodeSent)
            .unwrap()
            .apply(printed("Login failed: That code wasn't accepted."))
            .unwrap()
            .apply(LoginInput::Exited(ProcessExit::Code(1)))
            .unwrap();
        assert_eq!(
            login,
            Login::Ended(LoginEnd::Exited {
                exit: ProcessExit::Code(1),
                said: Some("Login failed: That code wasn't accepted.".into()),
            })
        );
    }

    #[test]
    fn a_code_is_refused_unless_awaited_and_an_ended_login_takes_nothing() {
        assert_eq!(
            Login::start(SignInFlow::PasteCode).apply(LoginInput::CodeSent),
            Err(LoginRefused::NotAwaitingCode)
        );
        let ended = Login::start(SignInFlow::PasteCode)
            .apply(LoginInput::Cancelled)
            .unwrap();
        assert_eq!(ended, Login::Ended(LoginEnd::Cancelled));
        assert_eq!(
            ended.apply(LoginInput::Exited(ProcessExit::Code(0))),
            Err(LoginRefused::Ended)
        );
    }

    #[test]
    fn a_second_link_keeps_the_first() {
        let login = Login::start(SignInFlow::PasteCode)
            .apply(printed("visit: https://example.com/first"))
            .unwrap()
            .apply(LoginInput::Printed(LoginEvent::Link(
                ren_harness::sign_in::SignInLink::find("https://example.com/second").unwrap(),
            )))
            .unwrap();
        let Login::AwaitingCode { link, .. } = login else {
            panic!("not awaiting");
        };
        assert_eq!(link.as_str(), "https://example.com/first");
    }

    #[test]
    fn a_device_login_waits_for_approval_once_its_code_is_shown() {
        let device = |text: &str| {
            let event = LoginReader::new(ren_harness::Harness::Codex)
                .receive(Framed::Line(text.as_bytes().to_vec()));
            event.map(LoginInput::Printed)
        };
        let mut reader = LoginReader::new(ren_harness::Harness::Codex);
        let mut feed = |text: &str| {
            LoginInput::Printed(
                reader
                    .receive(Framed::Line(text.as_bytes().to_vec()))
                    .unwrap(),
            )
        };
        let login = Login::start(SignInFlow::DeviceCode)
            .apply(feed("https://auth.example.test/device"))
            .unwrap();
        assert!(!login.shown());
        let login = login.apply(feed("ACME-12345")).unwrap();
        assert!(login.shown());
        assert!(!login.awaits_code());
        let Login::AwaitingApproval {
            code: Some(code), ..
        } = &login
        else {
            panic!("no code: {login:?}");
        };
        assert_eq!(code.as_str(), "ACME-12345");
        // Nothing is pasted into a device login.
        assert_eq!(
            login.clone().apply(LoginInput::CodeSent),
            Err(LoginRefused::NotAwaitingCode)
        );
        let ended = login
            .apply(LoginInput::Exited(ProcessExit::Code(0)))
            .unwrap();
        assert!(matches!(ended, Login::Ended(LoginEnd::Exited { .. })));
        assert!(device("plain words").is_some());
    }
}
