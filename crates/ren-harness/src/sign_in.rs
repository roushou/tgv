//! Signing a client in on its host, through the client's own commands. Ren
//! reads whether a client is signed in from its status command, and runs
//! its login, which prints a link to open and reads back the code the page
//! shows. Ren relays that link and that code and nothing else: the client
//! receives, keeps, and refreshes its sign-in, and Ren never reads it.
//!
//! Each client that offers this fills the shared types here from its own
//! module (`claude_code`); [`Harness`] dispatches by `match`. A client whose
//! commands Ren does not know yet offers no sign-in, and its sign-in is
//! never checked.

use crate::{Harness, launch::LaunchPlan, lines::Framed, lines::Line};
use serde::Serialize;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::fmt;
use strum::{Display, EnumString, IntoStaticStr, VariantArray};
use url::Url;

/// How a client is asked to sign in: which account it signs in with.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum SignInMethod {
    /// Claude Code with a Claude subscription (Pro, Max, Team, Enterprise).
    ClaudeSubscription,
    /// Claude Code with an Anthropic Console account, billed by API usage.
    AnthropicConsole,
}

/// What a client's status command says of its sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Standing {
    /// Signed in, as the client names how.
    SignedIn {
        method: StatedMethod,
    },
    SignedOut,
}

/// How a client says it is signed in, in its own words (Claude Code's
/// `authMethod`): shown as it is, never matched on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct StatedMethod(String);

impl StatedMethod {
    pub(crate) fn new(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl fmt::Display for StatedMethod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A status command's output that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StatusUnread(pub(crate) String);

/// Why a client cannot be signed in this way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignInRefused {
    #[error("Ren cannot sign {0} in yet")]
    NotOffered(Harness),
    #[error("{harness} does not sign in with {method}")]
    NoSuchMethod {
        harness: Harness,
        method: SignInMethod,
    },
}

/// The page a client's login asks the operator to open: an `https` link,
/// read from the client's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInLink(Url);

impl Serialize for SignInLink {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl SignInLink {
    /// The first `https` link in `text`, if any.
    pub fn find(text: &str) -> Option<Self> {
        text.split_whitespace()
            .filter(|word| word.starts_with("https://"))
            .find_map(|word| Url::parse(word).ok())
            .filter(|url| url.scheme() == "https" && url.host_str().is_some())
            .map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// The code the sign-in page shows, for the client's login to read. It is a
/// one-time secret: never shown in `Debug`, logged, or recorded.
#[derive(Clone, PartialEq, Eq)]
pub struct SignInCode(String);

impl SignInCode {
    /// The longest code taken.
    const MAX: usize = 4096;

    /// Parse a code as pasted: surrounding space trimmed; one line of
    /// printable text.
    pub fn parse(text: &str) -> Result<Self, SignInCodeError> {
        let code = text.trim();
        if code.is_empty() {
            return Err(SignInCodeError::Empty);
        }
        if code.len() > Self::MAX {
            return Err(SignInCodeError::TooLong);
        }
        if code.chars().any(char::is_control) {
            return Err(SignInCodeError::NotOneLine);
        }
        Ok(Self(code.to_owned()))
    }

    /// The code as a line for the login's stdin.
    pub fn line(&self) -> Line {
        Line::text(&self.0)
    }
}

impl fmt::Debug for SignInCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SignInCode(<redacted>)")
    }
}

/// Why a pasted code was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignInCodeError {
    #[error("the code is empty")]
    Empty,
    #[error("the code is too long")]
    TooLong,
    #[error("the code must be one line")]
    NotOneLine,
}

/// Something a login printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginEvent {
    /// The page to open; the login now waits for the code it shows.
    Link(SignInLink),
    /// Any other line, cleaned to printable text: what the client last said
    /// (a failure's reason, when it fails).
    Said(String),
}

/// Reads a login's output, stdout and stderr together, a line at a time.
/// The client's prompt for the code ends with no newline, so it begins the
/// next line it prints; it is left out of what the client said. Sans-IO:
/// frames in, events out.
#[derive(Debug)]
pub struct LoginReader {
    prompt: Option<&'static str>,
    linked: bool,
}

impl LoginReader {
    /// The longest line kept as what the client said.
    const SAID: usize = 500;

    pub fn new(harness: Harness) -> Self {
        Self {
            prompt: match harness {
                Harness::ClaudeCode => Some(crate::claude_code::ClaudeAuth::PROMPT),
                Harness::Codex | Harness::Pi | Harness::OpenCode => None,
            },
            linked: false,
        }
    }

    pub fn receive(&mut self, frame: Framed) -> Option<LoginEvent> {
        let Framed::Line(bytes) = frame else {
            return None;
        };
        let text = String::from_utf8_lossy(&bytes);
        if !self.linked
            && let Some(link) = SignInLink::find(&text)
        {
            self.linked = true;
            return Some(LoginEvent::Link(link));
        }
        let text = self
            .prompt
            .and_then(|prompt| text.trim_start().strip_prefix(prompt))
            .unwrap_or(&text);
        let said: String = text
            .chars()
            .filter(|character| !character.is_control())
            .take(Self::SAID)
            .collect();
        let said = said.trim();
        (!said.is_empty()).then(|| LoginEvent::Said(said.to_owned()))
    }
}

