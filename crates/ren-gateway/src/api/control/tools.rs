//! `/control/v1/tools`: the tools Ren installs on the daemon's host for
//! its agents, through the daemon's [`Toolbox`].
//!
//! - `GET /control/v1/tools`: every package, what the host has of it, and
//!   the generations kept.
//! - `POST /control/v1/tools/change` `{"add": [...], "remove": [...]}`:
//!   install what is added and make the next generation current, only if
//!   every install succeeds. Answers when done.
//! - `POST /control/v1/tools/gc`: remove generations and installs nothing
//!   current or running uses.
//!
//! Access is the control API's. Without a toolbox every route answers
//! `tools_disabled`.

use super::{ControlApi, Refusal};
use crate::tools::{ToolChange, ToolCollection, ToolListing, Toolbox, ToolboxError};
use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use http::{HeaderMap, StatusCode};
use ren_depot::{package::PackageId, toolset::Edit};
use serde::Serialize;
use std::{collections::BTreeSet, sync::Arc};

pub(super) struct ToolsApi {
    pub control: Arc<ControlApi>,
    pub toolbox: Option<Arc<dyn Toolbox>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeRequest {
    #[serde(default)]
    add: BTreeSet<PackageId>,
    #[serde(default)]
    remove: BTreeSet<PackageId>,
}

#[derive(Serialize)]
struct Versioned<T> {
    api_version: u32,
    #[serde(flatten)]
    body: T,
}

/// Why a tools request was refused, beyond access.
#[derive(Debug)]
enum Untooled {
    Access(Refusal),
    Disabled,
    Toolbox(ToolboxError),
}

impl IntoResponse for Untooled {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Access(refusal) => return refusal.into_response(),
            Self::Disabled => (StatusCode::SERVICE_UNAVAILABLE, "tools_disabled", None),
            Self::Toolbox(ToolboxError::Busy) => (StatusCode::CONFLICT, "tools_busy", None),
            Self::Toolbox(ToolboxError::Unsupported) => {
                (StatusCode::NOT_IMPLEMENTED, "tools_unsupported", None)
            }
            Self::Toolbox(error @ ToolboxError::Refused(_)) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "tools_refused",
                Some(error.to_string()),
            ),
            Self::Toolbox(ToolboxError::Failed(reason)) => {
                tracing::warn!(%reason, "the tool store failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "tools_failed", None)
            }
        };
        (
            status,
            Json(serde_json::json!({
                "api_version": ControlApi::API_VERSION,
                "error": code,
                "message": message,
            })),
        )
            .into_response()
    }
}

impl ToolsApi {
    pub fn router(self) -> Router {
        Router::new()
            .route("/control/v1/tools", get(Self::listing))
            .route("/control/v1/tools/change", post(Self::change))
            .route("/control/v1/tools/gc", post(Self::collect))
            .with_state(Arc::new(self))
    }

    fn toolbox(&self, headers: &HeaderMap) -> Result<Arc<dyn Toolbox>, Untooled> {
        self.control.authorize(headers).map_err(Untooled::Access)?;
        self.toolbox.clone().ok_or(Untooled::Disabled)
    }

    fn answer<T: Serialize>(result: Result<T, ToolboxError>) -> Response {
        match result {
            Ok(body) => Json(Versioned {
                api_version: ControlApi::API_VERSION,
                body,
            })
            .into_response(),
            Err(error) => Untooled::Toolbox(error).into_response(),
        }
    }

    async fn listing(State(api): State<Arc<Self>>, headers: HeaderMap) -> Response {
        match api.toolbox(&headers) {
            Ok(toolbox) => Self::answer::<ToolListing>(toolbox.listing().await),
            Err(refused) => refused.into_response(),
        }
    }

    async fn change(
        State(api): State<Arc<Self>>,
        headers: HeaderMap,
        body: Result<Json<ChangeRequest>, JsonRejection>,
    ) -> Response {
        let toolbox = match api.toolbox(&headers) {
            Ok(toolbox) => toolbox,
            Err(refused) => return refused.into_response(),
        };
        let Ok(Json(request)) = body else {
            return Refusal::Invalid.into_response();
        };
        let edit = Edit {
            add: request.add,
            remove: request.remove,
        };
        Self::answer::<ToolChange>(toolbox.change(edit).await)
    }

    async fn collect(State(api): State<Arc<Self>>, headers: HeaderMap) -> Response {
        match api.toolbox(&headers) {
            Ok(toolbox) => Self::answer::<ToolCollection>(toolbox.collect().await),
            Err(refused) => refused.into_response(),
        }
    }
}
