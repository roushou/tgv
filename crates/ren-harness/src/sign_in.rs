//! Signing a client in on its host, through the client's own commands. Ren
//! reads whether a client is signed in from its status commands, and runs
//! its login, which goes one of four ways ([`SignInFlow`]): it prints a link
//! and reads back the code the page shows (Claude Code); it prints a link
//! and a one-time code to enter there, and finishes once that is approved
//! (Codex, Pi's ChatGPT sign-in); it asks for an API key (Pi and OpenCode);
//! or it prints a link whose approval ends on a `127.0.0.1` address on the
//! host, which the operator pastes back for the daemon to open there
//! (OpenCode's ChatGPT sign-in). Some logins need a terminal ([`Console`]).
//! Ren relays the link, the codes, the key, and the address, and nothing
//! else: the client receives, keeps, and refreshes its sign-in, and Ren
//! never reads, stores, or records it.
//!
//! Each client fills the shared types here from its own module
//! (`claude_code`, `codex`, `pi`, `opencode`); [`Harness`] dispatches by
//! `match`.

use crate::{Harness, launch::LaunchPlan, lines::Framed, lines::Line};
use serde::Serialize;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::fmt;
use strum::{Display, EnumString, IntoStaticStr, VariantArray};
use url::Url;

/// How a client is asked to sign in: with which account or key.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum SignInMethod {
    /// Claude Code with a Claude subscription (Pro, Max, Team, Enterprise).
    ClaudeSubscription,
    /// Claude Code with an Anthropic Console account, billed by API usage.
    AnthropicConsole,
    /// A ChatGPT account: Codex and Pi by device code, OpenCode by OpenAI's
    /// token sharing.
    Chatgpt,
    /// An Anthropic API key, saved by the client (Pi, OpenCode): Anthropic
    /// does not permit Claude subscriptions in them.
    AnthropicApiKey,
    /// An OpenAI API key, saved by the client (Pi, OpenCode).
    OpenaiApiKey,
}

/// How a client's login goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInFlow {
    /// A link; the page shows a code, pasted back to the login.
    PasteCode,
    /// A link and a one-time code to enter there; the login ends by itself
    /// once the operator approves.
    DeviceCode,
    /// The login asks for an API key.
    EnterKey,
    /// A link whose approval ends on a `127.0.0.1` address on the host: the
    /// operator pastes it back, and the daemon opens it there.
    PasteRedirect,
}

/// Whether a login runs on pipes or needs a terminal of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Pipes,
    /// A pseudo-terminal: its input and output are the terminal's.
    Terminal,
}

/// How to run one client's login: its command, what it runs on, and how it
/// goes.
pub struct SignInPlan {
    pub launch: LaunchPlan,
    pub console: Console,
    pub flow: SignInFlow,
}

/// The one-time code a device login shows, for the operator to enter on
/// the sign-in page. Shown to the operator, never logged or recorded.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCode(String);

impl DeviceCode {
    /// A code as a device login prints it: groups of four to ten capital
    /// letters and digits joined by `-` (`ABCD-12345`).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let shaped = text.len() <= 32
            && text.contains('-')
            && text.split('-').all(|group| {
                (4..=10).contains(&group.len())
                    && group.chars().all(|character| {
                        character.is_ascii_uppercase() || character.is_ascii_digit()
                    })
            });
        shaped.then(|| Self(text.to_owned()))
    }

    /// The first word of `text` shaped as a code (`… enter code: ABCD-1234`).
    pub fn find(text: &str) -> Option<Self> {
        text.split_whitespace().find_map(|word| {
            Self::parse(word.trim_matches(|character: char| !character.is_ascii_alphanumeric()))
        })
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

/// What a client's status commands say of its sign-in.
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
/// `authMethod`, Pi's ready providers): shown as it is, never matched on.
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

    /// Where approving this link sends the browser, when that is a
    /// loopback address on the host (its `redirect_uri`).
    pub fn redirect(&self) -> Option<LocalRedirect> {
        let (_, value) = self
            .0
            .query_pairs()
            .find(|(name, _)| name == "redirect_uri")?;
        LocalRedirect::loopback(Url::parse(&value).ok()?)
    }
}

/// A loopback address on the host a login listens on for its approval
/// (`http://127.0.0.1:<port>/<path>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRedirect(Url);

impl LocalRedirect {
    fn loopback(url: Url) -> Option<Self> {
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        (url.scheme() == "http" && loopback && url.port().is_some()).then_some(Self(url))
    }

