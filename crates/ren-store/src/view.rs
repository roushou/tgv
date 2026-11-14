//! Query views: the typed rows the read API and CLI return.
//!
//! Labels are read as [`Stored`], so a value written by a newer binary is shown
//! verbatim instead of failing the query.

use crate::{StoreError, query::Cursor};
use ren_depot::toolset::Generation;
use ren_harness::Harness;
use ren_harness::attribution::{HintIssue, HintSource};
use ren_harness::tools::CallPurpose;
use ren_harness::{
    drive::DriveEvent,
    settings::{ApprovalPolicy, Effort},
};
use ren_protocol::{
    direction::Direction,
    encoding::{ResponseEncoding, ResponseFormat},
    inspect::ModelOutcome,
    label::{Labeled, Stored},
    route::{Endpoint, Method, Provider, Transport, Wire},
    websocket::Payload,
};
use ren_recorder::Observation;
use ren_recorder::batch::Recorded;
use ren_recorder::caller::CallerKind;
use ren_recorder::inspection::{InspectionRecord, UsageRecord};
use ren_recorder::routing::OperatorRevision;
use ren_recorder::{
    capture::{
        AttemptPhase, AttemptTerminal, BodyEnding, CallRejection, CallState, ChannelEnd,
        ProcessExit, RunEnd, TerminalCause,
    },
    id::{AttemptId, CallId, ChannelId, RecordingId, RunId, SessionId, TurnId},
    time::UnixMillis,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, sqlite::SqliteRow};
use std::str::FromStr;

/// Typed column access with corruption reported as [`StoreError::Corrupt`].
pub(super) struct Columns<'r>(pub(super) &'r SqliteRow);

impl Columns<'_> {
    /// The observation log's columns, for [`Columns::observation`].
    pub(super) const OBSERVATION_SELECT: &'static str =
        "SELECT seq, at_wall, kind, channel, call, attempt FROM observations";

    pub(super) fn observation(&self) -> Result<Observation, StoreError> {
        Ok(Observation {
            seq: self.count("seq")?,
            at: self.time("at_wall")?,
            kind: self.label("kind")?,
            channel: self.id("channel")?,
            call: self.optional_id("call")?,
            attempt: self.optional_id("attempt")?,
        })
    }

    pub(super) fn id<T: FromStr>(&self, column: &str) -> Result<T, StoreError> {
        let text: String = self.0.try_get(column)?;
        text.parse().map_err(|_| StoreError::Corrupt("identifier"))
    }

    pub(super) fn optional_id<T: FromStr>(&self, column: &str) -> Result<Option<T>, StoreError> {
        let text: Option<String> = self.0.try_get(column)?;
        text.map(|text| text.parse().map_err(|_| StoreError::Corrupt("identifier")))
            .transpose()
    }

    pub(super) fn label<T: Labeled>(&self, column: &str) -> Result<Stored<T>, StoreError> {
        let text: String = self.0.try_get(column)?;
        Ok(Stored::from_label(text.as_str()))
    }

    pub(super) fn optional_label<T: Labeled>(
        &self,
        column: &str,
    ) -> Result<Option<Stored<T>>, StoreError> {
        let text: Option<String> = self.0.try_get(column)?;
        Ok(text.as_deref().map(Stored::from_label))
    }

    pub(super) fn count(&self, column: &str) -> Result<u64, StoreError> {
        Ok(self.0.try_get(column)?)
    }

    pub(super) fn optional_count(&self, column: &str) -> Result<Option<u64>, StoreError> {
        Ok(self.0.try_get(column)?)
    }

    pub(super) fn time(&self, column: &str) -> Result<UnixMillis, StoreError> {
        Ok(UnixMillis::from_millis(self.0.try_get(column)?))
    }

    pub(super) fn optional_time(&self, column: &str) -> Result<Option<UnixMillis>, StoreError> {
        let value: Option<u64> = self.0.try_get(column)?;
        Ok(value.map(UnixMillis::from_millis))
    }

    pub(super) fn text(&self, column: &str) -> Result<Option<String>, StoreError> {
        Ok(self.0.try_get(column)?)
    }

    pub(super) fn json<T: for<'de> Deserialize<'de>>(
        &self,
        column: &str,
    ) -> Result<Option<T>, StoreError> {
        let text: Option<String> = self.0.try_get(column)?;
        text.map(|text| serde_json::from_str(&text).map_err(StoreError::from))
            .transpose()
    }

    pub(super) fn flag(&self, column: &str) -> Result<bool, StoreError> {
        Ok(self.0.try_get(column)?)
    }
}

