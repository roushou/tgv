//! The capture side: what the relay calls while forwarding. Nothing here
//! blocks or awaits. Admission to the queue is decided with `try_` operations,
//! and anything that does not fit is counted as omitted.

use super::item::{AttemptSlots, Item, Piece, Shared, Track};
use crate::caller::Caller;
use crate::{
    batch::{
        AttemptBody, BodyEnd, CloseChannel, HistoryChange, HistoryEdit, HistorySubject,
        MessageMark, OpenCall, OperatorAction, Recorded, ResponseHead, RunEntry, RunFact,
        RunRecord, SessionFact, SessionRecord, Stamp, StartAttempt, Unattributed,
    },
    capture::{
        BodyEnding, CallRejection, ChannelEnd, Omission, ProcessExit, RunEnd, TerminalRecord,
    },
    id::{AttemptId, CallId, ChannelId, RunId, SessionId},
    routing::{ConfigRevision, OperatorRevision, UpstreamId},
};
use bytes::Bytes;
use ren_harness::{Harness, attribution::Attribution, drive::DriveEvent, settings::RunSettings};
use ren_protocol::{
    direction::Direction,
    encoding::{ResponseEncoding, ResponseFormat},
    native::{NativeThreadId, NativeTurnId},
    route::{Api, Endpoint, Method, Provider, Transport},
    websocket::Payload,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
};
use tokio::sync::{OwnedSemaphorePermit, mpsc::OwnedPermit};

/// What the gateway knows when a call is admitted.
#[derive(Debug, Clone)]
pub struct CallStart {
    pub transport: Transport,
    /// The provider and wire format of the entry the call was routed to.
    pub api: Api,
    pub endpoint: Endpoint,
    pub method: Method,
    pub config_revision: ConfigRevision,
    pub attribution: Attribution,
    /// Who made the call.
    pub caller: Caller,
    /// The operator state the call was planned against, once planned.
    pub operator_revision: Option<OperatorRevision>,
    /// The routing plan, once planned.
    pub plan: Option<Recorded>,
    /// Attempts the plan allows; their records are reserved at admission.
    pub attempts: usize,
}

/// What is known when an attempt starts.
#[derive(Debug, Clone)]
pub struct AttemptStart {
    pub upstream: UpstreamId,
    /// The provider of the upstream entry.
    pub provider: Provider,
    /// What Ren changed in the request, if anything.
    pub changes: Option<Recorded>,
    pub body: AttemptBody,
}

impl AttemptStart {
    /// An attempt that sends the client's request unchanged.
    pub fn unchanged(upstream: UpstreamId, provider: Provider) -> Self {
        Self {
            upstream,
            provider,
            changes: None,
            body: AttemptBody::Original,
        }
    }
}

/// Capture of one call and its attempts, one at a time. Dropping it without
/// [`CallTap::finish`] records the open attempt as interrupted.
pub struct CallTap {
    shared: Arc<Shared>,
    stamp: Stamp,
    state: Mutex<TapState>,
}

struct TapState {
    recording: Recording,
    request: Tally,
    attempt: Option<AttemptTally>,
    ordinal: u32,
}

/// The open attempt's identity and bodies.
#[derive(Debug, Clone, Copy)]
struct AttemptTally {
    id: AttemptId,
    /// Present when the attempt sends a changed body.
    request: Option<Tally>,
    response: Tally,
}

/// Reservations taken at admission. The finish slots are always reserved, so
/// a terminal record is never lost to a full queue.
enum Recording {
    Active {
        /// The open attempt's head and finish slots.
        current: Option<(Option<OwnedPermit<Item>>, OwnedPermit<Item>)>,
        /// Slots of the attempts the plan still allows.
        spare: VecDeque<AttemptSlots>,
        finish: OwnedPermit<Item>,
        call: OwnedSemaphorePermit,
    },
    Unrecorded,
    Finished,
}

#[derive(Debug, Clone, Copy)]
struct Tally {
    observed: u64,
    ending: BodyEnding,
}

