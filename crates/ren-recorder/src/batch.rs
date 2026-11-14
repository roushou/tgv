//! A group of writes committed together: chunk bytes, then the metadata and
//! log rows that reference them.
//!
//! A batch is plain data built by the recorder. [`super::Store::commit`]
//! applies it: chunks commit first, then one metadata transaction writes every
//! operation together with its observation. Each call's operations are in the
//! order the recorder received them, so a terminal never commits ahead of its
//! call's data.

use super::inspection::InspectionRecord;
use crate::caller::Caller;
use crate::digest::CallDigest;
use crate::{
    capture::{BodyEnding, CallRejection, ChannelEnd, ProcessExit, RunEnd, TerminalRecord},
    id::{AttemptId, CallId, ChannelId, RecordingId, RunId, SessionId},
    routing::{ConfigRevision, OperatorRevision, UpstreamId},
    time::{Moment, UnixMillis},
};
use bytes::Bytes;
use ren_depot::toolset::Generation;
use ren_harness::{Harness, attribution::Attribution, drive::DriveEvent, settings::RunSettings};
use ren_protocol::{
    direction::Direction,
    encoding::{ResponseEncoding, ResponseFormat},
    native::{NativeThreadId, NativeTurnId},
    route::{Endpoint, Method, Provider, Transport, Wire},
    websocket::Payload,
};
use serde::Serialize;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use strum::{Display, EnumString, IntoStaticStr, VariantArray};

#[derive(Debug, Default)]
pub struct Batch {
    chunks: Vec<Bytes>,
    ops: Vec<Op>,
}

/// An index into a batch's chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRef(usize);

impl ChunkRef {
    /// The position in [`Batch::chunks`].
    pub fn index(self) -> usize {
        self.0
    }
}

impl Batch {
    pub fn add_chunk(&mut self, bytes: Bytes) -> ChunkRef {
        self.chunks.push(bytes);
        ChunkRef(self.chunks.len().saturating_sub(1))
    }

    pub fn push(&mut self, op: Op) {
        self.ops.push(op);
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.chunks.is_empty()
    }

    pub fn ops(&self) -> usize {
        self.ops.len()
    }

    /// The chunk bytes, in the order [`ChunkRef`]s index them.
    pub fn chunks(&self) -> &[Bytes] {
        &self.chunks
    }

    /// The operations, in the order they were recorded.
    pub fn operations(&self) -> &[Op] {
        &self.ops
    }

    /// The lifecycle operations alone, for a retry after a failed commit. The
    /// body bytes are lost, and the bodies' stored byte counts say so.
    pub fn without_data(self) -> Self {
        Self {
            chunks: Vec::new(),
            ops: self
                .ops
                .into_iter()
                .filter(|op| match op {
                    // Derived data: derived again from the stored bodies.
                    Op::Segments { .. } | Op::Digest { .. } => false,
                    // Boundaries stay: the bodies' stored byte counts show
                    // which bytes they frame were lost.
                    Op::Message { .. }
                    | Op::OpenCall(_)
                    | Op::StartAttempt(_)
                    | Op::ResponseHead(_)
                    | Op::FinishAttempt(_)
                    | Op::FinishCall(_)
                    | Op::CloseChannel(_)
                    | Op::Operator(_)
                    | Op::Session(_)
                    | Op::Run(_)
                    | Op::RunEntry(_)
                    | Op::History(_) => true,
                })
                .collect(),
        }
    }

    /// Raw bytes of the body data in this batch.
    pub fn data_bytes(&self) -> u64 {
        self.chunks
            .iter()
            .map(|chunk| u64::try_from(chunk.len()).unwrap_or(u64::MAX))
            .sum()
    }
}

/// Which captured byte stream: a call's request as the client sent it, an
/// attempt's request as Ren changed it, or an attempt's response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyKey {
    Request(CallId),
    AttemptRequest(AttemptId),
    Response(AttemptId),
}

impl BodyKey {
    pub fn owner(self) -> String {
        match self {
            Self::Request(call) => call.to_string(),
            Self::AttemptRequest(attempt) | Self::Response(attempt) => attempt.to_string(),
        }
    }

    pub fn direction(self) -> Direction {
        match self {
            Self::Request(_) | Self::AttemptRequest(_) => Direction::Request,
            Self::Response(_) => Direction::Response,
        }
    }
}

/// A decision document, serialized once when it is recorded: a call's plan,
/// or what Ren changed in an attempt.
#[derive(Debug, Clone)]
pub struct Recorded(Box<serde_json::value::RawValue>);

impl Recorded {
    pub fn of<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        serde_json::value::to_raw_value(value).map(Self)
    }

    /// A document read back from storage; it must still be valid JSON.
    pub fn restore(text: String) -> Result<Self, serde_json::Error> {
        serde_json::value::RawValue::from_string(text).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.get()
    }
}

impl Serialize for Recorded {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// A stored range of a body: `chunk`'s bytes start at `offset`.
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub offset: u64,
    pub chunk: ChunkRef,
}