/// A body's capture accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyView {
    /// Bytes the relay saw; unknown until the body's end is recorded.
    pub observed_bytes: Option<u64>,
    /// Bytes committed to storage. Less than observed means bytes were omitted.
    pub stored_bytes: u64,
    pub ending: Stored<BodyEnding>,
}

impl BodyView {
    pub(super) fn from_columns(
        row: &Columns<'_>,
        prefix: &str,
    ) -> Result<Option<Self>, StoreError> {
        let ending: Option<String> = row.0.try_get(format!("{prefix}_ending").as_str())?;
        let Some(ending) = ending else {
            return Ok(None);
        };
        Ok(Some(Self {
            observed_bytes: row.0.try_get(format!("{prefix}_observed").as_str())?,
            stored_bytes: row.count(&format!("{prefix}_stored"))?,
            ending: Stored::from_label(ending.as_str()),
        }))
    }

    /// Every byte observed is stored and the end was seen.
    pub fn complete(&self) -> bool {
        self.ending.known() == Some(BodyEnding::Ended)
            && self.observed_bytes == Some(self.stored_bytes)
    }
}

/// Attribution as stored, keeping unknown labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AttributionView {
    pub hint: Option<SessionHintView>,
    pub issues: Vec<Stored<HintIssue>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHintView {
    pub source: Stored<HintSource>,
    pub session: String,
    pub turn: Option<String>,
    pub thread: Option<String>,
}

/// Who made a call, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerView {
    pub kind: Stored<CallerKind>,
    /// The named user it proved to be.
    pub user: Option<String>,
    /// The Tailscale login the request carried.
    pub login: Option<String>,
}

impl CallerView {
    fn from_columns(row: &Columns<'_>) -> Result<Option<Self>, StoreError> {
        let Some(kind) = row.optional_label("caller")? else {
            return Ok(None);
        };
        Ok(Some(Self {
            kind,
            user: row.text("user")?,
            login: row.text("tailscale_login")?,
        }))
    }
}

/// One call, as listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallView {
    pub id: CallId,
    pub channel: ChannelId,
    pub started_at: UnixMillis,
    pub ended_at: Option<UnixMillis>,
    pub provider: Stored<Provider>,
    /// The wire format; absent when an older version's provider label was
    /// unknown to the migration that filled it.
    pub wire: Option<Stored<Wire>>,
    pub endpoint: Stored<Endpoint>,
    pub method: Stored<Method>,
    pub config_revision: String,
    pub session: Option<RecordingId>,
    pub turn: Option<TurnId>,
    pub attribution: AttributionView,
    /// Who made the call; absent for calls recorded before callers were.
    pub caller: Option<CallerView>,
    pub state: Stored<CallState>,
    pub terminal: Option<Stored<AttemptTerminal>>,
    pub status: Option<u16>,
    pub model: Option<String>,
    pub outcome: Option<Stored<ModelOutcome>>,
    pub stop_reason: Option<String>,
    pub tool_names: Vec<String>,
    pub usage: Option<UsageRecord>,
    /// Both bodies ended and were stored without omissions.
    pub capture_complete: bool,
    pub request: Option<BodyView>,
    pub response: Option<BodyView>,
    /// Why Ren answered without forwarding; absent for a forwarded call.
    pub rejection: Option<Stored<CallRejection>>,
    /// The operator state revision the call was planned against.
    pub operator_revision: Option<u64>,
    /// The recorded routing plan, as written.
    pub plan: Option<serde_json::Value>,
    /// Why the client sent it (work, or a check of its own such as Claude
    /// Code's quota probe), from the call's digest; absent until derived.
    pub purpose: Option<Stored<CallPurpose>>,
}

