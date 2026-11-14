//! A client's sign-in on this host, under
//! `/control/v1/runs/clients/{harness}/sign-in`: checking it with the
//! client's own status command, and running the client's own login, whose
//! link this answers with and whose code it hands back. The code goes only
//! to the login's stdin: it is never logged, recorded, or answered with.
//! Access is the control API's, as for the other run routes.

use super::{
    ControlApi, Refusal,
    runs::{Denied, RunRefusal, RunsApi},
};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    response::{IntoResponse, Response},
};
use http::HeaderMap;
use ren_harness::{
    Harness,
    sign_in::{SignInCode, SignInMethod, SignInRefused},
};
use ren_supervisor::SignInError;
use std::sync::Arc;

/// Starting a sign-in: how, or the client's usual way.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StartRequest {
    #[serde(default)]
    method: Option<SignInMethod>,
}

/// The code the sign-in page showed. Not `Debug`: it is a secret.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CodeRequest {
    code: String,
}

type Api = State<Arc<RunsApi>>;

impl RunsApi {
    fn harness(harness: &str) -> Result<Harness, Denied> {
        harness
            .parse::<Harness>()
            .map_err(|_| Denied::Run(RunRefusal::NotFound))
    }

    fn sign_in_refused(error: SignInError) -> Denied {
        Denied::Run(match error {
            SignInError::Refused(SignInRefused::NotOffered(_)) => {
                RunRefusal::Refused("sign_in_not_offered")
            }
            SignInError::Refused(SignInRefused::NoSuchMethod { .. }) => {
                return Denied::Control(Refusal::Invalid);
            }
            SignInError::NotInstalled(_) => RunRefusal::Refused("not_installed"),
            SignInError::NoRoot => RunRefusal::Disabled,
            SignInError::Spawn => RunRefusal::SpawnFailed,
            SignInError::NoLogin(_) => RunRefusal::Refused("no_sign_in"),
            SignInError::NotAwaitingCode => RunRefusal::Refused("not_awaiting_code"),
            SignInError::Gone => RunRefusal::Unavailable,
        })
    }

    /// `POST …/sign-in/check`: run the client's status command now.
    pub(super) async fn check_sign_in(
        State(api): Api,
        headers: HeaderMap,
        Path(harness): Path<String>,
    ) -> Result<Response, Denied> {
        let supervisor = api.admit(&headers)?;
        let harness = Self::harness(&harness)?;
        let state = supervisor
            .check_sign_in(harness)
            .await
            .map_err(Self::sign_in_refused)?;
        Ok(Json(serde_json::json!({
            "api_version": ControlApi::API_VERSION,
            "harness": harness,
            "sign_in": state,
        }))
        .into_response())
    }

    /// `POST …/sign-in`: start the client's login, answering with its link
    /// once it prints one.
    pub(super) async fn start_sign_in(
        State(api): Api,
        headers: HeaderMap,
        Path(harness): Path<String>,
        body: Result<Json<StartRequest>, JsonRejection>,
    ) -> Result<Response, Denied> {
        let supervisor = api.admit(&headers)?;
        let harness = Self::harness(&harness)?;
        let requested = Self::body(body)?.method;
        let method = match requested.or_else(|| harness.sign_in_methods().first().copied()) {
            Some(method) => method,
            None => {
                return Err(Self::sign_in_refused(SignInError::Refused(
                    SignInRefused::NotOffered(harness),
                )));
            }
        };
        let login = supervisor
            .sign_in(harness, method)
            .await
            .map_err(Self::sign_in_refused)?;
        Ok(Json(serde_json::json!({
            "api_version": ControlApi::API_VERSION,
            "harness": harness,
            "method": method,
            "login": login,
        }))
        .into_response())
    }

    /// `POST …/sign-in/code`: hand the page's code to the running login,
    /// answering once it has ended and the client's sign-in was checked.
    pub(super) async fn sign_in_code(
        State(api): Api,
        headers: HeaderMap,
        Path(harness): Path<String>,
        body: Result<Json<CodeRequest>, JsonRejection>,
    ) -> Result<Response, Denied> {
        let supervisor = api.admit(&headers)?;
        let harness = Self::harness(&harness)?;
        let code = SignInCode::parse(&Self::body(body)?.code)
            .map_err(|_| Denied::Control(Refusal::Invalid))?;
        let outcome = supervisor
            .sign_in_code(harness, code)
            .await
            .map_err(Self::sign_in_refused)?;
        Ok(Json(serde_json::json!({
            "api_version": ControlApi::API_VERSION,
            "harness": harness,
            "outcome": outcome,
        }))
        .into_response())
    }

    /// `POST …/sign-in/cancel`: stop the running login.
    pub(super) async fn cancel_sign_in(
        State(api): Api,
        headers: HeaderMap,
        Path(harness): Path<String>,
    ) -> Result<Response, Denied> {
        let supervisor = api.admit(&headers)?;
        let harness = Self::harness(&harness)?;
        supervisor
            .cancel_sign_in(harness)
            .await
            .map_err(Self::sign_in_refused)?;
        Ok(
            Json(serde_json::json!({ "api_version": ControlApi::API_VERSION, "ok": true }))
                .into_response(),
        )
    }
}
