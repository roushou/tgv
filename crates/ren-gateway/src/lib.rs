//! Ren's HTTP interface: native forwarding, management, and control routes.
//! The primary type is [`Gateway`].
//!
//! A [`Gateway`] admits a call, records it through the recorder's tap, and
//! relays the native request and response unchanged except for the header
//! policy. Each attempt's lifecycle is the pure [`HttpAttempt`] machine; this
//! module only feeds it events and runs its actions.
//!
//! A request is buffered whole, planned by the [`Policy`]
//! against one operator snapshot, changed by its transform rules and
//! the upstream entry's credential override, and tried on its
//! candidates in order while the retry rule allows. The plan, each
//! attempt's changes, and any rejection are recorded with the call.
//!
//! The crate also owns what forwarding decides with: the pure attempt and
//! channel lifecycles (`attempt`, `channel`), retry eligibility (`retry`),
//! operator state (`operator`) and the [`Controls`] that hold it (`control`),
//! who is calling (`user`), and call planning (`policy`).
//!
//! Management handlers read history through the store's `Reader`, stream live
//! observations from the recorder, and run authenticated controls and backups.
//! [`ManagementSetup`] supplies their resources. All routes share one router
//! and listener; the host owns resource construction and task lifecycles.
//!
//! Contract:
//! - configuration: [`GatewayConfig`] (admission limit, header deadline,
//!   configuration revision, buffer, policy, upstream entries);
//! - capabilities: an [`Upstream`], a [`Recorder`], the shared [`Controls`],
//!   a [`Clock`], and the `Reader` and `Backup` in [`ManagementSetup`],
//!   which also names the running [`Build`] that `/health` reports;
//! - inputs: inference requests on the upstream table's routes, management reads, and controls;
//! - outputs: provider responses, management data, and HTTP error responses;
//! - lifecycle: [`Shutdown`] drains, then aborts, through [`Ingress`].
//!
//! A WebSocket handshake on a [`Accepts::PostOrWebSocket`] route is itself an
//! HTTP attempt (recorded with method `GET`): a refusal or a failed connect is
//! relayed and recorded like any response. Once both legs upgrade, the
//! channel's calls are relayed by the `websocket` module.
//!
//! [`HttpAttempt`]: crate::attempt::HttpAttempt

pub mod api;
pub mod attempt;
mod buffer;
pub mod build;
pub mod call;
pub mod channel;
pub mod control;
mod headers;
mod ingress;
pub mod instance;
mod limits;
pub mod operator;
pub mod policy;
#[cfg(test)]
mod purity;
mod relay;
pub mod retry;
mod route;
pub mod tools;
pub mod upstream;
pub mod user;
mod websocket;

pub use api::{ApiRead, ControlOptions, ControlOptionsError, ControlToken, ManagementSetup};
pub use buffer::{BufferOptions, BufferOptionsError, BufferRefusal, RequestBuffer, RequestLimits};
pub use build::Build;
pub use headers::HeaderPolicy;
pub use ingress::{Ingress, IngressError, Shutdown, Stopped};
pub use instance::{Environment, InstanceOptions};
pub use route::{Accepts, BoundRoute, Forward, Route};
pub use upstream::{
    AuthOverride, ConfiguredEntry, Connected, EntryIngress, NativeUrls, OperatorKey,
    ReqwestUpstream, Socket, TransportTimeouts, Upstream, UpstreamAuth, UpstreamBody,
    UpstreamConfigError, UpstreamEntry, UpstreamHandshake, UpstreamRequest, UpstreamSocket,
    UpstreamTable, UpstreamTableError, UpstreamUrl,
};
pub use websocket::{WebSocketMode, WebSocketOptions, WebSocketOptionsError};

