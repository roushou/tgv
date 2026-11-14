//! OpenCode's own sign-in commands, each run with `--standalone` so no
//! background service is left running: `opencode auth list`, which names the
//! integrations it holds credentials for, and `opencode auth login
//! <provider> --method <method>`: OpenAI's token sharing (a link whose
//! approval ends on a `127.0.0.1` address on the host) or an API key, which
//! needs a terminal.

use crate::{
    Harness,
    launch::LaunchPlan,
    sign_in::{LoginReader, Standing, StatedMethod, StatusUnread},
};
use std::collections::BTreeMap;

/// Reads what OpenCode's sign-in commands print.
pub struct OpenCodeAuth;

impl OpenCodeAuth {
    const NONE: &'static str = "No authenticated integrations";

    /// Read `opencode auth list`: one line per integration, its name first,
    /// columns apart by runs of spaces.
    pub(crate) fn read_list(output: &[u8]) -> Result<Standing, StatusUnread> {
        let text = String::from_utf8_lossy(output);
        let mut names = Vec::new();
        for line in text.lines().map(LoginReader::plain) {
            let line = line.trim();
            if line.starts_with(Self::NONE) {
                return Ok(Standing::SignedOut);
            }
            let mut columns = line
                .split("  ")
                .map(str::trim)
                .filter(|column| !column.is_empty());
            if let (Some(name), Some(_)) = (columns.next(), columns.next()) {
                names.push(name.to_owned());
            }
        }
        if names.is_empty() {
            return Err(StatusUnread("cannot read `opencode auth list`".into()));
        }
        Ok(Standing::SignedIn {
            method: StatedMethod::new(&names.join(", ")),
        })
    }
}

impl LaunchPlan {
    /// `opencode auth list --standalone`.
    pub(crate) fn opencode_auth_list() -> Self {
        Self::opencode_auth(&["auth", "list", "--standalone"])
    }

    /// `opencode auth login <provider> --standalone --method <method>`.
    pub(crate) fn opencode_auth_login(provider: &str, method: &str) -> Self {
        Self::opencode_auth(&[
            "auth",
            "login",
            provider,
            "--standalone",
            "--method",
            method,
        ])
    }

    fn opencode_auth(arguments: &[&str]) -> Self {
        Self {
            harness: Harness::OpenCode,
            executable: "opencode",
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
    use super::OpenCodeAuth;
    use crate::sign_in::Standing;

    #[test]
    fn the_list_names_its_integrations() {
        assert_eq!(
            OpenCodeAuth::read_list(b"No authenticated integrations\n"),
            Ok(Standing::SignedOut)
        );
        let Ok(Standing::SignedIn { method }) = OpenCodeAuth::read_list(
            b"Anthropic  Anthropic                   stored\nOpenAI  ChatGPT   stored\n",
        ) else {
            panic!("not signed in");
        };
        assert_eq!(method.to_string(), "Anthropic, OpenAI");
        assert!(OpenCodeAuth::read_list(b"").is_err());
    }
}