impl Tally {
    const OPEN: Self = Self {
        observed: 0,
        ending: BodyEnding::Open,
    };

    fn end(self, ending: BodyEnding) -> BodyEnd {
        BodyEnd {
            observed_bytes: self.observed,
            ending: match self.ending {
                BodyEnding::Open => ending,
                BodyEnding::Ended | BodyEnding::Unfinished => self.ending,
            },
        }
    }
}

/// Bytes to capture: copied when borrowed from a network buffer, shared when
/// the gateway already owns exactly these bytes.
#[derive(Clone, Copy)]
enum Source<'a> {
    Borrowed(&'a [u8]),
    Shared(&'a Bytes),
}

impl Source<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }

    fn piece(&self, from: usize, to: usize) -> Bytes {
        match self {
            Self::Borrowed(bytes) => {
                Bytes::copy_from_slice(bytes.get(from..to).unwrap_or_default())
            }
            Self::Shared(bytes) => bytes.slice(from..to),
        }
    }
}

impl CallTap {
    pub(super) fn start(
        shared: Arc<Shared>,
        channel: ChannelId,
        start: CallStart,
        attempt: Option<AttemptStart>,
    ) -> Self {
        let stamp = Stamp {
            at: shared.clock.now(),
            channel,
            call: CallId::generate(shared.ids.as_ref()),
        };
        let first = attempt.map(|attempt| (AttemptId::generate(shared.ids.as_ref()), attempt));
        let tally = first
            .as_ref()
            .map(|(id, attempt)| AttemptTally::new(*id, attempt.body));
        let attempts = match first {
            Some(_) => start.attempts.max(1),
            None => 0,
        };
        let recording = match shared.reserve(attempts) {
            None => {
                shared.counters.omit(Omission::Unrecorded, 1);
                Recording::Unrecorded
            }
            Some(mut reservation) => {
                let endpoint = start.endpoint;
                let current = reservation
                    .attempts
                    .pop_front()
                    .map(|slots| (Some(slots.head), slots.finish));
                reservation.open.send(Item::Open {
                    open: Box::new(OpenCall {
                        stamp,
                        transport: start.transport,
                        provider: start.api.provider(),
                        wire: start.api.wire(),
                        endpoint: start.endpoint,
                        method: start.method,
                        config_revision: start.config_revision,
                        attribution: start.attribution,
                        caller: start.caller,
                        operator_revision: start.operator_revision,
                        plan: start.plan,
                    }),
                    attempt: first.map(|(id, attempt)| attempt.record(stamp, id, 0)),
                    endpoint,
                });
                Recording::Active {
                    current,
                    spare: reservation.attempts,
                    finish: reservation.finish,
                    call: reservation.call,
                }
            }
        };
        Self {
            shared,
            stamp,
            state: Mutex::new(TapState {
                recording,
                request: Tally::OPEN,
                attempt: tally,
                ordinal: 0,
            }),
        }
    }

    pub fn call(&self) -> CallId {
        self.stamp.call
    }

    pub fn channel(&self) -> ChannelId {
        self.stamp.channel
    }

    fn state(&self) -> std::sync::MutexGuard<'_, TapState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bytes forwarded in `direction`. They are copied only once admitted, so a
    /// slice never pins a larger network buffer than the budget accounts for.
    pub fn data(&self, direction: Direction, bytes: &[u8]) {
        self.capture(Track::from(direction), Source::Borrowed(bytes), None);
    }

    /// The whole request as the client sent it, buffered by the gateway. The
    /// recorder shares these bytes instead of copying them.
    pub fn request(&self, bytes: &Bytes) {
        self.capture(Track::Request, Source::Shared(bytes), None);
        self.end(Direction::Request);
    }

    /// The open attempt's changed request, shared like [`Self::request`].
    pub fn attempt_request(&self, bytes: &Bytes) {
        self.capture(Track::AttemptRequest, Source::Shared(bytes), None);
        let mut state = self.state();
        if let Some(Some(tally)) = state
            .attempt
            .as_mut()
            .map(|attempt| attempt.request.as_mut())
        {
            tally.ending = BodyEnding::Ended;
        }
    }

