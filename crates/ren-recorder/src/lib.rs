//! Recording: the tap the relay feeds, one ordered queue with reserved
//! lifecycle slots, and the task that turns records into batches for a
//! [`RecordSink`]. The primary type is [`Recorder`], the handle forwarding
//! uses; [`RecorderTask`] runs it.
//!
//! The crate also owns what is recorded: Ren's identities (`id`) and the
//! `Clock` and `IdSource` capabilities (`time`, `id`), attempt and call
//! terminals (`capture`), routing facts (`routing`), who called (`caller`), batches and their chunks
//! (`batch`, `chunk`), the stored inspection format (`inspection`), and the
//! retention policy the sink applies (`retention`).
//!
//! Recording never stalls forwarding. The tap only uses non-blocking queue
//! admission; data that does not fit is omitted with a reason, and lifecycle
//! records cannot be starved because their slots are reserved when a call is
//! admitted. Counters are atomics, readable while the queue is saturated.
//!
//! The task is returned to the caller to run, because components do not spawn.

pub mod batch;
pub mod caller;
pub mod capture;
pub mod chunk;
pub mod digest;
pub mod id;
pub mod inspection;
mod item;
pub mod metrics;
pub mod options;
pub mod progress;
#[cfg(test)]
mod purity;
mod recording;
pub mod retention;
pub mod routing;
pub mod secrets;
mod sink;
mod tap;
mod task;
pub mod time;

pub use metrics::RecorderMetrics;
pub use options::{CaptureOptions, CaptureOptionsError};
pub use sink::{Committed, Observation, Pruned, RecordSink};
pub use tap::{
    AttemptStart, CallStart, CallTap, ChannelTap, HistorySlot, OperatorSlot, RunSession, RunTap,
    RunUnrecorded, SessionSlot,
};
pub use task::RecorderTask;

use crate::{
    id::{ChannelId, IdSource},
    retention::RetentionPolicy,
    secrets::SecretScanner,
    time::Clock,
};
use item::Shared;
use metrics::{Counters, QueueMetrics};
use recording::RecorderCore;
use std::sync::{Arc, atomic::Ordering};
use tokio::sync::broadcast;

/// The recorder's counters, readable after every [`Recorder`] handle is
/// dropped. Queue figures are the caller's last reading, since the queue
/// itself closes with the handles.
#[derive(Clone)]
pub struct RecorderMeter {
    counters: Arc<Counters>,
}

impl RecorderMeter {
    pub fn snapshot(&self, queue: QueueMetrics) -> RecorderMetrics {
        self.counters.snapshot(queue)
    }
}

/// What the recorder is sized for.
#[derive(Debug, Clone)]
pub struct RecorderSetup {
    pub options: CaptureOptions,
    pub retention: RetentionPolicy,
    /// Calls open at once: the gateway's admission limit.
    pub max_calls: usize,
    /// Committed observations kept for live subscribers that fall behind.
    pub live_buffer: usize,
    /// Redacts the text of every call's digest before it is stored.
    pub scanner: Arc<SecretScanner>,
}

/// A handle to the recorder: starts call taps, reads metrics, and subscribes
/// to committed observations.
#[derive(Clone)]
pub struct Recorder {
    shared: Arc<Shared>,
    live: broadcast::Sender<Observation>,
    /// Updates on streaming calls, for live viewers; never stored.
    progress: broadcast::Sender<progress::Progress>,
}

