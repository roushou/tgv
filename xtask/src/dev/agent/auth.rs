//! A fake `claude auth` and `codex login` for the dev slot, so the sign-in
//! sheet can be tried with no real account. `claude auth status --json` says
//! whether a fake login took its code, and `claude auth login` prints Claude
//! Code's lines with an invented link, reads the code, and signs in only for
//! [`FakeClaudeAuth::CODE`]. `codex login status` says whether a fake device
//! login finished, and `codex login --device-auth` prints Codex's lines with
//! an invented link and code and "approves" itself after
//! [`FakeCodexAuth::APPROVED_AFTER`]. Both start signed in, so the slot's
//! sessions work; a `claude-signed-out` or `codex-signed-out` file in the
//! slot's data directory (or the fake `logout`) signs one out, to try its
//! sign-in, and a fake login removes it. Neither keeps a credential.

use super::AgentError;
use std::{
    io::{BufRead as _, Write as _},
    path::Path,
};

pub struct FakeClaudeAuth;

impl FakeClaudeAuth {
    /// The code the fake sign-in page "shows".
    pub const CODE: &'static str = "acme";
    const LINK: &'static str = "https://login.example.test/claude/authorize?state=dev-slot";

    /// Signed in: the signed-out marker removed.
    fn signed_in(marker: &Path) -> Result<(), AgentError> {
        match std::fs::remove_file(marker) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AgentError::Stdio(error)),
        }
    }

    /// Whether `arguments` (the daemon's, for `claude`) ask for `auth`.
    pub fn asked(arguments: &[String]) -> bool {
        arguments.first().is_some_and(|first| first == "auth")
    }

    pub fn run(data_dir: &Path, arguments: &[String]) -> Result<(), AgentError> {
        let marker = data_dir.join("claude-signed-out");
        let mut out = std::io::stdout().lock();
        match arguments.get(1).map(String::as_str) {
            Some("status") => {
                let signed_in = !marker.exists();
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
                Self::signed_in(&marker)?;
                writeln!(out, "Login successful.").map_err(AgentError::Stdio)
            }
            Some("logout") => std::fs::write(&marker, b"").map_err(AgentError::Stdio),
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
        let marker = data_dir.join("codex-signed-out");
        let mut err = std::io::stderr().lock();
        match arguments.get(1).map(String::as_str) {
            Some("status") => {
                if !marker.exists() {
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
                FakeClaudeAuth::signed_in(&marker)?;
                writeln!(out, "Successfully logged in").map_err(AgentError::Stdio)
            }
            Some("logout") => std::fs::write(&marker, b"").map_err(AgentError::Stdio),
            Some(_) | None => Err(AgentError::NoSession),
        }
    }
}