    /// One whole WebSocket message forwarded in `direction`. Its boundary is
    /// recorded with its first stored piece; a message whose first piece is
    /// omitted keeps no boundary, and the body shows the gap.
    pub fn message(&self, direction: Direction, payload: Payload, bytes: &[u8]) {
        self.capture(
            Track::from(direction),
            Source::Borrowed(bytes),
            Some(payload),
        );
    }

    fn capture(&self, track: Track, source: Source<'_>, message: Option<Payload>) {
        let bytes = source.bytes();
        let mut state = self.state();
        let recording = matches!(state.recording, Recording::Active { .. });
        if matches!(state.recording, Recording::Finished) || bytes.is_empty() {
            return;
        }
        let tally = match track {
            Track::Request => Some(&mut state.request),
            Track::AttemptRequest => state
                .attempt
                .as_mut()
                .and_then(|attempt| attempt.request.as_mut()),
            Track::Response => state.attempt.as_mut().map(|attempt| &mut attempt.response),
        };
        let Some(tally) = tally else {
            return;
        };
        let offset = tally.observed;
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        tally.observed = offset.saturating_add(len);
        if !recording {
            return;
        }
        let counters = &self.shared.counters;
        let split = self.shared.options.split(offset, bytes.len());
        counters.omit(Omission::PolicyLimit, split.over_limit);
        let mut mark = message.map(|payload| MessageMark {
            offset,
            len,
            payload,
            at: self.shared.clock.now().wall,
        });
        for piece in split.pieces {
            let piece_len = u64::try_from(piece.len()).unwrap_or(u64::MAX);
            let piece_offset =
                offset.saturating_add(u64::try_from(piece.start).unwrap_or(u64::MAX));
            match self.shared.admit(piece.len()) {
                Err(reason) => counters.omit(reason, piece_len),
                Ok(admission) => admission.send(Piece {
                    call: self.stamp.call,
                    track,
                    offset: piece_offset,
                    bytes: source.piece(piece.start, piece.end),
                    enqueued: self.shared.clock.now().monotonic,
                    message: mark.take(),
                }),
            }
            mark = None;
        }
    }

    /// The end of the body in `direction` was observed.
    pub fn end(&self, direction: Direction) {
        let mut state = self.state();
        let tally = match direction {
            Direction::Request => Some(&mut state.request),
            Direction::Response => state.attempt.as_mut().map(|attempt| &mut attempt.response),
        };
        if let Some(tally) = tally {
            tally.ending = BodyEnding::Ended;
        }
    }

    pub fn response_head(&self, status: u16, format: ResponseFormat, encoding: ResponseEncoding) {
        let mut state = self.state();
        let Some(attempt) = state.attempt.map(|attempt| attempt.id) else {
            return;
        };
        let Recording::Active {
            current: Some((head, _)),
            ..
        } = &mut state.recording
        else {
            return;
        };
        if let Some(slot) = head.take() {
            slot.send(Item::Head(ResponseHead {
                stamp: self.stamped(),
                attempt,
                status,
                format,
                encoding,
            }));
        }
    }

    fn stamped(&self) -> Stamp {
        Stamp {
            at: self.shared.clock.now(),
            ..self.stamp
        }
    }

    /// Record the open attempt's terminal and close the call. Bodies whose
    /// end was not observed are recorded as unfinished. Later calls are
    /// ignored.
    pub fn finish(&self, end: TerminalRecord) {
        let mut state = self.state();
        self.close_attempt(&mut state, end);
        self.close_call(&mut state, None);
    }

    /// Record the open attempt's terminal; the call stays open for its next
    /// attempt.
    pub fn finish_attempt(&self, end: TerminalRecord) {
        let mut state = self.state();
        self.close_attempt(&mut state, end);
    }

