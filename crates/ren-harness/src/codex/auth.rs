//! Codex's own sign-in commands: `codex login status`, which says on stderr
//! whether and how it is signed in, and `codex login --device-auth`, which
//! prints a link and a one-time code and finishes once the operator
//! approves on that page.

use crate::{
    Harness,
    launch::LaunchPlan,
    sign_in::{LoginReader, Standing, StatedMethod, StatusUnread},
};
use std::collections::BTreeMap;

/// Reads what Codex's sign-in commands print.
pub struct CodexAuth;

impl CodexAuth {
    const SIGNED_IN: &'static str = "Logged in using ";
    const SIGNED_OUT: &'static str = "Not logged in";

    /// Read `codex login status`, stdout and stderr together. How it is
    /// signed in is kept only as its kind: what follows it on the line (an
    /// API key's masked text) is never kept.
    pub(crate) fn read_status(output: &[u8]) -> Result<Standing, StatusUnread> {
        let text = String::from_utf8_lossy(output);
        for line in text.lines().map(LoginReader::plain) {
            let line = line.trim();
            if line.starts_with(Self::SIGNED_OUT) {
                return Ok(Standing::SignedOut);
            }
            if let Some(how) = line.strip_prefix(Self::SIGNED_IN) {
                let method = if how.contains("API key") {
                    "API key"
                } else {
                    how.split(" - ").next().unwrap_or(how).trim()
                };
                return Ok(Standing::SignedIn {
                    method: StatedMethod::new(method),
                });
            }
        }
        Err(StatusUnread(
            "`codex login status` said neither signed in nor out".into(),
        ))
    }
}

impl LaunchPlan {
    /// `codex login status`.
    pub(crate) fn codex_login_status() -> Self {
        Self::codex_login(&["login", "status"])
    }

    /// `codex login --device-auth`: a ChatGPT sign-in by device code.
    pub(crate) fn codex_device_login() -> Self {
        Self::codex_login(&["login", "--device-auth"])
    }

    fn codex_login(arguments: &[&str]) -> Self {
        Self {
            harness: Harness::Codex,
            executable: "codex",
            arguments: arguments
                .iter()
                .map(|argument| (*argument).into())
                .collect(),
            environment: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CodexAuth;
    use crate::sign_in::Standing;

    #[test]
    fn status_reads_how_it_is_signed_in_and_never_a_keys_text() {
        assert_eq!(
            CodexAuth::read_status(b"WARNING: something\nNot logged in\n"),
            Ok(Standing::SignedOut)
        );
        let Ok(Standing::SignedIn { method }) =
            CodexAuth::read_status(b"Logged in using ChatGPT\n")
        else {
            panic!("not signed in");
        };
        assert_eq!(method.to_string(), "ChatGPT");
        // An invented masked key: only its kind is kept.
        let Ok(Standing::SignedIn { method }) =
            CodexAuth::read_status(b"Logged in using an API key - sk-acme***1234\n")
        else {
            panic!("not signed in");
        };
        assert_eq!(method.to_string(), "API key");
        assert!(CodexAuth::read_status(b"").is_err());
    }
}
