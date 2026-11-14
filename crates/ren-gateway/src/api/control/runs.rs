//! Runs under `/control/v1/runs`: the client processes serving sessions,
//! listed live and followed as server-sent events, and what a session can
//! start with (the projects, the clients, and each client's models), with
//! each client's sign-in (`sign_in`). A run
//! is driven through its session (`sessions`), whose routes share the
//! requests and refusals here. Access is the control API's; with no run
//! roots configured, every route answers `runs_disabled`.
//!
//! Live updates carry what the agent reported (its reply as it streams, its
//! commands, and its approvals). The supervisor stores every event but the
//! streamed pieces as the run's conversation; its model traffic is recorded
//! like any other client's.

use super::{ControlApi, Refusal};
use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use http::{HeaderMap, StatusCode};
use ren_harness::{
    Harness,
    commands::ClientCommand,
    drive::{CommandRefused, Decision},
    models::ModelId,
    settings::{ApprovalPolicy, Effort, RunSettings},
};
use ren_supervisor::{
    CommandError, Delivery, ModelsError, RunRefused, RunSnapshot, StartError, Supervisor,
    WorkdirError,
};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

/// A search of a run's folder for a prompt to name a file: `q`, what was
/// typed after `@`, and how many to answer.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FilesRequest {
    #[serde(default)]
    pub(super) q: String,
    #[serde(default)]
    pub(super) limit: Option<usize>,
}

impl FilesRequest {
    pub(super) const LIMIT: usize = 20;
    pub(super) const MOST: usize = 100;
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptRequest {
    pub(super) text: String,
    /// How it goes: start a turn (the default), queue it for after the
    /// running turn, or steer the running turn with it.
    #[serde(default = "PromptRequest::start")]
    pub(super) delivery: Delivery,
}

impl SettingsRequest {
    /// The settings asked for: a model must be one a client could name.
    pub(super) fn settings(self) -> Result<RunSettings, Denied> {
        let model = self
            .model
            .map(|model| model.parse::<ModelId>())
            .transpose()
            .map_err(|_| Denied::Control(Refusal::Invalid))?;
        Ok(RunSettings {
            model,
            effort: self.effort,
            approvals: self.approvals,
        })
    }
}

impl PromptRequest {
    fn start() -> Delivery {
        Delivery::Start
    }
}

/// The settings a run moves to; each left out is the client's own.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsRequest {
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) effort: Option<Effort>,
    #[serde(default)]
    pub(super) approvals: Option<ApprovalPolicy>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AnswerRequest {
    pub(super) decision: Decision,
}

#[derive(serde::Serialize)]
struct Runs {
    api_version: u32,
    runs: Vec<RunSnapshot>,
}

/// A refused request: the control API's access check or a run refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Denied {
    Control(Refusal),
    Run(RunRefusal),
}

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        match self {
            Self::Control(refusal) => refusal.into_response(),
            Self::Run(refusal) => refusal.into_response(),
        }
    }
}

/// Why a run request was refused, with the label clients match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunRefusal {
    Disabled,
    InvalidDirectory,
    DirectoryNotFound,
    NotDrivable,
    NoApprovals,
    NoScratch,
    Unrecorded,
    SpawnFailed,
    NotFound,
    Refused(&'static str),
    Unavailable,
}

impl IntoResponse for RunRefusal {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Disabled => (StatusCode::NOT_FOUND, "runs_disabled"),
            Self::InvalidDirectory => (StatusCode::BAD_REQUEST, "invalid_directory"),
            Self::DirectoryNotFound => (StatusCode::BAD_REQUEST, "directory_not_found"),
            Self::NotDrivable => (StatusCode::BAD_REQUEST, "not_drivable"),
            Self::NoApprovals => (StatusCode::BAD_REQUEST, "no_approvals"),
            Self::NoScratch => (StatusCode::SERVICE_UNAVAILABLE, "no_scratch"),
            Self::Unrecorded => (StatusCode::SERVICE_UNAVAILABLE, "unrecorded"),
            Self::SpawnFailed => (StatusCode::BAD_GATEWAY, "spawn_failed"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::Refused(reason) => (StatusCode::CONFLICT, reason),
            Self::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        };
        (
            status,
            Json(serde_json::json!({ "api_version": ControlApi::API_VERSION, "error": code })),
        )
            .into_response()
    }
}