    /// Start the call's next attempt, after [`Self::finish_attempt`]. The plan
    /// reserved its records at admission.
    pub fn next_attempt(&self, attempt: AttemptStart) {
        let mut state = self.state();
        if state.attempt.is_some() || matches!(state.recording, Recording::Finished) {
            return;
        }
        let id = AttemptId::generate(self.shared.ids.as_ref());
        state.ordinal = state.ordinal.saturating_add(1);
        state.attempt = Some(AttemptTally::new(id, attempt.body));
        let ordinal = state.ordinal;
        let stamp = self.stamped();
        if let Recording::Active { current, spare, .. } = &mut state.recording {
            match spare.pop_front() {
                Some(AttemptSlots {
                    start: Some(start),
                    head,
                    finish,
                }) => {
                    start.send(Item::Attempt(attempt.record(stamp, id, ordinal)));
                    *current = Some((Some(head), finish));
                }
                Some(AttemptSlots { start: None, .. }) | None => {
                    // More attempts than the plan allowed: a gateway bug.
                    tracing::warn!("an attempt beyond the call's plan is not recorded");
                    self.shared.counters.omit(Omission::Unrecorded, 1);
                }
            }
        }
    }

    /// Close a call that made no attempt: Ren answered it.
    pub fn reject(&self, rejection: CallRejection) {
        let mut state = self.state();
        self.close_call(&mut state, Some(rejection));
    }

    fn close_attempt(&self, state: &mut TapState, end: TerminalRecord) {
        let Some(attempt) = state.attempt.take() else {
            return;
        };
        if let Recording::Active { current, .. } = &mut state.recording
            && let Some((_, finish)) = current.take()
        {
            finish.send(Item::FinishAttempt {
                stamp: self.stamped(),
                attempt: attempt.id,
                end,
                response: attempt.response.end(BodyEnding::Unfinished),
                request: attempt
                    .request
                    .map(|request| request.end(BodyEnding::Unfinished)),
            });
        }
    }

    fn close_call(&self, state: &mut TapState, rejection: Option<CallRejection>) {
        let recording = std::mem::replace(&mut state.recording, Recording::Finished);
        let Recording::Active { finish, call, .. } = recording else {
            return;
        };
        finish.send(Item::FinishCall {
            stamp: self.stamped(),
            request: state.request.end(BodyEnding::Unfinished),
            rejection,
            _call: call,
        });
    }
}

impl AttemptTally {
    fn new(id: AttemptId, body: AttemptBody) -> Self {
        Self {
            id,
            request: match body {
                AttemptBody::Original => None,
                AttemptBody::Changed => Some(Tally::OPEN),
            },
            response: Tally::OPEN,
        }
    }
}

impl AttemptStart {
    fn record(self, stamp: Stamp, attempt: AttemptId, ordinal: u32) -> StartAttempt {
        StartAttempt {
            stamp,
            attempt,
            ordinal,
            upstream: self.upstream,
            provider: self.provider,
            changes: self.changes,
            body: self.body,
        }
    }
}

impl From<Direction> for Track {
    fn from(direction: Direction) -> Self {
        match direction {
            Direction::Request => Self::Request,
            Direction::Response => Self::Response,
        }
    }
}

impl Drop for CallTap {
    fn drop(&mut self) {
        self.finish(TerminalRecord::DROPPED);
    }
}

/// One reserved slot for an operator action. Without one, the command is not
/// applied: every operator decision is recorded.
pub struct OperatorSlot {
    shared: Arc<Shared>,
    slot: OwnedPermit<Item>,
    permit: OwnedSemaphorePermit,
}

impl OperatorSlot {
    pub(super) fn reserve(shared: Arc<Shared>) -> Option<Self> {
        let (slot, permit) = shared.reserve_close()?;
        Some(Self {
            shared,
            slot,
            permit,
        })
    }

