//! The recorder with the real store: what it records is what the store reads
//! back. Synthetic payloads; no network.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests are a shell: helpers outside #[test] fail loudly"
)]

use ren_harness::attribution::Attribution;
use ren_protocol::{
    direction::Direction,
    encoding::{ResponseEncoding, ResponseFormat},
    inspect::{Limitation, ModelOutcome},
    label::Stored,
    route::{Endpoint, Method, Provider, Transport, Wire},
};
use ren_recorder::RecordSink;
use ren_recorder::digest::CallDigest;
use ren_recorder::routing::UpstreamId;
use ren_recorder::secrets::SecretScanner;
use ren_recorder::time::Clock;
use ren_recorder::{
    AttemptStart, CallStart, CallTap, CaptureOptions, Recorder, RecorderMetrics, RecorderSetup,
    RecorderTask,
};
use ren_recorder::{
    batch::ObservationKind,
    capture::{AttemptTerminal, BodyEnding, CallState, TerminalRecord},
    id::{CallId, SequentialIds},
    retention::RetentionPolicy,
    routing::ConfigRevision,
    time::{ManualClock, UnixMillis},
};
use ren_store::{Reader, Store, query::CallQuery, view::CallDetail};
use std::sync::Arc;

/// A terminal without cause or phase, as tests of the recorder need no more.
fn ended(terminal: AttemptTerminal) -> TerminalRecord {
    TerminalRecord {
        terminal,
        cause: None,
        phase: None,
    }
}

const PIECE: usize = CaptureOptions::PIECE_BYTES;
const MESSAGE_STREAM: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

struct Harness {
    dir: tempfile::TempDir,
    recorder: Recorder,
    task: RecorderTask<Store>,
}

impl Harness {
    async fn new(options: CaptureOptions, max_calls: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::at(UnixMillis::from_millis(5_000)));
        let ids = Arc::new(SequentialIds::default());
        let (store, recovery) = Store::open(dir.path(), clock.as_ref(), ids.clone())
            .await
            .unwrap();
        let setup = RecorderSetup {
            options,
            retention: RetentionPolicy::default(),
            max_calls,
            live_buffer: 64,
            scanner: std::sync::Arc::new(ren_recorder::secrets::SecretScanner::new().unwrap()),
        };
        let (recorder, task) = Recorder::new(store, recovery.calls, setup, clock, ids).unwrap();
        Self {
            dir,
            recorder,
            task,
        }
    }

    fn start(&self) -> Arc<CallTap> {
        self.recorder.start(
            CallStart {
                transport: Transport::Http,
                api: Provider::Anthropic.native_api(),
                endpoint: Endpoint::Messages,
                method: Method::Post,
                config_revision: ConfigRevision::new("rev"),
                attribution: Attribution::default(),
                caller: ren_recorder::caller::Caller::Unidentified,
                operator_revision: None,
                plan: None,
                attempts: 1,
            },
            AttemptStart::unchanged(
                UpstreamId::default_for(Provider::Anthropic),
                Provider::Anthropic,
            ),
        )
    }

    /// Drop every handle and run the task until it drains and closes the store.
    async fn drain(self) -> (tempfile::TempDir, RecorderMetrics, Reader) {
        let Self {
            dir,
            recorder,
            task,
        } = self;
        let meter = recorder.meter();
        let queue = recorder.metrics().queue;
        drop(recorder);
        task.run().await.unwrap();
        let metrics = meter.snapshot(queue);
        let reader = Reader::open(dir.path(), 1).await.unwrap();
        (dir, metrics, reader)
    }
}

async fn detail(reader: &Reader, call: CallId) -> CallDetail {
    reader.call(call).await.unwrap()
}

fn complete(tap: &CallTap, request: &[u8], response: &[u8]) {
    tap.data(Direction::Request, request);
    tap.end(Direction::Request);
    tap.response_head(200, ResponseFormat::EventStream, ResponseEncoding::Identity);
    tap.data(Direction::Response, response);
    tap.end(Direction::Response);
    tap.finish(ended(AttemptTerminal::BodyComplete));
}

