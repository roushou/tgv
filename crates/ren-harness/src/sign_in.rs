//! Signing a client in on its host, through the client's own commands. Ren
//! reads whether a client is signed in from its status command, and runs
//! its login, which prints a link to open and then either reads back the
//! code the page shows ([`SignInFlow::PasteCode`], Claude Code) or shows a
//! one-time code to enter on the page and finishes once it is approved
//! ([`SignInFlow::DeviceCode`], Codex). Ren relays the link and the codes
//! and nothing else: the client receives, keeps, and refreshes its sign-in,
//! and Ren never reads it.
//!
//! Each client that offers this fills the shared types here from its own
//! module (`claude_code`, `codex`); [`Harness`] dispatches by `match`. A client whose
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
    /// Codex with a ChatGPT account, by device code.
    Chatgpt,
}

/// How a client's login goes once it has printed its link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInFlow {
    /// The page shows a code, pasted back to the login, which then ends.
    PasteCode,
    /// The login shows a one-time code to enter on the page, and ends by
    /// itself once the operator approves there.
    DeviceCode,
}

/// The one-time code a device login shows, for the operator to enter on
/// the sign-in page. Shown to the operator, never logged or recorded.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCode(String);

impl DeviceCode {
    /// A code as a device login prints it alone on a line: groups of
    /// capital letters and digits joined by `-` (`ABCD-12345`).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let mut groups = text.split('-');
        let shaped = text.len() <= 32
            && text.contains('-')
            && groups.all(|group| {
                (3..=10).contains(&group.len())
                    && group.chars().all(|character| {
                        character.is_ascii_uppercase() || character.is_ascii_digit()
                    })
            });
        shaped.then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeviceCode(<redacted>)")
    }
}

