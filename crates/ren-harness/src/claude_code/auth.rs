//! Claude Code's own sign-in commands: `claude auth status --json`, which
//! says whether it is signed in, and `claude auth login`, which prints a
//! link and reads back the code the page shows.

use crate::{
    Harness,
    launch::LaunchPlan,
    sign_in::{Standing, StatedMethod, StatusUnread},
};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Reads what Claude Code's sign-in commands print.
pub struct ClaudeAuth;

/// The fields of `claude auth status --json` Ren reads. Others (account
/// details among them) are not read.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    logged_in: bool,
    auth_method: Option<String>,
}

impl ClaudeAuth {
    /// What `claude auth login` prints, with no newline, before reading the
    /// code.
    pub(crate) const PROMPT: &'static str = "Paste code here if prompted >";

    pub(crate) fn read_status(output: &[u8]) -> Result<Standing, StatusUnread> {
        // Read with stderr: the JSON object is what lies between its braces.
        let text = String::from_utf8_lossy(output);
        let object = text
            .find('{')
            .zip(text.rfind('}'))
            .and_then(|(start, end)| text.get(start..=end))
            .unwrap_or(&text);
        let status: Status = serde_json::from_str(object)
            .map_err(|error| StatusUnread(format!("cannot read `claude auth status`: {error}")))?;
        Ok(match (status.logged_in, status.auth_method) {
            (false, _) => Standing::SignedOut,
            (true, method) => Standing::SignedIn {
                method: StatedMethod::new(method.as_deref().unwrap_or("signed in")),
            },
        })
    }
}

impl LaunchPlan {
    /// `claude auth status --json`.
    pub(crate) fn claude_auth_status() -> Self {
        Self::claude_auth(&["auth", "status", "--json"])
    }

    /// `claude auth login`, with `account` (`--claudeai` or `--console`).
    pub(crate) fn claude_auth_login(account: &str) -> Self {
        Self::claude_auth(&["auth", "login", account])
    }

    fn claude_auth(arguments: &[&str]) -> Self {
        Self {
            harness: Harness::ClaudeCode,
            executable: "claude",
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
    use super::ClaudeAuth;
    use crate::sign_in::{SignInMethod, Standing};
    use crate::{Harness, launch::LaunchArg};

    #[test]
    fn status_reads_signed_out_and_signed_in() {
        // As Claude Code prints it signed out (paths are illustrative).
        let out = br#"{"loggedIn": false, "authMethod": "none", "apiProvider": "firstParty",
            "configDirectory": "/home/acme/.claude", "loginExpiresAt": null}"#;
        assert_eq!(ClaudeAuth::read_status(out), Ok(Standing::SignedOut));
        let signed_in =
            br#"{"loggedIn": true, "authMethod": "claude.ai", "email": "dev@acme.test"}"#;
        let Ok(Standing::SignedIn { method }) = ClaudeAuth::read_status(signed_in) else {
            panic!("not signed in");
        };
        assert_eq!(method.to_string(), "claude.ai");
        assert!(ClaudeAuth::read_status(b"Not logged in.").is_err());
        // A warning on stderr around it does not hide it.
        let warned = b"Warning: something\n{\"loggedIn\": false}\n";
        assert_eq!(ClaudeAuth::read_status(warned), Ok(Standing::SignedOut));
    }

    #[test]
    fn plans_run_claudes_own_auth_commands() {
        let words = |plan: crate::launch::LaunchPlan| {
            assert_eq!(plan.executable, "claude");
            assert!(plan.environment.is_empty());
            plan.arguments
                .into_iter()
                .map(|argument| match argument {
                    LaunchArg::Literal(text) => text,
                    LaunchArg::PiExtension => panic!("no extension"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            words(Harness::ClaudeCode.sign_in_status().unwrap()),
            "auth status --json"
        );
        assert_eq!(
            words(
                Harness::ClaudeCode
                    .sign_in(SignInMethod::ClaudeSubscription)
                    .unwrap()
            ),
            "auth login --claudeai"
        );
        assert_eq!(
            words(
                Harness::ClaudeCode
                    .sign_in(SignInMethod::AnthropicConsole)
                    .unwrap()
            ),
            "auth login --console"
        );
    }
}