#[tokio::test]
async fn a_streamed_call_is_recorded_inspected_and_stored_completely() {
    let harness = Harness::new(CaptureOptions::default(), 4).await;
    let tap = harness.start();
    let call = tap.call();
    tap.data(Direction::Request, b"{\"stream\":true}");
    tap.end(Direction::Request);
    tap.response_head(200, ResponseFormat::EventStream, ResponseEncoding::Identity);
    // Arbitrary fragments, including inside an event.
    for piece in MESSAGE_STREAM.chunks(7) {
        tap.data(Direction::Response, piece);
    }
    tap.end(Direction::Response);
    tap.finish(ended(AttemptTerminal::BodyComplete));
    drop(tap);
    let (_dir, metrics, reader) = harness.drain().await;
    assert_eq!(metrics.omissions.unrecorded_calls, 0);
    let detail = detail(&reader, call).await;
    assert_eq!(detail.call.state, Stored::Known(CallState::Finished));
    assert!(detail.call.capture_complete);
    assert_eq!(
        detail.call.outcome,
        Some(Stored::Known(ModelOutcome::Completed))
    );
    assert_eq!(detail.call.model.as_deref(), Some("claude-test"));
    assert_eq!(detail.call.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(
        detail.call.wire,
        Some(Stored::Known(Wire::AnthropicMessages))
    );
    assert_eq!(
        detail.attempts[0].provider,
        Some(Stored::Known(Provider::Anthropic))
    );
    let inspection = detail.attempts[0].inspection.as_ref().unwrap();
    assert!(
        inspection.limitations.is_empty(),
        "{:?}",
        inspection.limitations
    );
}

#[tokio::test]
async fn an_exhausted_byte_budget_omits_data_but_never_the_terminal() {
    // The task does not run until drain, so the budget stays held.
    let options = CaptureOptions {
        queue_bytes: PIECE,
        ..CaptureOptions::default()
    };
    let harness = Harness::new(options, 4).await;
    let tap = harness.start();
    let call = tap.call();
    tap.data(Direction::Request, &vec![7; PIECE * 3]);
    tap.end(Direction::Request);
    tap.response_head(200, ResponseFormat::EventStream, ResponseEncoding::Identity);
    tap.data(Direction::Response, MESSAGE_STREAM);
    tap.end(Direction::Response);
    tap.finish(ended(AttemptTerminal::BodyComplete));
    drop(tap);
    let (_dir, metrics, reader) = harness.drain().await;
    let omitted = u64::try_from(PIECE * 2 + MESSAGE_STREAM.len()).unwrap();
    assert_eq!(metrics.omissions.queue_bytes_bytes, omitted);
    let detail = detail(&reader, call).await;
    assert_eq!(detail.call.state, Stored::Known(CallState::Finished));
    assert_eq!(
        detail.call.terminal,
        Some(Stored::Known(AttemptTerminal::BodyComplete))
    );
    assert!(!detail.call.capture_complete);
    let request = detail.call.request.unwrap();
    assert_eq!(
        (request.observed_bytes, request.stored_bytes),
        (
            Some(u64::try_from(PIECE * 3).unwrap()),
            u64::try_from(PIECE).unwrap()
        )
    );
    let inspection = detail.attempts[0].inspection.as_ref().unwrap();
    assert!(
        inspection
            .limitations
            .contains(&Stored::Known(Limitation::CaptureGap))
    );
    assert_eq!(inspection.outcome, None);
}

#[tokio::test]
async fn exhausted_data_slots_are_counted_by_reason() {
    let options = CaptureOptions {
        queue_capacity: 4,
        ..CaptureOptions::default()
    };
    let harness = Harness::new(options, 1).await;
    let tap = harness.start();
    for _ in 0..6 {
        tap.data(Direction::Request, b"0123456789");
    }
    tap.finish(ended(AttemptTerminal::Cancelled));
    drop(tap);
    let (_dir, metrics, reader) = harness.drain().await;
    assert_eq!(metrics.omissions.queue_slots_bytes, 20);
    let page = reader.calls(&CallQuery::default()).await.unwrap();
    let call = &page.calls[0];
    assert_eq!(
        call.terminal,
        Some(Stored::Known(AttemptTerminal::Cancelled))
    );
    let request = call.request.as_ref().unwrap();
    assert_eq!(
        (request.observed_bytes, request.stored_bytes),
        (Some(60), 40)
    );
    assert_eq!(request.ending, Stored::Known(BodyEnding::Unfinished));
}

#[tokio::test]
async fn every_admitted_call_keeps_its_terminal_when_data_saturates_the_queue() {
    let options = CaptureOptions {
        queue_capacity: 4,
        queue_bytes: PIECE,
        ..CaptureOptions::default()
    };
    let harness = Harness::new(options, 8).await;
    let taps: Vec<_> = (0..8).map(|_| harness.start()).collect();
    for tap in &taps {
        tap.data(Direction::Request, &vec![1; PIECE]);
        tap.response_head(200, ResponseFormat::EventStream, ResponseEncoding::Identity);
        tap.data(Direction::Response, MESSAGE_STREAM);
    }
    let calls: Vec<_> = taps.iter().map(|tap| tap.call()).collect();
    for tap in &taps {
        tap.finish(ended(AttemptTerminal::BodyComplete));
    }
    drop(taps);
    let (_dir, metrics, reader) = harness.drain().await;
    assert_eq!(metrics.omissions.unrecorded_calls, 0);
    assert!(metrics.omissions.queue_bytes_bytes + metrics.omissions.queue_slots_bytes > 0);
    for call in calls {
        let detail = detail(&reader, call).await;
        assert_eq!(detail.call.state, Stored::Known(CallState::Finished));
        let kinds: Vec<_> = detail
            .observations
            .iter()
            .map(|o| o.kind.known().unwrap())
            .collect();
        assert_eq!(kinds.last(), Some(&ObservationKind::CallFinished));
    }
}

#[tokio::test]
async fn calls_beyond_the_reservation_limit_are_forwarded_unrecorded_and_counted() {
    let harness = Harness::new(CaptureOptions::default(), 1).await;
    let first = harness.start();
    let second = harness.start();
    complete(&second, b"x", b"y");
    complete(&first, b"a", MESSAGE_STREAM);
    let recorded = first.call();
    drop((first, second));
    // After the first call's finish is dequeued its reservation is free again.
    let (_dir, metrics, reader) = harness.drain().await;
    assert_eq!(metrics.omissions.unrecorded_calls, 1);
    let page = reader.calls(&CallQuery::default()).await.unwrap();
    assert_eq!(page.calls.len(), 1);
    assert_eq!(page.calls[0].id, recorded);
}

#[tokio::test]
async fn a_dropped_tap_records_an_interrupted_attempt_with_unfinished_bodies() {
    let harness = Harness::new(CaptureOptions::default(), 2).await;
    let tap = harness.start();
    let call = tap.call();
    tap.data(Direction::Request, b"partial upload");
    drop(tap);
    let (_dir, _, reader) = harness.drain().await;
    let detail = detail(&reader, call).await;
    assert_eq!(
        detail.call.terminal,
        Some(Stored::Known(AttemptTerminal::Interrupted))
    );
    assert_eq!(
        detail.call.request.unwrap().ending,
        Stored::Known(BodyEnding::Unfinished)
    );
    assert!(detail.attempts[0].inspection.is_none());
}

#[tokio::test]
async fn the_body_limit_is_a_policy_omission() {
    let options = CaptureOptions {
        body_limit_bytes: 3,
        ..CaptureOptions::default()
    };
    let harness = Harness::new(options, 2).await;
    let tap = harness.start();
    complete(&tap, b"abcdef", b"");
    drop(tap);
    let (_dir, metrics, reader) = harness.drain().await;
    assert_eq!(metrics.omissions.policy_limit_bytes, 3);
    let call = reader
        .calls(&CallQuery::default())
        .await
        .unwrap()
        .calls
        .remove(0);
    let request = call.request.unwrap();
    assert_eq!((request.observed_bytes, request.stored_bytes), (Some(6), 3));
}

#[tokio::test]
async fn live_subscribers_see_committed_observations_in_order() {
    let harness = Harness::new(CaptureOptions::default(), 2).await;
    let mut live = harness.recorder.subscribe();
    let Harness {
        dir: _dir,
        recorder,
        task,
    } = harness;
    let tap = recorder.start(
        CallStart {
            transport: Transport::Http,
            api: Provider::Codex.native_api(),
            endpoint: Endpoint::Responses,
            method: Method::Post,
            config_revision: ConfigRevision::new("rev"),
            attribution: Attribution::default(),
            caller: ren_recorder::caller::Caller::Unidentified,
            operator_revision: None,
            plan: None,
            attempts: 1,
        },
        AttemptStart::unchanged(UpstreamId::default_for(Provider::Codex), Provider::Codex),
    );
    let observe = async move {
        complete(
            &tap,
            b"{}",
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        drop(tap);
        let mut kinds = Vec::new();
        while kinds.len() < 5 {
            let observation = live.recv().await.unwrap();
            kinds.push(observation.kind.known().unwrap());
        }
        drop(recorder);
        kinds
    };
    let (result, kinds) = tokio::join!(task.run(), observe);
    result.unwrap();
    assert_eq!(
        kinds,
        [
            ObservationKind::CallOpened,
            ObservationKind::AttemptStarted,
            ObservationKind::ResponseHead,
            ObservationKind::AttemptFinished,
            ObservationKind::CallFinished,
        ]
    );
}

#[tokio::test]
async fn a_failed_commit_keeps_lifecycle_records_and_reports_the_lost_bytes() {
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    let harness = Harness::new(CaptureOptions::default(), 2).await;
    let mut sabotage = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(harness.dir.path().join("ren.sqlite3")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER reject_segments BEFORE INSERT ON segments BEGIN SELECT RAISE(FAIL, 'fixture failure'); END")
        .execute(&mut sabotage)
        .await
        .unwrap();
    sabotage.close().await.unwrap();
    let tap = harness.start();
    let call = tap.call();
    complete(&tap, b"request body", MESSAGE_STREAM);
    drop(tap);
    let (_dir, metrics, reader) = harness.drain().await;
    assert_eq!(metrics.storage_errors, 1);
    assert!(!metrics.healthy());
    assert!(metrics.omissions.storage_failure_bytes > 0);
    assert!(metrics.commits >= 1);
    let detail = detail(&reader, call).await;
    assert_eq!(detail.call.state, Stored::Known(CallState::Finished));
    assert!(!detail.call.capture_complete);
    assert_eq!(detail.call.request.unwrap().stored_bytes, 0);
    // Inspection describes the bytes the recorder saw, before storage failed.
    assert_eq!(
        detail.call.outcome,
        Some(Stored::Known(ModelOutcome::Completed))
    );
}

/// A Messages request whose newest user input mentions `word` and carries a
/// synthetic key, and a streamed response with text and a tool call.
fn exchange(word: &str) -> (Vec<u8>, Vec<u8>) {
    let key = format!("sk-ant-{}a1b2", "api03xY9z".repeat(5));
    let request = serde_json::json!({
        "model": "claude-test",
        "messages": [
            { "role": "user", "content": format!("Find the {word} with {key}") },
            { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Bash", "input": {} }] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t1", "content": format!("{word} found; K={key}") },
                { "type": "text", "text": format!("Now explain the {word}") },
            ] },
        ],
    })
    .to_string()
    .into_bytes();
    let event = |name: &str, data: serde_json::Value| format!("event: {name}\ndata: {data}\n\n");
    let response = [
        event("message_start", serde_json::json!({ "type": "message_start", "message": { "model": "claude-test" } })),
        event("content_block_start", serde_json::json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
        event("content_block_delta", serde_json::json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": format!("The {word} is here.") } })),
        event("content_block_stop", serde_json::json!({ "type": "content_block_stop", "index": 0 })),
        event("content_block_start", serde_json::json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "t2", "name": "Read", "input": {} } })),
        event("content_block_delta", serde_json::json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"file_path\":\"src/lib.rs\"}" } })),
        event("content_block_stop", serde_json::json!({ "type": "content_block_stop", "index": 1 })),
        event("message_delta", serde_json::json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } })),
        event("message_stop", serde_json::json!({ "type": "message_stop" })),
    ]
    .concat()
    .into_bytes();
    (request, response)
}