impl From<StartError> for RunRefusal {
    fn from(error: StartError) -> Self {
        match error {
            StartError::Workdir(
                WorkdirError::Relative
                | WorkdirError::Unnormalized
                | WorkdirError::OutsideRoots
                | WorkdirError::NotUtf8,
            ) => Self::InvalidDirectory,
            StartError::Workdir(WorkdirError::NoRoots) => Self::Disabled,
            StartError::Resolve(_) => Self::DirectoryNotFound,
            StartError::NotDrivable(_) => Self::NotDrivable,
            StartError::NoApprovals(_) => Self::NoApprovals,
            StartError::NoScratch | StartError::Scratch(_) => Self::NoScratch,
            StartError::Unrecorded(_) => Self::Unrecorded,
            StartError::Spawn { .. } => Self::SpawnFailed,
            StartError::Gone | StartError::NilId => Self::Unavailable,
        }
    }
}

impl From<CommandError> for RunRefusal {
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::NoSuchRun(_) => Self::NotFound,
            CommandError::Refused(refused) => Self::Refused(match refused {
                RunRefused::Starting => "starting",
                RunRefused::Busy => "busy",
                RunRefused::NotWorking => "not_working",
                RunRefused::TurnNotStarted => "turn_not_started",
                RunRefused::NoSuchApproval(_) => "no_such_approval",
                RunRefused::Stopping => "stopping",
                RunRefused::Ended => "ended",
                RunRefused::NothingQueued => "nothing_queued",
            }),
            // What the client has no way to do mid-run.
            CommandError::Protocol(CommandRefused::NotOffered(_)) => Self::Refused("not_offered"),
            CommandError::Protocol(
                CommandRefused::NotReady
                | CommandRefused::TurnRunning
                | CommandRefused::NoTurn
                | CommandRefused::TurnNotStarted
                | CommandRefused::UnknownApproval(_),
            ) => Self::Refused("protocol"),
            CommandError::Gone => Self::Unavailable,
        }
    }
}

/// The run routes' state: the control API's access check, the supervisor
/// when runs are enabled, and the token that ends update feeds at shutdown.
pub(crate) struct RunsApi {
    pub(super) control: Arc<ControlApi>,
    pub(super) supervisor: Option<Supervisor>,
    pub(super) shutdown: CancellationToken,
}

type Api = State<Arc<RunsApi>>;

impl RunsApi {
    pub(super) fn router(self) -> Router {
        Router::new()
            .route("/control/v1/runs", get(Self::list))
            .route("/control/v1/runs/events", get(Self::events))
            .route("/control/v1/runs/projects", get(Self::projects))
            .route("/control/v1/runs/clients", get(Self::clients))
            .route(
                "/control/v1/runs/clients/{harness}/models",
                get(Self::models),
            )
            .route(
                "/control/v1/runs/clients/{harness}/sign-in",
                post(Self::start_sign_in),
            )
            .route(
                "/control/v1/runs/clients/{harness}/sign-in/code",
                post(Self::sign_in_code),
            )
            .route(
                "/control/v1/runs/clients/{harness}/sign-in/check",
                post(Self::check_sign_in),
            )
            .route(
                "/control/v1/runs/clients/{harness}/sign-in/cancel",
                post(Self::cancel_sign_in),
            )
            .route(
                "/control/v1/runs/clients/{harness}/sign-in/wait",
                post(Self::wait_sign_in),
            )
            .with_state(Arc::new(self))
    }

    /// The supervisor, once the request is authorized.
    pub(super) fn admit(&self, headers: &HeaderMap) -> Result<&Supervisor, Denied> {
        self.control.authorize(headers).map_err(Denied::Control)?;
        self.supervisor
            .as_ref()
            .ok_or(Denied::Run(RunRefusal::Disabled))
    }

    pub(super) fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, Denied> {
        body.map(|Json(body)| body)
            .map_err(|_| Denied::Control(Refusal::Invalid))
    }

