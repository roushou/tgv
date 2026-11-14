//! Read-only access through a pool of query-only connections. Each query uses
//! its own short transaction; readers never block the writer.

use super::chunk::{ChunkHash, Codec, EncodedChunk};
use crate::activity::{
    ActivityBudget, CallActivity, Excerpt, ReadCall, TurnActivity, TurnHead, TurnSummary,
    report::RecordingReport,
};
use crate::search::ExposureQuery;
use crate::{
    Store, StoreError,
    query::{CallQuery, Cursor, KindFilter, PageLimit, Shelf, StatusFilter, TimeRange, ToolFilter},
    view::{
        AttemptView, BodyPrefix, BodyView, CallDetail, CallPage, CallView, ChannelView, Columns,
        ConversationView, DiskUsage, HistoryTarget, MessageView, ModelUsage, OperatorActionView,
        RecordingView, RunEntryView, RunView, SessionView, StorageView, TokenTotals, UsageBucket,
        UsageCounts, UsageView, UserUsage,
    },
    writer::BlobAccess,
};
use ren_depot::toolset::Generation;
use ren_harness::Harness;
use ren_harness::attribution::HintSource;
use ren_harness::tools::CallPurpose;
use ren_protocol::encoding::{ResponseEncoding, ResponseFormat};
use ren_protocol::inspect::{InputAccounting, ModelOutcome};
use ren_protocol::{
    direction::Direction,
    label::{Labeled, Stored},
    route::{Endpoint, Method},
};
use ren_recorder::Observation;
use ren_recorder::batch::{BodyKey, HistorySubject};
use ren_recorder::digest::CallDigest;
use ren_recorder::{
    capture::{AttemptTerminal, BodyEnding, CallState, TerminalClass},
    digest::{Captured, StoredBody, StoredExchange, StoredResponse},
    id::{CallId, RecordingId, RunId, SessionId, TurnId},
};
use sqlx::{
    AssertSqlSafe, QueryBuilder, Row, Sqlite, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::collections::HashMap;
use std::{path::Path, sync::Arc, time::Duration};

/// Bounds on a body prefix read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixLimit {
    pub bytes: usize,
    pub segments: u32,
}

#[derive(Clone)]
pub struct Reader {
    pub(crate) pool: SqlitePool,
}

impl Reader {
    /// Message boundaries returned with one call's detail.
    const MAX_MESSAGES: i64 = 10_000;