impl CallView {
    /// Columns every call query selects, joined to the request body (`rq`) and
    /// the final attempt's response body (`rs`).
    pub(super) const SELECT: &'static str = "SELECT c.id, c.channel, c.started_at, c.ended_at, c.provider, c.wire, c.endpoint, c.method, c.config_revision, c.session, c.turn, c.attribution, c.caller, c.user, c.tailscale_login, c.state, c.terminal, c.status, c.model, c.outcome, c.stop_reason, c.tool_names, c.usage, c.capture_complete, c.rejection, c.operator_revision, c.plan, rq.observed_bytes AS rq_observed, rq.stored_bytes AS rq_stored, rq.ending AS rq_ending, rs.observed_bytes AS rs_observed, rs.stored_bytes AS rs_stored, rs.ending AS rs_ending, dp.purpose AS purpose FROM calls c LEFT JOIN bodies rq ON rq.owner = c.id AND rq.direction = 'request' LEFT JOIN bodies rs ON rs.owner = c.final_attempt AND rs.direction = 'response' LEFT JOIN call_digests dp ON dp.call = c.id";

    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let row = Columns(row);
        Ok(Self {
            id: row.id("id")?,
            channel: row.id("channel")?,
            started_at: row.time("started_at")?,
            ended_at: row.optional_time("ended_at")?,
            provider: row.label("provider")?,
            wire: row.optional_label("wire")?,
            endpoint: row.label("endpoint")?,
            method: row.label("method")?,
            config_revision: row.text("config_revision")?.unwrap_or_default(),
            session: row.optional_id("session")?,
            turn: row.optional_id("turn")?,
            attribution: row.json("attribution")?.unwrap_or_default(),
            caller: CallerView::from_columns(&row)?,
            state: row.label("state")?,
            terminal: row.optional_label("terminal")?,
            status: row.0.try_get("status")?,
            model: row.text("model")?,
            outcome: row.optional_label("outcome")?,
            stop_reason: row.text("stop_reason")?,
            tool_names: row.json("tool_names")?.unwrap_or_default(),
            usage: row.json("usage")?,
            capture_complete: row.flag("capture_complete")?,
            request: BodyView::from_columns(&row, "rq")?,
            response: BodyView::from_columns(&row, "rs")?,
            rejection: row.optional_label("rejection")?,
            operator_revision: row.0.try_get("operator_revision")?,
            plan: row.json("plan")?,
            purpose: row.optional_label("purpose")?,
        })
    }

    pub fn cursor(&self) -> Cursor {
        Cursor {
            started_at: self.started_at,
            call: self.id,
        }
    }
}

/// One upstream attempt of a call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttemptView {
    pub id: AttemptId,
    pub ordinal: u32,
    pub started_at: UnixMillis,
    pub ended_at: Option<UnixMillis>,
    /// The upstream entry's id.
    pub upstream: String,
    /// The upstream entry's provider.
    pub provider: Option<Stored<Provider>>,
    pub status: Option<u16>,
    pub response_format: Option<Stored<ResponseFormat>>,
    pub response_encoding: Option<Stored<ResponseEncoding>>,
    pub terminal: Option<Stored<AttemptTerminal>>,
    /// Why it ended, where the terminal alone does not say.
    pub terminal_cause: Option<Stored<TerminalCause>>,
    /// How far it had got when it ended.
    pub terminal_phase: Option<Stored<AttemptPhase>>,
    pub response: Option<BodyView>,
    pub inspection_version: Option<u32>,
    pub inspection: Option<InspectionRecord>,
    /// What Ren changed in this attempt's request, as recorded.
    pub changes: Option<serde_json::Value>,
    /// The changed request body this attempt sent, if a transform changed it.
    pub request: Option<BodyView>,
}