impl Serialize for DeviceCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
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
    /// The page to open; the login now waits for the code it shows, or for
    /// the operator to approve there.
    Link(SignInLink),
    /// The one-time code to enter on the page (a device login).
    DeviceCode(DeviceCode),
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
    flow: SignInFlow,
    linked: bool,
    coded: bool,
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
            flow: harness.sign_in_flow(),
            linked: false,
            coded: false,
        }
    }

    /// `text` as a terminal shows it: its escape sequences (colors) and
    /// other control characters left out.
    pub(crate) fn plain(text: &str) -> String {
        let mut plain = String::with_capacity(text.len());
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\u{1b}' {
                // CSI: `ESC [`, parameters, then one final byte.
                if characters.next_if_eq(&'[').is_some() {
                    for next in characters.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                continue;
            }
            if !character.is_control() {
                plain.push(character);
            }
        }
        plain
    }

    pub fn receive(&mut self, frame: Framed) -> Option<LoginEvent> {
        let Framed::Line(bytes) = frame else {
            return None;
        };
        let text = Self::plain(&String::from_utf8_lossy(&bytes));
        if !self.linked
            && let Some(link) = SignInLink::find(&text)
        {
            self.linked = true;
            return Some(LoginEvent::Link(link));
        }
        if self.linked
            && !self.coded
            && self.flow == SignInFlow::DeviceCode
            && let Some(code) = DeviceCode::parse(&text)
        {
            self.coded = true;
            return Some(LoginEvent::DeviceCode(code));
        }
        let text = self
            .prompt
            .and_then(|prompt| text.trim_start().strip_prefix(prompt))
            .unwrap_or(&text);
        let said: String = text.chars().take(Self::SAID).collect();
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
            Self::Codex => &[SignInMethod::Chatgpt],
            Self::Pi | Self::OpenCode => &[],
        }
    }

    /// How this client's login goes once it has printed its link.
    pub fn sign_in_flow(self) -> SignInFlow {
        match self {
            Self::Codex => SignInFlow::DeviceCode,
            Self::ClaudeCode | Self::Pi | Self::OpenCode => SignInFlow::PasteCode,
        }
    }

    /// The command that says whether this client is signed in; none where
    /// Ren cannot read it yet. It is read from stdout and stderr together.
    pub fn sign_in_status(self) -> Option<LaunchPlan> {
        match self {
            Self::ClaudeCode => Some(LaunchPlan::claude_auth_status()),
            Self::Codex => Some(LaunchPlan::codex_login_status()),
            Self::Pi | Self::OpenCode => None,
        }
    }

    /// Read what the status command printed, stdout and stderr together.
    pub fn read_sign_in(self, output: &[u8]) -> Result<Standing, StatusUnread> {
        match self {
            Self::ClaudeCode => crate::claude_code::ClaudeAuth::read_status(output),
            Self::Codex => crate::codex::CodexAuth::read_status(output),
            Self::Pi | Self::OpenCode => Err(StatusUnread(format!(
                "Ren cannot read {self}'s sign-in yet"
            ))),
        }
    }

    /// The client's own login, signing in with `method`: it prints a link,
    /// then goes as [`Self::sign_in_flow`] says.
    pub fn sign_in(self, method: SignInMethod) -> Result<LaunchPlan, SignInRefused> {
        match (self, method) {
            (Self::ClaudeCode, SignInMethod::ClaudeSubscription) => {
                Ok(LaunchPlan::claude_auth_login("--claudeai"))
            }
            (Self::ClaudeCode, SignInMethod::AnthropicConsole) => {
                Ok(LaunchPlan::claude_auth_login("--console"))
            }
            (Self::Codex, SignInMethod::Chatgpt) => Ok(LaunchPlan::codex_device_login()),
            (Self::Pi | Self::OpenCode, _) => Err(SignInRefused::NotOffered(self)),
            (
                Self::ClaudeCode | Self::Codex,
                SignInMethod::ClaudeSubscription
                | SignInMethod::AnthropicConsole
                | SignInMethod::Chatgpt,
            ) => Err(SignInRefused::NoSuchMethod {
                harness: self,
                method,
            }),
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
            Some(LoginEvent::Said("See https://example.com/help".into()))
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
        assert!(Harness::Codex.sign_in(SignInMethod::Chatgpt).is_ok());
        assert!(Harness::Pi.sign_in_status().is_none());
    }

    #[test]
    fn a_device_login_gives_its_link_then_its_code_without_colors() {
        // As Codex prints it, colors and all (the code is invented).
        let mut reader = LoginReader::new(Harness::Codex);
        let events: Vec<_> = [
            "Welcome to Codex [v\u{1b}[90m0.162.1\u{1b}[0m]",
            "1. Open this link in your browser and sign in to your account",
            "   \u{1b}[94mhttps://auth.example.test/codex/device\u{1b}[0m",
            "2. Enter this one-time code \u{1b}[90m(expires in 15 minutes)\u{1b}[0m",
            "   \u{1b}[94mACME-12345\u{1b}[0m",
        ]
        .into_iter()
        .filter_map(|text| reader.receive(line(text)))
        .collect();
        assert_eq!(
            events.first(),
            Some(&LoginEvent::Said("Welcome to Codex [v0.162.1]".into()))
        );
        let Some(LoginEvent::Link(link)) = events.get(2) else {
            panic!("no link: {events:?}");
        };
        assert_eq!(link.as_str(), "https://auth.example.test/codex/device");
        let Some(LoginEvent::DeviceCode(code)) = events.get(4) else {
            panic!("no code: {events:?}");
        };
        assert_eq!(code.as_str(), "ACME-12345");
        assert_eq!(format!("{code:?}"), "DeviceCode(<redacted>)");
    }

    #[test]
    fn only_code_shaped_lines_are_device_codes() {
        use super::DeviceCode;
        assert!(DeviceCode::parse("ABCD-1234").is_some());
        assert!(DeviceCode::parse("abcd-1234").is_none());
        assert!(DeviceCode::parse("ABCD1234").is_none());
        assert!(DeviceCode::parse("Login failed").is_none());
        // A paste-code client never reads a device code.
        let mut reader = LoginReader::new(Harness::ClaudeCode);
        reader.receive(line("visit: https://example.com/a"));
        assert_eq!(
            reader.receive(line("ABCD-1234")),
            Some(LoginEvent::Said("ABCD-1234".into()))
        );
    }
}