#[derive(Debug)]
pub enum Op {
    OpenCall(OpenCall),
    StartAttempt(StartAttempt),
    ResponseHead(ResponseHead),
    Segments {
        body: BodyKey,
        segments: Vec<Segment>,
    },
    /// Where one WebSocket message starts in a body.
    Message {
        body: BodyKey,
        mark: MessageMark,
    },
    FinishAttempt(FinishAttempt),
    FinishCall(FinishCall),
    CloseChannel(CloseChannel),
    Operator(OperatorAction),
    Session(SessionRecord),
    Run(RunRecord),
    /// One entry of a run's conversation. It is stored, not logged: the
    /// run feed carries it live.
    RunEntry(RunEntry),
    History(HistoryEdit),
    /// What a finished call's bodies showed, derived as they were recorded
    /// or again from storage. It replaces any earlier digest of the call.
    Digest {
        call: CallId,
        digest: Box<CallDigest>,
    },
}

/// Who issued an operator command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "login")]
pub enum Actor {
    /// A request on the loopback listener (the CLI or a local tool).
    Loopback,
    /// A Tailscale Serve user, by login name.
    Tailscale(String),
}

/// An operator command and the state revision it made. The command and state
/// are the gateway's documents, serialized when recorded; the recorder and
/// store keep them as they are and never interpret them.
#[derive(Debug, Clone, Serialize)]
pub struct OperatorAction {
    #[serde(skip)]
    pub at: Moment,
    pub revision: OperatorRevision,
    pub actor: Actor,
    pub command: Recorded,
    /// The state the command made, stored as the current snapshot.
    #[serde(skip)]
    pub state: Recorded,
}

/// One entry of a run's conversation, whole, as its client reported it:
/// what the run's transcript is read from. An item reported again (a
/// command that finished, a reply that completed) replaces the entry it
/// had. Streamed pieces are not entries; the item they make is.
#[derive(Debug, Clone)]
pub struct RunEntry {
    pub at: Moment,
    pub run: RunId,
    /// The turn it belongs to, once the client started one.
    pub turn: Option<NativeTurnId>,
    pub event: DriveEvent,
}

/// The operator archived, unarchived, or deleted what was recorded of a
/// session or a run. Not tied to a channel or call.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEdit {
    #[serde(skip)]
    pub at: Moment,
    pub actor: Actor,
    pub subject: HistorySubject,
    pub change: HistoryChange,
}

/// What a history edit applies to: a recording (its `kind` is `session`, as
/// the read API names recordings), a run with the recording its client
/// recorded, when there is one, or an operator session with all its runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum HistorySubject {
    Session(RecordingId),
    Run(RunId),
    OperatorSession(SessionId),
}

/// What a history edit does.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum HistoryChange {
    /// Hide it from listings until it is unarchived, or until a session
    /// has new traffic.
    Archive,
    Unarchive,
    /// Remove its finished calls, their bodies and derived data, and its
    /// ended runs. Calls still open, and runs still running, stay.
    Delete,
}

/// A fact about a session, when it happened. Not tied to a channel or call.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRecord {
    #[serde(skip)]
    pub at: Moment,
    pub session: SessionId,
    #[serde(flatten)]
    pub fact: SessionFact,
}

/// What happened to a session.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "fact")]
pub enum SessionFact {
    /// The operator created it: `harness` working in `workdir`, on these
    /// settings (each left out is the client's own).
    Created {
        harness: Harness,
        workdir: String,
        settings: RunSettings,
    },
    /// Its next turn runs on these settings, with no process to take them
    /// now: the next one starts on them.
    Configured {
        settings: RunSettings,
    },
    /// The operator chose another client while it was a draft.
    ClientChosen {
        harness: Harness,
    },
    Archived,
    Unarchived,
}

impl SessionFact {
    pub fn kind(&self) -> ObservationKind {
        match self {
            Self::Created { .. } => ObservationKind::SessionCreated,
            Self::Configured { .. } => ObservationKind::SessionConfigured,
            Self::ClientChosen { .. } => ObservationKind::SessionClientChosen,
            Self::Archived => ObservationKind::SessionArchived,
            Self::Unarchived => ObservationKind::SessionUnarchived,
        }
    }
}

/// A fact about a run, when it happened. Not tied to a channel or call.
#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    #[serde(skip)]
    pub at: Moment,
    pub run: RunId,
    #[serde(flatten)]
    pub fact: RunFact,
}

/// What happened to a run.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "fact")]
pub enum RunFact {
    Started {
        /// The session the run serves.
        session: SessionId,
        harness: Harness,
        workdir: String,
        /// The settings it started with; each left out is the client's own.
        settings: RunSettings,
        /// The toolset generation on its search path: 0 when Ren had
        /// installed none, `None` when the tool store could not be read.
        toolset: Option<Generation>,
    },
    /// The client opened its conversation, under its own thread id.
    Opened { thread: NativeThreadId },
    /// The run moved to these settings, from its next turn on.
    Configured { settings: RunSettings },
    Ended {
        end: RunEnd,
        exit: Option<ProcessExit>,
    },
}