    /// The address the operator pasted, taken only when it is this one
    /// (scheme, host, port, and path) and carries a `code` and a `state`.
    pub fn accept(&self, answer: &SignInAnswer) -> Result<LocalRedirect, RedirectRefused> {
        let pasted = Url::parse(answer.text()).map_err(|_| RedirectRefused::NotAnAddress)?;
        let same = pasted.scheme() == self.0.scheme()
            && pasted.host_str() == self.0.host_str()
            && pasted.port() == self.0.port()
            && pasted.path() == self.0.path();
        if !same {
            return Err(RedirectRefused::Elsewhere);
        }
        let has = |wanted: &str| pasted.query_pairs().any(|(name, _)| name == wanted);
        if !(has("code") && has("state")) {
            return Err(RedirectRefused::Incomplete);
        }
        Ok(Self(pasted))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Why a pasted address was not opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RedirectRefused {
    #[error("paste the whole address from the browser's address bar")]
    NotAnAddress,
    #[error("that is not the address this sign-in waits on")]
    Elsewhere,
    #[error("the address has no code; approve on the page first")]
    Incomplete,
}

/// What a login waits for from the operator: the code the page showed, an
/// API key, or the address the browser could not open. A secret: never
/// shown in `Debug`, logged, or recorded.
#[derive(Clone, PartialEq, Eq)]
pub struct SignInAnswer(String);

impl SignInAnswer {
    /// The longest answer taken.
    const MAX: usize = 8192;

    /// Parse an answer as pasted: surrounding space trimmed; one line of
    /// printable text.
    pub fn parse(text: &str) -> Result<Self, SignInAnswerError> {
        let answer = text.trim();
        if answer.is_empty() {
            return Err(SignInAnswerError::Empty);
        }
        if answer.len() > Self::MAX {
            return Err(SignInAnswerError::TooLong);
        }
        if answer.chars().any(char::is_control) {
            return Err(SignInAnswerError::NotOneLine);
        }
        Ok(Self(answer.to_owned()))
    }

    /// The answer as typed into the login: a line on pipes, Enter on a
    /// terminal.
    pub fn line(&self, console: Console) -> Line {
        match console {
            Console::Pipes => Line::text(&self.0),
            Console::Terminal => Line::keys(&self.0),
        }
    }

    /// The answer's text, to leave out of anything relayed.
    pub fn text(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SignInAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SignInAnswer(<redacted>)")
    }
}

/// Why a pasted answer was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignInAnswerError {
    #[error("the answer is empty")]
    Empty,
    #[error("the answer is too long")]
    TooLong,
    #[error("the answer must be one line")]
    NotOneLine,
}

/// Something a login printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginEvent {
    /// The page to open.
    Link(SignInLink),
    /// The one-time code to enter on the page (a device login).
    DeviceCode(DeviceCode),
    /// Any other line, cleaned to printable text: what the client last said
    /// (a failure's reason, when it fails).
    Said(String),
}