use crate::{
    api::{ApiState, ControlApi, ReadSources},
    attempt::{AttemptEnd, Canceller, EndKind, HttpEvent, SendFailure},
    call::{CallRun, CallStep, CallStop, Dispatched, Refused},
    channel::Deadlines,
    control::{Cancel, Controls},
    limits::{AtLimit, UserLimits},
    policy::{Admission, CallFacts, CallPlan, Policy, Rejection, RequestJson, StepRecord},
    user::IdentifyError,
};
use axum::{
    Router,
    body::Body,
    extract::{FromRequestParts, Request, ws::WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::{MethodRouter, get, post},
};
use buffer::Decoded;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use http_body_util::BodyExt;
use relay::{AttemptCell, Dispatch, ObservedUpload};
use ren_harness::Harness;
use ren_protocol::{
    label::Labeled,
    route::{Method, Transport},
};
use ren_recorder::{
    AttemptStart, CallStart, CallTap, Recorder,
    batch::{AttemptBody, Recorded},
    caller::Caller,
    capture::CallRejection,
    routing::{ConfigRevision, UpstreamId},
    time::Clock,
};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_util::sync::CancellationToken;

/// Declarative gateway settings, fixed for the daemon's lifetime.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Calls forwarded at once; more are rejected with `503`.
    pub max_concurrent_requests: usize,
    /// Longest wait for a response head after dispatch, and on WebSocket for
    /// a call's first upstream message.
    pub response_header_timeout: Duration,
    /// Longest quiet gap in a WebSocket call's upstream messages. HTTP bodies
    /// get theirs from the transport (`TransportTimeouts::idle`).
    pub stream_idle_timeout: Duration,
    /// Recorded with every call.
    pub config_revision: ConfigRevision,
    pub websocket: WebSocketOptions,
    pub buffer: BufferOptions,
    pub policy: Policy,
    pub upstreams: UpstreamTable,
    /// The named users forwarded requests are matched to.
    pub users: user::Users,
    /// Calls every caller that is not a named user may have running at
    /// once, together; unlimited beyond the global limit when absent.
    pub max_concurrent_unnamed: Option<u32>,
}

/// Why the gateway answered instead of the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GatewayError {
    #[error("gateway concurrency limit reached")]
    Busy,
    #[error("gateway is shutting down")]
    ShuttingDown,
    #[error("{}", Self::describe(*.0))]
    Ended(AttemptEnd),
}

impl GatewayError {
    fn describe(end: AttemptEnd) -> &'static str {
        match end.how {
            EndKind::Unsent(SendFailure::NotSent) => "upstream connection failed",
            EndKind::Unsent(SendFailure::Refused) => "upstream refused the request",
            EndKind::Unsent(SendFailure::SendFailed) => "upstream request failed",
            EndKind::Unsent(SendFailure::HeaderTimeout) => "upstream request timed out",
            EndKind::Cancelled(Canceller::Shutdown) => "gateway is shutting down",
            EndKind::Cancelled(Canceller::Client) => "request body failed",
            EndKind::Cancelled(Canceller::Operator) => "cancelled by the Ren operator",
            EndKind::BodyComplete
            | EndKind::LengthDelivered
            | EndKind::StreamFailed(_)
            | EndKind::Interrupted => "upstream request failed",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::Busy | Self::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            Self::Ended(end) => match end.how {
                EndKind::Unsent(SendFailure::HeaderTimeout) => StatusCode::GATEWAY_TIMEOUT,
                EndKind::Cancelled(Canceller::Shutdown) => StatusCode::SERVICE_UNAVAILABLE,
                EndKind::Cancelled(Canceller::Client) => StatusCode::BAD_REQUEST,
                EndKind::Cancelled(Canceller::Operator) => StatusCode::CONFLICT,
                EndKind::Unsent(
                    SendFailure::NotSent | SendFailure::Refused | SendFailure::SendFailed,
                )
                | EndKind::BodyComplete
                | EndKind::LengthDelivered
                | EndKind::StreamFailed(_)
                | EndKind::Interrupted => StatusCode::BAD_GATEWAY,
            },
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        match self {
            // An operator's cancel is Ren-labelled and must not be retried.
            Self::Ended(AttemptEnd {
                how: EndKind::Cancelled(Canceller::Operator),
                ..
            }) => RenReply {
                status: self.status(),
                code: "operator_cancelled",
                message: self.to_string(),
                retry: ClientRetry::Never,
            }
            .into_response(),
            Self::Busy | Self::ShuttingDown | Self::Ended(_) => {
                (self.status(), format!("{self}\n")).into_response()
            }
        }
    }
}

/// Whether a Ren answer invites the client to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientRetry {
    /// `retry-after`, in seconds.
    After(u32),
    /// `x-should-retry: false`.
    Never,
}