impl Recorder {
    /// A recorder writing to `sink`. `recovered_calls` is how many calls the
    /// sink found left open by the previous run, reported in the metrics.
    pub fn new<S: RecordSink>(
        sink: S,
        recovered_calls: u64,
        setup: RecorderSetup,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> Result<(Self, RecorderTask<S>), CaptureOptionsError> {
        setup.options.validate()?;
        let counters = Arc::new(Counters::default());
        counters
            .recovered_calls
            .store(recovered_calls, Ordering::Relaxed);
        let (shared, receiver) = Shared::new(
            setup.options.clone(),
            setup.max_calls,
            counters.clone(),
            clock.clone(),
            ids,
        );
        let (live, _) = broadcast::channel(setup.live_buffer.max(16));
        // Updates are worth having only while fresh: a slow viewer skips some.
        let (progress, _) = broadcast::channel(256);
        let task = RecorderTask {
            receiver,
            sink,
            live: live.clone(),
            progress: progress.clone(),
            counters,
            clock,
            options: setup.options,
            retention: setup.retention,
            core: RecorderCore::new(setup.scanner),
            held: Vec::new(),
            oldest: None,
        };
        Ok((
            Self {
                shared: Arc::new(shared),
                live,
                progress,
            },
            task,
        ))
    }

    /// Admit a call and its first attempt for capture. Never blocks; without
    /// a free reservation the call is forwarded unrecorded and counted.
    pub fn start(&self, start: CallStart, attempt: AttemptStart) -> Arc<CallTap> {
        let channel = ChannelId::generate(self.shared.ids.as_ref());
        Arc::new(CallTap::start(
            self.shared.clone(),
            channel,
            start,
            Some(attempt),
        ))
    }

    /// Admit a call on a channel that already exists: a WebSocket message call.
    pub fn start_in(
        &self,
        channel: ChannelId,
        start: CallStart,
        attempt: AttemptStart,
    ) -> Arc<CallTap> {
        Arc::new(CallTap::start(
            self.shared.clone(),
            channel,
            start,
            Some(attempt),
        ))
    }

    /// Record a call Ren answered without forwarding, with its request as
    /// far as it was read. `channel` is the WebSocket's, for a message call.
    pub fn reject(
        &self,
        channel: Option<ChannelId>,
        start: CallStart,
        request: Option<&bytes::Bytes>,
        rejection: crate::capture::CallRejection,
    ) {
        let channel = channel.unwrap_or_else(|| ChannelId::generate(self.shared.ids.as_ref()));
        let tap = CallTap::start(self.shared.clone(), channel, start, None);
        if let Some(request) = request {
            tap.request(request);
        }
        tap.reject(rejection);
    }

    /// A slot to record one operator action, or `None` when the recorder has
    /// no room.
    pub fn operator_slot(&self) -> Option<OperatorSlot> {
        OperatorSlot::reserve(self.shared.clone())
    }

    /// A slot to record one history edit, or `None` when the recorder has no
    /// room.
    pub fn history_slot(&self) -> Option<HistorySlot> {
        HistorySlot::reserve(self.shared.clone())
    }

    /// A slot to record one fact about a session, or `None` when the
    /// recorder has no room.
    pub fn session_slot(&self) -> Option<SessionSlot> {
        SessionSlot::reserve(self.shared.clone())
    }

    /// Record the start of `run`, serving `session` (recorded first when it
    /// is new), `harness` working in `workdir`, and reserve its end record.
    /// Never blocks; without room the run must not start.
    pub fn run(
        &self,
        run: crate::id::RunId,
        session: RunSession,
        harness: ren_harness::Harness,
        workdir: String,
        settings: ren_harness::settings::RunSettings,
        toolset: Option<ren_depot::toolset::Generation>,
    ) -> Result<RunTap, RunUnrecorded> {
        RunTap::start(
            self.shared.clone(),
            run,
            session,
            harness,
            workdir,
            settings,
            toolset,
        )
    }

    /// Reserve the end record of a WebSocket channel whose handshake call
    /// already opened it. Never blocks; without a reservation the end is not
    /// recorded and the channel is counted as unrecorded.
    pub fn channel(&self, channel: ChannelId) -> ChannelTap {
        ChannelTap::open(self.shared.clone(), channel)
    }

    /// A reader of this recorder's counters that does not keep its queue
    /// open, so they can be read once the task has drained.
    pub fn meter(&self) -> RecorderMeter {
        RecorderMeter {
            counters: self.shared.counters.clone(),
        }
    }

    /// Bytes captured per body: past this, the rest is omitted by policy.
    pub fn body_limit_bytes(&self) -> usize {
        self.shared.options.body_limit_bytes
    }

    pub fn metrics(&self) -> RecorderMetrics {
        let sender = &self.shared.sender;
        let byte_capacity = self.shared.options.queue_bytes;
        self.shared.counters.snapshot(QueueMetrics {
            items: sender.max_capacity().saturating_sub(sender.capacity()),
            capacity: sender.max_capacity(),
            bytes: byte_capacity.saturating_sub(self.shared.bytes.available_permits()),
            byte_capacity,
        })
    }

    /// Committed observations from now on. A subscriber that falls more than
    /// the live buffer behind is told it lagged and should catch up from the
    /// store.
    pub fn subscribe(&self) -> broadcast::Receiver<Observation> {
        self.live.subscribe()
    }

    /// Updates on streaming calls, sent after each commit. They are never
    /// stored or resumed: a subscriber that falls behind skips some.
    pub fn subscribe_progress(&self) -> broadcast::Receiver<progress::Progress> {
        self.progress.subscribe()
    }
}