    async fn list(State(api): Api, headers: HeaderMap) -> Result<Response, Denied> {
        let runs = api
            .admit(&headers)?
            .runs()
            .await
            .map_err(|_| Denied::Run(RunRefusal::Unavailable))?;
        Ok(Json(Runs {
            api_version: ControlApi::API_VERSION,
            runs,
        })
        .into_response())
    }

    async fn projects(State(api): Api, headers: HeaderMap) -> Result<Response, Denied> {
        let projects = api
            .admit(&headers)?
            .projects()
            .await
            .map_err(|_| Denied::Run(RunRefusal::Unavailable))?;
        Ok(Json(serde_json::json!({
            "api_version": ControlApi::API_VERSION,
            "projects": projects.projects,
            "scratch": projects.scratch,
        }))
        .into_response())
    }

    async fn clients(State(api): Api, headers: HeaderMap) -> Result<Response, Denied> {
        let clients = api
            .admit(&headers)?
            .clients()
            .await
            .map_err(|_| Denied::Run(RunRefusal::Unavailable))?;
        Ok(Json(serde_json::json!({
            "api_version": ControlApi::API_VERSION,
            "clients": clients,
        }))
        .into_response())
    }

    async fn models(
        State(api): Api,
        headers: HeaderMap,
        Path(harness): Path<String>,
    ) -> Result<Response, Denied> {
        let supervisor = api.admit(&headers)?;
        let harness = harness
            .parse::<Harness>()
            .map_err(|_| Denied::Run(RunRefusal::NotFound))?;
        let unlisted = |error: ModelsError| {
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "api_version": ControlApi::API_VERSION,
                    "error": "models_unavailable",
                    "message": error.to_string(),
                })),
            )
                .into_response()
        };
        Ok(match supervisor.models(harness).await {
            Ok(models) => Json(serde_json::json!({
                "api_version": ControlApi::API_VERSION,
                "harness": harness,
                "models": models,
            }))
            .into_response(),
            Err(ModelsError::NotDrivable(_)) => RunRefusal::NotDrivable.into_response(),
            Err(ModelsError::NoRoot) => RunRefusal::Disabled.into_response(),
            Err(ModelsError::Gone) => RunRefusal::Unavailable.into_response(),
            Err(
                error @ (ModelsError::Spawn
                | ModelsError::Refused(_)
                | ModelsError::Exited
                | ModelsError::TimedOut),
            ) => unlisted(error),
        })
    }

    /// The answer listing `commands`, each with what a prompt writes to run it.
    pub(super) fn commands_body(commands: Vec<ClientCommand>) -> Response {
        let commands: Vec<serde_json::Value> = commands
            .into_iter()
            .map(|command| {
                let invocation = command.invocation();
                let mut value = serde_json::json!(command);
                if let serde_json::Value::Object(fields) = &mut value {
                    fields.insert("invocation".into(), invocation.into());
                }
                value
            })
            .collect();
        Json(serde_json::json!({
            "api_version": ControlApi::API_VERSION,
            "commands": commands,
        }))
        .into_response()
    }

    /// Updates from now on, as `run` events; `lagged` when the feed fell
    /// behind and skipped some (list the runs again to catch up).
    async fn events(State(api): Api, headers: HeaderMap) -> Result<Response, Denied> {
        let supervisor = api.admit(&headers)?;
        let receiver = supervisor.subscribe();
        let shutdown = api.shutdown.clone();
        let stream = futures_util::stream::unfold(
            (receiver, shutdown),
            |(mut receiver, shutdown)| async move {
                let event = tokio::select! {
                    () = shutdown.cancelled() => return None,
                    received = receiver.recv() => match received {
                        Ok(update) => Event::default().event("run").json_data(&update).ok()?,
                        Err(RecvError::Lagged(skipped)) => Event::default()
                            .event("lagged")
                            .json_data(serde_json::json!({ "skipped": skipped }))
                            .ok()?,
                        Err(RecvError::Closed) => return None,
                    },
                };
                Some((Ok::<_, Infallible>(event), (receiver, shutdown)))
            },
        );
        Ok(Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response())
    }
}