/// An answer Ren gives instead of the provider: a JSON error body both the
/// Anthropic and OpenAI clients read, with a `ren_` error type so it is never
/// mistaken for the provider's.
#[derive(Debug)]
struct RenReply {
    status: StatusCode,
    code: &'static str,
    message: String,
    retry: ClientRetry,
}

impl RenReply {
    fn rejected(rejection: &Rejection) -> Self {
        match rejection {
            Rejection::Paused { retry_after } => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "paused",
                message: "Ren is paused by its operator".into(),
                retry: ClientRetry::After(retry_after.get()),
            },
            Rejection::SessionBlocked => Self {
                status: StatusCode::FORBIDDEN,
                code: "session_blocked",
                message: "this session is blocked by the Ren operator".into(),
                retry: ClientRetry::Never,
            },
            Rejection::UserBlocked => Self {
                status: StatusCode::FORBIDDEN,
                code: "user_blocked",
                message: "this user is blocked by the Ren operator".into(),
                retry: ClientRetry::Never,
            },
            Rejection::Blocked { rule, reason } => Self {
                status: StatusCode::FORBIDDEN,
                code: "blocked",
                message: format!("blocked by Ren policy rule {}: {reason}", rule.as_str()),
                retry: ClientRetry::Never,
            },
            Rejection::UserNotAllowed { upstream } => Self {
                status: StatusCode::FORBIDDEN,
                code: "user_not_allowed",
                message: format!("upstream {upstream} is not open to this caller"),
                retry: ClientRetry::Never,
            },
        }
    }

    fn user_busy() -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "user_busy",
            message: "this caller already has as many calls running as Ren allows it".into(),
            retry: ClientRetry::After(1),
        }
    }

    fn unidentified(refused: &IdentifyError) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "identity_refused",
            message: refused.to_string(),
            retry: ClientRetry::Never,
        }
    }

    fn refused(refusal: UploadRefusal) -> Self {
        let (status, code, retry) = match refusal {
            UploadRefusal::Shutdown => (
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                ClientRetry::After(1),
            ),
            UploadRefusal::Buffer(BufferRefusal::TooLarge) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                ClientRetry::Never,
            ),
            UploadRefusal::Buffer(BufferRefusal::BufferFull) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "buffer_full",
                ClientRetry::After(1),
            ),
            UploadRefusal::Buffer(BufferRefusal::UploadTimeout) => (
                StatusCode::REQUEST_TIMEOUT,
                "upload_timeout",
                ClientRetry::After(1),
            ),
            UploadRefusal::Buffer(BufferRefusal::UploadFailed) => {
                (StatusCode::BAD_REQUEST, "upload_failed", ClientRetry::Never)
            }
        };
        Self {
            status,
            code,
            message: match refusal {
                UploadRefusal::Shutdown => "gateway is shutting down".into(),
                UploadRefusal::Buffer(refusal) => refusal.to_string(),
            },
            retry,
        }
    }
}

impl IntoResponse for RenReply {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "type": "error",
            "error": { "type": format!("ren_{}", self.code), "message": self.message },
        });
        let mut response = (self.status, axum::Json(body)).into_response();
        let headers = response.headers_mut();
        match self.retry {
            ClientRetry::After(seconds) => {
                headers.insert(http::header::RETRY_AFTER, HeaderValue::from(seconds));
            }
            ClientRetry::Never => {
                headers.insert("x-should-retry", HeaderValue::from_static("false"));
            }
        }
        response
    }
}

/// One attempt's request: headers and body after transforms and the
/// upstream entry's credential override.
struct Prepared {
    headers: HeaderMap,
    body: Bytes,
    start: AttemptStart,
}

/// What Ren changed in an attempt's request, as recorded. No header values.
#[derive(serde::Serialize)]
struct AttemptChanges<'a> {
    auth: AuthOverride,
    steps: &'a [StepRecord],
    body: AttemptBody,
}

/// What came back from a WebSocket handshake.
enum Handshaken {
    Connected(Connected),
    Failed(SendFailure),
    Aborted,
}

/// Serves Ren's HTTP interface, forwarding native requests to one [`Upstream`].
pub struct Gateway<U> {
    config: GatewayConfig,
    upstream: U,
    recorder: Recorder,
    controls: Arc<Controls>,
    clock: Arc<dyn Clock>,
    buffer: RequestBuffer,
    admission: Arc<Semaphore>,
    /// Upgraded WebSocket channels; separate from call admission.
    channels: Arc<Semaphore>,
    /// Each caller's own limit, checked before call admission (so a caller
    /// over it gets 429, not 503).
    user_limits: UserLimits,
    shutdown: Shutdown,
}