    /// Open a read pool on a data directory a [`Store`] has initialised.
    pub async fn open(dir: &Path, connections: u32) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(dir.join(Store::METADATA_FILE))
            .read_only(true)
            .pragma("query_only", "ON")
            .busy_timeout(Duration::from_secs(5));
        let dir: Arc<Path> = dir.into();
        let attach = dir.clone();
        let pool = SqlitePoolOptions::new()
            .max_connections(connections.max(1))
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |connection, _| {
                let dir = attach.clone();
                Box::pin(
                    async move { Store::attach_blobs(connection, &dir, BlobAccess::Read).await },
                )
            })
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    /// Newest calls first, filtered.
    pub async fn calls(&self, query: &CallQuery) -> Result<CallPage, StoreError> {
        let mut sql = QueryBuilder::<Sqlite>::new(CallView::SELECT);
        sql.push(" WHERE 1 = 1");
        if let Some(cursor) = query.after {
            sql.push(" AND (c.started_at, c.id) < (")
                .push_bind(i64::try_from(cursor.started_at.as_millis()).unwrap_or(i64::MAX))
                .push(", ")
                .push_bind(cursor.call.to_string())
                .push(")");
        }
        query.push_filters(&mut sql);
        sql.push(" ORDER BY c.started_at DESC, c.id DESC LIMIT ")
            .push_bind(i64::from(query.limit.get()).saturating_add(1));
        let rows = sql.build().fetch_all(&self.pool).await?;
        CallPage::from_rows(&rows, query.limit)
    }

    /// A session's calls in chronological order.
    pub async fn recording_calls(
        &self,
        session: RecordingId,
        limit: PageLimit,
        after: Option<Cursor>,
    ) -> Result<CallPage, StoreError> {
        self.require_recording(session).await?;
        let (time, call) = after.map_or((-1, String::new()), |cursor| {
            (
                i64::try_from(cursor.started_at.as_millis()).unwrap_or(i64::MAX),
                cursor.call.to_string(),
            )
        });
        let rows = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE c.session = ? AND (c.started_at, c.id) > (?, ?) ORDER BY c.started_at, c.id LIMIT ?",
            CallView::SELECT
        )))
        .bind(session.to_string())
        .bind(time)
        .bind(call)
        .bind(i64::from(limit.get()).saturating_add(1))
        .fetch_all(&self.pool)
        .await?;
        CallPage::from_rows(&rows, limit)
    }

    pub async fn call(&self, id: CallId) -> Result<CallDetail, StoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE c.id = ?",
            CallView::SELECT
        )))
        .bind(id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        let call = CallView::from_row(&row)?;
        let channel = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE id = ?",
            ChannelView::SELECT
        )))
        .bind(call.channel.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::from)
        .and_then(|row| ChannelView::from_row(&row))?;
        let attempts: Vec<AttemptView> = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE a.call = ? ORDER BY a.ordinal",
            AttemptView::SELECT
        )))
        .bind(id.to_string())
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(AttemptView::from_row)
        .collect::<Result<_, _>>()?;
        let observations = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE call = ? ORDER BY seq LIMIT 1000",
            Columns::OBSERVATION_SELECT
        )))
        .bind(id.to_string())
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|row| Columns(row).observation())
        .collect::<Result<_, _>>()?;
        let messages = sqlx::query(MessageView::SELECT_FOR_CALL)
            .bind(id.to_string())
            .bind(Direction::Request.label())
            .bind(Direction::Response.label())
            .bind(
                attempts
                    .last()
                    .map(|attempt: &AttemptView| attempt.id.to_string()),
            )
            .bind(Self::MAX_MESSAGES)
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(MessageView::from_row)
            .collect::<Result<_, _>>()?;
        tx.commit().await?;
        Ok(CallDetail {
            call,
            transport: channel.transport.clone(),
            channel,
            attempts,
            observations,
            messages,
        })
    }

    /// The `limit` most recently active sessions on `shelf`.
    pub async fn recordings(
        &self,
        limit: PageLimit,
        shelf: Shelf,
    ) -> Result<Vec<RecordingView>, StoreError> {
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT s.id, s.provider, s.source, s.native_id, s.first_seen, s.last_seen, CASE WHEN s.archived_at >= s.last_seen THEN s.archived_at END AS archived_at, (SELECT COUNT(*) FROM turns WHERE session = s.id) AS turn_count, ",
        );
        CallQuery::push_usage_columns(&mut sql);
        sql.push(" FROM sessions s LEFT JOIN calls c ON c.session = s.id WHERE ")
            .push(shelf.recordings())
            .push(" GROUP BY s.id ORDER BY s.last_seen DESC, s.id DESC LIMIT ")
            .push_bind(i64::from(limit.get()));
        sql.build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(RecordingView::from_row)
            .collect()
    }

    /// Calls, errors, and tokens of the calls started in `range`: in total,
    /// by the hour, by model, and by user.
    pub async fn usage(&self, range: TimeRange) -> Result<UsageView, StoreError> {
        let select = |prefix: &str| {
            let mut sql = QueryBuilder::<Sqlite>::new(prefix);
            CallQuery::push_usage_columns(&mut sql);
            sql.push(" FROM calls c WHERE 1 = 1");
            range.push_started(&mut sql);
            sql
        };
        let mut totals = select("SELECT ");
        let totals = totals.build().fetch_one(&self.pool).await?;
        let mut hours = select("SELECT (c.started_at / 3600000) * 3600000 AS start, ");
        hours.push(" GROUP BY start ORDER BY start");
        let hours = hours
            .build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|row| {
                Ok(UsageBucket {
                    start: Columns(row).time("start")?,
                    usage: UsageCounts::from_row(row)?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        let mut models = select("SELECT c.provider, c.model, ");
        models.push(
            " GROUP BY c.provider, c.model ORDER BY input_tokens DESC NULLS LAST, calls DESC",
        );
        let models = models
            .build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|row| {
                let columns = Columns(row);
                Ok(ModelUsage {
                    provider: columns.label("provider")?,
                    model: columns.text("model")?,
                    usage: UsageCounts::from_row(row)?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        let mut users = select("SELECT c.user, ");
        users.push(" GROUP BY c.user ORDER BY input_tokens DESC NULLS LAST, calls DESC");
        let users = users
            .build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|row| {
                Ok(UserUsage {
                    user: Columns(row).text("user")?,
                    usage: UsageCounts::from_row(row)?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(UsageView {
            totals: UsageCounts::from_row(&totals)?,
            hours,
            models,
            users,
        })
    }

    /// The newest `limit` recorded operator commands, oldest first.
    pub async fn operator_actions(
        &self,
        limit: PageLimit,
    ) -> Result<Vec<OperatorActionView>, StoreError> {
        sqlx::query("SELECT seq, at_wall, revision, actor, command FROM (SELECT * FROM operator_actions ORDER BY seq DESC LIMIT ?) ORDER BY seq")
            .bind(i64::from(limit.get()))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(OperatorActionView::from_row)
            .collect()
    }

    /// The toolset generations runs without an end were started with:
    /// generations a collection must keep. A run left by a daemon that
    /// stopped counts until the next startup marks it recovered.
    pub async fn live_toolsets(&self) -> Result<Vec<Generation>, StoreError> {
        let numbers: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT toolset_generation FROM runs WHERE ended_at IS NULL AND toolset_generation IS NOT NULL ORDER BY toolset_generation",
        )
        .fetch_all(&self.pool)
        .await?;
        numbers
            .into_iter()
            .map(|number| {
                u64::try_from(number)
                    .map(Generation::from)
                    .map_err(|_| StoreError::Corrupt("run toolset"))
            })
            .collect()
    }

    /// The newest `limit` runs on `shelf`, newest first.
    pub async fn runs(&self, limit: PageLimit, shelf: Shelf) -> Result<Vec<RunView>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "{} WHERE {} ORDER BY started_at DESC, id DESC LIMIT ?",
            RunView::SELECT,
            shelf.runs(),
        )))
        .bind(i64::from(limit.get()))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(RunView::from_row)
        .collect()
    }

    /// Operator sessions on `shelf`, most recently active first (by their
    /// newest run, else their creation), each with its runs.
    pub async fn sessions(
        &self,
        limit: PageLimit,
        shelf: Shelf,
    ) -> Result<Vec<SessionView>, StoreError> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE {} ORDER BY COALESCE((SELECT MAX(started_at) FROM runs WHERE runs.operator_session = operator_sessions.id), created_at) DESC, id DESC LIMIT ?",
            SessionView::SELECT,
            shelf.sessions(),
        )))
        .bind(i64::from(limit.get()))
        .fetch_all(&self.pool)
        .await?;
        let mut sessions = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: String = row.try_get("id")?;
            sessions.push(SessionView::from_row(row, self.session_runs(&id).await?)?);
        }
        Ok(sessions)
    }

    /// One operator session, with its runs, if it was recorded.
    pub async fn session(&self, id: SessionId) -> Result<Option<SessionView>, StoreError> {
        let id = id.to_string();
        let Some(row) = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE id = ?",
            SessionView::SELECT
        )))
        .bind(&id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        SessionView::from_row(&row, self.session_runs(&id).await?).map(Some)
    }

    /// A session's runs, newest first.
    async fn session_runs(&self, session: &str) -> Result<Vec<RunView>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "{} WHERE operator_session = ? ORDER BY started_at DESC, id DESC",
            RunView::SELECT,
        )))
        .bind(session)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(RunView::from_row)
        .collect()
    }

    /// One run, if it was recorded.
    pub async fn run(&self, id: RunId) -> Result<Option<RunView>, StoreError> {
        sqlx::query(AssertSqlSafe(format!("{} WHERE id = ?", RunView::SELECT)))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(RunView::from_row)
            .transpose()
    }

    /// The body of `call` in `direction`: the request, or the final attempt's response.
    pub async fn body_key(
        &self,
        call: CallId,
        direction: Direction,
    ) -> Result<BodyKey, StoreError> {
        match direction {
            Direction::Request => {
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM calls WHERE id = ?)")
                        .bind(call.to_string())
                        .fetch_one(&self.pool)
                        .await?;
                exists
                    .then_some(BodyKey::Request(call))
                    .ok_or(StoreError::NotFound)
            }
            Direction::Response => {
                let attempt: Option<String> =
                    sqlx::query_scalar("SELECT final_attempt FROM calls WHERE id = ?")
                        .bind(call.to_string())
                        .fetch_optional(&self.pool)
                        .await?
                        .flatten();
                let attempt = attempt.ok_or(StoreError::NotFound)?;
                Ok(BodyKey::Response(
                    attempt
                        .parse()
                        .map_err(|_| StoreError::Corrupt("identifier"))?,
                ))
            }
        }
    }

    /// A bounded, contiguous prefix of a body. Reading stops at a gap: bytes
    /// are never joined across missing capture.
    pub async fn body_prefix(
        &self,
        body: BodyKey,
        limit: PrefixLimit,
    ) -> Result<BodyPrefix, StoreError> {
        let mut tx = self.pool.begin().await?;
        let view = Self::body_view(&mut tx, body).await?;
        let rows = sqlx::query("SELECT s.byte_offset, s.len, c.hash, c.codec, c.raw_len, c.data FROM segments s LEFT JOIN blobs.chunks c ON c.hash = s.chunk WHERE s.owner = ? AND s.direction = ? ORDER BY s.byte_offset LIMIT ?")
            .bind(body.owner())
            .bind(body.direction().label())
            .bind(i64::from(limit.segments).saturating_add(1))
            .fetch_all(&mut *tx)
            .await?;
        let encoding = match body {
            BodyKey::Response(attempt) => sqlx::query_scalar::<_, Option<String>>(
                "SELECT response_encoding FROM attempts WHERE id = ?",
            )
            .bind(attempt.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .flatten()
            .map(|label| Stored::from_label(&label)),
            BodyKey::Request(_) | BodyKey::AttemptRequest(_) => None,
        };
        tx.commit().await?;
        let mut prefix = BodyPrefix {
            body: view,
            bytes: Vec::new(),
            gap: false,
            truncated: false,
            encoding,
        };
        let mut next = 0_u64;
        for (index, row) in rows.iter().enumerate() {
            if index >= usize::try_from(limit.segments).unwrap_or(usize::MAX)
                || prefix.bytes.len() >= limit.bytes
            {
                prefix.truncated = true;
                break;
            }
            let offset: u64 = row.try_get("byte_offset")?;
            if offset != next {
                prefix.gap = true;
                break;
            }
            let raw = EncodedChunk::read_segment(row)?;
            next = next.saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
            let room = limit.bytes.saturating_sub(prefix.bytes.len());
            prefix.truncated = raw.len() > room;
            prefix
                .bytes
                .extend_from_slice(raw.get(..room.min(raw.len())).unwrap_or_default());
        }
        if !prefix.truncated && !prefix.gap && next < prefix.body.stored_bytes {
            prefix.truncated = true;
        }
        if !prefix.truncated
            && prefix
                .body
                .observed_bytes
                .is_some_and(|observed| next < observed)
        {
            prefix.gap = true;
        }
        Ok(prefix)
    }

    /// Stream a complete body, in order, a page of segments per transaction.
    /// Fails with [`StoreError::IncompleteBody`] unless every observed byte is
    /// stored and the end was seen.
    pub async fn export(
        &self,
        body: BodyKey,
        mut write: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<u64, StoreError> {
        let view = {
            let mut tx = self.pool.begin().await?;
            let view = Self::body_view(&mut tx, body).await?;
            tx.commit().await?;
            view
        };
        if !view.complete() {
            return Err(StoreError::IncompleteBody);
        }
        let mut next = 0_u64;
        loop {
            let rows = sqlx::query("SELECT s.byte_offset, s.len, c.hash, c.codec, c.raw_len, c.data FROM segments s LEFT JOIN blobs.chunks c ON c.hash = s.chunk WHERE s.owner = ? AND s.direction = ? AND s.byte_offset >= ? ORDER BY s.byte_offset LIMIT 64")
                .bind(body.owner())
                .bind(body.direction().label())
                .bind(i64::try_from(next).unwrap_or(i64::MAX))
                .fetch_all(&self.pool)
                .await?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let offset: u64 = row.try_get("byte_offset")?;
                if offset != next {
                    return Err(StoreError::IncompleteBody);
                }
                let raw = EncodedChunk::read_segment(row)?;
                next = next.saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
                write(&raw)?;
            }
        }
        if Some(next) == view.observed_bytes {
            Ok(next)
        } else {
            Err(StoreError::IncompleteBody)
        }
    }

    async fn body_view(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        body: BodyKey,
    ) -> Result<BodyView, StoreError> {
        let row = sqlx::query("SELECT observed_bytes AS b_observed, stored_bytes AS b_stored, ending AS b_ending FROM bodies WHERE owner = ? AND direction = ?")
            .bind(body.owner())
            .bind(body.direction().label())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        BodyView::from_columns(&Columns(&row), "b")?.ok_or(StoreError::NotFound)
    }

    /// Committed observations after `seq`, oldest first.
    pub async fn observations_after(
        &self,
        seq: u64,
        limit: PageLimit,
    ) -> Result<Vec<Observation>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "{} WHERE seq > ? ORDER BY seq LIMIT ?",
            Columns::OBSERVATION_SELECT
        )))
        .bind(i64::try_from(seq).unwrap_or(i64::MAX))
        .bind(i64::from(limit.get()))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|row| Columns(row).observation())
        .collect()
    }

    /// The oldest retained observation, if any.
    pub async fn oldest_seq(&self) -> Result<Option<u64>, StoreError> {
        Ok(sqlx::query_scalar("SELECT MIN(seq) FROM observations")
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn storage(&self) -> Result<StorageView, StoreError> {
        let mut connection = self.pool.acquire().await?;
        let disk = DiskUsage::measure(&mut connection).await?;
        let calls =
            sqlx::query("SELECT COUNT(*) AS calls, COALESCE(SUM(state = ?), 0) AS open FROM calls")
                .bind(CallState::Open.label())
                .fetch_one(&mut *connection)
                .await?;
        let chunks = sqlx::query("SELECT COUNT(*) AS chunks, COALESCE(SUM(raw_len), 0) AS unique_bytes, COALESCE(SUM(length(data)), 0) AS compressed FROM blobs.chunks")
            .fetch_one(&mut *connection)
            .await?;
        let referenced: u64 = sqlx::query_scalar("SELECT COALESCE(SUM(len), 0) FROM segments")
            .fetch_one(&mut *connection)
            .await?;
        Ok(StorageView {
            disk,
            calls: calls.try_get("calls")?,
            open_calls: calls.try_get("open")?,
            digested_calls: self.digested_count().await?,
            chunks: chunks.try_get("chunks")?,
            referenced_bytes: referenced,
            unique_bytes: chunks.try_get("unique_bytes")?,
            compressed_bytes: chunks.try_get("compressed")?,
        })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

impl EncodedChunk {
    /// The raw bytes of a segment row joined to its chunk, verified.
    fn read_segment(row: &sqlx::sqlite::SqliteRow) -> Result<Vec<u8>, StoreError> {
        let hash: Option<Vec<u8>> = row.try_get("hash")?;
        let hash = hash
            .as_deref()
            .and_then(ChunkHash::restore)
            .ok_or(StoreError::Corrupt("missing chunk"))?;
        let codec: String = row.try_get("codec")?;
        let codec = Codec::from_label(&codec).ok_or(StoreError::Corrupt("chunk codec"))?;
        let raw_len: u64 = row.try_get("raw_len")?;
        let len: u64 = row.try_get("len")?;
        if raw_len != len {
            return Err(StoreError::Corrupt("segment length"));
        }
        EncodedChunk::decode(
            hash,
            codec,
            usize::try_from(raw_len).map_err(|_| StoreError::Corrupt("chunk length"))?,
            row.try_get("data")?,
        )
        .ok_or(StoreError::Corrupt("chunk content"))
    }
}

impl CallQuery {
    /// An `AND` clause for calls (aliased `c`) a harness made: known from
    /// the convention that attributed each one.
    pub(crate) fn push_client(sql: &mut QueryBuilder<Sqlite>, client: Harness) {
        sql.push(" AND json_extract(c.attribution, '$.hint.source') IN (");
        let mut sources = sql.separated(", ");
        for source in HintSource::ALL
            .iter()
            .filter(|source| source.harness() == client)
        {
            sources.push_bind(source.label());
        }
        sql.push(")");
    }

    /// Whether `calls c` is a model call: a request for model output, not a
    /// token count, a model listing, a WebSocket handshake (recorded as a
    /// GET on a model endpoint), or a client's own check such as Claude
    /// Code's quota probe. A call whose digest is not yet derived counts as
    /// a model call.
    fn push_model_call(sql: &mut QueryBuilder<Sqlite>) {
        sql.push("(c.method <> ")
            .push_bind(Method::Get.label())
            .push(" AND c.endpoint IN (");
        let mut endpoints = sql.separated(", ");
        for endpoint in Endpoint::ALL.iter().filter(|endpoint| endpoint.generates()) {
            endpoints.push_bind(endpoint.label());
        }
        sql.push(
            ") AND NOT EXISTS (SELECT 1 FROM call_digests d WHERE d.call = c.id AND d.purpose IN (",
        );
        let mut purposes = sql.separated(", ");
        for purpose in CallPurpose::ALL
            .iter()
            .filter(|purpose| **purpose != CallPurpose::Work)
        {
            purposes.push_bind(purpose.label());
        }
        sql.push(")))");
    }

    /// `calls`, `errors`, `input_tokens`, `output_tokens`, and
    /// `cache_read_tokens` aggregated over the rows of `calls c`. Errors are
    /// the model calls the `errors` status filter finds: a client's own
    /// check or a failed token count is not a failure of the session's
    /// work. Input adds cached
    /// context where the provider reported it separately. A token SUM over
    /// calls that reported no usage is NULL: "not reported", not zero.
    fn push_usage_columns(sql: &mut QueryBuilder<Sqlite>) {
        sql.push("COUNT(c.id) AS calls, COALESCE(SUM(CASE WHEN ");
        Self::push_model_call(sql);
        sql.push(" AND (c.status >= 400 OR c.terminal IN (");
        Self::push_terminals(sql, TerminalClass::Failed);
        sql.push(") OR c.outcome = ")
            .push_bind(ModelOutcome::Failed.label())
            .push(") THEN 1 ELSE 0 END), 0) AS errors, ")
            .push("SUM(CASE WHEN c.usage IS NULL THEN NULL ELSE COALESCE(json_extract(c.usage, '$.input_tokens'), 0) + CASE WHEN json_extract(c.usage, '$.accounting') = ")
            .push_bind(InputAccounting::ExcludesCache.label())
            .push(" THEN COALESCE(json_extract(c.usage, '$.cache_read_tokens'), 0) + COALESCE(json_extract(c.usage, '$.cache_creation_tokens'), 0) ELSE 0 END END) AS input_tokens, ")
            .push("SUM(json_extract(c.usage, '$.output_tokens')) AS output_tokens, ")
            .push("SUM(json_extract(c.usage, '$.cache_read_tokens')) AS cache_read_tokens");
    }

    /// Append this query's filters as WHERE clauses.
    /// Bind every terminal label of one class, comma-separated.
    fn push_terminals(sql: &mut QueryBuilder<Sqlite>, class: TerminalClass) {
        let mut labels = sql.separated(", ");
        for terminal in AttemptTerminal::ALL
            .iter()
            .filter(|terminal| terminal.class() == class)
        {
            labels.push_bind(terminal.label());
        }
    }

    fn push_filters(&self, sql: &mut QueryBuilder<Sqlite>) {
        let filter = &self.filter;
        filter.time.push_started(sql);
        if let Some(provider) = filter.provider {
            sql.push(" AND c.provider = ").push_bind(provider.label());
        }
        if let Some(model) = &filter.model {
            sql.push(" AND instr(lower(COALESCE(c.model, '')), lower(")
                .push_bind(model.as_str().to_owned())
                .push(")) > 0");
        }
        if let Some(tool) = &filter.tool {
            sql.push(" AND EXISTS (SELECT 1 FROM json_each(c.tool_names) t WHERE instr(lower(t.value), lower(")
                .push_bind(tool.as_str().to_owned())
                .push(")) > 0)");
        }
        if let Some(text) = &filter.text {
            let text = text.as_str().to_owned();
            sql.push(" AND (instr(lower(c.id), lower(")
                .push_bind(text.clone())
                .push(")) > 0 OR instr(lower(COALESCE(c.model, '')), lower(")
                .push_bind(text.clone())
                .push(")) > 0 OR EXISTS (SELECT 1 FROM json_each(c.tool_names) t WHERE instr(lower(t.value), lower(")
                .push_bind(text)
                .push(")) > 0))");
        }
        if let Some(session) = filter.session {
            sql.push(" AND c.session = ").push_bind(session.to_string());
        }
        if let Some(client) = filter.client {
            Self::push_client(sql, client);
        }
        if let Some(user) = &filter.user {
            sql.push(" AND c.user = ")
                .push_bind(user.as_str().to_owned());
        }
        if let Some(kind) = filter.kind {
            sql.push(match kind {
                KindFilter::Model => " AND ",
                KindFilter::Other => " AND NOT ",
            });
            Self::push_model_call(sql);
        }
        if let Some(tools) = filter.tools {
            sql.push(match tools {
                ToolFilter::WithTools => " AND c.tool_count > 0",
                ToolFilter::WithoutTools => " AND c.tool_count = 0 AND c.state <> 'open'",
            });
        }
        if let Some(status) = filter.status {
            match status {
                StatusFilter::Active => {
                    sql.push(" AND c.state = ")
                        .push_bind(CallState::Open.label());
                }
                StatusFilter::Finished => {
                    sql.push(" AND c.state = ")
                        .push_bind(CallState::Finished.label());
                }
                StatusFilter::Interrupted => {
                    sql.push(" AND (c.state = ")
                        .push_bind(CallState::Recovered.label())
                        .push(" OR c.terminal IN (");
                    Self::push_terminals(sql, TerminalClass::Stopped);
                    sql.push("))");
                }
                StatusFilter::Errors => {
                    sql.push(" AND (c.status >= 400 OR c.terminal IN (");
                    Self::push_terminals(sql, TerminalClass::Failed);
                    sql.push(") OR c.outcome = ")
                        .push_bind(ModelOutcome::Failed.label())
                        .push(")");
                }
            }
        }
    }
}

impl CallPage {
    fn from_rows(rows: &[sqlx::sqlite::SqliteRow], limit: PageLimit) -> Result<Self, StoreError> {
        let mut calls = rows
            .iter()
            .take(limit.rows())
            .map(CallView::from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let next = (rows.len() > limit.rows())
            .then(|| calls.last().map(CallView::cursor))
            .flatten();
        calls.shrink_to_fit();
        Ok(Self { calls, next })
    }
}

/// Agent activity, read from stored bodies when asked (`crate::activity`).
impl Reader {
    /// The most of one body read to derive a digest again: enough for a
    /// long conversation, which clients resend on every call.
    const ACTIVITY_READ: PrefixLimit = PrefixLimit {
        bytes: 16 * 1024 * 1024,
        segments: 4096,
    };
    /// The most calls one turn's activity reads.
    const MAX_TURN_CALLS: i64 = 100;
    /// The most calls a session report reads, oldest first.
    const MAX_REPORT_CALLS: i64 = 5_000;

    /// One turn: its prompt, calls, tool calls, and effects, from its
    /// calls' digests.
    pub async fn turn_activity(
        &self,
        turn: TurnId,
        budget: ActivityBudget,
    ) -> Result<TurnActivity, StoreError> {
        let row = sqlx::query("SELECT id, session, native_id FROM turns WHERE id = ?")
            .bind(turn.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        let columns = Columns(&row);
        let head = TurnHead {
            turn: columns.id("id")?,
            session: columns.id("session")?,
            native_id: columns.text("native_id")?.unwrap_or_default(),
        };
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calls WHERE turn = ?")
            .bind(turn.to_string())
            .fetch_one(&self.pool)
            .await?;
        let views = self.turn_calls(turn, Self::MAX_TURN_CALLS).await?;
        let read = i64::try_from(views.len()).unwrap_or(i64::MAX);
        let calls = self.digested(views).await?;
        let omitted = u64::try_from(total.saturating_sub(read)).unwrap_or(0);
        Ok(TurnActivity::assemble(head, calls, omitted, budget))
    }

    /// A session's turns, newest first, each with the start of its prompt:
    /// the first one among its calls' digests.
    pub async fn recording_turns(
        &self,
        session: RecordingId,
        limit: PageLimit,
    ) -> Result<Vec<TurnSummary>, StoreError> {
        self.require_recording(session).await?;
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT t.id, t.native_id, MIN(c.started_at) AS started_at, \
             CASE WHEN COUNT(c.ended_at) = COUNT(*) THEN MAX(c.ended_at) END AS ended_at, ",
        );
        CallQuery::push_usage_columns(&mut sql);
        sql.push(" FROM turns t JOIN calls c ON c.turn = t.id WHERE t.session = ")
            .push_bind(session.to_string())
            .push(" GROUP BY t.id ORDER BY started_at DESC, t.id DESC LIMIT ")
            .push_bind(i64::from(limit.get()));
        let rows = sql.build().fetch_all(&self.pool).await?;
        let ids = rows
            .iter()
            .map(|row| Columns(row).id::<TurnId>("id"))
            .collect::<Result<Vec<_>, _>>()?;
        let mut prompts = self.turn_prompts(&ids).await?;
        let mut turns = Vec::with_capacity(rows.len());
        for (row, turn) in rows.iter().zip(ids) {
            let columns = Columns(row);
            turns.push(TurnSummary {
                turn,
                native_id: columns.text("native_id")?.unwrap_or_default(),
                started_at: columns.time("started_at")?,
                ended_at: columns.optional_time("ended_at")?,
                call_count: columns.count("calls")?,
                error_count: columns.count("errors")?,
                tokens: TokenTotals::from_row(row)?,
                prompt: prompts
                    .remove(&turn)
                    .map(|prompt| prompt.narrow(TurnSummary::PROMPT_CHARS)),
            });
        }
        Ok(turns)
    }

    /// Each turn's prompt: the first one, by call start, among its calls'
    /// digests.
    async fn turn_prompts(&self, turns: &[TurnId]) -> Result<HashMap<TurnId, Excerpt>, StoreError> {
        let mut prompts = HashMap::new();
        if turns.is_empty() {
            return Ok(prompts);
        }
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT c.turn, d.prompt, d.prompt_omitted FROM calls c JOIN call_digests d ON d.call = c.id WHERE d.prompt IS NOT NULL AND c.turn IN (",
        );
        let mut list = sql.separated(", ");
        for turn in turns {
            list.push_bind(turn.to_string());
        }
        sql.push(") ORDER BY c.started_at, c.id");
        for row in sql.build().fetch_all(&self.pool).await? {
            let columns = Columns(&row);
            let turn: TurnId = columns.id("turn")?;
            if prompts.contains_key(&turn) {
                continue;
            }
            prompts.insert(
                turn,
                Excerpt {
                    text: columns.text("prompt")?.unwrap_or_default(),
                    omitted_chars: columns.count("prompt_omitted")?,
                },
            );
        }
        Ok(prompts)
    }

    /// A session's context and cost measurements, from call records alone,
    /// with the secrets its calls carried.
    pub async fn recording_report(
        &self,
        session: RecordingId,
    ) -> Result<RecordingReport, StoreError> {
        self.require_recording(session).await?;
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calls WHERE session = ?")
            .bind(session.to_string())
            .fetch_one(&self.pool)
            .await?;
        let calls: Vec<CallView> = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE c.session = ? ORDER BY c.started_at, c.id LIMIT ?",
            CallView::SELECT
        )))
        .bind(session.to_string())
        .bind(Self::MAX_REPORT_CALLS)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(CallView::from_row)
        .collect::<Result<_, _>>()?;
        let read = i64::try_from(calls.len()).unwrap_or(i64::MAX);
        let omitted = u64::try_from(total.saturating_sub(read)).unwrap_or(0);
        let mut report = RecordingReport::build(session, &calls, omitted);
        report.add_exposures(
            self.exposures(&ExposureQuery {
                session: Some(session),
                time: TimeRange::default(),
                limit: PageLimit::LARGEST,
            })
            .await?,
        );
        Ok(report)
    }

    /// One call's activity, with results matched only within the call.
    pub async fn call_activity(
        &self,
        call: CallId,
        budget: ActivityBudget,
    ) -> Result<CallActivity, StoreError> {
        let view = self.call_view(call).await?;
        let mut calls = self.digested(vec![view]).await?;
        calls
            .pop()
            .map(|call| call.activity(budget))
            .ok_or(StoreError::NotFound)
    }

    /// Calls with their digests, in the order given.
    async fn digested(&self, views: Vec<CallView>) -> Result<Vec<ReadCall>, StoreError> {
        let mut digests: HashMap<CallId, CallDigest> = HashMap::new();
        if !views.is_empty() {
            let mut sql = QueryBuilder::<Sqlite>::new(
                "SELECT call, digest FROM call_digests WHERE call IN (",
            );
            let mut list = sql.separated(", ");
            for view in &views {
                list.push_bind(view.id.to_string());
            }
            sql.push(")");
            for row in sql.build().fetch_all(&self.pool).await? {
                let columns = Columns(&row);
                // A digest this build cannot read is derived again; until
                // then the call shows as not yet derived.
                if let Ok(digest) =
                    serde_json::from_str::<CallDigest>(&columns.text("digest")?.unwrap_or_default())
                {
                    digests.insert(columns.id("call")?, digest);
                }
            }
        }
        Ok(views
            .into_iter()
            .map(|view| ReadCall {
                digest: digests.remove(&view.id),
                view,
            })
            .collect())
    }

    /// `NotFound` unless the store has `session`.
    /// The newest `limit` entries of `run`'s conversation, oldest first:
    /// its own, and those of the runs before it on the same client thread
    /// (a conversation resumed in a new run reads as one). `None` when the
    /// run was never recorded.
    pub async fn conversation(
        &self,
        run: RunId,
        limit: u32,
    ) -> Result<Option<ConversationView>, StoreError> {
        let id = run.to_string();
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE id = ?)")
            .bind(&id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Ok(None);
        }
        // The runs that make the conversation, in the order they started.
        let chain = "SELECT r.id, r.started_at FROM runs r, runs this WHERE this.id = ?1 AND (r.id = this.id OR (this.thread IS NOT NULL AND r.thread = this.thread AND r.started_at <= this.started_at))";
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT * FROM (SELECT e.run, e.seq, e.at, e.turn, e.event, chain.started_at FROM run_entries e JOIN ({chain}) chain ON chain.id = e.run ORDER BY chain.started_at DESC, e.run DESC, e.seq DESC LIMIT ?2) ORDER BY started_at, run, seq"
        )))
        .bind(&id)
        .bind(i64::from(limit) + 1)
        .fetch_all(&self.pool)
        .await?;
        let earlier = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        let entries = rows
            .iter()
            .skip(usize::from(earlier))
            .map(|row| {
                let columns = Columns(row);
                let event: String = row.try_get("event")?;
                Ok(RunEntryView {
                    run: columns.id("run")?,
                    seq: columns.count("seq")?,
                    at: columns.time("at")?,
                    turn: columns.text("turn")?,
                    event: serde_json::from_str(&event)
                        .map_err(|_| StoreError::Corrupt("run entry"))?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Some(ConversationView { entries, earlier }))
    }

    /// What an edit of `subject` would touch, or `None` when it was never
    /// recorded: the open calls of its sessions and its runs still running.
    /// A session's runs are those whose thread it is; a run's sessions those
    /// its thread names.
    pub async fn history_target(
        &self,
        subject: HistorySubject,
    ) -> Result<Option<HistoryTarget>, StoreError> {
        // The recordings a subject names (by their native ids), and its runs.
        let (id, exists, threads, runs) = match subject {
            HistorySubject::Session(session) => (
                session.to_string(),
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                "(SELECT native_id FROM sessions WHERE id = ?1)",
                "id = ?1 OR thread = (SELECT native_id FROM sessions WHERE id = ?1)",
            ),
            HistorySubject::Run(run) => (
                run.to_string(),
                "SELECT EXISTS(SELECT 1 FROM runs WHERE id = ?1)",
                "(SELECT thread FROM runs WHERE id = ?1)",
                "id = ?1 OR thread = (SELECT thread FROM runs WHERE id = ?1)",
            ),
            HistorySubject::OperatorSession(session) => (
                session.to_string(),
                "SELECT EXISTS(SELECT 1 FROM operator_sessions WHERE id = ?1)",
                "(SELECT thread FROM runs WHERE operator_session = ?1)",
                "operator_session = ?1",
            ),
        };
        let exists: bool = sqlx::query_scalar(AssertSqlSafe(exists))
            .bind(&id)
            .fetch_one(&self.pool)
            .await?;
        if !exists {
            return Ok(None);
        }
        let counts = sqlx::query(AssertSqlSafe(format!(
            "SELECT (SELECT COUNT(*) FROM calls WHERE state = ?2 AND session IN (SELECT id FROM sessions WHERE native_id IN {threads})) AS open_calls, (SELECT COUNT(*) FROM runs WHERE ended_at IS NULL AND ({runs})) AS running_runs"
        )))
        .bind(&id)
        .bind(CallState::Open.label())
        .fetch_one(&self.pool)
        .await?;
        let columns = Columns(&counts);
        Ok(Some(HistoryTarget {
            open_calls: columns.count("open_calls")?,
            running_runs: columns.count("running_runs")?,
        }))
    }

    async fn require_recording(&self, session: RecordingId) -> Result<(), StoreError> {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?)")
            .bind(session.to_string())
            .fetch_one(&self.pool)
            .await?;
        if exists {
            Ok(())
        } else {
            Err(StoreError::NotFound)
        }
    }

    async fn call_view(&self, call: CallId) -> Result<CallView, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "{} WHERE c.id = ?",
            CallView::SELECT
        )))
        .bind(call.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        CallView::from_row(&row)
    }

    async fn turn_calls(&self, turn: TurnId, limit: i64) -> Result<Vec<CallView>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "{} WHERE c.turn = ? ORDER BY c.started_at, c.id LIMIT ?",
            CallView::SELECT
        )))
        .bind(turn.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(CallView::from_row)
        .collect()
    }

    /// Up to `limit` finished calls whose digest is missing or older than
    /// `version`, newest first, with their stored bodies.
    pub(crate) async fn undigested(
        &self,
        version: u32,
        limit: usize,
    ) -> Result<Vec<StoredExchange>, StoreError> {
        let calls: Vec<String> = sqlx::query_scalar(
            "SELECT c.id FROM calls c LEFT JOIN call_digests d ON d.call = c.id WHERE c.state <> 'open' AND (d.call IS NULL OR d.version < ?) ORDER BY c.started_at DESC, c.id DESC LIMIT ?",
        )
        .bind(i64::from(version))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        let mut exchanges = Vec::with_capacity(calls.len());
        for call in calls {
            let call: CallId = call
                .parse()
                .map_err(|_| StoreError::Corrupt("identifier"))?;
            let view = match self.call_view(call).await {
                Ok(view) => view,
                // Pruned since it was listed.
                Err(StoreError::NotFound) => continue,
                Err(error) => return Err(error),
            };
            let request = self
                .captured(BodyKey::Request(call))
                .await?
                .map(CapturedPrefix::into_stored);
            let response = match self.body_key(call, Direction::Response).await {
                Ok(key) => {
                    let format: Option<String> = sqlx::query_scalar(
                        "SELECT a.response_format FROM attempts a JOIN calls c ON c.final_attempt = a.id WHERE c.id = ?",
                    )
                    .bind(call.to_string())
                    .fetch_optional(&self.pool)
                    .await?
                    .flatten();
                    self.captured(key).await?.map(|prefix| StoredResponse {
                        format: format
                            .and_then(|label| ResponseFormat::from_label(&label))
                            .unwrap_or(ResponseFormat::Other),
                        encoding: prefix.encoding.unwrap_or(ResponseEncoding::Identity),
                        body: prefix.into_stored(),
                    })
                }
                Err(StoreError::NotFound) => None,
                Err(error) => return Err(error),
            };
            exchanges.push(StoredExchange {
                call,
                endpoint: view.endpoint.known(),
                harness: view
                    .attribution
                    .hint
                    .as_ref()
                    .and_then(|hint| hint.source.known())
                    .map(|source| source.harness()),
                request,
                response,
            });
        }
        Ok(exchanges)
    }

    /// A body's stored prefix with its message boundaries; `None` when the
    /// body was never recorded.
    async fn captured(&self, key: BodyKey) -> Result<Option<CapturedPrefix>, StoreError> {
        let prefix = match self.body_prefix(key, Self::ACTIVITY_READ).await {
            Ok(prefix) => prefix,
            Err(StoreError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        let messages: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT byte_offset, len FROM messages WHERE owner = ? AND direction = ? ORDER BY byte_offset LIMIT ?",
        )
        .bind(key.owner())
        .bind(key.direction().label())
        .bind(Self::MAX_MESSAGES)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(CapturedPrefix::new(prefix, &messages)))
    }
}

