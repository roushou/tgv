//! A fake `claude auth` for the dev slot, so the sign-in sheet can be tried
//! with no real account: `auth status --json` says whether a fake login took
//! its code, and `auth login` prints Claude Code's lines with an invented
//! link, reads the code, and signs in only for [`FakeClaudeAuth::CODE`]. What
//! it keeps is a marker file in the slot's data directory, never a
//! credential.

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

    /// Whether `arguments` (the daemon's, for `claude`) ask for `auth`.
    pub fn asked(arguments: &[String]) -> bool {
        arguments.first().is_some_and(|first| first == "auth")
    }

    pub fn run(data_dir: &Path, arguments: &[String]) -> Result<(), AgentError> {
        let marker = data_dir.join("claude-signed-in");
        let mut out = std::io::stdout().lock();
        match arguments.get(1).map(String::as_str) {
            Some("status") => {
                let signed_in = marker.exists();
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
                std::fs::write(&marker, b"fake\n").map_err(AgentError::Stdio)?;
                writeln!(out, "Login successful.").map_err(AgentError::Stdio)
            }
            Some("logout") => match std::fs::remove_file(&marker) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(AgentError::Stdio(error)),
            },
            Some(_) | None => Err(AgentError::NoSession),
        }
    }
}
