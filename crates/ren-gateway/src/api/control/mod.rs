//! The control API, under `/control/v1`: the only mutating endpoints.
//!
//! Access:
//! - a request carrying `Origin` or `Sec-Fetch-Site` came from a browser and
//!   is refused, so a web page cannot drive the API (CSRF);
//! - a request with `Tailscale-User-Login` came through Tailscale Serve, which
//!   strips client-supplied identity headers; its login must be listed in
//!   `control.tailscale_logins`;
//! - any other request must carry the daemon's control token as a bearer
//!   token. The token is written to the data directory (mode 0600) at start;
//!   the CLI reads it there. Loopback alone is not trusted, because Serve
//!   forwards tagged devices and Funnel traffic to loopback without identity.

mod history;
mod runs;
mod sessions;
mod sign_in;
mod tools;

use crate::control::{ControlError, Controls, RunningCall};
use crate::operator::{OperatorCommand, OperatorState, RetryAfter};
use crate::user::Users;
use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use history::HistoryApi;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use ren_protocol::native::NativeSessionId;
use ren_recorder::{
    Recorder,
    batch::Actor,
    caller::UserId,
    id::{CallId, IdSource},
    time::Clock,
};
use ren_store::backup::{Backup, BackupError};
use ren_supervisor::Supervisor;
use runs::RunsApi;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The `[control]` configuration section.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControlOptions {
    /// Tailscale logins (as Tailscale Serve reports them in
    /// `Tailscale-User-Login`) allowed to use the control API. Empty means
    /// the control API answers loopback requests only.
    pub tailscale_logins: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("control.tailscale_logins entries must be 1 to 256 visible characters")]
pub struct ControlOptionsError;

impl ControlOptions {
    pub fn validate(&self) -> Result<(), ControlOptionsError> {
        let valid = |login: &String| {
            (1..=256).contains(&login.len()) && login.bytes().all(|byte| byte.is_ascii_graphic())
        };
        if self.tailscale_logins.iter().all(valid) {
            Ok(())
        } else {
            Err(ControlOptionsError)
        }
    }
}

/// A per-run secret that authorizes control requests without a Tailscale
/// identity. Never shown in `Debug`.
#[derive(Clone)]
pub struct ControlToken(Box<str>);

impl ControlToken {
    pub const FILE: &'static str = "control.token";

    pub fn generate<S: IdSource + ?Sized>(ids: &S) -> Self {
        Self(format!("{}{}", ids.next_uuid().simple(), ids.next_uuid().simple()).into())
    }

    /// A token read back from the token file.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_alphanumeric()))
            .then(|| Self(text.into()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison.
    fn matches(&self, candidate: &[u8]) -> bool {
        let expected = self.0.as_bytes();
        expected.len() == candidate.len()
            && expected
                .iter()
                .zip(candidate)
                .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
                == 0
    }
}

impl std::fmt::Debug for ControlToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlToken(<redacted>)")
    }
}

/// The control API's state and capabilities.
pub(crate) struct ControlApi {
    controls: Arc<Controls>,
    recorder: Recorder,
    clock: Arc<dyn Clock>,
    token: ControlToken,
    logins: Vec<String>,
    /// The named users an operator can block.
    users: Users,
    backup: Backup,
}