async fn stored_digest(dir: &std::path::Path, call: CallId) -> Option<CallDigest> {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join("ren.sqlite3"))
        .read_only(true);
    let mut connection = sqlx::ConnectOptions::connect(&options).await.unwrap();
    let digest: Option<String> =
        sqlx::query_scalar("SELECT digest FROM call_digests WHERE call = ?")
            .bind(call.to_string())
            .fetch_optional(&mut connection)
            .await
            .unwrap();
    digest.map(|digest| serde_json::from_str(&digest).unwrap())
}

async fn count(dir: &std::path::Path, sql: &'static str) -> i64 {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join("ren.sqlite3"))
        .read_only(true);
    let mut connection = sqlx::ConnectOptions::connect(&options).await.unwrap();
    sqlx::query_scalar(sql)
        .fetch_one(&mut connection)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_digest_derived_while_recording_equals_one_derived_from_storage() {
    let harness = Harness::new(CaptureOptions::default(), 4).await;
    let mut calls = Vec::new();
    for word in ["needle", "haystack"] {
        let (request, response) = exchange(word);
        let tap = harness.start();
        calls.push(tap.call());
        tap.data(Direction::Request, &request);
        tap.end(Direction::Request);
        tap.response_head(200, ResponseFormat::EventStream, ResponseEncoding::Identity);
        // Arbitrary fragments, including inside an event.
        for piece in response.chunks(11) {
            tap.data(Direction::Response, piece);
        }
        tap.end(Direction::Response);
        tap.finish(ended(AttemptTerminal::BodyComplete));
    }
    let (dir, _metrics, _reader) = harness.drain().await;

    let live = stored_digest(dir.path(), calls[0]).await.unwrap();
    assert_eq!(live.prompt.as_ref().unwrap().text, "Now explain the needle");
    assert_eq!(live.reply.as_ref().unwrap().text, "The needle is here.");
    assert_eq!(live.tool_uses[0].name, "Read");
    assert_eq!(
        live.tool_results[0].output.text,
        "needle found; K=«redacted: anthropic_key»"
    );
    // The key in the first message is history, not this request's input;
    // the tool output that repeats it is.
    assert_eq!(live.exposures.len(), 1);
    let stored = serde_json::to_string(&live).unwrap();
    assert!(!stored.contains("api03"));
    assert_eq!(
        count(
            dir.path(),
            "SELECT COUNT(*) FROM search WHERE search MATCH 'needle'"
        )
        .await,
        3
    );
    assert_eq!(
        count(dir.path(), "SELECT COUNT(*) FROM call_exposures").await,
        2
    );

    // Derived again from the stored bodies, through the sink port.
    let clock = ManualClock::at(UnixMillis::from_millis(10_000));
    let (mut store, _) = Store::open(dir.path(), &clock, Arc::new(SequentialIds::default()))
        .await
        .unwrap();
    let scanner = SecretScanner::new().unwrap();
    let exchanges = store.undigested(CallDigest::VERSION + 1, 10).await.unwrap();
    assert_eq!(exchanges.len(), 2);
    for exchange in exchanges {
        let call = exchange.call;
        let replayed = exchange.digest(&scanner);
        assert_eq!(Some(replayed), stored_digest(dir.path(), call).await);
    }
    // Current digests are not listed again.
    assert!(
        store
            .undigested(CallDigest::VERSION, 10)
            .await
            .unwrap()
            .is_empty()
    );

    // Retention deletes a call's derived rows with it, index included.
    let policy = RetentionPolicy {
        max_calls: 1,
        ..RetentionPolicy::default()
    };
    store.prune(&policy, clock.now().wall).await.unwrap();
    store.close().await.unwrap();
    assert_eq!(stored_digest(dir.path(), calls[0]).await, None);
    assert!(stored_digest(dir.path(), calls[1]).await.is_some());
    assert_eq!(
        count(
            dir.path(),
            "SELECT COUNT(*) FROM search WHERE search MATCH 'needle'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            dir.path(),
            "SELECT COUNT(*) FROM search WHERE search MATCH 'haystack'"
        )
        .await,
        3
    );
    assert_eq!(
        count(dir.path(), "SELECT COUNT(*) FROM call_exposures").await,
        1
    );
}

#[tokio::test]
async fn a_websocket_calls_digest_is_derived_from_its_messages_alike() {
    use ren_protocol::websocket::Payload;
    let harness = Harness::new(CaptureOptions::default(), 4).await;
    let channel = harness
        .recorder
        .channel(ren_recorder::id::ChannelId::restore(uuid::Uuid::from_u128(
            77,
        )));
    let tap = harness.recorder.start_in(
        channel.channel(),
        CallStart {
            transport: Transport::WebSocket,
            api: Provider::OpenAi.native_api(),
            endpoint: Endpoint::Responses,
            method: Method::Message,
            config_revision: ConfigRevision::new("rev"),
            attribution: Attribution::default(),
            caller: ren_recorder::caller::Caller::Unidentified,
            operator_revision: None,
            plan: None,
            attempts: 1,
        },
        AttemptStart::unchanged(UpstreamId::default_for(Provider::OpenAi), Provider::OpenAi),
    );
    let call = tap.call();
    let create = serde_json::json!({ "type": "response.create", "input": [
        { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "Run the suite" }] },
    ] })
    .to_string();
    tap.message(Direction::Request, Payload::Text, create.as_bytes());
    tap.end(Direction::Request);
    tap.response_head(
        101,
        ResponseFormat::WebSocketMessages,
        ResponseEncoding::Identity,
    );
    let item = serde_json::json!({ "type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"command\":[\"cargo\",\"test\"]}" });
    for event in [
        serde_json::json!({ "type": "response.created", "response": {} }),
        serde_json::json!({ "type": "response.output_item.done", "item": item }),
        serde_json::json!({ "type": "response.completed", "response": { "output": [item] } }),
    ] {
        tap.message(
            Direction::Response,
            Payload::Text,
            event.to_string().as_bytes(),
        );
    }
    tap.end(Direction::Response);
    tap.finish(ended(AttemptTerminal::TerminalEvent));
    drop(tap);
    drop(channel);
    let (dir, _metrics, _reader) = harness.drain().await;
    let live = stored_digest(dir.path(), call).await.unwrap();
    assert_eq!(live.prompt.as_ref().unwrap().text, "Run the suite");
    assert_eq!(live.tool_uses[0].name, "shell");

    let clock = ManualClock::at(UnixMillis::from_millis(10_000));
    let (mut store, _) = Store::open(dir.path(), &clock, Arc::new(SequentialIds::default()))
        .await
        .unwrap();
    let scanner = SecretScanner::new().unwrap();
    let exchanges = store.undigested(CallDigest::VERSION + 1, 10).await.unwrap();
    let replayed = exchanges
        .into_iter()
        .find(|exchange| exchange.call == call)
        .unwrap()
        .digest(&scanner);
    assert_eq!(replayed, live);
    store.close().await.unwrap();
}

