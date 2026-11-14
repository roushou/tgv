//! The single writer: ownership, migrations, recovery, commits, and retention.

use super::chunk::{ChunkHash, EncodedChunk};
use crate::Reader;
use crate::{
    StoreError,
    view::{DiskUsage, FileUsage},
};
use bytes::Bytes;
use ren_harness::{drive::DriveEvent, models::ModelId};
use ren_protocol::{
    direction::Direction,
    label::{Labeled, Stored},
    route::Transport,
};
use ren_recorder::caller::{TailscaleLogin, UserId};
use ren_recorder::digest::{CallDigest, StoredExchange};
use ren_recorder::{Committed, Observation, Pruned, RecordSink};
use ren_recorder::{
    batch::{
        AttemptBody, Batch, BodyEnd, BodyKey, CloseChannel, FinishAttempt, FinishCall,
        HistoryChange, HistoryEdit, HistorySubject, MessageMark, ObservationKind, Op, OpenCall,
        OperatorAction, ResponseHead, RunEntry, RunFact, RunRecord, Segment, SessionFact,
        SessionRecord, Stamp, StartAttempt,
    },
    inspection::InspectionRecord,
    retention::RetentionPolicy,
};
use ren_recorder::{
    capture::{BodyEnding, CallState, ChannelEnd, ProcessExit, RunEnd},
    id::{CallId, ChannelId, IdSource, RecordingId, RunId, TurnId},
    routing::OperatorRevision,
    time::{Clock, Moment, UnixMillis},
};
use sqlx::{
    AssertSqlSafe, Connection, QueryBuilder, Row, Sqlite, SqliteConnection, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    future::Future,
    path::Path,
    sync::Arc,
    time::Duration,
};

/// Owns the data directory for one daemon run.
pub struct Store {
    connection: SqliteConnection,
    ids: Arc<dyn IdSource>,
    sweeps: SweepGate,
    /// Reads stored bodies back, for digests derived again.
    bodies: Reader,
    _lock: File,
}

/// Holds off the chunk sweep while a backup copies chunks: retention may
/// remove calls, but not the chunks a backup's snapshot still references.
#[derive(Debug, Clone, Default)]
pub struct SweepGate(Arc<tokio::sync::RwLock<()>>);

/// While held, the sweep skips its pass; skipped chunks go on a later pass.
#[must_use = "the sweep resumes when the hold is dropped"]
pub struct SweepHold(
    #[expect(dead_code, reason = "held for its drop")] tokio::sync::OwnedRwLockReadGuard<()>,
);

impl SweepGate {
    /// Wait for a sweep in progress to finish, then hold off the next ones.
    pub async fn hold(&self) -> SweepHold {
        SweepHold(self.0.clone().read_owned().await)
    }
}

/// What startup found from the previous run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Calls left open by a daemon that stopped; now marked recovered.
    pub calls: u64,
    /// Agent runs a stopped daemon never saw end; now marked recovered.
    pub runs: u64,
    /// The operator state the last recorded command made, as stored; `None`
    /// in a new data directory.
    pub operator: Option<StoredOperatorState>,
}

/// The stored operator snapshot: the revision recorded beside it and its
/// document, unparsed. The gateway owns the document's format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOperatorState {
    pub revision: OperatorRevision,
    pub document: Box<str>,
}

/// The payload of a run's recovery observation.
#[derive(serde::Serialize)]
struct RecoveredRun {
    run: RunId,
}

const PRUNE_BATCH: i64 = 16;
const OPERATOR_ACTIONS_KEPT: i64 = 10_000;
const SWEEP_BATCH: i64 = 512;
const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

impl Store {
    pub(super) const LOCK_FILE: &'static str = "daemon.lock";
    pub const METADATA_FILE: &'static str = "ren.sqlite3";
    pub(super) const BLOBS_FILE: &'static str = "blobs.sqlite3";
    /// The chunk table's definition, shared with backups.
    pub(super) const CHUNKS_COLUMNS: &'static str = "(hash BLOB PRIMARY KEY, codec TEXT NOT NULL, raw_len INTEGER NOT NULL, data BLOB NOT NULL) WITHOUT ROWID";
    pub(super) const PAGE_SIZE_BLOBS: u32 = 16_384;