impl<U: Upstream> Gateway<U> {
    pub fn new(
        config: GatewayConfig,
        upstream: U,
        recorder: Recorder,
        controls: Arc<Controls>,
        clock: Arc<dyn Clock>,
        shutdown: Shutdown,
    ) -> Self {
        Self {
            admission: Arc::new(Semaphore::new(config.max_concurrent_requests)),
            channels: Arc::new(Semaphore::new(config.websocket.max_channels)),
            user_limits: UserLimits::new(&config.users, config.max_concurrent_unnamed),
            buffer: RequestBuffer::new(config.buffer),
            config,
            upstream,
            recorder,
            controls,
            clock,
            shutdown,
        }
    }

    /// The largest part of a retryable response read before the next
    /// candidate is tried; the rest is dropped.
    const DRAIN_LIMIT: usize = 64 * 1024;
    const DRAIN_TIME: Duration = Duration::from_secs(5);

    /// Assemble forwarding, management, and control routes on one listener.
    /// Transfers ownership of the gateway and management resources to the server.
    #[must_use]
    pub fn into_ingress(self, listener: TcpListener, management: ManagementSetup) -> Ingress {
        Ingress::new(listener, Arc::new(self).router(management))
    }

    /// All HTTP routes. Tests can drive this same router without a connection.
    pub fn router(self: Arc<Self>, management: ManagementSetup) -> Router {
        let reader = management.reader.clone();
        let api = ApiState::new(
            ReadSources {
                reader: management.reader,
                retention: management.retention,
                build: management.build,
                instance: management.instance,
            },
            self.recorder.clone(),
            self.controls.clone(),
            self.clock.clone(),
            self.shutdown.drain_token(),
            self.config.users.clone(),
        );
        let control = ControlApi::new(
            self.controls.clone(),
            self.recorder.clone(),
            self.clock.clone(),
            management.token,
            management.control.tailscale_logins,
            self.config.users.clone(),
            management.backup,
        );
        let feeds_end = self.shutdown.drain_token();
        self.forwarding_router()
            .merge(api.router())
            .merge(control.router(reader, management.runs, management.tools, feeds_end))
    }

    /// The forwarding routes, built from the upstream table's bound routes.
    /// Every other method on a route path is refused; other paths fall
    /// through to management routes.
    fn forwarding_router(self: Arc<Self>) -> Router {
        let routes = self.config.upstreams.routes();
        routes.into_iter().fold(Router::new(), |router, bound| {
            let accepts = bound.forward.accepts;
            let path = bound.path.clone();
            let bound = Arc::new(bound);
            let gateway = self.clone();
            let target = bound.clone();
            let forward =
                move |request: Request| async move { gateway.forward(&target, request).await };
            let refuse = move |request: Request| async move { Self::refuse(accepts, &request) };
            let methods: MethodRouter = match accepts {
                Accepts::Post | Accepts::PostOrRefusedHandshake => post(forward),
                Accepts::PostOrWebSocket => {
                    let gateway = self.clone();
                    let target = bound.clone();
                    let handshake = move |request: Request| async move {
                        gateway.handshake(&target, request).await
                    };
                    post(forward).get(handshake).head(refuse)
                }
                // Axum's GET also answers HEAD; keep the upstream method explicit.
                Accepts::Get => get(forward).head(refuse),
            };
            router.route(&path, methods.fallback(refuse))
        })
    }

    /// A method the route does not accept.
    fn refuse(accepts: Accepts, request: &Request) -> Response {
        match accepts {
            Accepts::PostOrRefusedHandshake
                if request.headers().contains_key(http::header::UPGRADE) =>
            {
                return Self::handshake_refused("WebSocket is not available here; use HTTP/SSE\n");
            }
            Accepts::Post
            | Accepts::Get
            | Accepts::PostOrRefusedHandshake
            | Accepts::PostOrWebSocket => {}
        }
        (
            StatusCode::METHOD_NOT_ALLOWED,
            [(http::header::ALLOW, accepts.method().label())],
        )
            .into_response()
    }