    /// Record `command`, which made `state` at `revision`. Both documents are
    /// serialized here and kept as they are; if either cannot be serialized,
    /// nothing is recorded and the slot is released.
    pub fn record<C: serde::Serialize, S: serde::Serialize>(
        self,
        revision: crate::routing::OperatorRevision,
        actor: crate::batch::Actor,
        command: &C,
        state: &S,
    ) -> Result<(), serde_json::Error> {
        let action = OperatorAction {
            at: self.shared.clock.now(),
            revision,
            actor,
            command: Recorded::of(command)?,
            state: Recorded::of(state)?,
        };
        self.slot.send(Item::Operator {
            action,
            _slot: self.permit,
        });
        Ok(())
    }
}

/// One reserved slot for a history edit. Without one, the edit is not made:
/// every archive and delete is recorded with who asked for it.
pub struct HistorySlot {
    shared: Arc<Shared>,
    slot: OwnedPermit<Item>,
    permit: OwnedSemaphorePermit,
}

impl HistorySlot {
    pub(super) fn reserve(shared: Arc<Shared>) -> Option<Self> {
        let (slot, permit) = shared.reserve_close()?;
        Some(Self {
            shared,
            slot,
            permit,
        })
    }

    /// Make `change` to `subject`, as `actor` asked. It applies when the
    /// recorder reaches it, after everything queued before.
    pub fn record(
        self,
        actor: crate::batch::Actor,
        subject: HistorySubject,
        change: HistoryChange,
    ) {
        self.slot.send(Item::History {
            edit: HistoryEdit {
                at: self.shared.clock.now(),
                actor,
                subject,
                change,
            },
            _slot: self.permit,
        });
    }
}

/// A reserved slot for one fact about a session, recorded when it is used.
pub struct SessionSlot {
    shared: Arc<Shared>,
    slot: OwnedPermit<Item>,
    permit: OwnedSemaphorePermit,
}

impl SessionSlot {
    pub(super) fn reserve(shared: Arc<Shared>) -> Option<Self> {
        let (slot, permit) = shared.reserve_close()?;
        Some(Self {
            shared,
            slot,
            permit,
        })
    }

    /// Record `fact` about `session`, after everything queued before.
    pub fn record(self, session: SessionId, fact: SessionFact) {
        self.slot.send(Item::Session {
            record: SessionRecord {
                at: self.shared.clock.now(),
                session,
                fact,
            },
            _slot: self.permit,
        });
    }
}

/// Capture of one WebSocket channel's end. Dropping it without
/// [`ChannelTap::close`] records the channel as dropped.
pub struct ChannelTap {
    shared: Arc<Shared>,
    channel: ChannelId,
    state: Mutex<ChannelState>,
}

struct ChannelState {
    reservation: Option<(OwnedPermit<Item>, OwnedSemaphorePermit)>,
    unattributed: Unattributed,
}

impl ChannelTap {
    pub(super) fn open(shared: Arc<Shared>, channel: ChannelId) -> Self {
        let reservation = shared.reserve_close();
        if reservation.is_none() {
            shared.counters.omit(Omission::Unrecorded, 1);
        }
        Self {
            shared,
            channel,
            state: Mutex::new(ChannelState {
                reservation,
                unattributed: Unattributed::default(),
            }),
        }
    }

    pub fn channel(&self) -> ChannelId {
        self.channel
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ChannelState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A message no call owns was forwarded. Its bytes are counted, not stored.
    pub fn unattributed(&self, bytes: &[u8]) {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let mut state = self.state();
        state.unattributed.messages = state.unattributed.messages.saturating_add(1);
        state.unattributed.bytes = state.unattributed.bytes.saturating_add(len);
        self.shared.counters.omit(Omission::Unattributed, len);
    }

    /// Record the channel's end. Later calls are ignored.
    pub fn close(&self, end: ChannelEnd, close_code: Option<u16>) {
        let mut state = self.state();
        let Some((slot, channel)) = state.reservation.take() else {
            return;
        };
        slot.send(Item::Close {
            close: CloseChannel {
                at: self.shared.clock.now(),
                channel: self.channel,
                end,
                close_code,
                unattributed: state.unattributed,
            },
            _channel: channel,
        });
    }
}

impl Drop for ChannelTap {
    fn drop(&mut self) {
        self.close(ChannelEnd::Dropped, None);
    }
}

/// The recorder had no room for a run's records, so the run was not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the recorder has no room to record a run")]
pub struct RunUnrecorded;

/// The session a run serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSession {
    /// A session created with the run, recorded before it.
    New(SessionId),
    /// A session already recorded.
    Existing(SessionId),
}