/// Reads a login's output, stdout and stderr (or its terminal) together, a
/// line at a time. A prompt for a code ends with no newline, so it begins
/// the next line printed; it is left out of what the client said. Sans-IO:
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

    pub fn new(harness: Harness, flow: SignInFlow) -> Self {
        Self {
            prompt: match harness {
                Harness::ClaudeCode => Some(crate::claude_code::ClaudeAuth::PROMPT),
                Harness::Codex | Harness::Pi | Harness::OpenCode => None,
            },
            flow,
            linked: false,
            coded: false,
        }
    }

    /// `text` as a terminal shows it: its escape sequences (colors, cursor
    /// moves) and other control characters left out.
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

    /// What one line says: a link, a device code (on its line or the next),
    /// or else the line itself.
    pub fn receive(&mut self, frame: Framed) -> Vec<LoginEvent> {
        let Framed::Line(bytes) = frame else {
            return Vec::new();
        };
        // A terminal's carriage returns redraw a line: its last drawing counts.
        let raw = String::from_utf8_lossy(&bytes);
        let drawn = raw
            .rsplit('\r')
            .find(|part| !part.trim().is_empty())
            .unwrap_or(&raw);
        let text = Self::plain(drawn);
        let mut events = Vec::new();
        if !self.linked
            && let Some(link) = SignInLink::find(&text)
        {
            self.linked = true;
            events.push(LoginEvent::Link(link));
        }
        if !self.coded
            && self.flow == SignInFlow::DeviceCode
            && let Some(code) = DeviceCode::find(&text)
        {
            self.coded = true;
            events.push(LoginEvent::DeviceCode(code));
        }
        if !events.is_empty() {
            return events;
        }
        let text = self
            .prompt
            .and_then(|prompt| text.trim_start().strip_prefix(prompt))
            .unwrap_or(&text);
        let said: String = text.chars().take(Self::SAID).collect();
        let said = said.trim_matches(|character: char| {
            character.is_whitespace() || "│┌└◇◆●◒◐◓◑".contains(character)
        });
        if said.is_empty() {
            return events;
        }
        events.push(LoginEvent::Said(said.to_owned()));
        events
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
            Self::Pi | Self::OpenCode => &[
                SignInMethod::Chatgpt,
                SignInMethod::AnthropicApiKey,
                SignInMethod::OpenaiApiKey,
            ],
        }
    }

    /// The commands that say whether this client is signed in, each read
    /// from stdout and stderr together; none where Ren cannot read it.
    pub fn sign_in_status(self) -> Vec<LaunchPlan> {
        match self {
            Self::ClaudeCode => vec![LaunchPlan::claude_auth_status()],
            Self::Codex => vec![LaunchPlan::codex_login_status()],
            Self::Pi => crate::pi::PiAuth::checks(),
            Self::OpenCode => vec![LaunchPlan::opencode_auth_list()],
        }
    }

    /// Read what the status commands printed, in their order.
    pub fn read_sign_in(self, outputs: &[Vec<u8>]) -> Result<Standing, StatusUnread> {
        let first = || outputs.first().map(Vec::as_slice).unwrap_or_default();
        match self {
            Self::ClaudeCode => crate::claude_code::ClaudeAuth::read_status(first()),
            Self::Codex => crate::codex::CodexAuth::read_status(first()),
            Self::Pi => crate::pi::PiAuth::read_checks(outputs),
            Self::OpenCode => crate::opencode::OpenCodeAuth::read_list(first()),
        }
    }

    /// The client's own login, signing in with `method`.
    pub fn sign_in(self, method: SignInMethod) -> Result<SignInPlan, SignInRefused> {
        let plan = |launch, console, flow| {
            Ok(SignInPlan {
                launch,
                console,
                flow,
            })
        };
        match (self, method) {
            (Self::ClaudeCode, SignInMethod::ClaudeSubscription) => plan(
                LaunchPlan::claude_auth_login("--claudeai"),
                Console::Pipes,
                SignInFlow::PasteCode,
            ),
            (Self::ClaudeCode, SignInMethod::AnthropicConsole) => plan(
                LaunchPlan::claude_auth_login("--console"),
                Console::Pipes,
                SignInFlow::PasteCode,
            ),
            (Self::Codex, SignInMethod::Chatgpt) => plan(
                LaunchPlan::codex_device_login(),
                Console::Pipes,
                SignInFlow::DeviceCode,
            ),
            (Self::Pi, SignInMethod::Chatgpt) => plan(
                LaunchPlan::pi_auth_login("openai-codex", "--no-browser"),
                Console::Terminal,
                SignInFlow::DeviceCode,
            ),
            (Self::Pi, SignInMethod::AnthropicApiKey) => plan(
                LaunchPlan::pi_auth_login("anthropic", "--api-key"),
                Console::Terminal,
                SignInFlow::EnterKey,
            ),
            (Self::Pi, SignInMethod::OpenaiApiKey) => plan(
                LaunchPlan::pi_auth_login("openai", "--api-key"),
                Console::Terminal,
                SignInFlow::EnterKey,
            ),
            (Self::OpenCode, SignInMethod::Chatgpt) => plan(
                LaunchPlan::opencode_auth_login("openai", "chatgpt-token-sharing"),
                Console::Pipes,
                SignInFlow::PasteRedirect,
            ),
            (Self::OpenCode, SignInMethod::AnthropicApiKey) => plan(
                LaunchPlan::opencode_auth_login("anthropic", "key"),
                Console::Terminal,
                SignInFlow::EnterKey,
            ),
            (Self::OpenCode, SignInMethod::OpenaiApiKey) => plan(
                LaunchPlan::opencode_auth_login("openai", "key"),
                Console::Terminal,
                SignInFlow::EnterKey,
            ),
            (
                Self::ClaudeCode | Self::Codex | Self::Pi | Self::OpenCode,
                SignInMethod::ClaudeSubscription
                | SignInMethod::AnthropicConsole
                | SignInMethod::Chatgpt
                | SignInMethod::AnthropicApiKey
                | SignInMethod::OpenaiApiKey,
            ) => Err(SignInRefused::NoSuchMethod {
                harness: self,
                method,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Console, DeviceCode, LoginEvent, LoginReader, RedirectRefused, SignInAnswer,
        SignInAnswerError, SignInFlow, SignInLink, SignInMethod,
    };
    use crate::{Harness, lines::Framed};
    use ren_protocol::label::Labeled;

    fn line(text: &str) -> Framed {
        Framed::Line(text.as_bytes().to_vec())
    }

    #[test]
    fn a_login_gives_its_link_once_then_what_it_says() {
        let mut reader = LoginReader::new(Harness::ClaudeCode, SignInFlow::PasteCode);
        assert_eq!(
            reader.receive(line("Opening browser to sign in…")),
            vec![LoginEvent::Said("Opening browser to sign in…".into())]
        );
        let events = reader.receive(line(
            "If the browser didn't open, visit: https://example.com/oauth/authorize?code=true&state=abc",
        ));
        let [LoginEvent::Link(link)] = events.as_slice() else {
            panic!("no link: {events:?}");
        };
        assert_eq!(
            link.as_str(),
            "https://example.com/oauth/authorize?code=true&state=abc"
        );
        assert_eq!(
            reader.receive(line("See https://example.com/help\u{1b}[0m")),
            vec![LoginEvent::Said("See https://example.com/help".into())]
        );
        assert!(reader.receive(line("   ")).is_empty());
        assert!(reader.receive(Framed::TooLong { bytes: 9 }).is_empty());
        // Its prompt, which has no newline, begins the next line.
        assert_eq!(
            reader.receive(line("Paste code here if prompted > Login failed: no.")),
            vec![LoginEvent::Said("Login failed: no.".into())]
        );
    }

    #[test]
    fn a_device_login_gives_its_link_then_its_code_without_colors() {
        // As Codex prints it, colors and all (the code is invented).
        let mut reader = LoginReader::new(Harness::Codex, SignInFlow::DeviceCode);
        let events: Vec<_> = [
            "Welcome to Codex [v\u{1b}[90m0.162.1\u{1b}[0m]",
            "   \u{1b}[94mhttps://auth.example.test/codex/device\u{1b}[0m",
            "2. Enter this one-time code \u{1b}[90m(expires in 15 minutes)\u{1b}[0m",
            "   \u{1b}[94mACME-12345\u{1b}[0m",
        ]
        .into_iter()
        .flat_map(|text| reader.receive(line(text)))
        .collect();
        assert_eq!(events.len(), 4, "{events:?}");
        assert!(
            matches!(&events[1], LoginEvent::Link(link) if link.as_str() == "https://auth.example.test/codex/device")
        );
        assert!(
            matches!(&events[3], LoginEvent::DeviceCode(code) if code.as_str() == "ACME-12345")
        );
    }

    #[test]
    fn a_link_and_a_code_on_one_terminal_line_are_both_read() {
        // As Pi prints it on its terminal, redrawn with a carriage return.
        let mut reader = LoginReader::new(Harness::Pi, SignInFlow::DeviceCode);
        let events = reader.receive(line(
            "Select login method: Device code\rOpen https://auth.example.test/codex/device and enter code: ACME-1234\r",
        ));
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[1], LoginEvent::DeviceCode(code) if code.as_str() == "ACME-1234"));
    }

    #[test]
    fn only_code_shaped_words_are_device_codes() {
        assert!(DeviceCode::parse("ABCD-1234").is_some());
        assert!(DeviceCode::parse("abcd-1234").is_none());
        assert!(DeviceCode::parse("ABCD1234").is_none());
        assert!(DeviceCode::parse("SHA-256").is_none());
        assert_eq!(
            DeviceCode::find("enter code: ACME-1234.").map(|code| code.as_str().to_owned()),
            Some("ACME-1234".into())
        );
        assert_eq!(
            format!("{:?}", DeviceCode::parse("ABCD-1234").unwrap()),
            "DeviceCode(<redacted>)"
        );
        // A paste-code login never reads a device code.
        let mut reader = LoginReader::new(Harness::ClaudeCode, SignInFlow::PasteCode);
        assert_eq!(
            reader.receive(line("ABCD-1234")),
            vec![LoginEvent::Said("ABCD-1234".into())]
        );
    }

    #[test]
    fn only_https_links_are_offered() {
        assert_eq!(SignInLink::find("visit: http://example.com/a"), None);
        assert_eq!(SignInLink::find("visit: https://"), None);
        assert!(SignInLink::find("visit: https://example.com/a").is_some());
    }

    #[test]
    fn an_answer_is_one_trimmed_line_typed_as_its_console_needs_and_never_shown() {
        let answer = SignInAnswer::parse("  abc#def \n").unwrap();
        assert_eq!(answer.line(Console::Pipes).as_bytes(), b"abc#def\n");
        assert_eq!(answer.line(Console::Terminal).as_bytes(), b"abc#def\r");
        assert_eq!(format!("{answer:?}"), "SignInAnswer(<redacted>)");
        assert_eq!(SignInAnswer::parse(" \n"), Err(SignInAnswerError::Empty));
        assert_eq!(
            SignInAnswer::parse("a\nb"),
            Err(SignInAnswerError::NotOneLine)
        );
        assert_eq!(
            SignInAnswer::parse(&"a".repeat(9000)),
            Err(SignInAnswerError::TooLong)
        );
    }

    #[test]
    fn a_pasted_redirect_is_opened_only_when_it_is_the_one_waited_on() {
        let link = SignInLink::find(
            "https://auth.example.test/authorize?client_id=app&redirect_uri=http%3A%2F%2F127.0.0.1%3A39547%2Fauth%2Fcallback&state=s1",
        )
        .unwrap();
        let waited = link.redirect().unwrap();
        let paste = |text: &str| waited.accept(&SignInAnswer::parse(text).unwrap());
        assert_eq!(
            paste("http://127.0.0.1:39547/auth/callback?code=c1&state=s1")
                .map(|url| url.as_str().to_owned()),
            Ok("http://127.0.0.1:39547/auth/callback?code=c1&state=s1".into())
        );
        assert_eq!(
            paste("http://127.0.0.1:1/auth/callback?code=c&state=s"),
            Err(RedirectRefused::Elsewhere)
        );
        assert_eq!(
            paste("http://evil.example/auth/callback?code=c&state=s"),
            Err(RedirectRefused::Elsewhere)
        );
        assert_eq!(
            paste("http://127.0.0.1:39547/auth/callback?error=denied"),
            Err(RedirectRefused::Incomplete)
        );
        assert_eq!(paste("c1"), Err(RedirectRefused::NotAnAddress));
        // A link that sends the browser elsewhere waits on no address here.
        let remote =
            SignInLink::find("https://a.example/x?redirect_uri=https%3A%2F%2Fb.example%2Fcb")
                .unwrap();
        assert!(remote.redirect().is_none());
    }

    #[test]
    fn each_method_has_its_console_and_flow_and_only_offered_ones_sign_in() {
        let plan = |harness: Harness, method| {
            harness
                .sign_in(method)
                .map(|plan| (plan.console, plan.flow))
        };
        assert_eq!(
            plan(Harness::ClaudeCode, SignInMethod::AnthropicConsole),
            Ok((Console::Pipes, SignInFlow::PasteCode))
        );
        assert_eq!(
            plan(Harness::Codex, SignInMethod::Chatgpt),
            Ok((Console::Pipes, SignInFlow::DeviceCode))
        );
        assert_eq!(
            plan(Harness::Pi, SignInMethod::Chatgpt),
            Ok((Console::Terminal, SignInFlow::DeviceCode))
        );
        assert_eq!(
            plan(Harness::Pi, SignInMethod::AnthropicApiKey),
            Ok((Console::Terminal, SignInFlow::EnterKey))
        );
        assert_eq!(
            plan(Harness::OpenCode, SignInMethod::Chatgpt),
            Ok((Console::Pipes, SignInFlow::PasteRedirect))
        );
        assert_eq!(
            plan(Harness::OpenCode, SignInMethod::OpenaiApiKey),
            Ok((Console::Terminal, SignInFlow::EnterKey))
        );
        // No Claude subscription in a third-party client.
        assert!(plan(Harness::Pi, SignInMethod::ClaudeSubscription).is_err());
        assert!(plan(Harness::OpenCode, SignInMethod::ClaudeSubscription).is_err());
        assert!(plan(Harness::Codex, SignInMethod::AnthropicApiKey).is_err());
        for harness in Harness::ALL {
            for method in harness.sign_in_methods() {
                assert!(harness.sign_in(*method).is_ok(), "{harness} {method}");
            }
            assert!(!harness.sign_in_status().is_empty(), "{harness}");
        }
    }
}
