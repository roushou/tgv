//! A fake `claude auth`, `codex login`, `pi auth`, and `opencode auth` for
//! the dev slot, so the sign-in
//! sheet can be tried with no real account. `claude auth status --json` says
//! whether a fake login took its code, and `claude auth login` prints Claude
//! Code's lines with an invented link, reads the code, and signs in only for
//! [`FakeClaudeAuth::CODE`]. `codex login status` says whether a fake device
//! login finished, and `codex login --device-auth` prints Codex's lines with
//! an invented link and code and "approves" itself after
//! [`FakeCodexAuth::APPROVED_AFTER`]. Both start signed in, so the slot's
//! sessions work; a `claude-signed-out` or `codex-signed-out` file in the
//! slot's data directory (or the fake `logout`) signs one out, to try its
//! sign-in, and a fake login removes it. The fake `pi auth` and `opencode
//! auth` ([`FakePiAuth`], [`FakeOpenCodeAuth`]) work the same way with
//! `pi-signed-out` and `opencode-signed-out`. None keeps a credential.

use super::AgentError;
use std::{
    io::{BufRead as _, Write as _},
    path::Path,
};

/// A fake client's signed-out marker: a `<client>-signed-out` file in the
/// slot's data directory, there while it is signed out.
struct SignedOut(std::path::PathBuf);

impl SignedOut {
    fn of(data_dir: &Path, client: &str) -> Self {
        Self(data_dir.join(format!("{client}-signed-out")))
    }

    fn signed_out(&self) -> bool {
        self.0.exists()
    }

    fn sign_in(&self) -> Result<(), AgentError> {
        match std::fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AgentError::Stdio(error)),
        }
    }

    fn sign_out(&self) -> Result<(), AgentError> {
        std::fs::write(&self.0, b"").map_err(AgentError::Stdio)
    }
}

pub struct FakeClaudeAuth;

impl FakeClaudeAuth {
    /// The code the fake sign-in page "shows".
    pub const CODE: &'static str = "acme";
    const LINK: &'static str = "https://login.example.test/claude/authorize?state=dev-slot";

    /// Whether `arguments` (the daemon's, for `claude`) ask for `auth`.
    pub fn asked(arguments: &[String]) -> bool {
        arguments.first().is_some_and(|first| first == "auth")
    }

    pub fn run(data_dir: &Path, arguments: &[String]) -> Result<(), AgentError> {
        let marker = SignedOut::of(data_dir, "claude");
        let mut out = std::io::stdout().lock();
        match arguments.get(1).map(String::as_str) {
            Some("status") => {
                let signed_in = !marker.signed_out();
                let status = serde_json::json!({
                    "loggedIn": signed_in,
                    "authMethod": if signed_in { "claude.ai" } else { "none" },
                });
                writeln!(out, "{status}").map_err(AgentError::Stdio)
            }
            Some("login") => {
                write!(
                    out,
                    "Opening browser to sign in…\nIf the browser didn't open, visit: {}\nPaste code here if prompted > ",
                    Self::LINK
                )
                .and_then(|()| out.flush())
                .map_err(AgentError::Stdio)?;
                let mut code = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut code)
                    .map_err(AgentError::Stdio)?;
                if code.trim() != Self::CODE {
                    return Err(AgentError::LoginRefused);
                }
                marker.sign_in()?;
                writeln!(out, "Login successful.").map_err(AgentError::Stdio)
            }
            Some("logout") => marker.sign_out(),
            Some(_) | None => Err(AgentError::NoSession),
        }
    }
}

pub struct FakeCodexAuth;

impl FakeCodexAuth {
    /// How long the fake page takes to "approve": long enough to see the
    /// sheet wait.
    pub const APPROVED_AFTER: std::time::Duration = std::time::Duration::from_secs(8);
    const LINK: &'static str = "https://login.example.test/codex/device";
    const CODE: &'static str = "ACME-12345";

    /// Whether `arguments` (the daemon's, for `codex`) ask for `login`.
    pub fn asked(arguments: &[String]) -> bool {
        arguments.first().is_some_and(|first| first == "login")
    }

    pub fn run(data_dir: &Path, arguments: &[String]) -> Result<(), AgentError> {
        let marker = SignedOut::of(data_dir, "codex");
        let mut err = std::io::stderr().lock();
        match arguments.get(1).map(String::as_str) {
            Some("status") => {
                if !marker.signed_out() {
                    writeln!(err, "Logged in using ChatGPT").map_err(AgentError::Stdio)
                } else {
                    writeln!(err, "Not logged in").map_err(AgentError::Stdio)?;
                    Err(AgentError::SignedOut)
                }
            }
            Some("--device-auth") => {
                let mut out = std::io::stdout().lock();
                write!(
                    out,
                    "\nWelcome to Codex [v\x1b[90mdev\x1b[0m]\n\nFollow these steps to sign in with ChatGPT using device code authorization:\n\n1. Open this link in your browser and sign in to your account\n   \x1b[94m{}\x1b[0m\n\n2. Enter this one-time code \x1b[90m(expires in 15 minutes)\x1b[0m\n   \x1b[94m{}\x1b[0m\n\n",
                    Self::LINK,
                    Self::CODE
                )
                .and_then(|()| out.flush())
                .map_err(AgentError::Stdio)?;
                std::thread::sleep(Self::APPROVED_AFTER);
                marker.sign_in()?;
                writeln!(out, "Successfully logged in").map_err(AgentError::Stdio)
            }
            Some("logout") => marker.sign_out(),
            Some(_) | None => Err(AgentError::NoSession),
        }
    }
}