impl RunSession {
    pub fn id(self) -> SessionId {
        match self {
            Self::New(session) | Self::Existing(session) => session,
        }
    }
}

/// Capture of one run's lifecycle. Its start is recorded when it is made, and
/// its end's slot is reserved until [`RunTap::end`]. Dropping it without an
/// end records nothing more: the next startup marks the run recovered.
pub struct RunTap {
    shared: Arc<Shared>,
    run: RunId,
    end: Option<(OwnedPermit<Item>, OwnedSemaphorePermit)>,
}

impl RunTap {
    pub(super) fn start(
        shared: Arc<Shared>,
        run: RunId,
        session: RunSession,
        harness: Harness,
        workdir: String,
        settings: RunSettings,
        toolset: Option<ren_depot::toolset::Generation>,
    ) -> Result<Self, RunUnrecorded> {
        // Every slot first: a run starts recorded whole, or not at all.
        let created = match session {
            RunSession::New(_) => Some(shared.reserve_close().ok_or(RunUnrecorded)?),
            RunSession::Existing(_) => None,
        };
        let start = shared.reserve_close().ok_or(RunUnrecorded)?;
        let end = shared.reserve_close().ok_or(RunUnrecorded)?;
        if let Some((slot, permit)) = created {
            slot.send(Item::Session {
                record: SessionRecord {
                    at: shared.clock.now(),
                    session: session.id(),
                    fact: SessionFact::Created {
                        harness,
                        workdir: workdir.clone(),
                        settings: settings.clone(),
                    },
                },
                _slot: permit,
            });
        }
        let tap = Self {
            shared,
            run,
            end: Some(end),
        };
        tap.send(
            start,
            RunFact::Started {
                session: session.id(),
                harness,
                workdir,
                settings,
                toolset,
            },
        );
        Ok(tap)
    }

    pub fn run(&self) -> RunId {
        self.run
    }

    /// The client opened its conversation as `thread`. Without room this is
    /// counted as unrecorded.
    pub fn opened(&self, thread: NativeThreadId) {
        match self.shared.reserve_close() {
            Some(slot) => self.send(slot, RunFact::Opened { thread }),
            None => self.shared.counters.omit(Omission::Unrecorded, 1),
        }
    }

    /// The run moved to `settings` mid-run. Without room this is counted
    /// as unrecorded.
    pub fn configured(&self, settings: RunSettings) {
        match self.shared.reserve_close() {
            Some(slot) => self.send(slot, RunFact::Configured { settings }),
            None => self.shared.counters.omit(Omission::Unrecorded, 1),
        }
    }

    /// Record one entry of the run's conversation, in `turn`. Never blocks:
    /// without room for its bytes it is counted as omitted, by reason.
    pub fn entry(&self, turn: Option<NativeTurnId>, event: DriveEvent) {
        let len = serde_json::to_vec(&event).map_or(0, |bytes| bytes.len());
        match self.shared.admit(len) {
            Ok(admission) => admission.send_entry(RunEntry {
                at: self.shared.clock.now(),
                run: self.run,
                turn,
                event,
            }),
            Err(omission) => self.shared.counters.omit(omission, 1),
        }
    }

    /// Record how the run ended.
    pub fn end(mut self, end: RunEnd, exit: Option<ProcessExit>) {
        if let Some(slot) = self.end.take() {
            self.send(slot, RunFact::Ended { end, exit });
        }
    }

    fn send(&self, (slot, permit): (OwnedPermit<Item>, OwnedSemaphorePermit), fact: RunFact) {
        slot.send(Item::Run {
            record: RunRecord {
                at: self.shared.clock.now(),
                run: self.run,
                fact,
            },
            _slot: permit,
        });
    }
}