#[derive(serde::Serialize)]
struct StateBody<'a> {
    api_version: u32,
    state: &'a OperatorState,
    running: Vec<RunningCall>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PauseRequest {
    retry_after_seconds: u32,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct BlockRequest {
    /// How long the block lasts; absent means until unblocked.
    duration_seconds: Option<u64>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct CancelRequest {
    /// Also block the call's session this long, so its client does not retry
    /// at once.
    block_session_seconds: Option<u64>,
}

/// Why a control request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    CrossSite,
    Unauthorized,
    Forbidden,
    Invalid,
    NotFound,
    Control(ControlError),
    BackupRunning,
    BackupIncomplete,
    BackupFailed,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::CrossSite => (StatusCode::FORBIDDEN, "cross_site"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Invalid => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::Control(ControlError::NotRunning) => (StatusCode::NOT_FOUND, "not_running"),
            Self::Control(ControlError::Unrecorded) => {
                (StatusCode::SERVICE_UNAVAILABLE, "unrecorded")
            }
            Self::Control(ControlError::Unrecordable) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "unrecordable")
            }
            Self::BackupRunning => (StatusCode::CONFLICT, "backup_running"),
            Self::BackupIncomplete => (StatusCode::SERVICE_UNAVAILABLE, "backup_incomplete"),
            Self::BackupFailed => (StatusCode::INTERNAL_SERVER_ERROR, "backup_failed"),
        };
        (
            status,
            Json(serde_json::json!({ "api_version": ControlApi::API_VERSION, "error": code })),
        )
            .into_response()
    }
}

impl ControlApi {
    pub const API_VERSION: u32 = 1;
    /// The longest session block a request may set.
    const MAX_BLOCK: Duration = Duration::from_secs(7 * 24 * 3600);

    pub fn new(
        controls: Arc<Controls>,
        recorder: Recorder,
        clock: Arc<dyn Clock>,
        token: ControlToken,
        logins: Vec<String>,
        users: Users,
        backup: Backup,
    ) -> Self {
        Self {
            controls,
            recorder,
            clock,
            token,
            logins,
            users,
            backup,
        }
    }

    /// The control routes, with the run routes when `supervisor` is given.
    /// `reader` tells what a history edit would touch; `shutdown` ends run
    /// update feeds so the server can drain.
    pub fn router(
        self,
        reader: ren_store::Reader,
        supervisor: Option<Supervisor>,
        toolbox: Option<Arc<dyn crate::tools::Toolbox>>,
        shutdown: CancellationToken,
    ) -> Router {
        let control = Arc::new(self);
        let session_routes = sessions::SessionsApi {
            control: control.clone(),
            supervisor: supervisor.clone(),
            reader: reader.clone(),
            shutdown: shutdown.clone(),
        }
        .router();
        let runs = RunsApi {
            control: control.clone(),
            supervisor,
            shutdown,
        };
        let run_routes = runs.router();
        let history_routes = HistoryApi {
            control: control.clone(),
            reader,
        }
        .router();
        let tools_routes = tools::ToolsApi {
            control: control.clone(),
            toolbox,
        }
        .router();
        Router::new()
            .route("/control/v1/state", get(Self::state))
            .route("/control/v1/pause", post(Self::pause))
            .route("/control/v1/resume", post(Self::resume))
            .route("/control/v1/sessions/{session}/block", post(Self::block))
            .route(
                "/control/v1/sessions/{session}/unblock",
                post(Self::unblock),
            )
            .route("/control/v1/users/{user}/block", post(Self::block_user))
            .route("/control/v1/users/{user}/unblock", post(Self::unblock_user))
            .route("/control/v1/calls/{call}/cancel", post(Self::cancel))
            .route("/control/v1/backup", post(Self::backup))
            .with_state(control)
            .merge(run_routes)
            .merge(tools_routes)
            .merge(session_routes)
            .merge(history_routes)
            .layer(axum::middleware::map_response(Self::private))
    }