/// A fake `pi auth`: `check` says its ChatGPT provider is ready unless the
/// `pi-signed-out` marker is there; `login openai-codex --no-browser` shows
/// an invented link and code and "approves" after
/// [`FakeCodexAuth::APPROVED_AFTER`]; `login <provider> --api-key` asks for
/// a key and saves nothing.
pub struct FakePiAuth;

impl FakePiAuth {
    pub fn asked(arguments: &[String]) -> bool {
        arguments.first().is_some_and(|first| first == "auth")
    }

    pub fn run(data_dir: &Path, arguments: &[String]) -> Result<(), AgentError> {
        let marker = SignedOut::of(data_dir, "pi");
        let mut out = std::io::stdout().lock();
        let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["auth", "check", "--provider", provider, ..] => {
                let ready = *provider == "openai-codex" && !marker.signed_out();
                let status = if ready {
                    serde_json::json!({ "status": "ready", "provider": provider, "authType": "oauth" })
                } else {
                    serde_json::json!({ "status": "not_ready", "provider": provider,
                                        "reason": "credentials_not_configured" })
                };
                writeln!(out, "{status}").map_err(AgentError::Stdio)?;
                if ready {
                    Ok(())
                } else {
                    Err(AgentError::SignedOut)
                }
            }
            ["auth", "login", _, "--no-browser"] => {
                write!(
                    out,
                    "Select OpenAI Codex login method: Device code login (headless)\r\nOpen https://login.example.test/pi/device and enter code: ACME-5678\r\nWaiting for authentication...\r\n"
                )
                .and_then(|()| out.flush())
                .map_err(AgentError::Stdio)?;
                std::thread::sleep(FakeCodexAuth::APPROVED_AFTER);
                marker.sign_in()?;
                writeln!(out, "Logged in to ChatGPT.\r").map_err(AgentError::Stdio)
            }
            ["auth", "login", provider, "--api-key"] => {
                write!(out, "Enter {provider} API key: ")
                    .and_then(|()| out.flush())
                    .map_err(AgentError::Stdio)?;
                let mut key = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut key)
                    .map_err(AgentError::Stdio)?;
                marker.sign_in()?;
                writeln!(out, "\r\nSaved API key for {provider}.\r").map_err(AgentError::Stdio)
            }
            ["auth", "logout", ..] => marker.sign_out(),
            _ => Err(AgentError::NoSession),
        }
    }
}

/// A fake `opencode auth`: `list` holds an OpenAI integration unless the
/// `opencode-signed-out` marker is there; `login openai --method
/// chatgpt-token-sharing` listens on a loopback port, prints a link that
/// sends there, and finishes once that address is opened (as the daemon
/// does with the address the operator pastes); `--method key` asks for a
/// key and saves nothing.
pub struct FakeOpenCodeAuth;

impl FakeOpenCodeAuth {
    pub fn asked(arguments: &[String]) -> bool {
        arguments.first().is_some_and(|first| first == "auth")
    }

    pub fn run(data_dir: &Path, arguments: &[String]) -> Result<(), AgentError> {
        let marker = SignedOut::of(data_dir, "opencode");
        let mut out = std::io::stdout().lock();
        let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["auth", "list", ..] => if marker.signed_out() {
                writeln!(out, "No authenticated integrations")
            } else {
                writeln!(out, "OpenAI  ChatGPT   stored")
            }
            .map_err(AgentError::Stdio),
            ["auth", "login", _, .., "chatgpt-token-sharing"] => {
                let listener =
                    std::net::TcpListener::bind("127.0.0.1:0").map_err(AgentError::Stdio)?;
                let port = listener.local_addr().map_err(AgentError::Stdio)?.port();
                write!(
                    out,
                    "│\n●  Complete authorization in your browser.\n│\n●  https://login.example.test/opencode/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Fauth%2Fcallback&state=dev\n│\n◇  Waiting for authorization...\n"
                )
                .and_then(|()| out.flush())
                .map_err(AgentError::Stdio)?;
                let (mut stream, _) = listener.accept().map_err(AgentError::Stdio)?;
                let mut request = [0; 2048];
                let read =
                    std::io::Read::read(&mut stream, &mut request).map_err(AgentError::Stdio)?;
                let line =
                    String::from_utf8_lossy(request.get(..read).unwrap_or_default()).into_owned();
                let approved = line.starts_with("GET /auth/callback?") && line.contains("code=");
                let body = if approved {
                    "Signed in."
                } else {
                    "Not signed in."
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .map_err(AgentError::Stdio)?;
                if !approved {
                    return Err(AgentError::LoginRefused);
                }
                marker.sign_in()?;
                writeln!(out, "◇  Connected to OpenAI\n└  Done").map_err(AgentError::Stdio)
            }
            ["auth", "login", provider, .., "key"] => {
                write!(out, "◆  Enter your {provider} API key\n│  ")
                    .and_then(|()| out.flush())
                    .map_err(AgentError::Stdio)?;
                let mut key = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut key)
                    .map_err(AgentError::Stdio)?;
                marker.sign_in()?;
                writeln!(out, "\r\n◇  Connected to {provider}\r\n└  Done\r")
                    .map_err(AgentError::Stdio)
            }
            ["auth", "logout", ..] => marker.sign_out(),
            _ => Err(AgentError::NoSession),
        }
    }
}