impl RunFact {
    pub fn kind(&self) -> ObservationKind {
        match self {
            Self::Started { .. } => ObservationKind::RunStarted,
            Self::Opened { .. } => ObservationKind::RunOpened,
            Self::Configured { .. } => ObservationKind::RunConfigured,
            Self::Ended { .. } => ObservationKind::RunEnded,
        }
    }
}

/// One WebSocket message inside a body: its bytes are `len` bytes from
/// `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageMark {
    pub offset: u64,
    pub len: u64,
    pub payload: Payload,
    pub at: UnixMillis,
}

/// Messages a channel carried while no call was open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Unattributed {
    pub messages: u64,
    pub bytes: u64,
}

/// A WebSocket channel ended.
#[derive(Debug, Serialize)]
pub struct CloseChannel {
    #[serde(skip)]
    pub at: Moment,
    #[serde(skip)]
    pub channel: ChannelId,
    pub end: ChannelEnd,
    pub close_code: Option<u16>,
    pub unattributed: Unattributed,
}

/// What an observation in the log records.
#[derive(Display, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum ObservationKind {
    CallOpened,
    AttemptStarted,
    ResponseHead,
    AttemptFinished,
    CallFinished,
    /// Recorded at startup for a call the previous run never finished.
    CallRecovered,
    /// A WebSocket channel ended. Not tied to a call.
    ChannelClosed,
    /// The operator changed the operator state. Not tied to a call.
    OperatorAction,
    /// Ren started an agent run. Not tied to a call.
    RunStarted,
    /// A run's client opened its conversation.
    RunOpened,
    /// A run's process ended.
    RunEnded,
    /// Recorded at startup for a run the previous daemon never saw end.
    RunRecovered,
    /// The operator archived, unarchived, or deleted recorded history. Not
    /// tied to a call.
    HistoryEdited,
    /// A run moved to other settings mid-run.
    RunConfigured,
    /// The operator created a session. Not tied to a call.
    SessionCreated,
    /// A session's settings changed while no process served it.
    SessionConfigured,
    /// A draft session's client changed.
    SessionClientChosen,
    SessionArchived,
    SessionUnarchived,
}

/// Where an operation belongs and when it happened. Every observation has it.
#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    pub at: Moment,
    pub channel: ChannelId,
    pub call: CallId,
}

#[derive(Debug, Serialize)]
pub struct OpenCall {
    #[serde(skip)]
    pub stamp: Stamp,
    pub transport: Transport,
    pub provider: Provider,
    pub wire: Wire,
    pub endpoint: Endpoint,
    pub method: Method,
    pub config_revision: ConfigRevision,
    pub attribution: Attribution,
    pub caller: Caller,
    /// The operator state the call was planned against.
    pub operator_revision: Option<OperatorRevision>,
    /// The routing plan; absent when the call was refused before planning.
    pub plan: Option<Recorded>,
}

#[derive(Debug, Serialize)]
pub struct StartAttempt {
    #[serde(skip)]
    pub stamp: Stamp,
    pub attempt: AttemptId,
    pub ordinal: u32,
    pub upstream: UpstreamId,
    /// The provider of the upstream entry, which a fallback may change.
    pub provider: Provider,
    /// What Ren changed in this attempt's request, if anything.
    pub changes: Option<Recorded>,
    /// Whether the attempt sent a changed body, stored as its own request.
    pub body: AttemptBody,
}

/// Which request body an attempt sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptBody {
    /// The call's request as the client sent it.
    Original,
    /// A body a transform changed, stored with the attempt.
    Changed,
}

#[derive(Debug, Serialize)]
pub struct ResponseHead {
    #[serde(skip)]
    pub stamp: Stamp,
    pub attempt: AttemptId,
    pub status: u16,
    pub format: ResponseFormat,
    pub encoding: ResponseEncoding,
}

/// How a body ended, as the relay observed it. Stored byte counts come from the
/// segments actually committed, never from this.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct BodyEnd {
    pub observed_bytes: u64,
    pub ending: BodyEnding,
}

#[derive(Debug, Serialize)]
pub struct FinishAttempt {
    #[serde(skip)]
    pub stamp: Stamp,
    pub attempt: AttemptId,
    #[serde(flatten)]
    pub end: TerminalRecord,
    pub response: BodyEnd,
    /// The changed request body's end, for an attempt that sent one.
    pub request: Option<BodyEnd>,
    pub inspection: Option<Box<InspectionRecord>>,
}

#[derive(Debug, Serialize)]
pub struct FinishCall {
    #[serde(skip)]
    pub stamp: Stamp,
    pub request: BodyEnd,
    /// Why Ren answered without forwarding, for a call with no attempt.
    pub rejection: Option<CallRejection>,
}
