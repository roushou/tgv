//! Pi's own sign-in commands. Pi signs in per provider: `pi auth check
//! --provider <p> --json` says whether one is ready (never with
//! `--credentials`, so no credential is printed), and `pi auth login <p>`
//! signs one in, by device code (`--no-browser`) or by an API key
//! (`--api-key`). Its login needs a terminal.

use crate::{
    Harness,
    launch::LaunchPlan,
    sign_in::{Standing, StatedMethod, StatusUnread},
};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Reads what Pi's sign-in commands print.
pub struct PiAuth;

/// The fields of `pi auth check --json` Ren reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Check {
    status: String,
    provider: String,
    auth_type: Option<String>,
}

impl PiAuth {
    /// The providers Ren routes for Pi, whose sign-in is checked.
    const PROVIDERS: [&'static str; 3] = ["openai-codex", "anthropic", "openai"];

    /// `pi auth check --provider <p> --json` for each provider Ren routes.
    pub(crate) fn checks() -> Vec<LaunchPlan> {
        Self::PROVIDERS
            .iter()
            .map(|provider| {
                LaunchPlan::pi_auth(&["auth", "check", "--provider", provider, "--json"])
            })
            .collect()
    }

    /// Signed in when any provider is ready, naming those that are; signed
    /// out when every check was read and none is.
    pub(crate) fn read_checks(outputs: &[Vec<u8>]) -> Result<Standing, StatusUnread> {
        let mut ready = Vec::new();
        let mut unread = None;
        for output in outputs {
            let text = String::from_utf8_lossy(output);
            let check = text
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with('{'))
                .find_map(|line| serde_json::from_str::<Check>(line).ok());
            match check {
                Some(check) if check.status == "ready" => ready.push(match check.auth_type {
                    Some(kind) => format!("{} ({})", check.provider, kind.replace('_', " ")),
                    None => check.provider,
                }),
                Some(_) => {}
                None => unread = Some(StatusUnread("cannot read `pi auth check`".into())),
            }
        }
        match (ready.is_empty(), unread) {
            (false, _) => Ok(Standing::SignedIn {
                method: StatedMethod::new(&ready.join(", ")),
            }),
            (true, Some(unread)) => Err(unread),
            (true, None) => Ok(Standing::SignedOut),
        }
    }
}

impl LaunchPlan {
    /// `pi auth login <provider> <how>`.
    pub(crate) fn pi_auth_login(provider: &str, how: &str) -> Self {
        Self::pi_auth(&["auth", "login", provider, how])
    }

    fn pi_auth(arguments: &[&str]) -> Self {
        Self {
            harness: Harness::Pi,
            executable: "pi",
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
    use super::PiAuth;
    use crate::sign_in::Standing;

    #[test]
    fn signed_in_when_any_routed_provider_is_ready() {
        let out = |text: &str| text.as_bytes().to_vec();
        let none = [
            out(
                r#"{"status":"not_ready","provider":"openai-codex","reason":"credentials_not_configured"}"#,
            ),
            out(
                r#"{"status":"not_ready","provider":"anthropic","reason":"credentials_not_configured"}"#,
            ),
            out(r#"{"status":"invalid","provider":"openai"}"#),
        ];
        assert_eq!(PiAuth::read_checks(&none), Ok(Standing::SignedOut));
        let some = [
            out(r#"{"status":"ready","provider":"openai-codex","authType":"oauth"}"#),
            out(r#"{"status":"ready","provider":"anthropic","authType":"api_key"}"#),
            out("not json"),
        ];
        let Ok(Standing::SignedIn { method }) = PiAuth::read_checks(&some) else {
            panic!("not signed in");
        };
        assert_eq!(
            method.to_string(),
            "openai-codex (oauth), anthropic (api key)"
        );
        assert!(PiAuth::read_checks(&[out("boom")]).is_err());
        assert_eq!(PiAuth::checks().len(), 3);
    }
}