impl AttemptView {
    pub(super) const SELECT: &'static str = "SELECT a.id, a.ordinal, a.started_at, a.ended_at, a.upstream, a.provider, a.status, a.response_format, a.response_encoding, a.terminal, a.terminal_cause, a.terminal_phase, a.inspection, a.inspection_version, a.changes, rs.observed_bytes AS rs_observed, rs.stored_bytes AS rs_stored, rs.ending AS rs_ending, rq.observed_bytes AS rq_observed, rq.stored_bytes AS rq_stored, rq.ending AS rq_ending FROM attempts a LEFT JOIN bodies rs ON rs.owner = a.id AND rs.direction = 'response' LEFT JOIN bodies rq ON rq.owner = a.id AND rq.direction = 'request'";

    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let row = Columns(row);
        Ok(Self {
            id: row.id("id")?,
            ordinal: row.0.try_get("ordinal")?,
            started_at: row.time("started_at")?,
            ended_at: row.optional_time("ended_at")?,
            upstream: row.text("upstream")?.unwrap_or_default(),
            provider: row.optional_label("provider")?,
            status: row.0.try_get("status")?,
            response_format: row.optional_label("response_format")?,
            response_encoding: row.optional_label("response_encoding")?,
            terminal: row.optional_label("terminal")?,
            terminal_cause: row.optional_label("terminal_cause")?,
            terminal_phase: row.optional_label("terminal_phase")?,
            response: BodyView::from_columns(&row, "rs")?,
            inspection_version: row.0.try_get("inspection_version")?,
            inspection: row.json("inspection")?,
            changes: row.json("changes")?,
            request: BodyView::from_columns(&row, "rq")?,
        })
    }
}

/// A call with its attempts and log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CallDetail {
    pub call: CallView,
    pub transport: Stored<Transport>,
    /// The channel that carried the call: for WebSocket, the upgraded socket
    /// shared with the channel's other calls.
    pub channel: ChannelView,
    pub attempts: Vec<AttemptView>,
    pub observations: Vec<Observation>,
    /// WebSocket message boundaries in the call's request body and its final
    /// attempt's response body, in body order. Empty for HTTP.
    pub messages: Vec<MessageView>,
}

/// One client connection or exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelView {
    pub id: ChannelId,
    pub transport: Stored<Transport>,
    pub opened_at: UnixMillis,
    /// When a WebSocket channel ended; absent while open, and for HTTP.
    pub closed_at: Option<UnixMillis>,
    pub ending: Option<Stored<ChannelEnd>>,
    pub close_code: Option<u16>,
    /// Messages forwarded while no call was open: counted, not stored.
    pub unattributed_messages: u64,
    pub unattributed_bytes: u64,
}

impl ChannelView {
    pub(super) const SELECT: &'static str = "SELECT id, transport, opened_at, closed_at, ending, close_code, unattributed_messages, unattributed_bytes FROM channels";

    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let columns = Columns(row);
        Ok(Self {
            id: columns.id("id")?,
            transport: columns.label("transport")?,
            opened_at: columns.time("opened_at")?,
            closed_at: columns.optional_time("closed_at")?,
            ending: columns.optional_label("ending")?,
            close_code: row.try_get("close_code")?,
            unattributed_messages: columns.count("unattributed_messages")?,
            unattributed_bytes: columns.count("unattributed_bytes")?,
        })
    }
}

/// Where one WebSocket message sits inside a body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageView {
    pub direction: Stored<Direction>,
    pub offset: u64,
    pub len: u64,
    pub payload: Stored<Payload>,
    pub at: UnixMillis,
}

impl MessageView {
    /// Rows for a call: `?1` is the call, `?2` the request direction, `?3`
    /// the response direction, `?4` the final attempt.
    pub(super) const SELECT_FOR_CALL: &'static str = "SELECT direction, byte_offset, len, payload, at_wall FROM messages WHERE (owner = ?1 AND direction = ?2) OR (owner = ?4 AND direction = ?3) ORDER BY direction, byte_offset LIMIT ?5";

    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let columns = Columns(row);
        Ok(Self {
            direction: columns.label("direction")?,
            offset: columns.count("byte_offset")?,
            len: columns.count("len")?,
            payload: columns.label("payload")?,
            at: columns.time("at_wall")?,
        })
    }
}