#[tokio::test]
async fn the_recorder_derives_missing_digests_from_stored_bodies() {
    let harness = Harness::new(CaptureOptions::default(), 4).await;
    let (request, response) = exchange("needle");
    let tap = harness.start();
    let call = tap.call();
    complete(&tap, &request, &response);
    drop(tap);
    let (dir, _metrics, _reader) = harness.drain().await;
    let live = stored_digest(dir.path(), call).await.unwrap();

    // As if recorded before digests existed.
    let options =
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("ren.sqlite3"));
    let mut connection = sqlx::ConnectOptions::connect(&options).await.unwrap();
    sqlx::query("DELETE FROM call_digests")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("DELETE FROM search_texts")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::Connection::close(connection).await.unwrap();
    assert_eq!(stored_digest(dir.path(), call).await, None);

    // The next recorder run derives it again on its first pass.
    let clock = Arc::new(ManualClock::at(UnixMillis::from_millis(20_000)));
    let ids = Arc::new(SequentialIds::default());
    let (store, recovery) = Store::open(dir.path(), clock.as_ref(), ids.clone())
        .await
        .unwrap();
    let setup = RecorderSetup {
        options: CaptureOptions::default(),
        retention: RetentionPolicy::default(),
        max_calls: 4,
        live_buffer: 64,
        scanner: Arc::new(SecretScanner::new().unwrap()),
    };
    let (recorder, task) = Recorder::new(store, recovery.calls, setup, clock, ids).unwrap();
    let (result, ()) = tokio::join!(task.run(), async move {
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if count(dir.path(), "SELECT COUNT(*) FROM call_digests").await > 0 {
                break;
            }
        }
        drop(recorder);
        assert_eq!(stored_digest(dir.path(), call).await, Some(live));
        assert_eq!(
            count(
                dir.path(),
                "SELECT COUNT(*) FROM search WHERE search MATCH 'needle'"
            )
            .await,
            3
        );
    });
    result.unwrap();
}