    /// Codex treats 426 on its handshake as a signal to use HTTP/SSE for the
    /// session.
    fn handshake_refused(reason: &'static str) -> Response {
        (StatusCode::UPGRADE_REQUIRED, reason).into_response()
    }

    async fn forward(&self, bound: &BoundRoute, request: Request) -> Response {
        let route = bound.forward;
        let provider = bound.api.provider();
        let (mut parts, body) = request.into_parts();
        if parts.headers.contains_key(http::header::UPGRADE) {
            return Self::handshake_refused("WebSocket is not available here; use HTTP/SSE\n");
        }
        if self.shutdown.is_draining() {
            return GatewayError::ShuttingDown.into_response();
        }
        // Attribution and identity read Ren's local headers and Serve's
        // identity before the policy removes them.
        let attribution = Harness::attribution(route.harness, &parts.headers);
        let identified = self.config.users.identify(&parts.headers);
        HeaderPolicy::REQUEST.apply(&mut parts.headers);
        let session = attribution.hint.as_ref().map(|hint| hint.session);
        let mut call = CallStart {
            transport: Transport::Http,
            api: bound.api,
            endpoint: route.endpoint,
            method: route.accepts.method(),
            config_revision: self.config.config_revision.clone(),
            attribution,
            caller: identified.clone().unwrap_or(Caller::Unidentified),
            operator_revision: None,
            plan: None,
            attempts: 1,
        };
        if let Err(refused) = identified {
            self.recorder
                .reject(None, call, None, CallRejection::IdentityRefused);
            return RenReply::unidentified(&refused).into_response();
        }
        // A caller's own limit comes first, so a caller over it is told so
        // (429) rather than seeing the gateway as busy. Held until the
        // response ends, like the call's admission.
        let mut user_permit = match self.user_limits.admit(&call.caller) {
            Ok(permit) => permit,
            Err(AtLimit) => {
                self.recorder
                    .reject(None, call, None, CallRejection::UserBusy);
                return RenReply::user_busy().into_response();
            }
        };
        let Ok(admission) = self.admission.clone().try_acquire_owned() else {
            return GatewayError::Busy.into_response();
        };
        let abort = self.shutdown.abort_token();
        // Reading is the one await before the call has a tap; a handler
        // dropped here would otherwise leave no record.
        let upload = UploadGuard::arm(&self.recorder, &call);
        let read = tokio::select! {
            biased;
            () = abort.cancelled() => Err(UploadRefusal::Shutdown),
            read = self.buffer.read(provider, &parts.headers, body) => read.map_err(UploadRefusal::Buffer),
        };
        upload.disarm();
        let buffered = match read {
            Ok(buffered) => buffered,
            Err(refusal) => {
                self.recorder.reject(None, call, None, refusal.rejection());
                return RenReply::refused(refusal).into_response();
            }
        };
        let decoded = Decoded::new(
            &parts.headers,
            buffered.bytes(),
            self.config.buffer.request_limits.get(provider),
        );
        let model = decoded.model();
        let operator = self.controls.snapshot();
        let plan = self.config.policy.plan(
            &CallFacts {
                transport: Transport::Http,
                provider,
                upstream: &bound.upstream,
                endpoint: route.endpoint,
                model: model.as_deref(),
                session,
                user: call.caller.user(),
                now: self.clock.now().wall,
            },
            &operator,
        );
        call.operator_revision = Some(plan.operator_revision);
        call.plan = Recorded::of(&plan.record()).ok();
        call.attempts = plan.candidates.len();
        if let Admission::Reject(rejection) = &plan.admission {
            self.recorder
                .reject(None, call, Some(buffered.bytes()), rejection.label());
            return RenReply::rejected(rejection).into_response();
        }
        let operator_cancel = CancellationToken::new();
        let mut admission = Some(admission);
        let mut tap: Option<Arc<CallTap>> = None;
        let mut registered = None;
        let mut run = CallRun::new(plan.candidates.len());
        loop {
            let Some(prepared) = plan.candidates.get(run.candidate()).and_then(|upstream| {
                self.prepare(&plan, upstream, &parts.headers, buffered.bytes(), &decoded)
            }) else {
                // Policy validates every candidate against the same table.
                tracing::error!("a planned upstream is not configured");
                return GatewayError::Busy.into_response();
            };
            let upstream = prepared.start.upstream.clone();
            let current = match &tap {
                None => {
                    let started = self.recorder.start(call.clone(), prepared.start.clone());
                    started.request(buffered.bytes());
                    registered = Some(self.controls.register(
                        started.call(),
                        Transport::Http,
                        session,
                        call.caller.user().cloned(),
                        Cancel::Http(operator_cancel.clone()),
                    ));
                    tap = Some(started.clone());
                    started
                }
                Some(started) => {
                    started.next_attempt(prepared.start.clone());
                    started.clone()
                }
            };
            match prepared.start.body {
                AttemptBody::Original => {}
                AttemptBody::Changed => current.attempt_request(&prepared.body),
            }
            let cell = AttemptCell::new(
                current,
                admission.take(),
                abort.clone(),
                operator_cancel.clone(),
            );
            let dispatch = Dispatch::new(cell.clone());
            let request = UpstreamRequest {
                upstream,
                path: route.upstream_path,
                query: parts.uri.query().map(str::to_owned),
                method: parts.method.clone(),
                headers: prepared.headers,
                body: ObservedUpload::wrap(Body::from(prepared.body), cell.clone()),
            };
            let deadline = self.config.response_header_timeout;
            let dispatched = tokio::select! {
                biased;
                () = cell.aborted() => Dispatched::Aborted,
                () = cell.operator_cancelled() => Dispatched::Cancelled,
                () = cell.ended() => Dispatched::Ended,
                result = tokio::time::timeout(deadline, self.upstream.send(request)) => match result {
                    Err(_) => Dispatched::Failed(SendFailure::HeaderTimeout),
                    Ok(Err(failure)) => Dispatched::Failed(failure),
                    Ok(Ok(response)) => Dispatched::Responded {
                        status: response.status().as_u16(),
                        response,
                    },
                },
            };
            let end = match run.step(dispatched, &plan.retry) {
                CallStep::FallOver { refused, next } => {
                    admission = cell.hand_over();
                    match refused {
                        // Read and record the refusal; nothing reached the client.
                        Refused::Response(response) => {
                            if let Ok(response) = dispatch.responded(response) {
                                Self::drain(response.into_body()).await;
                            }
                        }
                        Refused::Failure(failure) => {
                            let _first = dispatch.stopped(HttpEvent::SendFailed(failure));
                        }
                    }
                    run = next;
                    continue;
                }
                CallStep::Relay(response) => {
                    // The call stays cancellable while its response streams.
                    cell.hold(Box::new((registered.take(), user_permit.take())));
                    match dispatch.responded(response) {
                        Ok(response) => return Self::relayed(response),
                        Err(end) => end,
                    }
                }
                CallStep::Stop(CallStop::Failed(failure)) => {
                    dispatch.stopped(HttpEvent::SendFailed(failure))
                }
                CallStep::Stop(CallStop::Shutdown) => {
                    dispatch.stopped(HttpEvent::Cancelled(Canceller::Shutdown))
                }
                CallStep::Stop(CallStop::Operator) => {
                    dispatch.stopped(HttpEvent::Cancelled(Canceller::Operator))
                }
                CallStep::Stop(CallStop::Ended) => dispatch.concluded(),
            };
            return GatewayError::Ended(end).into_response();
        }
    }