impl Harness {
    /// The ways this client signs in through Ren, first the usual one;
    /// none where Ren cannot sign it in yet.
    pub fn sign_in_methods(self) -> &'static [SignInMethod] {
        match self {
            Self::ClaudeCode => &[
                SignInMethod::ClaudeSubscription,
                SignInMethod::AnthropicConsole,
            ],
            Self::Codex | Self::Pi | Self::OpenCode => &[],
        }
    }

    /// The command that says whether this client is signed in, printing it
    /// on stdout; none where Ren cannot read it yet.
    pub fn sign_in_status(self) -> Option<LaunchPlan> {
        match self {
            Self::ClaudeCode => Some(LaunchPlan::claude_auth_status()),
            Self::Codex | Self::Pi | Self::OpenCode => None,
        }
    }

    /// Read what the status command printed on stdout.
    pub fn read_sign_in(self, stdout: &[u8]) -> Result<Standing, StatusUnread> {
        match self {
            Self::ClaudeCode => crate::claude_code::ClaudeAuth::read_status(stdout),
            Self::Codex | Self::Pi | Self::OpenCode => Err(StatusUnread(format!(
                "Ren cannot read {self}'s sign-in yet"
            ))),
        }
    }

    /// The client's own login, signing in with `method`: it prints a link
    /// and reads the code back from stdin.
    pub fn sign_in(self, method: SignInMethod) -> Result<LaunchPlan, SignInRefused> {
        if !self.sign_in_methods().contains(&method) {
            return Err(match self.sign_in_methods() {
                [] => SignInRefused::NotOffered(self),
                _ => SignInRefused::NoSuchMethod {
                    harness: self,
                    method,
                },
            });
        }
        match self {
            Self::ClaudeCode => Ok(LaunchPlan::claude_auth_login(method)),
            Self::Codex | Self::Pi | Self::OpenCode => Err(SignInRefused::NotOffered(self)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LoginEvent, LoginReader, SignInCode, SignInCodeError, SignInLink, SignInMethod};
    use crate::{Harness, lines::Framed};

    fn line(text: &str) -> Framed {
        Framed::Line(text.as_bytes().to_vec())
    }

    #[test]
    fn a_login_gives_its_link_once_then_what_it_says() {
        let mut reader = LoginReader::new(Harness::ClaudeCode);
        assert_eq!(
            reader.receive(line("Opening browser to sign in…")),
            Some(LoginEvent::Said("Opening browser to sign in…".into()))
        );
        let Some(LoginEvent::Link(link)) = reader.receive(line(
            "If the browser didn't open, visit: https://example.com/oauth/authorize?code=true&state=abc",
        )) else {
            panic!("no link");
        };
        assert_eq!(
            link.as_str(),
            "https://example.com/oauth/authorize?code=true&state=abc"
        );
        assert_eq!(
            reader.receive(line("See https://example.com/help\u{1b}[0m")),
            Some(LoginEvent::Said("See https://example.com/help[0m".into()))
        );
        assert_eq!(reader.receive(line("   ")), None);
        // Its prompt, which has no newline, begins the next line.
        assert_eq!(
            reader.receive(line("Paste code here if prompted > Login failed: no.")),
            Some(LoginEvent::Said("Login failed: no.".into()))
        );
        assert_eq!(reader.receive(Framed::TooLong { bytes: 9 }), None);
    }

    #[test]
    fn only_https_links_are_offered() {
        assert_eq!(SignInLink::find("visit: http://example.com/a"), None);
        assert_eq!(SignInLink::find("visit: https://"), None);
        assert!(SignInLink::find("visit: https://example.com/a").is_some());
    }

    #[test]
    fn a_code_is_one_trimmed_line_and_never_shown() {
        let code = SignInCode::parse("  abc#def \n").unwrap();
        assert_eq!(code.line().as_bytes(), b"abc#def\n");
        assert_eq!(format!("{code:?}"), "SignInCode(<redacted>)");
        assert_eq!(SignInCode::parse(" \n"), Err(SignInCodeError::Empty));
        assert_eq!(SignInCode::parse("a\nb"), Err(SignInCodeError::NotOneLine));
        assert_eq!(
            SignInCode::parse(&"a".repeat(5000)),
            Err(SignInCodeError::TooLong)
        );
    }

    #[test]
    fn only_offered_methods_sign_in() {
        assert!(
            Harness::ClaudeCode
                .sign_in(SignInMethod::AnthropicConsole)
                .is_ok()
        );
        assert!(
            Harness::Codex
                .sign_in(SignInMethod::ClaudeSubscription)
                .is_err()
        );
        assert!(Harness::Codex.sign_in_status().is_none());
    }
}