#[tokio::test]
async fn a_runs_lifecycle_is_recorded_and_read_back() {
    use ren_recorder::capture::{ProcessExit, RunEnd};
    use ren_recorder::id::RunId;
    let harness = Harness::new(CaptureOptions::default(), 8).await;
    let ids = SequentialIds::starting_at(100);
    let run = RunId::generate(&ids);
    let session = ren_recorder::id::SessionId::generate(&ids);
    let tap = harness
        .recorder
        .run(
            run,
            ren_recorder::RunSession::New(session),
            ren_harness::Harness::Codex,
            "/work/acme".into(),
            ren_harness::settings::RunSettings {
                model: Some("gpt-deep".parse().unwrap()),
                effort: Some(ren_harness::settings::Effort::High),
                approvals: Some(ren_harness::settings::ApprovalPolicy::Edits),
            },
            Some(ren_depot::toolset::Generation::from(3)),
        )
        .unwrap();
    let thread = ren_protocol::native::NativeThreadId::new(uuid::Uuid::from_u128(7)).unwrap();
    tap.opened(thread);
    // The run's model traffic: Codex names its thread as the session.
    let call = harness.recorder.start(
        CallStart {
            transport: Transport::Http,
            api: Provider::Codex.native_api(),
            endpoint: Endpoint::Responses,
            method: Method::Post,
            config_revision: ConfigRevision::new("rev"),
            attribution: Attribution {
                hint: Some(ren_harness::attribution::SessionHint {
                    source: ren_harness::attribution::HintSource::CodexHeaders,
                    session: ren_protocol::native::NativeSessionId::new(thread.as_uuid()).unwrap(),
                    turn: None,
                    thread: Some(thread),
                }),
                issues: Vec::new(),
            },
            caller: ren_recorder::caller::Caller::Unidentified,
            operator_revision: None,
            plan: None,
            attempts: 1,
        },
        AttemptStart::unchanged(UpstreamId::default_for(Provider::Codex), Provider::Codex),
    );
    let call_id = call.call();
    drop(call);
    tap.end(RunEnd::Stopped, Some(ProcessExit::Signal(9)));
    let unfinished = RunId::generate(&ids);
    let left = harness
        .recorder
        .run(
            unfinished,
            ren_recorder::RunSession::Existing(session),
            ren_harness::Harness::Codex,
            "/work/b".into(),
            ren_harness::settings::RunSettings::default(),
            Some(ren_depot::toolset::Generation::from(2)),
        )
        .unwrap();
    drop(left);
    let (dir, _, reader) = harness.drain().await;

    let runs = reader
        .runs(
            ren_store::query::PageLimit::DEFAULT,
            ren_store::query::Shelf::Current,
        )
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);
    let ended = reader.run(run).await.unwrap().unwrap();
    assert_eq!(ended.harness, Stored::Known(ren_harness::Harness::Codex));
    assert_eq!(ended.workdir, "/work/acme");
    // Its settings as started; the other run left every one to its client.
    assert_eq!(ended.model.as_deref(), Some("gpt-deep"));
    assert_eq!(
        ended.effort,
        Some(Stored::Known(ren_harness::settings::Effort::High))
    );
    assert_eq!(
        ended.approvals,
        Some(Stored::Known(ren_harness::settings::ApprovalPolicy::Edits))
    );
    assert_eq!(ended.thread, Some(thread.to_string()));
    let recording = reader.call(call_id).await.unwrap().call.session;
    assert!(recording.is_some());
    assert_eq!(
        ended.session, recording,
        "the run's recording is its thread's"
    );
    assert_eq!(ended.end, Some(Stored::Known(RunEnd::Stopped)));
    assert_eq!(ended.exit, Some(ProcessExit::Signal(9)));
    let open = reader.run(unfinished).await.unwrap().unwrap();
    assert_eq!((open.end, open.exit), (None, None), "still running");
    // Each run's toolset generation; collection keeps the running one's.
    assert_eq!(ended.toolset, Some(ren_depot::toolset::Generation::from(3)));
    assert_eq!(open.toolset, Some(ren_depot::toolset::Generation::from(2)));
    assert_eq!(
        reader.live_toolsets().await.unwrap(),
        [ren_depot::toolset::Generation::from(2)]
    );
    assert_eq!(
        (open.model, open.effort, open.approvals),
        (None, None, None)
    );
    let kinds: Vec<_> = reader
        .observations_after(0, ren_store::query::PageLimit::DEFAULT)
        .await
        .unwrap()
        .into_iter()
        .filter(|observation| observation.call.is_none())
        .map(|observation| observation.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            ObservationKind::SessionCreated,
            ObservationKind::RunStarted,
            ObservationKind::RunOpened,
            ObservationKind::RunEnded,
            ObservationKind::RunStarted,
        ]
        .map(Stored::Known)
    );
    // Both runs served the one session, created with the first: newest run
    // first, and the session's settings are those it was created with.
    let sessions = reader
        .sessions(
            ren_store::query::PageLimit::DEFAULT,
            ren_store::query::Shelf::Current,
        )
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session);
    assert_eq!(sessions[0].workdir, "/work/acme");
    assert_eq!(sessions[0].model.as_deref(), Some("gpt-deep"));
    assert_eq!(
        sessions[0]
            .runs
            .iter()
            .map(|run| run.id)
            .collect::<Vec<_>>(),
        [unfinished, run]
    );
    assert_eq!(
        reader.session(session).await.unwrap(),
        Some(sessions[0].clone())
    );
    reader.close().await;

    // A restart marks the run that never ended as recovered.
    let clock = ManualClock::at(UnixMillis::from_millis(9_000));
    let (store, recovery) = Store::open(
        dir.path(),
        &clock,
        Arc::new(SequentialIds::starting_at(500)),
    )
    .await
    .unwrap();
    assert_eq!(recovery.runs, 1);
    store.close().await.unwrap();
    let reader = Reader::open(dir.path(), 1).await.unwrap();
    let recovered = reader.run(unfinished).await.unwrap().unwrap();
    assert_eq!(recovered.end, Some(Stored::Known(RunEnd::Recovered)));
    assert_eq!(recovered.ended_at, Some(UnixMillis::from_millis(9_000)));
    assert_eq!(recovered.exit, None);
    assert_eq!(
        reader.run(run).await.unwrap().unwrap().end,
        Some(Stored::Known(RunEnd::Stopped))
    );
}