/// One page of calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallPage {
    pub calls: Vec<CallView>,
    /// Pass as `after` for the next page; absent on the last page.
    pub next: Option<Cursor>,
}

/// A group of calls a client attributed to one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingView {
    pub id: RecordingId,
    pub provider: Stored<Provider>,
    pub source: Stored<HintSource>,
    pub native_id: String,
    pub first_seen: UnixMillis,
    pub last_seen: UnixMillis,
    /// When the operator archived it, while it has no newer traffic.
    pub archived_at: Option<UnixMillis>,
    pub call_count: u64,
    pub turn_count: u64,
    /// Calls with an error status, a failed transport, or a failed model
    /// outcome: the same calls the `errors` status filter finds.
    pub error_count: u64,
    /// Provider-reported tokens summed over the session's calls.
    pub tokens: TokenTotals,
}

/// Provider-reported tokens summed over calls; each is `None` when no call
/// reported it. Input counts cached context too, whichever way each call's
/// provider accounts it, because cached context is most of a coding agent's
/// input; `cache_read` is the part of it read from the provider's cache.
/// Token counts are not subscription allowance or charges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
}

impl TokenTotals {
    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let row = Columns(row);
        Ok(Self {
            input: row.optional_count("input_tokens")?,
            output: row.optional_count("output_tokens")?,
            cache_read: row.optional_count("cache_read_tokens")?,
        })
    }
}

/// Calls, errors, and tokens over some set of calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UsageCounts {
    pub calls: u64,
    /// The calls the `errors` status filter finds.
    pub errors: u64,
    pub tokens: TokenTotals,
}

impl UsageCounts {
    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let columns = Columns(row);
        Ok(Self {
            calls: columns.count("calls")?,
            errors: columns.count("errors")?,
            tokens: TokenTotals::from_row(row)?,
        })
    }
}

/// Usage over a time range, by the hour, by model, and by user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageView {
    pub totals: UsageCounts,
    /// Hours that had calls, oldest first, keyed by the hour's start (UTC).
    /// Clients group them into local days.
    pub hours: Vec<UsageBucket>,
    /// Each reported model's usage, most input first. Calls without a
    /// reported model are grouped under `null`.
    pub models: Vec<ModelUsage>,
    /// Each named user's usage, most input first. Calls no named user made
    /// (unlisted or unidentified callers, and calls recorded before callers
    /// were) are grouped under `null`.
    pub users: Vec<UserUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UsageBucket {
    pub start: UnixMillis,
    #[serde(flatten)]
    pub usage: UsageCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelUsage {
    pub provider: Stored<Provider>,
    pub model: Option<String>,
    #[serde(flatten)]
    pub usage: UsageCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserUsage {
    pub user: Option<String>,
    #[serde(flatten)]
    pub usage: UsageCounts,
}

impl RecordingView {
    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let row = Columns(row);
        Ok(Self {
            id: row.id("id")?,
            provider: row.label("provider")?,
            source: row.label("source")?,
            native_id: row.text("native_id")?.unwrap_or_default(),
            first_seen: row.time("first_seen")?,
            last_seen: row.time("last_seen")?,
            archived_at: row.optional_time("archived_at")?,
            call_count: row.count("calls")?,
            turn_count: row.count("turn_count")?,
            error_count: row.count("errors")?,
            tokens: TokenTotals::from_row(row.0)?,
        })
    }
}

/// One recorded operator command, as the gateway serialized it.
#[derive(Debug, Clone, Serialize)]
pub struct OperatorActionView {
    pub seq: u64,
    pub at: UnixMillis,
    /// The operator state revision the command made.
    pub revision: OperatorRevision,
    /// Who issued it: the recorder's `Actor` document.
    pub actor: Recorded,
    /// The gateway's command document, never interpreted by the store.
    pub command: Recorded,
}

impl OperatorActionView {
    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let row = Columns(row);
        let document = |column| {
            row.text(column)?
                .ok_or(StoreError::Corrupt("operator action"))
                .and_then(|text| {
                    Recorded::restore(text).map_err(|_| StoreError::Corrupt("operator action"))
                })
        };
        Ok(Self {
            seq: row.count("seq")?,
            at: row.time("at_wall")?,
            revision: OperatorRevision::restore(row.count("revision")?),
            actor: document("actor")?,
            command: document("command")?,
        })
    }
}