    /// Apply the plan's transforms and the upstream entry's credential
    /// override to a copy of the request. `None` for an unknown upstream.
    fn prepare(
        &self,
        plan: &CallPlan,
        upstream: &UpstreamId,
        headers: &HeaderMap,
        original: &Bytes,
        decoded: &Decoded<'_>,
    ) -> Option<Prepared> {
        let entry = self.config.upstreams.get(upstream)?;
        let mut headers = headers.clone();
        let mut json = match decoded {
            Decoded::Identity(bytes) => RequestJson::new(bytes),
            Decoded::Decompressed(_) | Decoded::Opaque => RequestJson::encoded(),
        };
        let mut steps = Vec::new();
        for rule in &plan.transforms {
            for (index, step) in rule.steps.iter().enumerate() {
                let outcome = step.apply(&mut headers, &mut json);
                steps.push(step.record(rule, index, outcome));
            }
        }
        let (body, kind) = match json.changed() {
            Some(changed) => {
                headers.insert(
                    http::header::CONTENT_LENGTH,
                    HeaderValue::from(changed.len()),
                );
                (Bytes::from(changed), AttemptBody::Changed)
            }
            None => (original.clone(), AttemptBody::Original),
        };
        let auth = entry.authorize(&mut headers);
        let changes = match (auth, steps.is_empty()) {
            (AuthOverride::None, true) => None,
            (AuthOverride::None | AuthOverride::OperatorKey, _) => Recorded::of(&AttemptChanges {
                auth,
                steps: &steps,
                body: kind,
            })
            .ok(),
        };
        Some(Prepared {
            headers,
            body,
            start: AttemptStart {
                upstream: upstream.clone(),
                provider: entry.api.provider(),
                changes,
                body: kind,
            },
        })
    }