    /// Take ownership of `dir`, migrate, and recover calls the previous run
    /// left open. Fails with [`StoreError::Locked`] if another daemon owns it.
    pub async fn open<C: Clock + ?Sized>(
        dir: &Path,
        clock: &C,
        ids: Arc<dyn IdSource>,
    ) -> Result<(Self, Recovery), StoreError> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
        let lock = Self::private_file(&dir.join(Self::LOCK_FILE))?;
        lock.try_lock().map_err(|_| StoreError::Locked)?;
        let mut recovery = Recovery::default();
        Self::private_file(&dir.join(Self::METADATA_FILE))?;
        Self::private_file(&dir.join(Self::BLOBS_FILE))?;
        let options = SqliteConnectOptions::new()
            .filename(dir.join(Self::METADATA_FILE))
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .pragma("journal_size_limit", JOURNAL_SIZE_LIMIT.to_string())
            .busy_timeout(Duration::from_secs(5));
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::migrate!("./migrations").run(&mut connection).await?;
        Self::attach_blobs(&mut connection, dir, BlobAccess::Write).await?;
        let mut store = Self {
            connection,
            ids,
            sweeps: SweepGate::default(),
            bodies: Reader::open(dir, 1).await?,
            _lock: lock,
        };
        recovery.calls = store.recover(clock.now()).await?;
        recovery.runs = store.recover_runs(clock.now()).await?;
        recovery.operator = store.operator_state().await?;
        Ok((store, recovery))
    }

    /// The stored operator snapshot, as stored.
    async fn operator_state(&mut self) -> Result<Option<StoredOperatorState>, StoreError> {
        let stored: Option<(i64, String)> =
            sqlx::query_as("SELECT revision, state FROM operator_state WHERE id = 1")
                .fetch_optional(&mut self.connection)
                .await?;
        stored
            .map(|(revision, document)| {
                Ok(StoredOperatorState {
                    revision: OperatorRevision::restore(
                        u64::try_from(revision)
                            .map_err(|_| StoreError::Corrupt("negative operator revision"))?,
                    ),
                    document: document.into(),
                })
            })
            .transpose()
    }

    pub(super) fn private_file(path: &Path) -> Result<File, StoreError> {
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(path)?)
    }

    pub(super) async fn attach_blobs(
        connection: &mut SqliteConnection,
        dir: &Path,
        access: BlobAccess,
    ) -> Result<(), sqlx::Error> {
        let path = dir.join(Self::BLOBS_FILE);
        let path = path
            .to_str()
            .ok_or_else(|| sqlx::Error::Configuration(StoreError::NonUtf8Path.into()))?;
        sqlx::query("ATTACH DATABASE ? AS blobs")
            .bind(path)
            .execute(&mut *connection)
            .await?;
        match access {
            BlobAccess::Read => {}
            BlobAccess::Write => {
                // Page size only applies before the first table exists.
                for pragma in [
                    format!("PRAGMA blobs.page_size = {}", Self::PAGE_SIZE_BLOBS),
                    "PRAGMA blobs.journal_mode = WAL".to_owned(),
                    "PRAGMA blobs.synchronous = NORMAL".to_owned(),
                    format!("PRAGMA blobs.journal_size_limit = {JOURNAL_SIZE_LIMIT}"),
                    format!(
                        "CREATE TABLE IF NOT EXISTS blobs.chunks {}",
                        Self::CHUNKS_COLUMNS
                    ),
                ] {
                    sqlx::query(AssertSqlSafe(pragma))
                        .execute(&mut *connection)
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Calls the previous run never finished keep an unknown outcome: they are
    /// marked recovered, with unfinished bodies, and get an observation.
    async fn recover(&mut self, now: Moment) -> Result<u64, StoreError> {
        let mut tx = MetadataTx {
            tx: self.connection.begin().await?,
            ids: self.ids.as_ref(),
        };
        let open = sqlx::query("SELECT id, channel FROM calls WHERE state = ?")
            .bind(CallState::Open.label())
            .fetch_all(&mut *tx.tx)
            .await?;
        sqlx::query("UPDATE bodies SET ending = ? WHERE ending = ?")
            .bind(BodyEnding::Unfinished.label())
            .bind(BodyEnding::Open.label())
            .execute(&mut *tx.tx)
            .await?;
        let channels =
            sqlx::query("SELECT id FROM channels WHERE transport = ? AND ending IS NULL")
                .bind(Transport::WebSocket.label())
                .fetch_all(&mut *tx.tx)
                .await?;
        for row in &channels {
            let channel: String = row.try_get("id")?;
            sqlx::query("UPDATE channels SET ending = ? WHERE id = ?")
                .bind(ChannelEnd::Recovered.label())
                .bind(&channel)
                .execute(&mut *tx.tx)
                .await?;
        }
        for row in &open {
            let call: String = row.try_get("id")?;
            let channel: String = row.try_get("channel")?;
            let stamp = Stamp {
                at: now,
                channel: channel
                    .parse()
                    .map_err(|_| StoreError::Corrupt("identifier"))?,
                call: call
                    .parse()
                    .map_err(|_| StoreError::Corrupt("identifier"))?,
            };
            sqlx::query("UPDATE calls SET state = ?, capture_complete = 0 WHERE id = ?")
                .bind(CallState::Recovered.label())
                .bind(&call)
                .execute(&mut *tx.tx)
                .await?;
            tx.append(&stamp, None, ObservationKind::CallRecovered, &())
                .await?;
        }
        tx.commit().await?;
        Ok(u64::try_from(open.len()).unwrap_or(u64::MAX))
    }

    /// Runs the previous daemon never saw end keep an unknown outcome: they
    /// are marked recovered, and get an observation.
    async fn recover_runs(&mut self, now: Moment) -> Result<u64, StoreError> {
        let mut tx = MetadataTx {
            tx: self.connection.begin().await?,
            ids: self.ids.as_ref(),
        };
        let open: Vec<String> = sqlx::query_scalar("SELECT id FROM runs WHERE run_end IS NULL")
            .fetch_all(&mut *tx.tx)
            .await?;
        for run in &open {
            sqlx::query("UPDATE runs SET run_end = ?, ended_at = ? WHERE id = ?")
                .bind(RunEnd::Recovered.label())
                .bind(i64::try_from(now.wall.as_millis()).unwrap_or(i64::MAX))
                .bind(run)
                .execute(&mut *tx.tx)
                .await?;
            let run: RunId = run.parse().map_err(|_| StoreError::Corrupt("identifier"))?;
            tx.log(
                now,
                ChannelId::restore(uuid::Uuid::nil()),
                None,
                None,
                ObservationKind::RunRecovered,
                &RecoveredRun { run },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(u64::try_from(open.len()).unwrap_or(u64::MAX))
    }

    /// Commit a batch: new chunks first, then every operation and its
    /// observation in one metadata transaction. A crash between the two leaves
    /// only unreferenced chunks, which the next sweep removes.
    pub async fn commit(&mut self, batch: &Batch) -> Result<Committed, StoreError> {
        let mut committed = Committed::default();
        let hashes = self
            .store_chunks(batch.chunks().to_vec(), &mut committed)
            .await?;
        let mut tx = MetadataTx {
            tx: self.connection.begin().await?,
            ids: self.ids.as_ref(),
        };
        for op in batch.operations() {
            if let Some(observation) = tx.apply(op, &hashes).await? {
                committed.observations.push(observation);
            }
        }
        tx.commit().await?;
        Ok(committed)
    }

    async fn store_chunks(
        &mut self,
        chunks: Vec<Bytes>,
        committed: &mut Committed,
    ) -> Result<Vec<StoredChunk>, StoreError> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        let (chunks, hashes) = tokio::task::spawn_blocking(move || {
            let hashes: Vec<_> = chunks.iter().map(|chunk| ChunkHash::of(chunk)).collect();
            (chunks, hashes)
        })
        .await?;
        let mut existing = HashSet::new();
        let mut query =
            QueryBuilder::<Sqlite>::new("SELECT hash FROM blobs.chunks WHERE hash IN (");
        let mut list = query.separated(", ");
        for hash in &hashes {
            list.push_bind(hash.as_bytes().to_vec());
        }
        query.push(")");
        for row in query.build().fetch_all(&mut self.connection).await? {
            let bytes: Vec<u8> = row.try_get("hash")?;
            if let Some(hash) = ChunkHash::restore(&bytes) {
                existing.insert(hash);
            }
        }
        let mut missing = Vec::new();
        for (hash, chunk) in hashes.iter().zip(&chunks) {
            committed.referenced_bytes = committed
                .referenced_bytes
                .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            if existing.insert(*hash) {
                missing.push((*hash, chunk.clone()));
            }
        }
        let encoded = tokio::task::spawn_blocking(move || {
            missing
                .into_iter()
                .map(|(hash, raw)| EncodedChunk::encode(hash, &raw))
                .collect::<Vec<_>>()
        })
        .await?;
        let mut tx = self.connection.begin().await?;
        for chunk in &encoded {
            committed.new_bytes = committed
                .new_bytes
                .saturating_add(u64::try_from(chunk.raw_len).unwrap_or(u64::MAX));
            committed.compressed_bytes = committed
                .compressed_bytes
                .saturating_add(u64::try_from(chunk.data.len()).unwrap_or(u64::MAX));
            sqlx::query(
                "INSERT OR IGNORE INTO blobs.chunks(hash, codec, raw_len, data) VALUES (?, ?, ?, ?)",
            )
            .bind(chunk.hash.as_bytes())
            .bind(chunk.codec.label())
            .bind(i64::try_from(chunk.raw_len).unwrap_or(i64::MAX))
            .bind(chunk.data.as_slice())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(hashes
            .into_iter()
            .zip(&chunks)
            .map(|(hash, chunk)| StoredChunk {
                hash,
                len: u64::try_from(chunk.len()).unwrap_or(u64::MAX),
            })
            .collect())
    }

    /// One retention pass: delete the oldest finished calls while any limit is
    /// exceeded, then sweep chunks nothing references, and measure again.
    pub async fn prune(
        &mut self,
        policy: &RetentionPolicy,
        now: UnixMillis,
    ) -> Result<Pruned, StoreError> {
        let mut pruned = Pruned::default();
        if !policy.enabled {
            return Ok(pruned);
        }
        let cutoff = now.as_millis().saturating_sub(policy.max_age_millis());
        let retained: u64 = sqlx::query_scalar("SELECT COUNT(*) FROM calls")
            .fetch_one(&mut self.connection)
            .await?;
        let over_bytes = self.disk_usage().await?.live_bytes() > policy.max_storage_bytes;
        let over_calls = retained.saturating_sub(policy.max_calls);
        let candidates = sqlx::query(
            "SELECT id, channel, session, started_at FROM calls WHERE state <> ? ORDER BY started_at, id LIMIT ?",
        )
        .bind(CallState::Open.label())
        .bind(PRUNE_BATCH)
        .fetch_all(&mut self.connection)
        .await?;
        let mut tx = MetadataTx {
            tx: self.connection.begin().await?,
            ids: self.ids.as_ref(),
        };
        for (index, row) in candidates.iter().enumerate() {
            let started: u64 = row.try_get("started_at")?;
            let rank = u64::try_from(index).unwrap_or(u64::MAX);
            if started >= cutoff && rank >= over_calls && !over_bytes {
                break;
            }
            let call: String = row.try_get("id")?;
            let channel: String = row.try_get("channel")?;
            let session: Option<String> = row.try_get("session")?;
            tx.retire(&call, &channel, session.as_deref()).await?;
            pruned.calls = pruned.calls.saturating_add(1);
        }
        // Operator actions are few; keep the newest, with their log rows.
        for statement in [
            "DELETE FROM observations WHERE kind = ?1 AND call IS NULL AND seq < (SELECT COALESCE(MIN(seq), 0) FROM (SELECT seq FROM observations WHERE kind = ?1 ORDER BY seq DESC LIMIT ?2))",
            "DELETE FROM operator_actions WHERE seq <= (SELECT COALESCE(MAX(seq), 0) FROM operator_actions) - ?2 AND ?1 = ?1",
        ] {
            sqlx::query(statement)
                .bind(ObservationKind::OperatorAction.label())
                .bind(OPERATOR_ACTIONS_KEPT)
                .execute(&mut *tx.tx)
                .await?;
        }
        tx.commit().await?;
        pruned.chunks = match self.sweeps.0.clone().try_write_owned() {
            Ok(_sweeping) => self.sweep().await?,
            Err(_) => {
                tracing::debug!("a backup is running; chunk sweep deferred");
                0
            }
        };
        pruned.more = pruned.calls == u64::try_from(PRUNE_BATCH).unwrap_or(u64::MAX);
        Ok(pruned)
    }

    /// Mark and sweep: remove chunks no segment references. Runs between
    /// commits on the writer, so no committed chunk is waiting for segments.
    async fn sweep(&mut self) -> Result<u64, StoreError> {
        let mut total = 0;
        loop {
            let removed = sqlx::query("DELETE FROM blobs.chunks WHERE hash IN (SELECT c.hash FROM blobs.chunks c WHERE NOT EXISTS (SELECT 1 FROM main.segments s WHERE s.chunk = c.hash) LIMIT ?)")
                .bind(SWEEP_BATCH)
                .execute(&mut self.connection)
                .await?
                .rows_affected();
            total += removed;
            if removed < u64::try_from(SWEEP_BATCH).unwrap_or(u64::MAX) {
                return Ok(total);
            }
        }
    }

    /// The gate a [`Backup`](super::backup::Backup) holds while it copies.
    pub fn sweep_gate(&self) -> SweepGate {
        self.sweeps.clone()
    }

    pub async fn disk_usage(&mut self) -> Result<DiskUsage, StoreError> {
        DiskUsage::measure(&mut self.connection).await
    }

    /// Move WAL contents into the database files and restart the logs.
    pub async fn checkpoint(&mut self) -> Result<(), StoreError> {
        for pragma in [
            "PRAGMA main.wal_checkpoint(RESTART)",
            "PRAGMA blobs.wal_checkpoint(RESTART)",
        ] {
            sqlx::query(pragma).execute(&mut self.connection).await?;
        }
        Ok(())
    }

    /// Close SQLite before releasing the ownership lock.
    pub async fn close(self) -> Result<(), StoreError> {
        self.connection.close().await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum BlobAccess {
    Read,
    Write,
}

impl DiskUsage {
    /// Both store files, through `connection`.
    pub(super) async fn measure(connection: &mut SqliteConnection) -> Result<Self, StoreError> {
        Ok(Self {
            metadata: FileUsage::measure(connection, "main").await?,
            blobs: FileUsage::measure(connection, "blobs").await?,
        })
    }
}

impl FileUsage {
    pub(crate) async fn measure(
        connection: &mut SqliteConnection,
        schema: &str,
    ) -> Result<Self, StoreError> {
        let mut values = [0_u64; 3];
        for (value, pragma) in values
            .iter_mut()
            .zip(["page_size", "page_count", "freelist_count"])
        {
            *value = sqlx::query_scalar(AssertSqlSafe(format!("PRAGMA {schema}.{pragma}")))
                .fetch_one(&mut *connection)
                .await?;
        }
        let [page_size, page_count, freelist_count] = values;
        Ok(Self {
            page_size,
            page_count,
            freelist_count,
        })
    }
}

/// A chunk the metadata transaction can reference.
#[derive(Debug, Clone, Copy)]
struct StoredChunk {
    hash: ChunkHash,
    len: u64,
}

/// One metadata transaction: batch operations, their log rows, and retention
/// deletes. Observations and the query tables always change together here.
struct MetadataTx<'a> {
    tx: Transaction<'a, Sqlite>,
    ids: &'a dyn IdSource,
}

impl MetadataTx<'_> {
    const LOG_SCHEMA: i64 = 1;

    async fn commit(self) -> Result<(), StoreError> {
        self.tx.commit().await?;
        Ok(())
    }

    async fn append<P: serde::Serialize>(
        &mut self,
        stamp: &Stamp,
        attempt: Option<String>,
        kind: ObservationKind,
        payload: &P,
    ) -> Result<Observation, StoreError> {
        self.log(
            stamp.at,
            stamp.channel,
            Some(stamp.call),
            attempt,
            kind,
            payload,
        )
        .await
    }

    async fn log<P: serde::Serialize>(
        &mut self,
        at: Moment,
        channel: ChannelId,
        call: Option<CallId>,
        attempt: Option<String>,
        kind: ObservationKind,
        payload: &P,
    ) -> Result<Observation, StoreError> {
        let payload = serde_json::to_string(payload)?;
        let mono = u64::try_from(at.monotonic.since_origin().as_millis()).unwrap_or(u64::MAX);
        let seq: u64 = sqlx::query_scalar("INSERT INTO observations(at_wall, at_mono, channel, call, attempt, kind, schema, payload) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING seq")
            .bind(i64::try_from(at.wall.as_millis()).unwrap_or(i64::MAX))
            .bind(i64::try_from(mono).unwrap_or(i64::MAX))
            .bind(channel.to_string())
            .bind(call.map(|call| call.to_string()))
            .bind(attempt.clone())
            .bind(kind.label())
            .bind(Self::LOG_SCHEMA)
            .bind(payload)
            .fetch_one(&mut *self.tx)
            .await?;
        Ok(Observation {
            seq,
            at: at.wall,
            kind: Stored::Known(kind),
            channel,
            call,
            attempt: attempt.and_then(|id| id.parse().ok()),
        })
    }

    async fn apply(
        &mut self,
        op: &Op,
        chunks: &[StoredChunk],
    ) -> Result<Option<Observation>, StoreError> {
        match op {
            Op::OpenCall(open) => self.open_call(open).await.map(Some),
            Op::StartAttempt(start) => self.start_attempt(start).await.map(Some),
            Op::ResponseHead(head) => self.response_head(head).await.map(Some),
            Op::Segments { body, segments } => {
                self.segments(*body, segments, chunks).await.map(|()| None)
            }
            Op::Message { body, mark } => self.message(*body, mark).await.map(|()| None),
            Op::FinishAttempt(finish) => self.finish_attempt(finish).await.map(Some),
            Op::FinishCall(finish) => self.finish_call(finish).await.map(Some),
            Op::CloseChannel(close) => self.close_channel(close).await.map(Some),
            Op::Operator(action) => self.operator(action).await.map(Some),
            Op::Session(record) => self.session(record).await.map(Some),
            Op::Run(record) => self.run(record).await.map(Some),
            Op::History(edit) => self.history(edit).await.map(Some),
            Op::RunEntry(entry) => self.run_entry(entry).await.map(|()| None),
            Op::Digest { call, digest } => self.digest(*call, digest).await.map(|()| None),
        }
    }

    /// Store a call's digest, replacing an earlier one with its exposures
    /// and searchable texts. A call pruned since its digest was derived is
    /// skipped.
    async fn digest(&mut self, call: CallId, digest: &CallDigest) -> Result<(), StoreError> {
        let id = call.to_string();
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM calls WHERE id = ?)")
            .bind(&id)
            .fetch_one(&mut *self.tx)
            .await?;
        if !exists {
            return Ok(());
        }
        for statement in [
            "DELETE FROM call_exposures WHERE call = ?",
            "DELETE FROM search_texts WHERE call = ?",
        ] {
            sqlx::query(statement)
                .bind(&id)
                .execute(&mut *self.tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO call_digests(call, version, purpose, prompt, prompt_omitted, digest) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(call) DO UPDATE SET version = ?2, purpose = ?3, prompt = ?4, prompt_omitted = ?5, digest = ?6",
        )
        .bind(&id)
        .bind(i64::from(digest.version))
        .bind(digest.purpose.label())
        .bind(digest.prompt.as_ref().map(|prompt| prompt.text.as_str()))
        .bind(
            digest
                .prompt
                .as_ref()
                .map(|prompt| i64::try_from(prompt.omitted_chars).unwrap_or(i64::MAX)),
        )
        .bind(serde_json::to_string(digest)?)
        .execute(&mut *self.tx)
        .await?;
        for exposure in &digest.exposures {
            sqlx::query(
                "INSERT INTO call_exposures(call, field, kind, hint, fingerprint, count) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(exposure.field.label())
            .bind(exposure.kind.label())
            .bind(&exposure.hint)
            .bind(&exposure.fingerprint)
            .bind(i64::try_from(exposure.count).unwrap_or(i64::MAX))
            .execute(&mut *self.tx)
            .await?;
        }
        for text in &digest.search {
            sqlx::query("INSERT INTO search_texts(call, field, text) VALUES (?, ?, ?)")
                .bind(&id)
                .bind(text.field.label())
                .bind(&text.text)
                .execute(&mut *self.tx)
                .await?;
        }
        Ok(())
    }

    async fn operator(&mut self, action: &OperatorAction) -> Result<Observation, StoreError> {
        sqlx::query(
            "INSERT INTO operator_actions(at_wall, revision, actor, command) VALUES (?, ?, ?, ?)",
        )
        .bind(i64::try_from(action.at.wall.as_millis()).unwrap_or(i64::MAX))
        .bind(i64::try_from(action.revision.get()).unwrap_or(i64::MAX))
        .bind(serde_json::to_string(&action.actor)?)
        .bind(action.command.as_str())
        .execute(&mut *self.tx)
        .await?;
        // Actions commit in revision order, so the last one is the current state.
        sqlx::query(
            "INSERT INTO operator_state(id, revision, state) VALUES (1, ?1, ?2) ON CONFLICT(id) DO UPDATE SET revision = ?1, state = ?2",
        )
        .bind(i64::try_from(action.revision.get()).unwrap_or(i64::MAX))
        .bind(action.state.as_str())
        .execute(&mut *self.tx)
        .await?;
        // The log needs a channel; operator actions share the nil channel,
        // which has no row.
        self.log(
            action.at,
            ChannelId::restore(uuid::Uuid::nil()),
            None,
            None,
            ObservationKind::OperatorAction,
            action,
        )
        .await
    }

    async fn session(&mut self, record: &SessionRecord) -> Result<Observation, StoreError> {
        let at = i64::try_from(record.at.wall.as_millis()).unwrap_or(i64::MAX);
        match &record.fact {
            SessionFact::Created {
                harness,
                workdir,
                settings,
            } => {
                sqlx::query(
                    "INSERT INTO operator_sessions(id, harness, workdir, created_at, model, effort, approvals) VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(record.session.to_string())
                .bind(harness.label())
                .bind(workdir)
                .bind(at)
                .bind(settings.model.as_ref().map(ModelId::as_str))
                .bind(settings.effort.map(Labeled::label))
                .bind(settings.approvals.map(Labeled::label))
                .execute(&mut *self.tx)
                .await?;
            }
            SessionFact::Configured { settings } => {
                sqlx::query(
                    "UPDATE operator_sessions SET model = ?, effort = ?, approvals = ? WHERE id = ?",
                )
                .bind(settings.model.as_ref().map(ModelId::as_str))
                .bind(settings.effort.map(Labeled::label))
                .bind(settings.approvals.map(Labeled::label))
                .bind(record.session.to_string())
                .execute(&mut *self.tx)
                .await?;
            }
            SessionFact::ClientChosen { harness } => {
                sqlx::query("UPDATE operator_sessions SET harness = ? WHERE id = ?")
                    .bind(harness.label())
                    .bind(record.session.to_string())
                    .execute(&mut *self.tx)
                    .await?;
            }
            SessionFact::Archived | SessionFact::Unarchived => {
                let archived = match record.fact {
                    SessionFact::Archived => Some(at),
                    SessionFact::Unarchived
                    | SessionFact::Created { .. }
                    | SessionFact::Configured { .. }
                    | SessionFact::ClientChosen { .. } => None,
                };
                sqlx::query("UPDATE operator_sessions SET archived_at = ? WHERE id = ?")
                    .bind(archived)
                    .bind(record.session.to_string())
                    .execute(&mut *self.tx)
                    .await?;
            }
        }
        // Sessions share the nil channel, which has no row, as runs do.
        self.log(
            record.at,
            ChannelId::restore(uuid::Uuid::nil()),
            None,
            None,
            record.fact.kind(),
            record,
        )
        .await
    }

    async fn run(&mut self, record: &RunRecord) -> Result<Observation, StoreError> {
        let id = record.run.to_string();
        let at = i64::try_from(record.at.wall.as_millis()).unwrap_or(i64::MAX);
        match &record.fact {
            RunFact::Started {
                session,
                harness,
                workdir,
                settings,
                toolset,
            } => {
                sqlx::query(
                    "INSERT INTO runs(id, operator_session, harness, workdir, started_at, model, effort, approvals, toolset_generation) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(session.to_string())
                .bind(harness.label())
                .bind(workdir)
                .bind(at)
                .bind(settings.model.as_ref().map(ModelId::as_str))
                .bind(settings.effort.map(Labeled::label))
                .bind(settings.approvals.map(Labeled::label))
                .bind(toolset.and_then(|generation| i64::try_from(generation.number()).ok()))
                .execute(&mut *self.tx)
                .await?;
            }
            RunFact::Configured { settings } => {
                // The session's settings are its newest, as its next turn runs.
                for table in ["runs", "operator_sessions"] {
                    let target = match table {
                        "runs" => "id = ?",
                        _ => "id = (SELECT operator_session FROM runs WHERE id = ?)",
                    };
                    sqlx::query(AssertSqlSafe(format!(
                        "UPDATE {table} SET model = ?, effort = ?, approvals = ? WHERE {target}"
                    )))
                    .bind(settings.model.as_ref().map(ModelId::as_str))
                    .bind(settings.effort.map(Labeled::label))
                    .bind(settings.approvals.map(Labeled::label))
                    .bind(&id)
                    .execute(&mut *self.tx)
                    .await?;
                }
            }
            RunFact::Opened { thread } => {
                sqlx::query("UPDATE runs SET thread = ? WHERE id = ?")
                    .bind(thread.to_string())
                    .bind(&id)
                    .execute(&mut *self.tx)
                    .await?;
            }
            RunFact::Ended { end, exit } => {
                let (kind, value) = match exit {
                    Some(ProcessExit::Code(code)) => (Some("code"), Some(*code)),
                    Some(ProcessExit::Signal(signal)) => (Some("signal"), Some(*signal)),
                    Some(ProcessExit::Unknown) => (Some("unknown"), None),
                    None => (None, None),
                };
                sqlx::query(
                    "UPDATE runs SET ended_at = ?, run_end = ?, exit_kind = ?, exit_value = ? WHERE id = ?",
                )
                .bind(at)
                .bind(end.label())
                .bind(kind)
                .bind(value)
                .bind(&id)
                .execute(&mut *self.tx)
                .await?;
            }
        }
        // Runs share the nil channel, which has no row, as operator actions do.
        self.log(
            record.at,
            ChannelId::restore(uuid::Uuid::nil()),
            None,
            None,
            record.fact.kind(),
            record,
        )
        .await
    }

    /// Store one entry of a run's conversation: appended, or, for an item
    /// already stored, in its place.
    async fn run_entry(&mut self, entry: &RunEntry) -> Result<(), StoreError> {
        let run = entry.run.to_string();
        let item = match &entry.event {
            DriveEvent::Item { item, .. } => Some(item.id.as_str().to_owned()),
            DriveEvent::Ready { .. }
            | DriveEvent::TurnStarted { .. }
            | DriveEvent::Delta { .. }
            | DriveEvent::Approval { .. }
            | DriveEvent::ApprovalResolved { .. }
            | DriveEvent::TurnEnded { .. }
            | DriveEvent::Error { .. }
            | DriveEvent::Refused { .. }
            | DriveEvent::Unsupported { .. }
            | DriveEvent::Unknown { .. }
            | DriveEvent::Unreadable { .. } => None,
        };
        let event = serde_json::to_value(&entry.event)?;
        let kind = event
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        sqlx::query(
            "INSERT INTO run_entries(run, seq, at, turn, item, kind, event) VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM run_entries WHERE run = ?1), ?2, ?3, ?4, ?5, ?6) ON CONFLICT(run, item) WHERE item IS NOT NULL DO UPDATE SET at = excluded.at, kind = excluded.kind, event = excluded.event",
        )
        .bind(&run)
        .bind(i64::try_from(entry.at.wall.as_millis()).unwrap_or(i64::MAX))
        .bind(entry.turn.map(|turn| turn.to_string()))
        .bind(item)
        .bind(kind)
        .bind(event.to_string())
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Archive, unarchive, or delete a session or a run. A run's edit
    /// applies to the sessions its client recorded under its thread too, and
    /// a session's to the runs that worked in it, so the two never disagree.
    async fn history(&mut self, edit: &HistoryEdit) -> Result<Observation, StoreError> {
        let at = i64::try_from(edit.at.wall.as_millis()).unwrap_or(i64::MAX);
        let (sessions, runs) = self.subjects(edit.subject).await?;
        match edit.change {
            HistoryChange::Archive | HistoryChange::Unarchive => {
                let at = match edit.change {
                    HistoryChange::Archive => Some(at),
                    HistoryChange::Unarchive | HistoryChange::Delete => None,
                };
                for (table, ids) in [("sessions", &sessions), ("runs", &runs)] {
                    for id in ids {
                        sqlx::query(AssertSqlSafe(format!(
                            "UPDATE {table} SET archived_at = ? WHERE id = ?"
                        )))
                        .bind(at)
                        .bind(id)
                        .execute(&mut *self.tx)
                        .await?;
                    }
                }
                // An operator session's own shelf; otherwise, while each
                // session has one run, it is archived with it.
                if let HistorySubject::OperatorSession(session) = edit.subject {
                    sqlx::query("UPDATE operator_sessions SET archived_at = ? WHERE id = ?")
                        .bind(at)
                        .bind(session.to_string())
                        .execute(&mut *self.tx)
                        .await?;
                }
                for run in &runs {
                    sqlx::query("UPDATE operator_sessions SET archived_at = ? WHERE id = (SELECT operator_session FROM runs WHERE id = ?)")
                        .bind(at)
                        .bind(run)
                        .execute(&mut *self.tx)
                        .await?;
                }
            }
            HistoryChange::Delete => {
                for session in &sessions {
                    let calls = sqlx::query(
                        "SELECT id, channel FROM calls WHERE session = ? AND state <> ?",
                    )
                    .bind(session)
                    .bind(CallState::Open.label())
                    .fetch_all(&mut *self.tx)
                    .await?;
                    for row in calls {
                        let call: String = row.try_get("id")?;
                        let channel: String = row.try_get("channel")?;
                        self.retire(&call, &channel, Some(session)).await?;
                    }
                }
                for run in &runs {
                    let session: Option<String> =
                        sqlx::query_scalar("SELECT operator_session FROM runs WHERE id = ?")
                            .bind(run)
                            .fetch_optional(&mut *self.tx)
                            .await?
                            .flatten();
                    // A running run stays, with its log rows.
                    let ended =
                        sqlx::query("DELETE FROM runs WHERE id = ? AND ended_at IS NOT NULL")
                            .bind(run)
                            .execute(&mut *self.tx)
                            .await?
                            .rows_affected();
                    if ended > 0 {
                        sqlx::query("DELETE FROM run_entries WHERE run = ?")
                            .bind(run)
                            .execute(&mut *self.tx)
                            .await?;
                        sqlx::query("DELETE FROM observations WHERE call IS NULL AND kind IN (?, ?, ?, ?, ?) AND json_extract(payload, '$.run') = ?")
                            .bind(ObservationKind::RunStarted.label())
                            .bind(ObservationKind::RunOpened.label())
                            .bind(ObservationKind::RunConfigured.label())
                            .bind(ObservationKind::RunEnded.label())
                            .bind(ObservationKind::RunRecovered.label())
                            .bind(run)
                            .execute(&mut *self.tx)
                            .await?;
                        // A session goes with its last run.
                        if let Some(session) = &session {
                            self.drop_session(session).await?;
                        }
                    }
                }
                // An operator session goes even with no run to take it.
                if let HistorySubject::OperatorSession(session) = edit.subject {
                    self.drop_session(&session.to_string()).await?;
                }
            }
        }
        // History edits share the nil channel, which has no row.
        self.log(
            edit.at,
            ChannelId::restore(uuid::Uuid::nil()),
            None,
            None,
            ObservationKind::HistoryEdited,
            edit,
        )
        .await
    }

    /// Delete operator session `session` and its own facts, once no run
    /// of it is left.
    async fn drop_session(&mut self, session: &str) -> Result<(), StoreError> {
        let gone = sqlx::query("DELETE FROM operator_sessions WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM runs WHERE operator_session = ?1)")
            .bind(session)
            .execute(&mut *self.tx)
            .await?
            .rows_affected();
        if gone > 0 {
            sqlx::query("DELETE FROM observations WHERE call IS NULL AND kind IN (?, ?, ?, ?, ?) AND json_extract(payload, '$.session') = ?")
                .bind(ObservationKind::SessionCreated.label())
                .bind(ObservationKind::SessionConfigured.label())
                .bind(ObservationKind::SessionClientChosen.label())
                .bind(ObservationKind::SessionArchived.label())
                .bind(ObservationKind::SessionUnarchived.label())
                .bind(session)
                .execute(&mut *self.tx)
                .await?;
        }
        Ok(())
    }

    /// The sessions and runs an edit of `subject` applies to: a recording
    /// and the runs whose thread it is, a run and the recordings its thread
    /// names, or an operator session's runs and the recordings they name.
    async fn subjects(
        &mut self,
        subject: HistorySubject,
    ) -> Result<(Vec<String>, Vec<String>), StoreError> {
        let (id, sessions, runs) = match subject {
            HistorySubject::Session(session) => (
                session.to_string(),
                "SELECT id FROM sessions WHERE id = ?1",
                "SELECT id FROM runs WHERE thread = (SELECT native_id FROM sessions WHERE id = ?1)",
            ),
            HistorySubject::Run(run) => (
                run.to_string(),
                "SELECT id FROM sessions WHERE native_id = (SELECT thread FROM runs WHERE id = ?1)",
                "SELECT id FROM runs WHERE id = ?1",
            ),
            HistorySubject::OperatorSession(session) => (
                session.to_string(),
                "SELECT id FROM sessions WHERE native_id IN (SELECT thread FROM runs WHERE operator_session = ?1)",
                "SELECT id FROM runs WHERE operator_session = ?1",
            ),
        };
        let sessions = sqlx::query_scalar(sessions)
            .bind(&id)
            .fetch_all(&mut *self.tx)
            .await?;
        let runs = sqlx::query_scalar(runs)
            .bind(&id)
            .fetch_all(&mut *self.tx)
            .await?;
        Ok((sessions, runs))
    }

    async fn message(&mut self, body: BodyKey, mark: &MessageMark) -> Result<(), StoreError> {
        sqlx::query("INSERT OR IGNORE INTO messages(owner, direction, byte_offset, len, payload, at_wall) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(body.owner())
            .bind(body.direction().label())
            .bind(i64::try_from(mark.offset).unwrap_or(i64::MAX))
            .bind(i64::try_from(mark.len).unwrap_or(i64::MAX))
            .bind(mark.payload.label())
            .bind(i64::try_from(mark.at.as_millis()).unwrap_or(i64::MAX))
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    async fn close_channel(&mut self, close: &CloseChannel) -> Result<Observation, StoreError> {
        sqlx::query("UPDATE channels SET closed_at = ?, ending = ?, close_code = ?, unattributed_messages = ?, unattributed_bytes = ? WHERE id = ?")
            .bind(i64::try_from(close.at.wall.as_millis()).unwrap_or(i64::MAX))
            .bind(close.end.label())
            .bind(close.close_code.map(u32::from))
            .bind(i64::try_from(close.unattributed.messages).unwrap_or(i64::MAX))
            .bind(i64::try_from(close.unattributed.bytes).unwrap_or(i64::MAX))
            .bind(close.channel.to_string())
            .execute(&mut *self.tx)
            .await?;
        self.log(
            close.at,
            close.channel,
            None,
            None,
            ObservationKind::ChannelClosed,
            close,
        )
        .await
    }

    async fn open_call(&mut self, open: &OpenCall) -> Result<Observation, StoreError> {
        let stamp = &open.stamp;
        let at = i64::try_from(stamp.at.wall.as_millis()).unwrap_or(i64::MAX);
        sqlx::query("INSERT OR IGNORE INTO channels(id, transport, opened_at, provider) VALUES (?, ?, ?, ?)")
            .bind(stamp.channel.to_string())
            .bind(open.transport.label())
            .bind(at)
            .bind(open.provider.label())
            .execute(&mut *self.tx)
            .await?;
        let (session, turn) = match &open.attribution.hint {
            None => (None, None),
            Some(hint) => {
                let session: String = sqlx::query_scalar("INSERT INTO sessions(id, provider, source, native_id, first_seen, last_seen) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(provider, source, native_id) DO UPDATE SET first_seen = MIN(first_seen, excluded.first_seen), last_seen = MAX(last_seen, excluded.last_seen) RETURNING id")
                    .bind(RecordingId::generate(self.ids).to_string())
                    .bind(open.provider.label())
                    .bind(hint.source.label())
                    .bind(hint.session.to_string())
                    .bind(at)
                    .bind(at)
                    .fetch_one(&mut *self.tx)
                    .await?;
                let turn = match hint.turn {
                    None => None,
                    Some(native) => Some(
                        sqlx::query_scalar::<_, String>("INSERT INTO turns(id, session, native_id) VALUES (?, ?, ?) ON CONFLICT(session, native_id) DO UPDATE SET native_id = excluded.native_id RETURNING id")
                            .bind(TurnId::generate(self.ids).to_string())
                            .bind(&session)
                            .bind(native.to_string())
                            .fetch_one(&mut *self.tx)
                            .await?,
                    ),
                };
                (Some(session), turn)
            }
        };
        sqlx::query("INSERT INTO calls(id, channel, started_at, provider, wire, endpoint, method, config_revision, session, turn, attribution, state, operator_revision, plan, caller, user, tailscale_login) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(stamp.call.to_string())
            .bind(stamp.channel.to_string())
            .bind(at)
            .bind(open.provider.label())
            .bind(open.wire.label())
            .bind(open.endpoint.label())
            .bind(open.method.label())
            .bind(open.config_revision.as_str())
            .bind(session)
            .bind(turn)
            .bind(serde_json::to_string(&open.attribution)?)
            .bind(CallState::Open.label())
            .bind(
                open.operator_revision
                    .map(|revision| i64::try_from(revision.get()).unwrap_or(i64::MAX)),
            )
            .bind(open.plan.as_ref().map(|plan| plan.as_str()))
            .bind(open.caller.kind().label())
            .bind(open.caller.user().map(UserId::as_str))
            .bind(open.caller.login().map(TailscaleLogin::as_str))
            .execute(&mut *self.tx)
            .await?;
        self.open_body(BodyKey::Request(stamp.call)).await?;
        self.append(stamp, None, ObservationKind::CallOpened, open)
            .await
    }

    async fn open_body(&mut self, body: BodyKey) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO bodies(owner, direction, ending) VALUES (?, ?, ?)")
            .bind(body.owner())
            .bind(body.direction().label())
            .bind(BodyEnding::Open.label())
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    async fn start_attempt(&mut self, start: &StartAttempt) -> Result<Observation, StoreError> {
        let stamp = &start.stamp;
        sqlx::query(
            "INSERT INTO attempts(id, call, ordinal, started_at, upstream, provider, changes) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(start.attempt.to_string())
        .bind(stamp.call.to_string())
        .bind(start.ordinal)
        .bind(i64::try_from(stamp.at.wall.as_millis()).unwrap_or(i64::MAX))
        .bind(start.upstream.as_str())
        .bind(start.provider.label())
        .bind(start.changes.as_ref().map(|changes| changes.as_str()))
        .execute(&mut *self.tx)
        .await?;
        self.open_body(BodyKey::Response(start.attempt)).await?;
        match start.body {
            AttemptBody::Original => {}
            AttemptBody::Changed => {
                self.open_body(BodyKey::AttemptRequest(start.attempt))
                    .await?
            }
        }
        sqlx::query("UPDATE calls SET final_attempt = ? WHERE id = ?")
            .bind(start.attempt.to_string())
            .bind(stamp.call.to_string())
            .execute(&mut *self.tx)
            .await?;
        self.append(
            stamp,
            Some(start.attempt.to_string()),
            ObservationKind::AttemptStarted,
            start,
        )
        .await
    }

    async fn response_head(&mut self, head: &ResponseHead) -> Result<Observation, StoreError> {
        let stamp = &head.stamp;
        sqlx::query("UPDATE attempts SET status = ?, response_format = ?, response_encoding = ? WHERE id = ?")
            .bind(u32::from(head.status))
            .bind(head.format.label())
            .bind(head.encoding.label())
            .bind(head.attempt.to_string())
            .execute(&mut *self.tx)
            .await?;
        sqlx::query("UPDATE calls SET status = ? WHERE id = ?")
            .bind(u32::from(head.status))
            .bind(stamp.call.to_string())
            .execute(&mut *self.tx)
            .await?;
        self.append(
            stamp,
            Some(head.attempt.to_string()),
            ObservationKind::ResponseHead,
            head,
        )
        .await
    }

    async fn segments(
        &mut self,
        body: BodyKey,
        segments: &[Segment],
        chunks: &[StoredChunk],
    ) -> Result<(), StoreError> {
        let mut stored = 0_u64;
        for segment in segments {
            let StoredChunk { hash, len } = *chunks
                .get(segment.chunk.index())
                .ok_or(StoreError::Corrupt("segment chunk reference"))?;
            sqlx::query("INSERT INTO segments(owner, direction, byte_offset, len, chunk) VALUES (?, ?, ?, ?, ?)")
                .bind(body.owner())
                .bind(body.direction().label())
                .bind(i64::try_from(segment.offset).unwrap_or(i64::MAX))
                .bind(i64::try_from(len).unwrap_or(i64::MAX))
                .bind(hash.as_bytes())
                .execute(&mut *self.tx)
                .await?;
            stored = stored.saturating_add(len);
        }
        sqlx::query(
            "UPDATE bodies SET stored_bytes = stored_bytes + ? WHERE owner = ? AND direction = ?",
        )
        .bind(i64::try_from(stored).unwrap_or(i64::MAX))
        .bind(body.owner())
        .bind(body.direction().label())
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    async fn end_body(&mut self, body: BodyKey, end: BodyEnd) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE bodies SET observed_bytes = ?, ending = ? WHERE owner = ? AND direction = ?",
        )
        .bind(i64::try_from(end.observed_bytes).unwrap_or(i64::MAX))
        .bind(end.ending.label())
        .bind(body.owner())
        .bind(body.direction().label())
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    async fn finish_attempt(&mut self, finish: &FinishAttempt) -> Result<Observation, StoreError> {
        let stamp = &finish.stamp;
        let ended = i64::try_from(stamp.at.wall.as_millis()).unwrap_or(i64::MAX);
        let inspection = finish.inspection.as_deref();
        sqlx::query("UPDATE attempts SET terminal = ?, terminal_cause = ?, terminal_phase = ?, ended_at = ?, inspection = ?, inspection_version = ? WHERE id = ?")
            .bind(finish.end.terminal.label())
            .bind(finish.end.cause.map(Labeled::label))
            .bind(finish.end.phase.map(Labeled::label))
            .bind(ended)
            .bind(inspection.map(serde_json::to_string).transpose()?)
            .bind(inspection.map(|_| InspectionRecord::VERSION))
            .bind(finish.attempt.to_string())
            .execute(&mut *self.tx)
            .await?;
        self.end_body(BodyKey::Response(finish.attempt), finish.response)
            .await?;
        if let Some(request) = finish.request {
            self.end_body(BodyKey::AttemptRequest(finish.attempt), request)
                .await?;
        }
        let tools = inspection
            .map(InspectionRecord::tool_names)
            .unwrap_or_default();
        sqlx::query("UPDATE calls SET terminal = ?, model = ?, outcome = ?, stop_reason = ?, tool_names = ?, tool_count = ?, usage = ? WHERE id = ?")
            .bind(finish.end.terminal.label())
            .bind(inspection.and_then(|record| record.model.clone()))
            .bind(inspection.and_then(|record| record.outcome.as_ref().map(|outcome| outcome.label().to_owned())))
            .bind(inspection.and_then(|record| record.stop_reason.clone()))
            .bind(serde_json::to_string(&tools)?)
            .bind(inspection.map_or(0, |record| i64::try_from(record.tool_calls.len()).unwrap_or(i64::MAX)))
            .bind(inspection.and_then(|record| record.usage.as_ref()).map(serde_json::to_string).transpose()?)
            .bind(stamp.call.to_string())
            .execute(&mut *self.tx)
            .await?;
        self.append(
            stamp,
            Some(finish.attempt.to_string()),
            ObservationKind::AttemptFinished,
            finish,
        )
        .await
    }

    async fn finish_call(&mut self, finish: &FinishCall) -> Result<Observation, StoreError> {
        let stamp = &finish.stamp;
        self.end_body(BodyKey::Request(stamp.call), finish.request)
            .await?;
        // Complete only when every body of the call ended and was fully stored.
        sqlx::query("UPDATE calls SET state = ?, ended_at = ?, rejection = ?, capture_complete = (SELECT COALESCE(MIN(b.ending = ? AND b.stored_bytes = b.observed_bytes), 0) FROM bodies b WHERE (b.owner = calls.id AND b.direction = ?) OR b.owner IN (SELECT id FROM attempts WHERE call = calls.id)) WHERE id = ?")
            .bind(CallState::Finished.label())
            .bind(i64::try_from(stamp.at.wall.as_millis()).unwrap_or(i64::MAX))
            .bind(finish.rejection.map(Labeled::label))
            .bind(BodyEnding::Ended.label())
            .bind(Direction::Request.label())
            .bind(stamp.call.to_string())
            .execute(&mut *self.tx)
            .await?;
        self.append(stamp, None, ObservationKind::CallFinished, finish)
            .await
    }

    /// Remove a call and everything only it references.
    async fn retire(
        &mut self,
        call: &str,
        channel: &str,
        session: Option<&str>,
    ) -> Result<(), StoreError> {
        let request = Direction::Request.label();
        for statement in [
            "DELETE FROM segments WHERE (owner = ?1 AND direction = ?2) OR owner IN (SELECT id FROM attempts WHERE call = ?1)",
            "DELETE FROM messages WHERE (owner = ?1 AND direction = ?2) OR owner IN (SELECT id FROM attempts WHERE call = ?1)",
            "DELETE FROM bodies WHERE (owner = ?1 AND direction = ?2) OR owner IN (SELECT id FROM attempts WHERE call = ?1)",
        ] {
            sqlx::query(statement)
                .bind(call)
                .bind(request)
                .execute(&mut *self.tx)
                .await?;
        }
        for statement in [
            "DELETE FROM observations WHERE call = ?",
            "DELETE FROM attempts WHERE call = ?",
            "DELETE FROM calls WHERE id = ?",
        ] {
            sqlx::query(statement)
                .bind(call)
                .execute(&mut *self.tx)
                .await?;
        }
        // A channel's own log rows go with its last call; an open channel
        // keeps its row, so its close can still be recorded.
        sqlx::query("DELETE FROM observations WHERE channel = ?1 AND call IS NULL AND NOT EXISTS (SELECT 1 FROM calls WHERE channel = ?1) AND EXISTS (SELECT 1 FROM channels WHERE id = ?1 AND (transport <> ?2 OR ending IS NOT NULL))")
            .bind(channel)
            .bind(Transport::WebSocket.label())
            .execute(&mut *self.tx)
            .await?;
        sqlx::query("DELETE FROM channels WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM calls WHERE channel = ?1) AND (transport <> ?2 OR ending IS NOT NULL)")
            .bind(channel)
            .bind(Transport::WebSocket.label())
            .execute(&mut *self.tx)
            .await?;
        if let Some(session) = session {
            sqlx::query("DELETE FROM turns WHERE session = ? AND NOT EXISTS (SELECT 1 FROM calls WHERE turn = turns.id)")
                .bind(session)
                .execute(&mut *self.tx)
                .await?;
            sqlx::query("DELETE FROM sessions WHERE id = ? AND NOT EXISTS (SELECT 1 FROM calls WHERE session = sessions.id)")
                .bind(session)
                .execute(&mut *self.tx)
                .await?;
        }
        Ok(())
    }
}

/// The store is the recorder's sink: each method is the inherent one.
impl RecordSink for Store {
    type Error = StoreError;

    fn commit(
        &mut self,
        batch: &Batch,
    ) -> impl Future<Output = Result<Committed, StoreError>> + Send {
        Store::commit(self, batch)
    }

    fn prune(
        &mut self,
        policy: &RetentionPolicy,
        now: UnixMillis,
    ) -> impl Future<Output = Result<Pruned, StoreError>> + Send {
        Store::prune(self, policy, now)
    }

    fn checkpoint(&mut self) -> impl Future<Output = Result<(), StoreError>> + Send {
        Store::checkpoint(self)
    }

    fn undigested(
        &mut self,
        version: u32,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<StoredExchange>, StoreError>> + Send {
        self.bodies.undigested(version, limit)
    }

    fn close(self) -> impl Future<Output = Result<(), StoreError>> + Send {
        Store::close(self)
    }
}