/// A run's conversation, whole: its entries, and those of the runs before
/// it that continued the same client conversation, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationView {
    pub entries: Vec<RunEntryView>,
    /// Older entries were left out: the conversation holds more than one
    /// read returns.
    pub earlier: bool,
}

/// One entry of a run's conversation: an event as its client reported it,
/// in the order it came.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEntryView {
    pub run: RunId,
    pub seq: u64,
    pub at: UnixMillis,
    /// The client's turn it belongs to, once one started.
    pub turn: Option<String>,
    pub event: DriveEvent,
}

/// What an archive or delete would touch, read before it is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryTarget {
    /// Calls of its sessions still open.
    pub open_calls: u64,
    /// Its runs that have not ended.
    pub running_runs: u64,
}

/// One agent run, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunView {
    pub id: RunId,
    pub harness: Stored<Harness>,
    pub workdir: String,
    pub started_at: UnixMillis,
    /// The client's own conversation id, once it opened one.
    pub thread: Option<String>,
    /// The session whose calls name the run's thread as their session: the
    /// run's model traffic, once a call was recorded.
    pub session: Option<RecordingId>,
    /// The operator session the run served; `None` for a run recorded
    /// before sessions were, until the migration gave it one.
    pub operator_session: Option<SessionId>,
    pub ended_at: Option<UnixMillis>,
    /// How the run ended; `None` while it runs.
    pub end: Option<Stored<RunEnd>>,
    /// How its process exited, when one did.
    pub exit: Option<ProcessExit>,
    /// When the operator archived it; it is listed as archived once ended.
    pub archived_at: Option<UnixMillis>,
    /// The model it started on, as its client names it; `None` is the
    /// client's own (or a run recorded before settings were).
    pub model: Option<String>,
    pub effort: Option<Stored<Effort>>,
    pub approvals: Option<Stored<ApprovalPolicy>>,
    /// The toolset generation on its search path: 0 when Ren had installed
    /// none; `None` when unknown (the tool store was unreadable, or the run
    /// was recorded before this was).
    pub toolset: Option<Generation>,
}

impl RunView {
    pub(super) const SELECT: &'static str = "SELECT id, operator_session, harness, workdir, started_at, thread, ended_at, run_end, exit_kind, exit_value, archived_at, model, effort, approvals, toolset_generation, (SELECT sessions.id FROM sessions WHERE sessions.native_id = runs.thread ORDER BY sessions.first_seen, sessions.id LIMIT 1) AS session FROM runs";

    pub(super) fn from_row(row: &SqliteRow) -> Result<Self, StoreError> {
        let row = Columns(row);
        let value: Option<i32> = row.0.try_get("exit_value")?;
        let exit = match (row.text("exit_kind")?.as_deref(), value) {
            (Some("code"), Some(code)) => Some(ProcessExit::Code(code)),
            (Some("signal"), Some(signal)) => Some(ProcessExit::Signal(signal)),
            (Some("unknown"), None) => Some(ProcessExit::Unknown),
            (None, None) => None,
            (Some(_), _) | (None, Some(_)) => return Err(StoreError::Corrupt("run exit")),
        };
        Ok(Self {
            id: row.id("id")?,
            harness: row.label("harness")?,
            workdir: row.text("workdir")?.ok_or(StoreError::Corrupt("run"))?,
            started_at: row.time("started_at")?,
            thread: row.text("thread")?,
            session: row.optional_id("session")?,
            operator_session: row.optional_id("operator_session")?,
            ended_at: row.optional_time("ended_at")?,
            end: row.optional_label("run_end")?,
            exit,
            archived_at: row.optional_time("archived_at")?,
            model: row.text("model")?,
            effort: row.optional_label("effort")?,
            approvals: row.optional_label("approvals")?,
            toolset: row
                .0
                .try_get::<Option<i64>, _>("toolset_generation")?
                .map(|number| u64::try_from(number).map(Generation::from))
                .transpose()
                .map_err(|_| StoreError::Corrupt("run toolset"))?,
        })
    }
}