    async fn private(mut response: Response) -> Response {
        let headers = response.headers_mut();
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<Actor, Refusal> {
        if headers.contains_key(header::ORIGIN) || headers.contains_key("sec-fetch-site") {
            return Err(Refusal::CrossSite);
        }
        if let Some(login) = headers.get("tailscale-user-login") {
            let login = login.to_str().map_err(|_| Refusal::Forbidden)?;
            return if self.logins.iter().any(|allowed| allowed == login) {
                Ok(Actor::Tailscale(login.to_owned()))
            } else {
                Err(Refusal::Forbidden)
            };
        }
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
            .ok_or(Refusal::Unauthorized)?;
        if self.token.matches(bearer) {
            Ok(Actor::Loopback)
        } else {
            Err(Refusal::Unauthorized)
        }
    }

    fn respond(&self, state: &OperatorState) -> Response {
        Json(StateBody {
            api_version: Self::API_VERSION,
            state,
            running: self.controls.running(),
        })
        .into_response()
    }

    fn command(&self, actor: Actor, command: OperatorCommand) -> Response {
        match self.controls.apply(&self.recorder, actor, command) {
            Ok(state) => self.respond(&state),
            Err(error) => Refusal::Control(error).into_response(),
        }
    }

    fn session(text: &str) -> Result<NativeSessionId, Refusal> {
        Uuid::try_parse(text)
            .ok()
            .and_then(NativeSessionId::new)
            .ok_or(Refusal::NotFound)
    }

    /// A configured user; any other name is not found.
    fn user(&self, text: &str) -> Result<UserId, Refusal> {
        UserId::parse(text)
            .ok()
            .filter(|user| self.users.knows(user))
            .ok_or(Refusal::NotFound)
    }

    fn duration(seconds: Option<u64>) -> Result<Option<Duration>, Refusal> {
        seconds
            .map(Duration::from_secs)
            .map(|duration| {
                if duration.is_zero() || duration > Self::MAX_BLOCK {
                    Err(Refusal::Invalid)
                } else {
                    Ok(duration)
                }
            })
            .transpose()
    }

    async fn state(State(api): State<Arc<Self>>, headers: HeaderMap) -> Response {
        match api.authorize(&headers) {
            Ok(_) => api.respond(&api.controls.snapshot()),
            Err(refusal) => refusal.into_response(),
        }
    }

    async fn pause(
        State(api): State<Arc<Self>>,
        headers: HeaderMap,
        body: Result<Json<PauseRequest>, JsonRejection>,
    ) -> Response {
        let actor = match api.authorize(&headers) {
            Ok(actor) => actor,
            Err(refusal) => return refusal.into_response(),
        };
        let Ok(retry_after) = body
            .map_err(drop)
            .and_then(|Json(body)| RetryAfter::seconds(body.retry_after_seconds).map_err(drop))
        else {
            return Refusal::Invalid.into_response();
        };
        api.command(actor, OperatorCommand::Pause { retry_after })
    }

    async fn resume(State(api): State<Arc<Self>>, headers: HeaderMap) -> Response {
        match api.authorize(&headers) {
            Ok(actor) => api.command(actor, OperatorCommand::Resume),
            Err(refusal) => refusal.into_response(),
        }
    }

    async fn block(
        State(api): State<Arc<Self>>,
        Path(session): Path<String>,
        headers: HeaderMap,
        body: Option<Json<BlockRequest>>,
    ) -> Response {
        let blocked = api.authorize(&headers).and_then(|actor| {
            let session = Self::session(&session)?;
            let duration = Self::duration(body.unwrap_or_default().0.duration_seconds)?;
            Ok((actor, session, duration))
        });
        match blocked {
            Ok((actor, session, duration)) => api.command(
                actor,
                OperatorCommand::block_session(session, duration, api.clock.now().wall),
            ),
            Err(refusal) => refusal.into_response(),
        }
    }

    async fn unblock(
        State(api): State<Arc<Self>>,
        Path(session): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        let unblocked = api
            .authorize(&headers)
            .and_then(|actor| Ok((actor, Self::session(&session)?)));
        match unblocked {
            Ok((actor, session)) => api.command(actor, OperatorCommand::UnblockSession { session }),
            Err(refusal) => refusal.into_response(),
        }
    }

    async fn block_user(
        State(api): State<Arc<Self>>,
        Path(user): Path<String>,
        headers: HeaderMap,
        body: Option<Json<BlockRequest>>,
    ) -> Response {
        let blocked = api.authorize(&headers).and_then(|actor| {
            let user = api.user(&user)?;
            let duration = Self::duration(body.unwrap_or_default().0.duration_seconds)?;
            Ok((actor, user, duration))
        });
        match blocked {
            Ok((actor, user, duration)) => api.command(
                actor,
                OperatorCommand::block_user(user, duration, api.clock.now().wall),
            ),
            Err(refusal) => refusal.into_response(),
        }
    }

    async fn unblock_user(
        State(api): State<Arc<Self>>,
        Path(user): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        let unblocked = api
            .authorize(&headers)
            .and_then(|actor| Ok((actor, api.user(&user)?)));
        match unblocked {
            Ok((actor, user)) => api.command(actor, OperatorCommand::UnblockUser { user }),
            Err(refusal) => refusal.into_response(),
        }
    }

    /// Back up the history into the data directory's `backup` subdirectory.
    /// It changes no operator state, so it is logged, not recorded. A
    /// request dropped mid-way leaves the previous backup restorable.
    async fn backup(State(api): State<Arc<Self>>, headers: HeaderMap) -> Response {
        let actor = match api.authorize(&headers) {
            Ok(actor) => actor,
            Err(refusal) => return refusal.into_response(),
        };
        match api.backup.run().await {
            Ok(report) => {
                tracing::info!(
                    ?actor,
                    chunks_copied = report.chunks_copied,
                    "backup written"
                );
                Json(serde_json::json!({ "api_version": Self::API_VERSION, "backup": report }))
                    .into_response()
            }
            Err(BackupError::Running) => Refusal::BackupRunning.into_response(),
            Err(error @ BackupError::Incomplete(_)) => {
                tracing::warn!(%error, "backup incomplete");
                Refusal::BackupIncomplete.into_response()
            }
            Err(
                error @ (BackupError::Io(_)
                | BackupError::Database(_)
                | BackupError::Store(_)
                | BackupError::NonUtf8Path),
            ) => {
                tracing::warn!(%error, "backup failed");
                Refusal::BackupFailed.into_response()
            }
        }
    }

    /// Cancel a running call. With `block_session_seconds`, its session is
    /// blocked first, so a client retry is refused.
    async fn cancel(
        State(api): State<Arc<Self>>,
        Path(call): Path<String>,
        headers: HeaderMap,
        body: Option<Json<CancelRequest>>,
    ) -> Response {
        let request = api.authorize(&headers).and_then(|actor| {
            let call: CallId = call.parse().map_err(|_| Refusal::NotFound)?;
            let block = Self::duration(body.unwrap_or_default().0.block_session_seconds)?;
            Ok((actor, call, block))
        });
        let (actor, call, block) = match request {
            Ok(request) => request,
            Err(refusal) => return refusal.into_response(),
        };
        // Recorded first: a cancel the recorder cannot record is not made.
        if let Err(error) = api.controls.running_call(call).and_then(|()| {
            api.controls.apply(
                &api.recorder,
                actor.clone(),
                OperatorCommand::CancelCall { call },
            )
        }) {
            return Refusal::Control(error).into_response();
        }
        let session = match api.controls.cancel(call) {
            Ok(session) => session,
            Err(error) => return Refusal::Control(error).into_response(),
        };
        match (block, session) {
            (Some(duration), Some(session)) => api.command(
                actor,
                OperatorCommand::block_session(session, Some(duration), api.clock.now().wall),
            ),
            (Some(_) | None, None) | (None, Some(_)) => {
                (StatusCode::ACCEPTED, api.respond(&api.controls.snapshot())).into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ControlOptions;

    #[test]
    fn logins_validate() {
        let options = |login: &str| ControlOptions {
            tailscale_logins: vec![login.into()],
        };
        assert!(options("me@example.com").validate().is_ok());
        assert!(options("").validate().is_err());
        assert!(options("two words").validate().is_err());
    }
}