    /// Read a refused attempt's response, within bounds, so it is recorded.
    async fn drain(mut body: Body) {
        let mut read = 0_usize;
        let _bounded = tokio::time::timeout(Self::DRAIN_TIME, async {
            while let Some(Ok(frame)) = body.frame().await {
                read = read.saturating_add(frame.data_ref().map_or(0, Bytes::len));
                if read > Self::DRAIN_LIMIT {
                    break;
                }
            }
        })
        .await;
    }

    fn relayed(mut response: http::Response<Body>) -> Response {
        HeaderPolicy::RESPONSE.apply(response.headers_mut());
        response.into_response()
    }

    /// A WebSocket handshake: connect upstream first, so a refusal reaches the
    /// client as the upstream sent it, then upgrade the client and relay.
    async fn handshake(self: Arc<Self>, bound: &BoundRoute, request: Request) -> Response {
        let route = bound.forward;
        if !request.headers().contains_key(http::header::UPGRADE) {
            return Self::refuse(route.accepts, &request);
        }
        match self.config.websocket.mode {
            WebSocketMode::Refuse => {
                return Self::handshake_refused("WebSocket relay is disabled; use HTTP/SSE\n");
            }
            WebSocketMode::Relay => {}
        }
        if self.shutdown.is_draining() {
            return GatewayError::ShuttingDown.into_response();
        }
        let (mut parts, _body) = request.into_parts();
        // The client leg's key, reused upstream. Axum answers HTTP/2 extended
        // CONNECT without one; the upstream leg needs HTTP/1.1, so refuse it.
        let key = parts.headers.get(http::header::SEC_WEBSOCKET_KEY).cloned();
        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => upgrade,
            Err(rejection) => return rejection.into_response(),
        };
        let Some(key) = key else {
            return Self::handshake_refused("WebSocket requires an HTTP/1.1 handshake\n");
        };
        let attribution = Harness::attribution(route.harness, &parts.headers);
        let identified = self.config.users.identify(&parts.headers);
        HeaderPolicy::HANDSHAKE_REQUEST.apply(&mut parts.headers);
        // The channel's calls inherit the handshake's caller.
        let start = CallStart {
            transport: Transport::WebSocket,
            api: bound.api,
            endpoint: route.endpoint,
            method: Method::Get,
            config_revision: self.config.config_revision.clone(),
            attribution: attribution.clone(),
            caller: identified.clone().unwrap_or(Caller::Unidentified),
            operator_revision: None,
            plan: None,
            attempts: 1,
        };
        if let Err(refused) = identified {
            self.recorder
                .reject(None, start, None, CallRejection::IdentityRefused);
            return RenReply::unidentified(&refused).into_response();
        }
        // A channel counts as one call of its user's for its whole life. A
        // caller's own limit comes before the gateway's, as on HTTP.
        let Ok(user_permit) = self.user_limits.admit(&start.caller) else {
            return Self::handshake_refused(
                "WebSocket limit for this caller reached; use HTTP/SSE\n",
            );
        };
        let Ok(channel_permit) = self.channels.clone().try_acquire_owned() else {
            return Self::handshake_refused("WebSocket channel limit reached; use HTTP/SSE\n");
        };
        let Ok(admission) = self.admission.clone().try_acquire_owned() else {
            return GatewayError::Busy.into_response();
        };
        // WebSocket channels are pinned to the route's entry.
        let upstream = bound.upstream.clone();
        let tap = self.recorder.start(
            start.clone(),
            AttemptStart::unchanged(upstream.clone(), bound.api.provider()),
        );
        tap.end(ren_protocol::direction::Direction::Request);
        let channel = tap.channel();
        let cell = AttemptCell::new(
            tap,
            Some(admission),
            self.shutdown.abort_token(),
            CancellationToken::new(),
        );
        // A handshake has no body: its upload ends at once.
        drop(ObservedUpload::wrap(Body::empty(), cell.clone()));
        let dispatch = Dispatch::new(cell.clone());
        let handshake = UpstreamHandshake {
            upstream: upstream.clone(),
            path: route.upstream_path,
            query: parts.uri.query().map(str::to_owned),
            headers: parts.headers,
            key,
        };
        let deadline = self.config.response_header_timeout;
        let handshaken = tokio::select! {
            biased;
            () = cell.aborted() => Handshaken::Aborted,
            result = tokio::time::timeout(deadline, self.upstream.connect(handshake)) => match result {
                Err(_) => Handshaken::Failed(SendFailure::HeaderTimeout),
                Ok(Err(failure)) => Handshaken::Failed(failure),
                Ok(Ok(connected)) => Handshaken::Connected(connected),
            },
        };
        let end = match handshaken {
            Handshaken::Connected(Connected::Refused(response)) => {
                match dispatch.responded(response) {
                    Ok(response) => return Self::relayed(response),
                    Err(end) => end,
                }
            }
            Handshaken::Connected(Connected::Upgraded {
                status,
                mut headers,
                socket,
            }) => {
                let mut head = http::Response::new(UpstreamBody::empty());
                *head.status_mut() = status;
                *head.headers_mut() = headers.clone();
                // The handshake attempt is complete: its empty body ends here,
                // which releases its admission. Calls take their own.
                match dispatch.responded(head) {
                    Ok(handshake) => drop(handshake),
                    Err(end) => return GatewayError::Ended(end).into_response(),
                }
                let setup = websocket::ChannelSetup {
                    recorder: self.recorder.clone(),
                    tap: self.recorder.channel(channel),
                    admission: self.admission.clone(),
                    call: CallStart {
                        method: Method::Message,
                        ..start
                    },
                    shutdown: self.shutdown.clone(),
                    channel_permit,
                    user_permit,
                    tracked: self.shutdown.track_channel(),
                    upstream,
                    policy: self.config.policy.clone(),
                    controls: self.controls.clone(),
                    clock: self.clock.clone(),
                    deadlines: Deadlines {
                        keepalive: self.config.websocket.keepalive(),
                        first_message: self.config.response_header_timeout,
                        stream_idle: self.config.stream_idle_timeout,
                    },
                };
                let mut response = upgrade.on_upgrade(setup.relay(socket));
                HeaderPolicy::RESPONSE.apply(&mut headers);
                HeaderPolicy::HANDSHAKE_RESPONSE.apply(&mut headers);
                response.headers_mut().extend(headers);
                return response;
            }
            Handshaken::Failed(failure) => dispatch.stopped(HttpEvent::SendFailed(failure)),
            Handshaken::Aborted => dispatch.stopped(HttpEvent::Cancelled(Canceller::Shutdown)),
        };
        GatewayError::Ended(end).into_response()
    }
}

/// Why a request was answered before its body was read whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadRefusal {
    /// The buffer refused it: size, budget, timeouts, or a failed body.
    Buffer(BufferRefusal),
    /// Shutdown aborted the read at the drain deadline.
    Shutdown,
}

impl UploadRefusal {
    fn rejection(self) -> CallRejection {
        match self {
            Self::Buffer(refusal) => refusal.rejection(),
            Self::Shutdown => CallRejection::Shutdown,
        }
    }
}

/// Records a call as [`CallRejection::Abandoned`] if its handler is dropped
/// while the request is still being read: a closing connection drops the
/// handler without the read ever failing.
#[must_use = "dropping the guard records the call as abandoned"]
struct UploadGuard<'a> {
    recorder: &'a Recorder,
    call: Option<CallStart>,
}

impl<'a> UploadGuard<'a> {
    fn arm(recorder: &'a Recorder, call: &CallStart) -> Self {
        Self {
            recorder,
            call: Some(call.clone()),
        }
    }

    /// The read finished, successfully or not; the handler records the call.
    fn disarm(mut self) {
        self.call = None;
    }
}

impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        if let Some(call) = self.call.take() {
            self.recorder
                .reject(None, call, None, CallRejection::Abandoned);
        }
    }
}