/// One session an operator works in, as recorded, with the runs (client
/// processes) that served it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: SessionId,
    pub harness: Stored<Harness>,
    pub workdir: String,
    pub created_at: UnixMillis,
    /// The settings its next turn runs on; `None` is the client's own.
    pub model: Option<String>,
    pub effort: Option<Stored<Effort>>,
    pub approvals: Option<Stored<ApprovalPolicy>>,
    pub archived_at: Option<UnixMillis>,
    /// When its first turn started; `None` while it is a draft (no prompt
    /// has gone out).
    pub first_turn_at: Option<UnixMillis>,
    /// The first line of its first prompt, as its client echoed it, cut to
    /// [`SessionView::FIRST_PROMPT_CHARS`]; `None` until one is recorded.
    pub first_prompt: Option<String>,
    /// Its runs, newest first.
    pub runs: Vec<RunView>,
}

impl SessionView {
    pub(super) const SELECT: &'static str = "SELECT id, harness, workdir, created_at, model, effort, approvals, archived_at, (SELECT MIN(e.at) FROM run_entries e JOIN runs r ON r.id = e.run WHERE r.operator_session = operator_sessions.id AND e.kind = 'turn_started') AS first_turn_at, (SELECT json_extract(e.event, '$.item.read.text') FROM run_entries e JOIN runs r ON r.id = e.run WHERE r.operator_session = operator_sessions.id AND e.kind = 'item' AND json_extract(e.event, '$.item.read.as') = 'prompt' ORDER BY e.at, r.started_at, e.seq LIMIT 1) AS first_prompt FROM operator_sessions";

    /// The most of a first prompt a session view carries.
    pub const FIRST_PROMPT_CHARS: usize = 200;

    /// The session in `row`, with `runs`.
    pub(super) fn from_row(row: &SqliteRow, runs: Vec<RunView>) -> Result<Self, StoreError> {
        let row = Columns(row);
        Ok(Self {
            id: row.id("id")?,
            harness: row.label("harness")?,
            workdir: row.text("workdir")?.ok_or(StoreError::Corrupt("session"))?,
            created_at: row.time("created_at")?,
            model: row.text("model")?,
            effort: row.optional_label("effort")?,
            approvals: row.optional_label("approvals")?,
            archived_at: row.optional_time("archived_at")?,
            first_turn_at: row.optional_time("first_turn_at")?,
            first_prompt: row
                .text("first_prompt")?
                .and_then(|text| Self::headline(&text)),
            runs,
        })
    }

    /// A prompt's first line with text, cut to its first
    /// [`Self::FIRST_PROMPT_CHARS`] characters; `None` when it has none.
    fn headline(text: &str) -> Option<String> {
        let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
        Some(line.chars().take(Self::FIRST_PROMPT_CHARS).collect())
    }
}

/// A bounded, contiguous prefix of a body, for previews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyPrefix {
    pub body: BodyView,
    pub bytes: Vec<u8>,
    /// Stored bytes are missing before the end of the prefix.
    pub gap: bool,
    /// The body continues past the prefix limit.
    pub truncated: bool,
    /// The `Content-Encoding` its attempt recorded; `None` for request bodies.
    pub encoding: Option<Stored<ResponseEncoding>>,
}

/// Real disk usage of one database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FileUsage {
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
}