impl TimeRange {
    /// `AND` clauses for calls (aliased `c`) started within the range, both
    /// ends inclusive.
    pub(crate) fn push_started(self, sql: &mut QueryBuilder<Sqlite>) {
        if let Some(from) = self.from() {
            sql.push(" AND c.started_at >= ")
                .push_bind(i64::try_from(from.as_millis()).unwrap_or(i64::MAX));
        }
        if let Some(until) = self.until() {
            sql.push(" AND c.started_at <= ")
                .push_bind(i64::try_from(until.as_millis()).unwrap_or(i64::MAX));
        }
    }
}

/// A stored body prefix, ready to replay.
struct CapturedPrefix {
    bytes: Vec<u8>,
    encoding: Option<ResponseEncoding>,
    messages: Option<Vec<std::ops::Range<usize>>>,
    /// Observed bytes are missing from `bytes`: a gap, the read limit, or
    /// bytes never stored.
    missing: bool,
    ending: BodyEnding,
}

impl CapturedPrefix {
    fn new(prefix: BodyPrefix, messages: &[(i64, i64)]) -> Self {
        let len = prefix.bytes.len();
        // Messages past the prefix are left out; the extent already says so.
        let messages = (!messages.is_empty()).then(|| {
            messages
                .iter()
                .filter_map(|&(offset, size)| {
                    let start = usize::try_from(offset).ok()?;
                    let end = start.checked_add(usize::try_from(size).ok()?)?;
                    (end <= len).then_some(start..end)
                })
                .collect()
        });
        Self {
            encoding: prefix
                .encoding
                .map(|encoding| encoding.known().unwrap_or(ResponseEncoding::Unsupported)),
            missing: prefix.gap
                || prefix.truncated
                || prefix
                    .body
                    .observed_bytes
                    .is_some_and(|observed| prefix.body.stored_bytes < observed),
            ending: prefix.body.ending.known().unwrap_or(BodyEnding::Unfinished),
            bytes: prefix.bytes,
            messages,
        }
    }

    /// The prefix as the recorder replays it.
    fn into_stored(self) -> StoredBody {
        StoredBody {
            complete: if self.missing {
                Captured::Partial
            } else {
                Captured::Whole
            },
            ending: self.ending,
            messages: self.messages,
            bytes: self.bytes,
        }
    }
}