impl FileUsage {
    pub fn live_bytes(self) -> u64 {
        self.page_count
            .saturating_sub(self.freelist_count)
            .saturating_mul(self.page_size)
    }

    pub fn free_bytes(self) -> u64 {
        self.freelist_count.saturating_mul(self.page_size)
    }
}

/// Disk usage of the store's files, as retention measures it. The search
/// index is part of the metadata file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiskUsage {
    pub metadata: FileUsage,
    pub blobs: FileUsage,
}

impl DiskUsage {
    /// What retention budgets: live pages of both store files.
    pub fn live_bytes(self) -> u64 {
        self.metadata
            .live_bytes()
            .saturating_add(self.blobs.live_bytes())
    }
}

/// What is retained, and how well chunk deduplication works on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StorageView {
    pub disk: DiskUsage,
    pub calls: u64,
    pub open_calls: u64,
    /// Calls whose digest the recorder has derived: every finished call,
    /// except while history recorded before digests is derived.
    pub digested_calls: u64,
    pub chunks: u64,
    /// Raw bytes all stored segments reference.
    pub referenced_bytes: u64,
    /// Raw bytes of the distinct chunks.
    pub unique_bytes: u64,
    /// Bytes the chunks occupy after compression.
    pub compressed_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::{CallPage, RecordingView};
    use serde_json::{Value, json};

    /// What the read API sends for a call listing, every optional field
    /// present, and some labels this binary does not know.
    fn call_page() -> Value {
        json!({
            "calls": [{
                "id": "00000000-0000-0000-0000-000000000001",
                "channel": "00000000-0000-0000-0000-000000000002",
                "started_at": 1000,
                "ended_at": 1500,
                "provider": "anthropic",
                "wire": "a_newer_wire",
                "endpoint": "messages",
                "method": "post",
                "config_revision": "r1",
                "session": "00000000-0000-0000-0000-000000000003",
                "turn": "00000000-0000-0000-0000-000000000004",
                "attribution": {
                    "hint": {
                        "source": "claude_code_headers",
                        "session": "s",
                        "turn": "t",
                        "thread": null
                    },
                    "issues": []
                },
                "caller": { "kind": "user_token", "user": "acme", "login": null },
                "state": "a_newer_state",
                "terminal": "completed",
                "status": 200,
                "model": "model",
                "outcome": "completed",
                "stop_reason": "end_turn",
                "tool_names": ["Read"],
                "usage": {
                    "accounting": "excludes_cache",
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "cache_read_tokens": 30,
                    "cache_creation_tokens": 40,
                    "reasoning_tokens": null
                },
                "capture_complete": true,
                "request": { "observed_bytes": 100, "stored_bytes": 100, "ending": "ended" },
                "response": { "observed_bytes": null, "stored_bytes": 0, "ending": "open" },
                "rejection": null,
                "operator_revision": 3,
                "plan": { "candidates": ["a"] },
                "purpose": "work"
            }],
            "next": "1000:00000000-0000-0000-0000-000000000001"
        })
    }

    #[test]
    fn a_call_page_reads_back_as_sent() {
        let page: CallPage = serde_json::from_value(call_page()).unwrap();
        assert_eq!(page.calls[0].wire.as_ref().unwrap().known(), None);
        assert_eq!(page.next, Some(page.calls[0].cursor()));
        assert_eq!(serde_json::to_value(&page).unwrap(), call_page());
    }

    #[test]
    fn a_session_reads_back_as_sent() {
        let sent = json!({
            "id": "00000000-0000-0000-0000-000000000003",
            "provider": "openai",
            "source": "a_newer_source",
            "native_id": "native",
            "first_seen": 1000,
            "last_seen": 2000,
            "archived_at": 1500,
            "call_count": 4,
            "turn_count": 2,
            "error_count": 1,
            "tokens": { "input": 10, "output": null, "cache_read": 5 }
        });
        let session: RecordingView = serde_json::from_value(sent.clone()).unwrap();
        assert_eq!(session.source.known(), None);
        assert_eq!(serde_json::to_value(&session).unwrap(), sent);
    }
}
