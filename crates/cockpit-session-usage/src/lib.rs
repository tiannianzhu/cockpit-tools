//! 从 Codex 会话 JSONL（rollout）汇总真实 Token 用量。
//!
//! 优先官方 `token_usage_record`，按 response_id 去重；旧日志使用
//! `last_token_usage` 或累计高水位差。分叉会话跳过父 rollout
//! 在 fork 时刻之前的重放前缀。
//!
//! 数据与官方配额、API 服务 `request_logs` 完全隔离，打开用量面板时才扫描。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[cfg(test)]
const DEFAULT_INSTANCE_ID: &str = "__default__";
#[cfg(test)]
const DEFAULT_INSTANCE_NAME: &str = "默认实例";
const REQUEST_ID_PREFIX: &str = "codex_session:thread-v1";
const SEGMENT_REQUEST_ID_PREFIX: &str = "codex_session:thread-v2";
const INSERT_BATCH_SIZE: usize = 500;
const LONG_CONTEXT_THRESHOLD_TOKENS: u64 = 272_000;
const USAGE_PARSER_VERSION: i64 = 2;

static REPLAY_CACHE: OnceLock<Mutex<ReplayCaches>> = OnceLock::new();

fn replay_caches() -> &'static Mutex<ReplayCaches> {
    REPLAY_CACHE.get_or_init(|| Mutex::new(ReplayCaches::default()))
}

#[derive(Debug, Clone)]
pub struct UsageInstance {
    pub id: String,
    pub name: String,
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionUsageTotals {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub request_count: u64,
    pub estimated_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionUsageBreakdownRow {
    pub key: String,
    pub label: String,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub request_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pricing_usage: Vec<CodexSessionUsagePricingGroup>,
}

/// Aggregate only requests that share a service tier and context-length rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionUsagePricingGroup {
    /// Mixed-model rows carry the model; model rows retain their key as fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub context_input_tokens: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl CodexSessionUsageBreakdownRow {
    pub fn apply_cost(
        &mut self,
        mut price: impl FnMut(&str, &CodexSessionUsagePricingGroup) -> Option<f64>,
    ) {
        self.estimated_cost_usd = if self.pricing_usage.is_empty() {
            None
        } else {
            self.pricing_usage.iter().try_fold(0.0, |sum, usage| {
                let cost = price(usage.model.as_deref().unwrap_or(&self.key), usage)?;
                let total = sum + cost;
                (cost.is_finite() && cost >= 0.0 && total.is_finite()).then_some(total)
            })
        };
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionUsageInstanceOption {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionUsageReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_tokens: Option<Vec<CodexSessionTokenStats>>,
    pub totals: CodexSessionUsageTotals,
    pub by_model: Vec<CodexSessionUsageBreakdownRow>,
    pub by_instance: Vec<CodexSessionUsageBreakdownRow>,
    pub by_day: Vec<CodexSessionUsageBreakdownRow>,
    pub instances: Vec<CodexSessionUsageInstanceOption>,
    pub from_timestamp: Option<i64>,
    pub to_timestamp: Option<i64>,
    pub last_synced_at: Option<i64>,
    pub files_tracked: u64,
    pub event_count: u64,
    pub deferred_files: u32,
    pub last_error_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionUsageSyncResult {
    pub imported: u32,
    pub skipped: u32,
    pub files_scanned: u32,
    pub files_changed: u32,
    pub deferred_files: u32,
    pub errors: Vec<String>,
    pub rebuilt: bool,
    pub report: Option<CodexSessionUsageReport>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionUsageQuery {
    pub from_timestamp: Option<i64>,
    pub to_timestamp: Option<i64>,
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionTokenStats {
    pub session_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub by_model: Vec<CodexSessionUsageBreakdownRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

impl CodexSessionTokenStats {
    /// Price this session only; child sessions retain their own independent rows.
    /// Missing models or prices must not be presented as a free session.
    pub fn apply_cost(
        &mut self,
        mut price: impl FnMut(&str, &CodexSessionUsagePricingGroup) -> Option<f64>,
    ) {
        self.estimated_cost_usd = if self.by_model.is_empty() {
            None
        } else {
            self.by_model.iter_mut().try_fold(0.0, |sum, row| {
                row.apply_cost(&mut price);
                let cost = row.estimated_cost_usd?;
                let total = sum + cost;
                (cost.is_finite() && cost >= 0.0 && total.is_finite()).then_some(total)
            })
        };
    }
}

#[derive(Debug, Clone, Default)]
struct CumulativeTokens {
    input: u64,
    cached_input: u64,
    output: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct DeltaTokens {
    input: u64,
    cached_input: u64,
    output: u64,
}

impl DeltaTokens {
    fn is_zero(self) -> bool {
        self.input == 0 && self.cached_input == 0 && self.output == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenCountersSignature {
    input: Option<u64>,
    cached_input: Option<u64>,
    output: Option<u64>,
    reasoning_output: Option<u64>,
    total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenUsageSignature {
    total: Option<TokenCountersSignature>,
    last: Option<TokenCountersSignature>,
}

#[derive(Debug, Clone)]
struct TimestampedTokenSignature {
    timestamp: DateTime<Utc>,
    signature: TokenUsageSignature,
}

#[derive(Debug, Default)]
struct ParentTokenTimeline {
    events: Vec<TimestampedTokenSignature>,
    has_token_without_timestamp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParentFileStamp {
    modified_nanos: i64,
    size: u64,
}

#[derive(Debug)]
struct CachedParentTimeline {
    stamp: ParentFileStamp,
    timeline: ParentTokenTimeline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingReason {
    MissingParent(String),
    Stable(String),
    Retryable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEntry {
    modified: i64,
    size: u64,
    reason: PendingReason,
}

#[derive(Debug, Default)]
struct ReplayCaches {
    parent_timelines: HashMap<PathBuf, CachedParentTimeline>,
    pending: HashMap<PathBuf, PendingEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentResolution {
    None,
    Parent,
    Deferred,
}

#[derive(Debug)]
struct ParsedTokenEvent {
    line_offset: i64,
    signature: TokenUsageSignature,
    delta: DeltaTokens,
    event_index: Option<u32>,
    model: String,
    service_tier: Option<String>,
    timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct ParsedCodexFile {
    root_thread_id: Option<String>,
    root_meta_seen: bool,
    root_timestamp: Option<DateTime<Utc>>,
    parent_id: Option<String>,
    parent: ParentResolution,
    deferred_reason: Option<String>,
    token_events: Vec<ParsedTokenEvent>,
    line_offset: i64,
    has_billable_tokens: bool,
}

#[derive(Default)]
struct TokenParseState {
    current_model: String,
    current_service_tier: Option<String>,
    usage_record_ids: HashSet<String>,
    total_high_water: Option<CumulativeTokens>,
    last_signature_by_source: HashMap<Option<String>, TokenUsageSignature>,
    previous_token_signature: Option<TokenUsageSignature>,
    event_index: u32,
}

#[derive(Debug, Clone)]
struct RolloutIdentity {
    root_id: String,
    segment_id: Option<String>,
    order_key: String,
}

#[derive(Debug, Default)]
struct FileSyncResult {
    imported: u32,
    skipped: u32,
    deferred: bool,
    changed: bool,
}

pub struct SessionUsageStore {
    db_path: PathBuf,
}

impl SessionUsageStore {
    pub fn open_path(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open_conn(&self) -> Result<Connection, String> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("创建会话用量目录失败: {error}"))?;
        }
        let mut conn = Connection::open(&self.db_path)
            .map_err(|error| format!("打开会话用量库失败: {error}"))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS session_log_sync (
                file_path TEXT PRIMARY KEY,
                instance_id TEXT NOT NULL,
                last_modified INTEGER NOT NULL,
                last_size INTEGER NOT NULL,
                last_line_offset INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_usage_events (
                request_id TEXT PRIMARY KEY,
                instance_id TEXT NOT NULL,
                instance_name TEXT NOT NULL,
                session_id TEXT NOT NULL,
                model TEXT NOT NULL,
                service_tier TEXT,
                timestamp INTEGER NOT NULL,
                input_tokens INTEGER NOT NULL,
                cached_input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                file_path TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_usage_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_token_stats (
                session_id TEXT PRIMARY KEY,
                input_tokens INTEGER,
                output_tokens INTEGER,
                total_tokens INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_session_usage_timestamp
                ON session_usage_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_session_usage_model
                ON session_usage_events(model, timestamp);
            CREATE INDEX IF NOT EXISTS idx_session_usage_instance
                ON session_usage_events(instance_id, timestamp);
            CREATE INDEX IF NOT EXISTS idx_session_usage_file
                ON session_usage_events(file_path);
            ",
        )
        .map_err(|error| format!("初始化会话用量库失败: {error}"))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let has_tier: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('session_usage_events') WHERE name = 'service_tier')",
                [], |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !has_tier {
            tx.execute_batch("ALTER TABLE session_usage_events ADD COLUMN service_tier TEXT;")
                .map_err(|error| error.to_string())?;
        }
        if !has_tier || get_meta_i64(&tx, "usage_parser_version") != Some(USAGE_PARSER_VERSION) {
            // Reparse once after changing the usage source; retain totals until sync succeeds.
            tx.execute_batch("
                DELETE FROM session_log_sync;
                DELETE FROM session_usage_meta WHERE key = 'last_synced_at';")
                .map_err(|error| error.to_string())?;
            set_meta(&tx, "usage_parser_version", &USAGE_PARSER_VERSION.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(conn)
    }

    pub fn sync(
        &self,
        rebuild: bool,
        instances: &[UsageInstance],
    ) -> Result<CodexSessionUsageSyncResult, String> {
        self.sync_with_logical_root(rebuild, instances)
    }

    fn sync_with_logical_root(
        &self,
        rebuild: bool,
        instances: &[UsageInstance],
    ) -> Result<CodexSessionUsageSyncResult, String> {
        let mut conn = self.open_conn()?;
        if rebuild {
            conn.execute_batch(
                "
                DELETE FROM session_usage_events;
                DELETE FROM session_log_sync;
                DELETE FROM session_usage_meta WHERE key != 'usage_parser_version';
                DELETE FROM session_token_stats;
                ",
            )
            .map_err(|error| format!("清空会话用量缓存失败: {error}"))?;
            if let Ok(mut caches) = replay_caches().lock() {
                *caches = ReplayCaches::default();
            }
        }

        let mut cursors = load_cursors(&conn)?;
        let mut result = CodexSessionUsageSyncResult {
            rebuilt: rebuild,
            ..CodexSessionUsageSyncResult::default()
        };

        for instance in instances {
            let files = collect_codex_session_files(&instance.data_dir);
            let rollout_index = build_rollout_index(&files);
            result.files_scanned = result.files_scanned.saturating_add(files.len() as u32);
            let mut handled_groups = HashSet::new();

            for file_path in &files {
                if let Some(root_id) = thread_id_from_filename(file_path) {
                    if let Some(group) = rollout_index.get(&root_id) {
                        if group.iter().any(|path| {
                            rollout_identity(path)
                                .is_some_and(|identity| identity.segment_id.is_some())
                        }) {
                            if !handled_groups.insert(root_id.clone()) {
                                continue;
                            }
                            match sync_segmented_group(
                                &mut conn,
                                instance,
                                &root_id,
                                group,
                                &rollout_index,
                                &mut cursors,
                            ) {
                                Ok(group_result) => {
                                    result.imported =
                                        result.imported.saturating_add(group_result.imported);
                                    result.skipped =
                                        result.skipped.saturating_add(group_result.skipped);
                                    if group_result.changed {
                                        result.files_changed =
                                            result.files_changed.saturating_add(group.len() as u32);
                                    }
                                    if group_result.deferred {
                                        result.deferred_files = result
                                            .deferred_files
                                            .saturating_add(group.len() as u32);
                                    }
                                }
                                Err(error) => {
                                    eprintln!(
                                        "[CODEX-SESSION-USAGE] 解析失败 {}: {error}",
                                        file_path.display()
                                    );
                                    if result.errors.len() < 20 {
                                        result
                                            .errors
                                            .push(format!("{}: {error}", file_path.display()));
                                    }
                                }
                            }
                            continue;
                        }
                    }
                }
                match sync_single_file(&mut conn, instance, file_path, &rollout_index, &mut cursors)
                {
                    Ok(file_result) => {
                        if let Some(id) = thread_id_from_filename(file_path) {
                            let missing_stats = conn
                                .query_row(
                                    "SELECT 1 FROM session_token_stats WHERE session_id = ?1",
                                    params![id],
                                    |_| Ok(()),
                                )
                                .optional()
                                .map_err(|error| error.to_string())?
                                .is_none();
                            if file_result.changed || missing_stats {
                                let stats = fs::metadata(file_path).ok().and_then(|metadata| {
                                    read_token_stats_from_rollout_uncached(
                                        file_path,
                                        metadata.len(),
                                    )
                                });
                                conn.execute(
                                    "INSERT INTO session_token_stats VALUES (?1, ?2, ?3, ?4)
                                     ON CONFLICT(session_id) DO UPDATE SET
                                       input_tokens = excluded.input_tokens,
                                       output_tokens = excluded.output_tokens,
                                       total_tokens = excluded.total_tokens",
                                    params![
                                        id,
                                        stats.map(|item| item.0 as i64),
                                        stats.map(|item| item.1 as i64),
                                        stats.map(|item| item.2 as i64)
                                    ],
                                )
                                .map_err(|error| error.to_string())?;
                            }
                        }
                        result.imported = result.imported.saturating_add(file_result.imported);
                        result.skipped = result.skipped.saturating_add(file_result.skipped);
                        if file_result.imported > 0 || file_result.skipped > 0 {
                            result.files_changed = result.files_changed.saturating_add(1);
                        }
                        if file_result.deferred {
                            result.deferred_files = result.deferred_files.saturating_add(1);
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "[CODEX-SESSION-USAGE] 解析失败 {}: {error}",
                            file_path.display()
                        );
                        if result.errors.len() < 20 {
                            result
                                .errors
                                .push(format!("{}: {error}", file_path.display()));
                        }
                    }
                }
            }
        }

        set_meta(&conn, "last_synced_at", &now_unix_seconds().to_string())?;
        set_meta(&conn, "last_error_count", &result.errors.len().to_string())?;
        set_meta(
            &conn,
            "last_deferred_files",
            &result.deferred_files.to_string(),
        )?;
        Ok(result)
    }

    pub fn query(
        &self,
        query: &CodexSessionUsageQuery,
        instances: &[UsageInstance],
    ) -> Result<CodexSessionUsageReport, String> {
        let conn = self.open_conn()?;
        let instance_names = instances
            .iter()
            .map(|instance| (instance.id.clone(), instance.name.clone()))
            .collect::<HashMap<_, _>>();
        let instance_filter = query
            .instance_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let mut where_sql = String::from("WHERE 1 = 1");
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(from) = query.from_timestamp {
            where_sql.push_str(" AND timestamp >= ?");
            params.push(rusqlite::types::Value::Integer(from));
        }
        if let Some(to) = query.to_timestamp {
            where_sql.push_str(" AND timestamp <= ?");
            params.push(rusqlite::types::Value::Integer(to));
        }
        if let Some(instance_id) = instance_filter {
            where_sql.push_str(" AND instance_id = ?");
            params.push(rusqlite::types::Value::Text(instance_id.to_string()));
        }

        let totals = query_totals(&conn, &where_sql, &params)?;
        let by_model = query_breakdown(&conn, &where_sql, &params, "model", None, &instance_names)?;
        let by_instance = query_breakdown(
            &conn,
            &where_sql,
            &params,
            "instance_id",
            Some("instance_name"),
            &instance_names,
        )?;
        let mut by_day = query_day_breakdown(&conn, &where_sql, &params)?;
        by_day.sort_by(|left, right| right.key.cmp(&left.key));

        let files_tracked = conn
            .query_row("SELECT COUNT(*) FROM session_log_sync", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0)
            .max(0) as u64;
        let event_count = conn
            .query_row("SELECT COUNT(*) FROM session_usage_events", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0)
            .max(0) as u64;

        Ok(CodexSessionUsageReport {
            session_tokens: Some(query_session_tokens(&conn)?),
            totals,
            by_model,
            by_instance,
            by_day,
            instances: instances
                .iter()
                .cloned()
                .map(|instance| CodexSessionUsageInstanceOption {
                    id: instance.id,
                    name: instance.name,
                })
                .collect(),
            from_timestamp: query.from_timestamp,
            to_timestamp: query.to_timestamp,
            last_synced_at: get_meta_i64(&conn, "last_synced_at"),
            files_tracked,
            event_count,
            deferred_files: get_meta_i64(&conn, "last_deferred_files")
                .unwrap_or(0)
                .max(0) as u32,
            last_error_count: get_meta_i64(&conn, "last_error_count").unwrap_or(0).max(0) as u32,
        })
    }
}

const TOKEN_STATS_READ_CHUNK_BYTES: usize = 64 * 1024;

fn query_session_tokens(conn: &Connection) -> Result<Vec<CodexSessionTokenStats>, String> {
    // Lifetime totals per session, independent of the summary's date filters.
    let rows = query_pricing_breakdown(conn, "", &[], "session_id")?;
    let mut sessions = std::collections::BTreeMap::<String, CodexSessionTokenStats>::new();
    for (id, model) in rows {
        let session = sessions.entry(id.clone()).or_insert_with(|| CodexSessionTokenStats {
            session_id: id, ..Default::default()
        });
        session.input_tokens = session.input_tokens.saturating_add(model.input_tokens);
        session.output_tokens = session.output_tokens.saturating_add(model.output_tokens);
        session.total_tokens = session.total_tokens.saturating_add(model.total_tokens);
        session.by_model.push(model);
    }
    Ok(sessions.into_values().collect())
}

fn query_pricing_breakdown(
    conn: &Connection,
    where_sql: &str,
    params: &[rusqlite::types::Value],
    key_column: &str,
) -> Result<Vec<(String, CodexSessionUsageBreakdownRow)>, String> {
    let sql = format!(
        "SELECT {key_column}, model, service_tier, MAX(input_tokens),
                SUM(input_tokens), SUM(cached_input_tokens), SUM(output_tokens), COUNT(*)
         FROM session_usage_events {where_sql}
         GROUP BY {key_column}, model, service_tier, input_tokens > {LONG_CONTEXT_THRESHOLD_TOKENS}
         ORDER BY {key_column}, model, service_tier, MAX(input_tokens)"
    );
    let mut stmt = conn.prepare(&sql).map_err(|error| error.to_string())?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            CodexSessionUsagePricingGroup {
                model: None,
                service_tier: row.get(2)?,
                context_input_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                input_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                cached_input_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                output_tokens: row.get::<_, i64>(6)?.max(0) as u64,
            },
            row.get::<_, i64>(7)?.max(0) as u64,
        ))
    }).map_err(|error| error.to_string())?;
    let mut grouped = std::collections::BTreeMap::<(String, String), CodexSessionUsageBreakdownRow>::new();
    for row in rows {
        let (id, model, usage, requests) = row.map_err(|error| error.to_string())?;
        let entry = grouped.entry((id, model.clone())).or_insert_with(|| CodexSessionUsageBreakdownRow {
            key: model.clone(), label: model,
            input_tokens: 0, cached_input_tokens: 0, output_tokens: 0,
            total_tokens: 0, request_count: 0, estimated_cost_usd: None,
            pricing_usage: Vec::new(),
        });
        entry.input_tokens = entry.input_tokens.saturating_add(usage.input_tokens);
        entry.cached_input_tokens = entry.cached_input_tokens.saturating_add(usage.cached_input_tokens);
        entry.output_tokens = entry.output_tokens.saturating_add(usage.output_tokens);
        entry.total_tokens = entry.input_tokens.saturating_add(entry.output_tokens);
        entry.request_count = entry.request_count.saturating_add(requests);
        entry.pricing_usage.push(usage);
    }
    Ok(grouped.into_iter().map(|((id, _), row)| (id, row)).collect())
}

pub fn read_token_stats_from_rollout_uncached(
    rollout_path: &Path,
    file_len: u64,
) -> Option<(u64, u64, u64)> {
    let mut file = File::open(rollout_path).ok()?;
    let mut offset = file_len;
    let mut pending_prefix = Vec::new();

    while offset > 0 {
        let chunk_len = TOKEN_STATS_READ_CHUNK_BYTES.min(offset as usize);
        offset -= chunk_len as u64;

        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut chunk = vec![0u8; chunk_len];
        file.read_exact(&mut chunk).ok()?;

        let starts_on_line_boundary =
            offset == 0 || byte_before_is_newline(&mut file, offset).ok()?;
        chunk.extend_from_slice(&pending_prefix);

        let parse_from_index = if starts_on_line_boundary {
            pending_prefix.clear();
            0
        } else if let Some(newline_index) = chunk.iter().position(|byte| *byte == b'\n') {
            pending_prefix = chunk[..newline_index].to_vec();
            newline_index + 1
        } else {
            pending_prefix = chunk;
            continue;
        };

        if let Some(stats) = parse_token_stats_lines(&chunk[parse_from_index..]) {
            return Some(stats);
        }
    }

    if pending_prefix.is_empty() {
        None
    } else {
        parse_token_stats_lines(&pending_prefix)
    }
}

fn byte_before_is_newline(file: &mut File, offset: u64) -> std::io::Result<bool> {
    if offset == 0 {
        return Ok(true);
    }

    file.seek(SeekFrom::Start(offset - 1))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    Ok(byte[0] == b'\n')
}

fn parse_token_stats_lines(content: &[u8]) -> Option<(u64, u64, u64)> {
    for line in content.split(|byte| *byte == b'\n').rev() {
        let raw = String::from_utf8_lossy(line);
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || !trimmed.contains("\"token_count\"")
            || !trimmed.contains("\"total_token_usage\"")
        {
            continue;
        }

        let Ok(parsed) = serde_json::from_str::<JsonValue>(trimmed) else {
            continue;
        };
        if parsed.get("type").and_then(|value| value.as_str()) != Some("event_msg") {
            continue;
        }
        let Some(payload) = parsed.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|value| value.as_str()) != Some("token_count") {
            continue;
        }
        let Some(usage) = payload
            .get("info")
            .and_then(|info| info.get("total_token_usage"))
        else {
            continue;
        };

        let input = usage
            .get("input_tokens")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let total = usage
            .get("total_tokens")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        return Some((input, output, total));
    }

    None
}

fn collect_codex_session_files(codex_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let sessions_dir = codex_dir.join("sessions");
    if sessions_dir.is_dir() {
        collect_jsonl_recursive(&sessions_dir, &mut files, 0, 3);
    }
    let archived_dir = codex_dir.join("archived_sessions");
    if archived_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&archived_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_rollout_path(&path) {
                    files.push(path);
                }
            }
        }
    }
    files.sort();
    files
}

fn collect_jsonl_recursive(dir: &Path, files: &mut Vec<PathBuf>, depth: u32, max_depth: u32) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth < max_depth {
            collect_jsonl_recursive(&path, files, depth + 1, max_depth);
        } else if is_rollout_path(&path) {
            files.push(path);
        }
    }
}

fn is_rollout_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_rollout_filename)
}

fn is_rollout_filename(file_name: &str) -> bool {
    rollout_identity(Path::new(file_name)).is_some()
}

fn thread_id_from_filename(path: &Path) -> Option<String> {
    rollout_identity(path).map(|identity| identity.root_id)
}

fn rollout_identity(path: &Path) -> Option<RolloutIdentity> {
    let file_name = path.file_name()?.to_str()?;
    let stem = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    // UUIDs contain dashes, so use the fixed 36-byte suffix.
    let trailing = stem.get(stem.len().checked_sub(36)?..)?;
    let trailing_id = uuid::Uuid::parse_str(trailing)
        .ok()?
        .hyphenated()
        .to_string();
    let before = stem.get(..stem.len() - 36)?;
    if let Some(prefix) = before.strip_suffix('_') {
        let root = prefix.get(prefix.len().checked_sub(36)?..)?;
        let root_id = uuid::Uuid::parse_str(root).ok()?.hyphenated().to_string();
        let order_key = prefix
            .get(..prefix.len() - 36)?
            .trim_end_matches('-')
            .to_string();
        return Some(RolloutIdentity {
            root_id,
            segment_id: Some(trailing_id),
            order_key,
        });
    }
    Some(RolloutIdentity {
        root_id: trailing_id,
        segment_id: None,
        order_key: before.trim_end_matches('-').to_string(),
    })
}

type RolloutIndex = HashMap<String, Vec<PathBuf>>;

fn build_rollout_index(files: &[PathBuf]) -> RolloutIndex {
    let mut index = RolloutIndex::new();
    for path in files {
        if let Some(thread_id) = thread_id_from_filename(path) {
            index.entry(thread_id).or_default().push(path.clone());
        }
    }
    for paths in index.values_mut() {
        paths.sort_by_key(|path| {
            let identity = rollout_identity(path).expect("indexed rollout has a valid name");
            (
                identity.order_key,
                path.file_name().map(|name| name.to_os_string()),
                path.clone(),
            )
        });
    }
    index
}

fn load_cursors(conn: &Connection) -> Result<HashMap<String, (i64, i64, i64)>, String> {
    let mut statement = conn
        .prepare(
            "SELECT file_path, last_modified, last_size, last_line_offset FROM session_log_sync",
        )
        .map_err(|error| format!("读取会话用量游标失败: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ),
            ))
        })
        .map_err(|error| format!("查询会话用量游标失败: {error}"))?;
    let mut cursors = HashMap::new();
    for row in rows {
        let (path, state) = row.map_err(|error| format!("解析会话用量游标失败: {error}"))?;
        cursors.insert(path, state);
    }
    Ok(cursors)
}

fn inherit_archived_cursor(
    file_path: &Path,
    cursors: &HashMap<String, (i64, i64, i64)>,
) -> Option<(i64, i64, i64)> {
    if file_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        != Some("archived_sessions")
    {
        return None;
    }
    let file_name = file_path.file_name()?.to_str()?;
    let slash_suffix = format!("/{file_name}");
    let backslash_suffix = format!("\\{file_name}");
    cursors
        .iter()
        .filter(|(path, _)| {
            path.as_str() != file_path.to_string_lossy().as_ref()
                && (path.ends_with(&slash_suffix) || path.ends_with(&backslash_suffix))
        })
        .map(|(_, &(modified, size, offset))| (offset, modified, size))
        .max()
        .map(|(offset, modified, size)| (modified, size, offset))
}

fn sync_segmented_group(
    conn: &mut Connection,
    instance: &UsageInstance,
    root_id: &str,
    paths: &[PathBuf],
    rollout_index: &RolloutIndex,
    cursors: &mut HashMap<String, (i64, i64, i64)>,
) -> Result<FileSyncResult, String> {
    // A rollout can exist at both live and archived paths while it is moving.
    // Prefer the larger (then newer) copy and count its filename only once.
    let mut selected_by_name: HashMap<String, PathBuf> = HashMap::new();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("Invalid rollout filename")?
            .to_string();
        let incoming = fs::metadata(path).map_err(|error| error.to_string())?;
        let replace = selected_by_name.get(&name).is_none_or(|previous| {
            fs::metadata(previous)
                .map(|current| {
                    (incoming.len(), metadata_modified_nanos(&incoming))
                        > (current.len(), metadata_modified_nanos(&current))
                })
                .unwrap_or(true)
        });
        if replace {
            selected_by_name.insert(name, path.clone());
        }
    }
    let mut selected = selected_by_name.into_values().collect::<Vec<_>>();
    selected.sort_by_key(|path| {
        let identity = rollout_identity(path).expect("grouped rollout has valid identity");
        (
            identity.order_key,
            path.file_name().map(|name| name.to_os_string()),
        )
    });

    let segment_ids = selected
        .iter()
        .filter_map(|path| rollout_identity(path).and_then(|identity| identity.segment_id))
        .collect::<Vec<_>>();
    let mut legacy_stats_exist = false;
    for segment_id in &segment_ids {
        legacy_stats_exist |= conn
            .query_row(
                "SELECT 1 FROM session_token_stats WHERE session_id = ?1",
                params![segment_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .is_some();
    }
    let root_stats_exist = conn
        .query_row(
            "SELECT 1 FROM session_token_stats WHERE session_id = ?1",
            params![root_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .is_some();
    let changed = !root_stats_exist
        || legacy_stats_exist
        || selected.iter().any(|path| {
            let path_str = path.to_string_lossy();
            let Some(&(old_modified, old_size, old_offset)) = cursors.get(path_str.as_ref()) else {
                return true;
            };
            fs::metadata(path)
                .map(|metadata| {
                    metadata.len() != old_size.max(0) as u64
                        || metadata_modified_nanos(&metadata) != old_modified
                        || old_offset == 0
                })
                .unwrap_or(true)
        });
    if !changed {
        return Ok(FileSyncResult::default());
    }

    let mut state = TokenParseState::default();
    let mut parsed_files = Vec::with_capacity(selected.len());
    for path in &selected {
        let parsed = parse_codex_file_with_state(path, Some(root_id.to_string()), &mut state)?;
        if !parsed.root_meta_seen && parsed.has_billable_tokens {
            return Ok(FileSyncResult {
                deferred: true,
                ..FileSyncResult::default()
            });
        }
        let replay_prefix = match parsed.parent {
            ParentResolution::None => 0,
            ParentResolution::Deferred => {
                return Ok(FileSyncResult {
                    deferred: true,
                    ..FileSyncResult::default()
                });
            }
            ParentResolution::Parent => {
                let Some(parent_id) = parsed.parent_id.as_deref() else {
                    return Ok(FileSyncResult {
                        deferred: true,
                        ..FileSyncResult::default()
                    });
                };
                let Some(cutoff) = parsed.root_timestamp else {
                    return Ok(FileSyncResult {
                        deferred: true,
                        ..FileSyncResult::default()
                    });
                };
                match resolve_parent_signatures(parent_id, cutoff, rollout_index) {
                    Ok(signatures) => matching_replay_prefix(&parsed.token_events, &signatures),
                    Err(_) => {
                        return Ok(FileSyncResult {
                            deferred: true,
                            ..FileSyncResult::default()
                        })
                    }
                }
            }
        };
        parsed_files.push((path, parsed, replay_prefix));
    }

    let active_names = selected
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .collect::<HashSet<_>>();
    let mut old_paths = HashSet::new();
    let mut statement = conn
        .prepare("SELECT DISTINCT file_path FROM session_usage_events WHERE session_id = ?1")
        .map_err(|error| error.to_string())?;
    let paths_in_events = statement
        .query_map(params![root_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    for old_path in paths_in_events {
        let old_path = old_path.map_err(|error| error.to_string())?;
        if Path::new(&old_path)
            .file_name()
            .is_some_and(|name| active_names.contains(name.to_string_lossy().as_ref()))
        {
            old_paths.insert(old_path);
        }
    }
    drop(statement);
    for path in cursors.keys() {
        if Path::new(path)
            .file_name()
            .is_some_and(|name| active_names.contains(name.to_string_lossy().as_ref()))
        {
            old_paths.insert(path.clone());
        }
    }
    let mut existing_ids = HashSet::new();
    let mut statement = conn
        .prepare("SELECT request_id FROM session_usage_events WHERE session_id = ?1")
        .map_err(|error| error.to_string())?;
    for id in statement
        .query_map(params![root_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
    {
        existing_ids.insert(id.map_err(|error| error.to_string())?);
    }
    drop(statement);

    let mut result = FileSyncResult {
        changed: true,
        ..FileSyncResult::default()
    };
    let tx = conn.transaction().map_err(|error| error.to_string())?;
    // v1 keyed tail token statistics by the continuation UUID. Remove those
    // only after the full group has passed parsing and metadata validation.
    for segment_id in &segment_ids {
        tx.execute(
            "DELETE FROM session_token_stats WHERE session_id = ?1",
            params![segment_id],
        )
        .map_err(|error| error.to_string())?;
    }
    for old_path in &old_paths {
        tx.execute(
            "DELETE FROM session_usage_events WHERE file_path = ?1",
            params![old_path],
        )
        .map_err(|error| error.to_string())?;
        tx.execute(
            "DELETE FROM session_log_sync WHERE file_path = ?1",
            params![old_path],
        )
        .map_err(|error| error.to_string())?;
    }
    let mut latest_stats = None;
    for (path, parsed, replay_prefix) in &parsed_files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("Invalid rollout filename")?;
        let path_str = path.to_string_lossy().to_string();
        for (token_offset, event) in parsed.token_events.iter().enumerate() {
            let Some(event_index) = event.event_index else {
                continue;
            };
            if token_offset < *replay_prefix {
                continue;
            }
            let request_id = format!("{SEGMENT_REQUEST_ID_PREFIX}:{root_id}:{name}:{event_index}");
            tx.execute(
                "INSERT INTO session_usage_events (
                    request_id, instance_id, instance_name, session_id, model,
                    timestamp, input_tokens, cached_input_tokens, output_tokens, file_path, service_tier
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    request_id,
                    instance.id,
                    instance.name,
                    root_id,
                    event.model,
                    event.timestamp.map(|value| value.timestamp()).unwrap_or(0),
                    event.delta.input as i64,
                    event.delta.cached_input as i64,
                    event.delta.output as i64,
                    path_str,
                    event.service_tier,
                ],
            )
            .map_err(|error| error.to_string())?;
            if existing_ids.contains(&request_id) {
                result.skipped += 1;
            } else {
                result.imported += 1;
            }
        }
        let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
        upsert_cursor_on_conn(
            &tx,
            &path_str,
            &instance.id,
            metadata_modified_nanos(&metadata),
            metadata.len(),
            parsed.line_offset,
        )?;
        if let Some(stats) = read_token_stats_from_rollout_uncached(path, metadata.len()) {
            latest_stats = Some(stats);
        }
    }
    tx.execute(
        "INSERT INTO session_token_stats VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO UPDATE SET
           input_tokens = excluded.input_tokens,
           output_tokens = excluded.output_tokens,
           total_tokens = excluded.total_tokens",
        params![
            root_id,
            latest_stats.map(|item| item.0 as i64),
            latest_stats.map(|item| item.1 as i64),
            latest_stats.map(|item| item.2 as i64)
        ],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    for old_path in old_paths {
        cursors.remove(&old_path);
    }
    for (path, parsed, _) in parsed_files {
        let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
        cursors.insert(
            path.to_string_lossy().to_string(),
            (
                metadata_modified_nanos(&metadata),
                metadata.len() as i64,
                parsed.line_offset,
            ),
        );
    }
    Ok(result)
}

fn sync_single_file(
    conn: &mut Connection,
    instance: &UsageInstance,
    file_path: &Path,
    rollout_index: &RolloutIndex,
    cursors: &mut HashMap<String, (i64, i64, i64)>,
) -> Result<FileSyncResult, String> {
    let file_path_str = file_path.to_string_lossy().to_string();
    let metadata =
        fs::metadata(file_path).map_err(|error| format!("无法读取文件元数据: {error}"))?;
    let file_modified = metadata_modified_nanos(&metadata);
    let file_size = metadata.len();

    let (last_modified, last_size, last_offset) = cursors
        .get(&file_path_str)
        .copied()
        .or_else(|| inherit_archived_cursor(file_path, cursors))
        .unwrap_or((0, 0, 0));

    if file_size == last_size.max(0) as u64 && file_modified <= last_modified && last_offset > 0 {
        return Ok(FileSyncResult::default());
    }

    if let Ok(mut caches) = replay_caches().lock() {
        if let Some(pending) = caches.pending.get(file_path).cloned() {
            if pending.modified == file_modified && pending.size == file_size {
                match &pending.reason {
                    PendingReason::MissingParent(parent) if !rollout_index.contains_key(parent) => {
                        return Ok(FileSyncResult {
                            deferred: true,
                            ..FileSyncResult::default()
                        });
                    }
                    PendingReason::Stable(_) => {
                        return Ok(FileSyncResult {
                            deferred: true,
                            ..FileSyncResult::default()
                        });
                    }
                    _ => {
                        caches.pending.remove(file_path);
                    }
                }
            }
        }
    }

    if file_size < last_size.max(0) as u64 {
        conn.execute(
            "DELETE FROM session_usage_events WHERE file_path = ?1",
            params![file_path_str],
        )
        .map_err(|error| format!("清理已重写会话用量失败: {error}"))?;
    }

    let parsed = parse_codex_file(file_path, thread_id_from_filename(file_path))?;
    if !parsed.has_billable_tokens {
        upsert_cursor(
            conn,
            cursors,
            &file_path_str,
            &instance.id,
            file_modified,
            file_size,
            parsed.line_offset,
        )?;
        return Ok(FileSyncResult {
            changed: true,
            ..FileSyncResult::default()
        });
    }

    let Some(root_thread_id) = parsed.root_thread_id.as_deref() else {
        return Ok(mark_deferred(
            file_path,
            file_modified,
            file_size,
            PendingReason::Stable("文件名缺少有效的尾部 UUID".to_string()),
        ));
    };
    if !parsed.root_meta_seen {
        return Ok(mark_deferred(
            file_path,
            file_modified,
            file_size,
            PendingReason::Stable("含计费 token 但尚无 session_meta".to_string()),
        ));
    }

    let replay_prefix = match parsed.parent {
        ParentResolution::None => 0,
        ParentResolution::Deferred => {
            return Ok(mark_deferred(
                file_path,
                file_modified,
                file_size,
                PendingReason::Stable(
                    parsed
                        .deferred_reason
                        .unwrap_or_else(|| "分叉会话元数据不完整".to_string()),
                ),
            ));
        }
        ParentResolution::Parent => {
            let Some(parent_id) = parsed.parent_id.as_deref() else {
                return Ok(mark_deferred(
                    file_path,
                    file_modified,
                    file_size,
                    PendingReason::Stable("分叉会话缺少父会话 ID".to_string()),
                ));
            };
            let Some(cutoff) = parsed.root_timestamp else {
                return Ok(mark_deferred(
                    file_path,
                    file_modified,
                    file_size,
                    PendingReason::Stable(
                        "parented rollout 的 root meta 缺少有效 timestamp".to_string(),
                    ),
                ));
            };
            match resolve_parent_signatures(parent_id, cutoff, rollout_index) {
                Ok(signatures) => matching_replay_prefix(&parsed.token_events, &signatures),
                Err(reason) => {
                    let pending_reason = if rollout_index.contains_key(parent_id) {
                        PendingReason::Retryable(reason)
                    } else {
                        PendingReason::MissingParent(parent_id.to_string())
                    };
                    return Ok(mark_deferred(
                        file_path,
                        file_modified,
                        file_size,
                        pending_reason,
                    ));
                }
            }
        }
    };

    if let Ok(mut caches) = replay_caches().lock() {
        caches.pending.remove(file_path);
    }

    let mut to_insert = Vec::new();
    let mut result = FileSyncResult {
        changed: true,
        ..FileSyncResult::default()
    };
    for (token_offset, event) in parsed.token_events.iter().enumerate() {
        let Some(event_index) = event.event_index else {
            continue;
        };
        if token_offset < replay_prefix {
            if event.line_offset > last_offset {
                result.skipped = result.skipped.saturating_add(1);
            }
            continue;
        }
        if event.line_offset <= last_offset && file_size >= last_size.max(0) as u64 {
            continue;
        }
        to_insert.push((event, event_index));
    }

    if !to_insert.is_empty() {
        let tx = conn
            .unchecked_transaction()
            .map_err(|error| format!("开启会话用量写入事务失败: {error}"))?;
        if last_offset == 0 {
            // Request indexes change when formerly omitted compaction calls are included.
            tx.execute("DELETE FROM session_usage_events WHERE file_path = ?1", params![file_path_str])
                .map_err(|error| error.to_string())?;
        }
        {
            let mut statement = tx
                .prepare_cached(
                    "INSERT INTO session_usage_events (
                        request_id, instance_id, instance_name, session_id, model,
                        timestamp, input_tokens, cached_input_tokens, output_tokens, file_path, service_tier
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                    ON CONFLICT(request_id) DO UPDATE SET
                        model = excluded.model, timestamp = excluded.timestamp,
                        input_tokens = excluded.input_tokens,
                        cached_input_tokens = excluded.cached_input_tokens,
                        output_tokens = excluded.output_tokens,
                        file_path = excluded.file_path, service_tier = excluded.service_tier",
                )
                .map_err(|error| format!("准备会话用量写入失败: {error}"))?;
            for chunk in to_insert.chunks(INSERT_BATCH_SIZE) {
                for (event, event_index) in chunk {
                    let request_id = format!("{REQUEST_ID_PREFIX}:{root_thread_id}:{event_index}");
                    let changed = statement
                        .execute(params![
                            request_id,
                            instance.id,
                            instance.name,
                            root_thread_id,
                            event.model,
                            event.timestamp.map(|value| value.timestamp()).unwrap_or(0),
                            event.delta.input as i64,
                            event.delta.cached_input as i64,
                            event.delta.output as i64,
                            file_path_str,
                            event.service_tier,
                        ])
                        .map_err(|error| format!("写入会话用量失败: {error}"))?;
                    if changed > 0 {
                        result.imported = result.imported.saturating_add(1);
                    } else {
                        result.skipped = result.skipped.saturating_add(1);
                    }
                }
            }
        }
        upsert_cursor_on_conn(
            &tx,
            &file_path_str,
            &instance.id,
            file_modified,
            file_size,
            parsed.line_offset,
        )?;
        tx.commit()
            .map_err(|error| format!("提交会话用量写入失败: {error}"))?;
        cursors.insert(
            file_path_str,
            (file_modified, file_size as i64, parsed.line_offset),
        );
    } else {
        upsert_cursor(
            conn,
            cursors,
            &file_path_str,
            &instance.id,
            file_modified,
            file_size,
            parsed.line_offset,
        )?;
    }

    Ok(result)
}

fn mark_deferred(
    file_path: &Path,
    modified: i64,
    size: u64,
    reason: PendingReason,
) -> FileSyncResult {
    let entry = PendingEntry {
        modified,
        size,
        reason,
    };
    if let Ok(mut caches) = replay_caches().lock() {
        caches.pending.insert(file_path.to_path_buf(), entry);
    }
    FileSyncResult {
        deferred: true,
        ..FileSyncResult::default()
    }
}

fn upsert_cursor(
    conn: &Connection,
    cursors: &mut HashMap<String, (i64, i64, i64)>,
    file_path: &str,
    instance_id: &str,
    modified: i64,
    size: u64,
    line_offset: i64,
) -> Result<(), String> {
    upsert_cursor_on_conn(conn, file_path, instance_id, modified, size, line_offset)?;
    cursors.insert(file_path.to_string(), (modified, size as i64, line_offset));
    Ok(())
}

fn upsert_cursor_on_conn(
    conn: &Connection,
    file_path: &str,
    instance_id: &str,
    modified: i64,
    size: u64,
    line_offset: i64,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO session_log_sync (
            file_path, instance_id, last_modified, last_size, last_line_offset
        ) VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(file_path) DO UPDATE SET
            instance_id = excluded.instance_id,
            last_modified = excluded.last_modified,
            last_size = excluded.last_size,
            last_line_offset = excluded.last_line_offset",
        params![file_path, instance_id, modified, size as i64, line_offset],
    )
    .map_err(|error| format!("更新会话用量游标失败: {error}"))?;
    Ok(())
}

fn parse_codex_file(
    file_path: &Path,
    root_thread_id: Option<String>,
) -> Result<ParsedCodexFile, String> {
    let mut state = TokenParseState::default();
    parse_codex_file_with_state(file_path, root_thread_id, &mut state)
}

fn parse_codex_file_with_state(
    file_path: &Path,
    root_thread_id: Option<String>,
    state: &mut TokenParseState,
) -> Result<ParsedCodexFile, String> {
    let file = File::open(file_path).map_err(|error| format!("无法打开文件: {error}"))?;
    let reader = BufReader::new(file);
    let mut root_meta_seen = false;
    let mut root_timestamp = None;
    let mut parent = ParentResolution::None;
    let mut parent_id = None;
    let mut deferred_reason = None;
    if state.current_model.is_empty() {
        state.current_model = "unknown".to_string();
    }
    let mut boundary_pending = true;
    let mut token_events = Vec::new();
    let mut line_offset = 0i64;
    let mut has_billable_tokens = false;

    for line_result in reader.lines() {
        line_offset += 1;
        let Ok(line) = line_result else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }

        let is_event_msg = line.contains("\"event_msg\"");
        let is_turn_context = line.contains("\"turn_context\"");
        let is_session_meta = line.contains("\"session_meta\"");
        let is_usage_record = line.contains("\"token_usage_record\"");
        if !is_event_msg && !is_turn_context && !is_session_meta && !is_usage_record {
            continue;
        }
        if is_event_msg && !line.contains("\"token_count\"") && !line.contains("\"thread_settings_applied\"") {
            continue;
        }

        let Ok(value) = serde_json::from_str::<JsonValue>(&line) else {
            continue;
        };
        let Some(event_type) = value.get("type").and_then(JsonValue::as_str) else {
            continue;
        };

        match event_type {
            "session_meta" if !root_meta_seen => {
                root_meta_seen = true;
                root_timestamp = parse_timestamp(value.get("timestamp"));
                let payload = value.get("payload").unwrap_or(&JsonValue::Null);
                match replay_parent_from_meta(payload) {
                    Ok(None) => {}
                    Ok(Some(parent_thread_id)) => {
                        if root_thread_id.as_deref() == Some(parent_thread_id.as_str()) {
                            parent = ParentResolution::Deferred;
                            deferred_reason =
                                Some("parent_thread_id 与 root_thread_id 相同".to_string());
                        } else {
                            parent = ParentResolution::Parent;
                            parent_id = Some(parent_thread_id);
                        }
                    }
                    Err(reason) => {
                        parent = ParentResolution::Deferred;
                        deferred_reason = Some(reason);
                    }
                }

                let meta_thread_id = non_empty_string(
                    payload
                        .get("id")
                        .or_else(|| payload.get("thread_id"))
                        .or_else(|| payload.get("threadId")),
                );
                if let Some(filename_id) = &root_thread_id {
                    let normalized_meta_id = meta_thread_id
                        .as_deref()
                        .and_then(|id| uuid::Uuid::parse_str(id).ok())
                        .map(|id| id.hyphenated().to_string());
                    if normalized_meta_id.as_deref() != Some(filename_id.as_str()) {
                        parent = ParentResolution::Deferred;
                        deferred_reason = Some(format!(
                            "文件名线程 ID ({filename_id}) 与 root meta ID ({}) 不一致",
                            meta_thread_id.unwrap_or_default()
                        ));
                    }
                }
            }
            "turn_context" => {
                if let Some(payload) = value.get("payload") {
                    if let Some(model) = payload
                        .get("model")
                        .or_else(|| payload.get("info").and_then(|info| info.get("model")))
                        .and_then(JsonValue::as_str)
                    {
                        state.current_model = normalize_codex_model(model);
                    }
                }
            }
            "event_msg" | "token_usage_record" => {
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                let is_usage_record = event_type == "token_usage_record";
                if !is_usage_record && payload.get("type").and_then(JsonValue::as_str) == Some("thread_settings_applied") {
                    // Forked histories retain their original settings owner.
                    let owner = payload.get("thread_id").and_then(JsonValue::as_str);
                    if owner.is_some() && owner != root_thread_id.as_deref() {
                        continue;
                    }
                    if let Some(settings) = payload.get("thread_settings") {
                        if let Some(model) = settings.get("model").and_then(JsonValue::as_str) {
                            state.current_model = normalize_codex_model(model);
                        }
                        state.current_service_tier = settings.get("service_tier")
                            .and_then(JsonValue::as_str)
                            .map(|tier| tier.trim().to_ascii_lowercase())
                            .filter(|tier| !tier.is_empty());
                    }
                    continue;
                }
                let (signature, total, last, response_id) = if is_usage_record {
                    let Some(response_id) = non_empty_string(payload.get("response_id")) else {
                        continue;
                    };
                    let Some(last) = payload.get("usage").and_then(parse_cumulative_tokens) else {
                        continue;
                    };
                    (
                        TokenUsageSignature {
                            total: parse_signature_counters(payload.get("thread_token_usage")),
                            last: parse_signature_counters(payload.get("usage")),
                        },
                        payload.get("thread_token_usage").and_then(parse_cumulative_tokens),
                        Some(last),
                        Some(response_id),
                    )
                } else {
                    if payload.get("type").and_then(JsonValue::as_str) != Some("token_count")
                        || !state.usage_record_ids.is_empty()
                    {
                        // Official records precede the UI snapshots of the same requests.
                        continue;
                    }
                    let Some(info) = payload.get("info").filter(|info| !info.is_null()) else {
                        continue;
                    };
                    let Some(signature) = parse_token_signature(info) else {
                        continue;
                    };
                    (
                        signature,
                        info.get("total_token_usage").and_then(parse_cumulative_tokens),
                        info.get("last_token_usage").and_then(parse_cumulative_tokens),
                        None,
                    )
                };
                if let Some(model) = payload
                    .get("info")
                    .and_then(|info| info.get("model").or_else(|| info.get("model_name")))
                    .or_else(|| payload.get("model"))
                    .and_then(JsonValue::as_str)
                {
                    state.current_model = normalize_codex_model(model);
                }

                let snapshot_source = token_snapshot_source(payload);
                if total.is_none() && last.is_none() {
                    continue;
                }
                if boundary_pending {
                    if let Some(current) = &total {
                        if state.total_high_water.as_ref().is_some_and(|previous| {
                            current.input < previous.input
                                || current.cached_input < previous.cached_input
                                || current.output < previous.output
                        }) {
                            state.total_high_water = None;
                            state.last_signature_by_source.clear();
                            state.previous_token_signature = None;
                        }
                        boundary_pending = false;
                    }
                }
                let has_total_snapshot = total.is_some();
                let duplicate_snapshot = if let Some(response_id) = response_id {
                    !state.usage_record_ids.insert(response_id)
                } else {
                    has_total_snapshot
                        && (state.last_signature_by_source.get(&snapshot_source) == Some(&signature)
                            || state.previous_token_signature.as_ref() == Some(&signature))
                };
                if has_total_snapshot {
                    state
                        .last_signature_by_source
                        .insert(snapshot_source, signature.clone());
                }
                state.previous_token_signature = Some(signature.clone());

                let delta = if duplicate_snapshot {
                    DeltaTokens::default()
                } else if let Some(last) = last {
                    DeltaTokens {
                        input: last.input,
                        cached_input: last.cached_input,
                        output: last.output,
                    }
                } else if let Some(total) = total.as_ref() {
                    compute_delta(&state.total_high_water, total)
                } else {
                    continue;
                };
                if let Some(total) = total {
                    if let Some(high_water) = state.total_high_water.as_mut() {
                        update_high_water(high_water, &total);
                    } else {
                        state.total_high_water = Some(total);
                    }
                }
                let delta = DeltaTokens {
                    cached_input: delta.cached_input.min(delta.input),
                    ..delta
                };
                let nonzero_index = if delta.is_zero() {
                    None
                } else {
                    has_billable_tokens = true;
                    state.event_index = state.event_index.saturating_add(1);
                    Some(state.event_index)
                };
                token_events.push(ParsedTokenEvent {
                    line_offset,
                    signature,
                    delta,
                    event_index: nonzero_index,
                    model: state.current_model.clone(),
                    service_tier: state.current_service_tier.clone(),
                    timestamp: parse_timestamp(value.get("timestamp")),
                });
            }
            _ => {}
        }
    }

    Ok(ParsedCodexFile {
        root_thread_id,
        root_meta_seen,
        root_timestamp,
        parent_id,
        parent,
        deferred_reason,
        token_events,
        line_offset,
        has_billable_tokens,
    })
}

fn replay_parent_from_meta(payload: &JsonValue) -> Result<Option<String>, String> {
    // Spawn ownership does not imply copied usage history. Only a history fork
    // needs its source timeline for replay deduplication.
    let Some(parent) = non_empty_string(payload.get("forked_from_id")) else {
        return Ok(None);
    };
    uuid::Uuid::parse_str(&parent)
        .map(|value| Some(value.hyphenated().to_string()))
        .map_err(|_| format!("forked_from_id 不是有效 UUID: {parent}"))
}

fn resolve_parent_signatures(
    parent_id: &str,
    cutoff: DateTime<Utc>,
    rollout_index: &RolloutIndex,
) -> Result<Vec<TokenUsageSignature>, String> {
    let Some(candidates) = rollout_index.get(parent_id) else {
        return Err(format!("找不到父 rollout: {parent_id}"));
    };
    let mut signatures = Vec::new();
    let mut seen_names = HashSet::new();
    for candidate in candidates {
        // The same rollout can temporarily exist in sessions and archived_sessions.
        // Its filename is its stable segment identity across that move.
        if seen_names.insert(candidate.file_name().map(|name| name.to_os_string())) {
            signatures.extend(parent_signatures_before(candidate, cutoff)?);
        }
    }
    if signatures.is_empty() {
        return Err(format!("找不到父 rollout: {parent_id}"));
    }
    Ok(signatures)
}

fn parent_signatures_before(
    parent_path: &Path,
    cutoff: DateTime<Utc>,
) -> Result<Vec<TokenUsageSignature>, String> {
    let metadata = fs::metadata(parent_path)
        .map_err(|error| format!("无法读取父 rollout {}: {error}", parent_path.display()))?;
    let stamp = ParentFileStamp {
        modified_nanos: metadata_modified_nanos(&metadata),
        size: metadata.len(),
    };
    if let Ok(caches) = replay_caches().lock() {
        if let Some(cached) = caches
            .parent_timelines
            .get(parent_path)
            .filter(|entry| entry.stamp == stamp)
        {
            return signatures_before(&cached.timeline, parent_path, cutoff);
        }
    }

    let parsed = parse_codex_file(parent_path, thread_id_from_filename(parent_path))?;
    let mut events = Vec::new();
    let mut has_token_without_timestamp = false;
    for event in parsed.token_events {
        let Some(timestamp) = event.timestamp else {
            has_token_without_timestamp = true;
            continue;
        };
        events.push(TimestampedTokenSignature {
            timestamp,
            signature: event.signature,
        });
    }

    let timeline = ParentTokenTimeline {
        events,
        has_token_without_timestamp,
    };
    let result = signatures_before(&timeline, parent_path, cutoff);
    if let Ok(mut caches) = replay_caches().lock() {
        caches.parent_timelines.insert(
            parent_path.to_path_buf(),
            CachedParentTimeline { stamp, timeline },
        );
    }
    result
}

fn signatures_before(
    timeline: &ParentTokenTimeline,
    parent_path: &Path,
    cutoff: DateTime<Utc>,
) -> Result<Vec<TokenUsageSignature>, String> {
    if timeline.has_token_without_timestamp {
        return Err(format!(
            "父 rollout {} 的用量记录缺少有效 timestamp",
            parent_path.display()
        ));
    }
    Ok(timeline
        .events
        .iter()
        .filter(|event| event.timestamp <= cutoff)
        .map(|event| event.signature.clone())
        .collect())
}

fn matching_replay_prefix(child: &[ParsedTokenEvent], parent: &[TokenUsageSignature]) -> usize {
    let mut parent_offset = 0usize;
    let mut matched = 0usize;
    for event in child {
        let Some(relative_match) = parent[parent_offset..]
            .iter()
            .position(|signature| signature == &event.signature)
        else {
            break;
        };
        parent_offset += relative_match + 1;
        matched += 1;
    }
    matched
}

fn normalize_codex_model(raw: &str) -> String {
    let mut name = raw.to_lowercase();
    if let Some(pos) = name.rfind('/') {
        name = name[pos + 1..].to_string();
    }
    if name.len() > 11 && name.is_char_boundary(name.len() - 11) {
        let suffix = &name[name.len() - 11..];
        if suffix.is_ascii()
            && suffix.as_bytes()[0] == b'-'
            && suffix[1..5].chars().all(|c| c.is_ascii_digit())
            && suffix.as_bytes()[5] == b'-'
            && suffix[6..8].chars().all(|c| c.is_ascii_digit())
            && suffix.as_bytes()[8] == b'-'
            && suffix[9..11].chars().all(|c| c.is_ascii_digit())
        {
            name.truncate(name.len() - 11);
        }
    }
    if name.len() > 9 {
        let parts: Vec<&str> = name.rsplitn(2, '-').collect();
        if parts.len() == 2 {
            if let Some(suffix) = parts.first() {
                if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
                    name = parts[1].to_string();
                }
            }
        }
    }
    name
}

fn compute_delta(prev: &Option<CumulativeTokens>, current: &CumulativeTokens) -> DeltaTokens {
    match prev {
        None => DeltaTokens {
            input: current.input,
            cached_input: current.cached_input,
            output: current.output,
        },
        Some(previous) => DeltaTokens {
            input: current.input.saturating_sub(previous.input),
            cached_input: current.cached_input.saturating_sub(previous.cached_input),
            output: current.output.saturating_sub(previous.output),
        },
    }
}

fn update_high_water(high_water: &mut CumulativeTokens, current: &CumulativeTokens) {
    high_water.input = high_water.input.max(current.input);
    high_water.cached_input = high_water.cached_input.max(current.cached_input);
    high_water.output = high_water.output.max(current.output);
}

fn parse_cumulative_tokens(total_usage: &JsonValue) -> Option<CumulativeTokens> {
    let fields = total_usage.as_object()?;
    if ![
        "input_tokens",
        "cached_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ]
    .iter()
    .any(|field| fields.contains_key(*field))
    {
        return None;
    }
    Some(CumulativeTokens {
        input: total_usage
            .get("input_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0),
        cached_input: total_usage
            .get("cached_input_tokens")
            .or_else(|| total_usage.get("cache_read_input_tokens"))
            .and_then(JsonValue::as_u64)
            .unwrap_or(0),
        output: total_usage
            .get("output_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0),
    })
}

fn parse_signature_counters(value: Option<&JsonValue>) -> Option<TokenCountersSignature> {
    let value = value?.as_object()?;
    Some(TokenCountersSignature {
        input: value.get("input_tokens").and_then(JsonValue::as_u64),
        cached_input: value
            .get("cached_input_tokens")
            .or_else(|| value.get("cache_read_input_tokens"))
            .and_then(JsonValue::as_u64),
        output: value.get("output_tokens").and_then(JsonValue::as_u64),
        reasoning_output: value
            .get("reasoning_output_tokens")
            .and_then(JsonValue::as_u64),
        total: value.get("total_tokens").and_then(JsonValue::as_u64),
    })
}

fn parse_token_signature(info: &JsonValue) -> Option<TokenUsageSignature> {
    let total = parse_signature_counters(info.get("total_token_usage"));
    let last = parse_signature_counters(info.get("last_token_usage"));
    (total.is_some() || last.is_some()).then_some(TokenUsageSignature { total, last })
}

fn token_snapshot_source(payload: &JsonValue) -> Option<String> {
    payload
        .get("rate_limits")
        .and_then(|rate_limits| rate_limits.get("limit_id"))
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_timestamp(value: Option<&JsonValue>) -> Option<DateTime<Utc>> {
    value
        .and_then(JsonValue::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn non_empty_string(value: Option<&JsonValue>) -> Option<String> {
    value
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn metadata_modified_nanos(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0)
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO session_usage_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(|error| format!("写入会话用量元数据失败: {error}"))?;
    Ok(())
}

fn get_meta_i64(conn: &Connection, key: &str) -> Option<i64> {
    conn.query_row(
        "SELECT value FROM session_usage_meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|value| value.parse().ok())
}

fn query_totals(
    conn: &Connection,
    where_sql: &str,
    params: &[rusqlite::types::Value],
) -> Result<CodexSessionUsageTotals, String> {
    let sql = format!(
        "SELECT
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(cached_input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(COUNT(*), 0)
         FROM session_usage_events {where_sql}"
    );
    conn.query_row(&sql, rusqlite::params_from_iter(params.iter()), |row| {
        let input = row.get::<_, i64>(0)?.max(0) as u64;
        let cached = row.get::<_, i64>(1)?.max(0) as u64;
        let output = row.get::<_, i64>(2)?.max(0) as u64;
        let requests = row.get::<_, i64>(3)?.max(0) as u64;
        Ok(CodexSessionUsageTotals {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            total_tokens: input.saturating_add(output),
            request_count: requests,
            estimated_cost_usd: 0.0,
        })
    })
    .map_err(|error| format!("汇总会话用量失败: {error}"))
}

fn query_breakdown(
    conn: &Connection,
    where_sql: &str,
    params: &[rusqlite::types::Value],
    key_column: &str,
    label_column: Option<&str>,
    instance_names: &HashMap<String, String>,
) -> Result<Vec<CodexSessionUsageBreakdownRow>, String> {
    if key_column == "model" {
        let mut rows = query_pricing_breakdown(conn, where_sql, params, "model")?
            .into_iter().map(|(_, row)| row).collect::<Vec<_>>();
        rows.sort_by(|left, right| right.total_tokens.cmp(&left.total_tokens).then(left.key.cmp(&right.key)));
        return Ok(rows);
    }
    let label_sql = label_column.unwrap_or(key_column);
    let sql = format!(
        "SELECT
            {key_column},
            MAX({label_sql}),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(cached_input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(COUNT(*), 0)
         FROM session_usage_events {where_sql}
         GROUP BY {key_column}
         ORDER BY SUM(input_tokens + output_tokens) DESC, {key_column} ASC"
    );
    let mut statement = conn
        .prepare(&sql)
        .map_err(|error| format!("查询会话用量分组失败: {error}"))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let key = row.get::<_, String>(0)?;
            let stored_label = row.get::<_, String>(1)?;
            let input = row.get::<_, i64>(2)?.max(0) as u64;
            let cached = row.get::<_, i64>(3)?.max(0) as u64;
            let output = row.get::<_, i64>(4)?.max(0) as u64;
            let requests = row.get::<_, i64>(5)?.max(0) as u64;
            let label = if key_column == "instance_id" {
                instance_names
                    .get(&key)
                    .cloned()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(stored_label)
            } else {
                stored_label
            };
            Ok(CodexSessionUsageBreakdownRow {
                key,
                label,
                input_tokens: input,
                cached_input_tokens: cached,
                output_tokens: output,
                total_tokens: input.saturating_add(output),
                request_count: requests,
                estimated_cost_usd: None,
                pricing_usage: Vec::new(),
            })
        })
        .map_err(|error| format!("遍历会话用量分组失败: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("解析会话用量分组失败: {error}"))
}

fn query_day_breakdown(
    conn: &Connection,
    where_sql: &str,
    params: &[rusqlite::types::Value],
) -> Result<Vec<CodexSessionUsageBreakdownRow>, String> {
    let sql = format!(
        "SELECT timestamp, model, service_tier, input_tokens, cached_input_tokens, output_tokens
         FROM session_usage_events {where_sql}"
    );
    let mut statement = conn
        .prepare(&sql)
        .map_err(|error| format!("查询会话用量日期失败: {error}"))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?.max(0) as u64,
                row.get::<_, i64>(4)?.max(0) as u64,
                row.get::<_, i64>(5)?.max(0) as u64,
            ))
        })
        .map_err(|error| format!("遍历会话用量日期失败: {error}"))?;

    let mut days = std::collections::BTreeMap::<String, CodexSessionUsageBreakdownRow>::new();
    let mut groups = std::collections::BTreeMap::<
        (String, String, Option<String>, bool),
        CodexSessionUsagePricingGroup,
    >::new();
    for row in rows {
        let (timestamp, model, service_tier, input, cached, output) =
            row.map_err(|error| format!("解析会话用量日期失败: {error}"))?;
        let key = local_day_key(timestamp);
        let day = days
            .entry(key.clone())
            .or_insert_with(|| CodexSessionUsageBreakdownRow {
                label: key.clone(),
                key: key.clone(),
                input_tokens: 0,
                cached_input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                request_count: 0,
                estimated_cost_usd: None,
                pricing_usage: Vec::new(),
            });
        day.input_tokens = day.input_tokens.saturating_add(input);
        day.cached_input_tokens = day.cached_input_tokens.saturating_add(cached);
        day.output_tokens = day.output_tokens.saturating_add(output);
        day.total_tokens = day
            .total_tokens
            .saturating_add(input.saturating_add(output));
        day.request_count = day.request_count.saturating_add(1);

        // Match model/session grouping: summed tokens cannot select context rates.
        let group = groups
            .entry((
                key,
                model.clone(),
                service_tier.clone(),
                input > LONG_CONTEXT_THRESHOLD_TOKENS,
            ))
            .or_insert_with(|| CodexSessionUsagePricingGroup {
                model: Some(model),
                service_tier,
                context_input_tokens: 0,
                input_tokens: 0,
                cached_input_tokens: 0,
                output_tokens: 0,
            });
        group.context_input_tokens = group.context_input_tokens.max(input);
        group.input_tokens = group.input_tokens.saturating_add(input);
        group.cached_input_tokens = group.cached_input_tokens.saturating_add(cached);
        group.output_tokens = group.output_tokens.saturating_add(output);
    }
    for ((day, _, _, _), usage) in groups {
        days.get_mut(&day)
            .expect("pricing group has a daily row")
            .pricing_usage
            .push(usage);
    }
    Ok(days.into_values().collect())
}

fn local_day_key(timestamp: i64) -> String {
    if timestamp <= 0 {
        return String::new();
    }
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|value| {
            format!(
                "{:04}-{:02}-{:02}",
                value.year(),
                value.month(),
                value.day()
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PARENT_ID: &str = "00000000-0000-4000-8000-000000000001";
    const CHILD_ID: &str = "00000000-0000-4000-8000-000000000002";

    fn daily_cost_connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_usage_events (
            timestamp INTEGER, model TEXT, service_tier TEXT,
            input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER,
            instance_id TEXT
        )",
        )
        .unwrap();
        conn
    }

    fn daily_fixture_price(model: &str, usage: &CodexSessionUsagePricingGroup) -> Option<f64> {
        let rate = match model {
            "model-a" => 2.0,
            "model-b" => 4.0,
            _ => return None,
        };
        let tier = if usage.service_tier.as_deref() == Some("fast") {
            2.0
        } else {
            1.0
        };
        let context = if usage.context_input_tokens > LONG_CONTEXT_THRESHOLD_TOKENS {
            3.0
        } else {
            1.0
        };
        Some(
            ((usage.input_tokens - usage.cached_input_tokens) as f64
                + usage.cached_input_tokens as f64 * 0.25
                + usage.output_tokens as f64 * 4.0)
                * rate
                * tier
                * context,
        )
    }

    #[test]
    fn daily_cost_preserves_filters_models_tiers_context_and_remote_transport() {
        let conn = daily_cost_connection();
        let noon = Local
            .with_ymd_and_hms(2026, 10, 3, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        let next_day = Local
            .with_ymd_and_hms(2026, 10, 4, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        for (timestamp, model, tier, input, cached, output, instance) in [
            (noon, "model-a", "default", 150_000, 100_000, 10, "a"),
            (noon + 1, "model-a", "default", 160_000, 120_000, 20, "a"),
            (noon + 2, "model-a", "fast", 100, 80, 10, "a"),
            (noon + 3, "model-a", "default", 272_001, 272_000, 30, "a"),
            (noon + 4, "model-b", "default", 200, 100, 20, "a"),
            (next_day, "model-a", "default", 150_000, 100_000, 10, "a"),
            (noon, "model-a", "default", 150_000, 100_000, 10, "b"),
        ] {
            conn.execute(
                "INSERT INTO session_usage_events VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![timestamp, model, tier, input, cached, output, instance],
            )
            .unwrap();
        }
        let params = vec![
            rusqlite::types::Value::Integer(noon),
            rusqlite::types::Value::Integer(noon + 4),
            rusqlite::types::Value::Text("a".into()),
        ];
        let filter = "WHERE timestamp >= ?1 AND timestamp <= ?2 AND instance_id = ?3";
        let days = query_day_breakdown(&conn, filter, &params).unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].key, "2026-10-03");
        assert_eq!(days[0].request_count, 5);
        assert_eq!(days[0].total_tokens, 582_391);
        assert_eq!(days[0].pricing_usage.len(), 4);
        let short = days[0]
            .pricing_usage
            .iter()
            .find(|usage| usage.input_tokens == 310_000)
            .unwrap();
        assert_eq!(short.model.as_deref(), Some("model-a"));
        assert_eq!(short.context_input_tokens, 160_000);
        assert_eq!(short.service_tier.as_deref(), Some("default"));

        // SSH transports unpriced groups; the desktop applies its price book later.
        let serialized = serde_json::to_value(&days).unwrap();
        assert!(serialized[0]["estimatedCostUsd"].is_null());
        let mut transported: Vec<CodexSessionUsageBreakdownRow> =
            serde_json::from_value(serialized).unwrap();
        transported[0].apply_cost(daily_fixture_price);
        let expected = 145_120.0 * 2.0 + 80.0 * 2.0 * 2.0 + 68_121.0 * 2.0 * 3.0 + 205.0 * 4.0;
        assert_eq!(transported[0].estimated_cost_usd, Some(expected));
        let mut models =
            query_breakdown(&conn, filter, &params, "model", None, &HashMap::new()).unwrap();
        for model in &mut models {
            model.apply_cost(daily_fixture_price);
        }
        assert_eq!(
            models
                .iter()
                .map(|row| row.estimated_cost_usd.unwrap())
                .sum::<f64>(),
            expected
        );
        let restored: Vec<CodexSessionUsageBreakdownRow> =
            serde_json::from_str(&serde_json::to_string(&transported).unwrap()).unwrap();
        assert_eq!(restored[0].estimated_cost_usd, Some(expected));
        assert_eq!(restored[0].pricing_usage.len(), 4);
    }

    #[test]
    fn daily_cost_distinguishes_unknown_models_from_priced_zero() {
        let conn = daily_cost_connection();
        let noon = Local
            .with_ymd_and_hms(2026, 10, 3, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        conn.execute(
            "INSERT INTO session_usage_events VALUES (?1, 'model-a', NULL, 0, 0, 0, 'a')",
            [noon],
        )
        .unwrap();
        let mut day = query_day_breakdown(&conn, "", &[]).unwrap().remove(0);
        day.apply_cost(daily_fixture_price);
        assert_eq!(day.estimated_cost_usd, Some(0.0));
        conn.execute(
            "INSERT INTO session_usage_events VALUES (?1, 'unknown-model', NULL, 500, 0, 20, 'a')",
            [noon],
        )
        .unwrap();
        let mut day = query_day_breakdown(&conn, "", &[]).unwrap().remove(0);
        day.apply_cost(daily_fixture_price);
        assert_eq!(day.estimated_cost_usd, None);
        assert_eq!(day.request_count, 2);
        assert_eq!(day.total_tokens, 520);
    }

    #[test]
    fn older_pricing_groups_use_model_row_key_and_unpriced_days_stay_unknown() {
        let mut model: CodexSessionUsageBreakdownRow = serde_json::from_value(json!({
            "key": "model-a", "label": "Model A", "inputTokens": 100,
            "cachedInputTokens": 80, "outputTokens": 10, "totalTokens": 110,
            "requestCount": 1, "pricingUsage": [{ "contextInputTokens": 100,
                "inputTokens": 100, "cachedInputTokens": 80, "outputTokens": 10 }]
        }))
        .unwrap();
        assert_eq!(model.pricing_usage[0].model, None);
        model.apply_cost(daily_fixture_price);
        assert_eq!(model.estimated_cost_usd, Some(160.0));
        let mut day = model.clone();
        day.key = "2026-10-03".into();
        day.pricing_usage.clear();
        day.apply_cost(daily_fixture_price);
        assert_eq!(day.estimated_cost_usd, None);
    }

    #[test]
    fn session_cost_groups_models_and_keeps_child_usage_separate() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE session_usage_events (session_id TEXT, model TEXT,
            service_tier TEXT,
            input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER);
            INSERT INTO session_usage_events VALUES
            ('parent', 'model-a', NULL, 100, 80, 10), ('parent', 'model-a', NULL, 50, 20, 5),
            ('parent', 'model-b', NULL, 200, 100, 20), ('child', 'model-a', NULL, 30, 10, 3);").unwrap();
        let mut sessions = query_session_tokens(&conn).unwrap();
        let price = |model: &str, usage: &CodexSessionUsagePricingGroup| {
            let rate = match model { "model-a" => 2.0, "model-b" => 4.0, _ => return None };
            Some((usage.input_tokens - usage.cached_input_tokens) as f64 * rate
                + usage.cached_input_tokens as f64 * 0.5 + usage.output_tokens as f64 * 10.0)
        };
        for session in &mut sessions { session.apply_cost(price); }
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "child");
        assert_eq!(sessions[0].estimated_cost_usd, Some(75.0));
        let parent = &mut sessions[1];
        assert_eq!((parent.input_tokens, parent.output_tokens), (350, 35));
        assert_eq!(parent.by_model.len(), 2);
        assert_eq!(parent.estimated_cost_usd, Some(950.0));
        parent.by_model[1].key = "unknown".into();
        parent.apply_cost(price);
        assert_eq!(parent.estimated_cost_usd, None);
        parent.apply_cost(|_, _| Some(0.0));
        assert_eq!(parent.estimated_cost_usd, Some(0.0));
    }

    #[test]
    fn older_session_stats_do_not_claim_zero_cost() {
        let mut stats: CodexSessionTokenStats = serde_json::from_value(json!({
            "sessionId": "old", "inputTokens": 10, "outputTokens": 1, "totalTokens": 11
        })).unwrap();
        stats.apply_cost(|_, _| Some(0.0));
        assert_eq!(stats.estimated_cost_usd, None);
    }

    #[test]
    fn pricing_groups_preserve_tier_and_per_request_context_size() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE session_usage_events (session_id TEXT, model TEXT,
            service_tier TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER);
            INSERT INTO session_usage_events VALUES
            ('parent', 'fixture-model', 'default', 150000, 100000, 10),
            ('parent', 'fixture-model', 'default', 160000, 120000, 20),
            ('parent', 'fixture-model', 'fast', 100, 80, 10),
            ('parent', 'fixture-model', 'default', 272001, 272000, 30),
            ('child', 'fixture-model', 'fast', 100, 80, 10);").unwrap();
        let price = |_: &str, usage: &CodexSessionUsagePricingGroup| {
            let tier = if usage.service_tier.as_deref() == Some("fast") { 2.0 } else { 1.0 };
            let context = if usage.context_input_tokens > 272_000 { 3.0 } else { 1.0 };
            Some((usage.input_tokens - usage.cached_input_tokens) as f64 * tier * context
                + usage.cached_input_tokens as f64 * 0.1 * tier * context
                + usage.output_tokens as f64 * 10.0 * tier)
        };
        let mut sessions = query_session_tokens(&conn).unwrap();
        for session in &mut sessions { session.apply_cost(price); }
        let parent = &sessions[1];
        assert_eq!(parent.by_model.len(), 1);
        let groups = &parent.by_model[0].pricing_usage;
        assert_eq!(groups.len(), 3);
        let short = groups.iter().find(|group| group.input_tokens == 310_000).unwrap();
        assert_eq!(short.context_input_tokens, 160_000);
        assert_eq!(sessions[0].estimated_cost_usd, Some(256.0));
        assert_eq!(parent.estimated_cost_usd, Some(194_459.0));

        let mut summary = query_breakdown(&conn, "WHERE session_id = ?", &[rusqlite::types::Value::Text("parent".into())], "model", None, &HashMap::new()).unwrap();
        assert_eq!(summary.len(), 1);
        summary[0].apply_cost(price);
        assert_eq!(summary[0].estimated_cost_usd, parent.estimated_cost_usd);
        let transported: Vec<CodexSessionTokenStats> = serde_json::from_str(&serde_json::to_string(&sessions).unwrap()).unwrap();
        assert_eq!(transported[1].by_model[0].pricing_usage.len(), 3);
        assert_eq!(transported[1].estimated_cost_usd, parent.estimated_cost_usd);
    }

    #[test]
    fn parser_tracks_official_tier_settings_and_ignores_other_threads() {
        let dir = make_temp_dir("codex-usage-tiers");
        let file = rollout_path(&dir, PARENT_ID);
        write_jsonl(&file, &[
            session_meta(PARENT_ID),
            thread_settings(PARENT_ID, Some("default")),
            turn_context("fixture-model"),
            token_count_with_last(100, 80, 10, 100, 80, 10, "codex", "2026-07-10T03:00:01Z"),
            thread_settings(PARENT_ID, Some("priority")),
            turn_context("fixture-model"),
            thread_settings(CHILD_ID, Some("flex")),
            token_count_with_last(200, 160, 20, 100, 80, 10, "codex", "2026-07-10T03:00:02Z"),
            thread_settings(PARENT_ID, None),
            token_count_with_last(300, 240, 30, 100, 80, 10, "codex", "2026-07-10T03:00:03Z"),
        ]);
        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string())).unwrap();
        let tiers = parsed.token_events.iter().map(|event| event.service_tier.as_deref()).collect::<Vec<_>>();
        assert_eq!(tiers, vec![Some("default"), Some("priority"), None]);
        assert_eq!(nonzero_deltas(&parsed), vec![(100, 80, 10); 3]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_upgrade_recovers_tiers_once_without_losing_cached_totals() {
        let dir = make_temp_dir("codex-usage-tier-migration");
        let file = rollout_path(&dir.join("sessions"), PARENT_ID);
        write_jsonl(&file, &[
            session_meta(PARENT_ID),
            thread_settings(PARENT_ID, Some("priority")),
            token_count_with_last(100, 80, 10, 100, 80, 10, "codex", "2026-07-10T03:00:01Z"),
        ]);
        let instances = vec![UsageInstance { id: "fixture".into(), name: "Fixture".into(), data_dir: dir.clone() }];
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
        let conn = store.open_conn().unwrap();
        conn.execute_batch("ALTER TABLE session_usage_events DROP COLUMN service_tier;").unwrap();
        drop(conn);

        let cached = store.query(&CodexSessionUsageQuery::default(), &instances).unwrap();
        assert_eq!(cached.totals.input_tokens, 100);
        assert_eq!(cached.totals.request_count, 1);
        assert_eq!(cached.last_synced_at, None);
        store.sync(false, &instances).unwrap();
        let repaired = store.query(&CodexSessionUsageQuery::default(), &instances).unwrap();
        assert_eq!(repaired.totals.request_count, 1);
        assert_eq!(repaired.by_model[0].pricing_usage[0].service_tier.as_deref(), Some("priority"));
        assert!(repaired.last_synced_at.is_some());
        assert_eq!(store.sync(false, &instances).unwrap().files_changed, 0);
        fs::remove_dir_all(dir).unwrap();
    }

    fn thread_settings(thread_id: &str, service_tier: Option<&str>) -> JsonValue {
        json!({"type": "event_msg", "payload": {
            "type": "thread_settings_applied", "thread_id": thread_id,
            "thread_settings": {"model": "fixture-model", "service_tier": service_tier},
        }})
    }

    #[test]
    fn official_records_include_compaction_and_keep_legacy_prefix_without_double_counting() {
        let dir = make_temp_dir("codex-usage-records");
        let file = rollout_path(&dir, PARENT_ID);
        let first = usage_record("response-a", 200, 150, 20, 300, 230, 30, "2026-07-10T03:00:03Z");
        write_jsonl(&file, &[
            session_meta(PARENT_ID),
            turn_context("fixture-model"),
            token_count_with_last(100, 80, 10, 100, 80, 10, "codex", "2026-07-10T03:00:02Z"),
            thread_settings(PARENT_ID, Some("priority")),
            first.clone(),
            token_count_with_last(300, 230, 30, 200, 150, 20, "codex", "2026-07-10T03:00:03Z"),
            token_count_with_last(300, 230, 30, 200, 150, 20, "other", "2026-07-10T03:00:03Z"),
            usage_record("response-compaction", 50, 40, 5, 350, 270, 35, "2026-07-10T03:00:04Z"),
            json!({"type": "compacted", "payload": {}}),
            // Equal request usage with a different response ID is a separate request.
            usage_record("response-b", 200, 150, 20, 550, 420, 55, "2026-07-10T03:00:05Z"),
            token_count_with_last(500, 380, 50, 200, 150, 20, "codex", "2026-07-10T03:00:05Z"),
            first,
        ]);
        let parsed = parse_codex_file(&file, Some(PARENT_ID.into())).unwrap();
        assert_eq!(nonzero_deltas(&parsed), vec![(100, 80, 10), (200, 150, 20), (50, 40, 5), (200, 150, 20)]);
        assert_eq!(parsed.token_events[0].service_tier, None);
        assert!(parsed.token_events[1..].iter().all(|event| event.service_tier.as_deref() == Some("priority")));
        assert!(parsed.token_events.last().unwrap().delta.is_zero());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parser_upgrade_replaces_old_indexes_once_and_keeps_incremental_sync() {
        let dir = make_temp_dir("codex-usage-record-upgrade");
        let file = rollout_path(&dir.join("sessions"), PARENT_ID);
        let mut log = vec![
            session_meta(PARENT_ID),
            thread_settings(PARENT_ID, Some("priority")),
            usage_record("response-a", 100, 80, 10, 100, 80, 10, "2026-07-10T03:00:01Z"),
            token_count_with_last(100, 80, 10, 100, 80, 10, "codex", "2026-07-10T03:00:01Z"),
            usage_record("response-compaction", 50, 40, 5, 150, 120, 15, "2026-07-10T03:00:02Z"),
            usage_record("response-b", 200, 160, 20, 350, 280, 35, "2026-07-10T03:00:03Z"),
            token_count_with_last(300, 240, 30, 200, 160, 20, "codex", "2026-07-10T03:00:03Z"),
        ];
        write_jsonl(&file, &log);
        let instances = [UsageInstance { id: "fixture".into(), name: "Fixture".into(), data_dir: dir.clone() }];
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        assert_eq!(store.sync(false, &instances).unwrap().imported, 3);
        let conn = store.open_conn().unwrap();
        conn.execute("DELETE FROM session_usage_events WHERE request_id = ?1", params![format!("{REQUEST_ID_PREFIX}:{PARENT_ID}:3")]).unwrap();
        conn.execute("UPDATE session_usage_events SET input_tokens=200, cached_input_tokens=160, output_tokens=20 WHERE request_id = ?1",
            params![format!("{REQUEST_ID_PREFIX}:{PARENT_ID}:2")]).unwrap();
        conn.execute("DELETE FROM session_usage_meta WHERE key='usage_parser_version'", []).unwrap();
        drop(conn);

        let cached = store.query(&CodexSessionUsageQuery::default(), &instances).unwrap();
        assert_eq!((cached.totals.input_tokens, cached.totals.request_count), (300, 2));
        assert_eq!(cached.last_synced_at, None);
        assert_eq!(store.sync(false, &instances).unwrap().imported, 3);
        let repaired = store.query(&CodexSessionUsageQuery::default(), &instances).unwrap();
        assert_eq!((repaired.totals.input_tokens, repaired.totals.output_tokens, repaired.totals.request_count), (350, 35, 3));
        assert_eq!(repaired.session_tokens.unwrap()[0].input_tokens, 350);
        assert_eq!(store.sync(false, &instances).unwrap().files_changed, 0);

        log.push(usage_record("response-c", 70, 60, 7, 420, 340, 42, "2026-07-10T03:00:04Z"));
        log.push(token_count_with_last(370, 300, 37, 70, 60, 7, "codex", "2026-07-10T03:00:04Z"));
        write_jsonl(&file, &log);
        assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
        let appended = store.query(&CodexSessionUsageQuery::default(), &instances).unwrap();
        assert_eq!((appended.totals.input_tokens, appended.totals.request_count), (420, 4));
        assert_eq!(store.sync(true, &instances).unwrap().imported, 4);
        let rebuilt = store.query(&CodexSessionUsageQuery::default(), &instances).unwrap();
        assert!(rebuilt.last_synced_at.is_some());
        assert_eq!(rebuilt.files_tracked, 1);
        assert_eq!(store.sync(false, &instances).unwrap().files_changed, 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn official_record_fork_replay_including_compaction_is_not_billed_twice() {
        let dir = make_temp_dir("codex-usage-record-fork");
        let first = usage_record("response-a", 100, 80, 10, 100, 80, 10, "2026-07-10T03:00:01Z");
        let compaction = usage_record("response-compaction", 50, 40, 5, 150, 120, 15, "2026-07-10T03:00:02.300Z");
        write_jsonl(&rollout_path(&dir.join("sessions"), PARENT_ID), &[
            session_meta(PARENT_ID), turn_context("fixture-model"), first.clone(),
        ]);
        let segment_id = "10000000-0000-4000-8000-000000000001";
        let continuation = dir.join("sessions").join(format!("rollout-2026-07-10T03-00-02-{PARENT_ID}_{segment_id}.jsonl"));
        write_jsonl(&continuation, &[
            session_meta_at(PARENT_ID, None, "2026-07-10T03:00:02Z"), first.clone(), compaction.clone(),
            // Equal counters after the fork must not hide an independent child request.
            usage_record("response-parent-later", 70, 60, 7, 220, 180, 22, "2026-07-10T03:00:02.900Z"),
        ]);
        write_jsonl(&rollout_path(&dir.join("sessions"), CHILD_ID), &[
            session_meta_at(CHILD_ID, Some(PARENT_ID), "2026-07-10T03:00:02.500Z"),
            turn_context("fixture-model"), first, compaction,
            usage_record("response-child", 70, 60, 7, 220, 180, 22, "2026-07-10T03:00:02.600Z"),
        ]);
        let instances = [UsageInstance { id: "fixture".into(), name: "Fixture".into(), data_dir: dir.clone() }];
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        let sync = store.sync(false, &instances).unwrap();
        assert_eq!((sync.imported, sync.deferred_files), (4, 0));
        let report = store.query(&CodexSessionUsageQuery::default(), &instances).unwrap();
        assert_eq!((report.totals.input_tokens, report.totals.output_tokens), (290, 29));
        let child = report.session_tokens.unwrap().into_iter().find(|session| session.session_id == CHILD_ID).unwrap();
        assert_eq!((child.input_tokens, child.output_tokens), (70, 7));
        fs::remove_dir_all(dir).unwrap();
    }

    fn usage_record(
        response_id: &str, input: u64, cached: u64, output: u64,
        total_input: u64, total_cached: u64, total_output: u64, timestamp: &str,
    ) -> JsonValue {
        json!({"type": "token_usage_record", "timestamp": timestamp, "payload": {
            "thread_id": PARENT_ID, "response_id": response_id,
            "usage": {"input_tokens": input, "cached_input_tokens": cached,
                "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output},
            "thread_token_usage": {"input_tokens": total_input, "cached_input_tokens": total_cached,
                "output_tokens": total_output, "reasoning_output_tokens": 0, "total_tokens": total_input + total_output},
        }})
    }

    #[test]
    fn breakdown_transport_preserves_optional_model_cost() {
        let payload = json!({
            "key": "fixture-model", "label": "Fixture model",
            "inputTokens": 100, "cachedInputTokens": 80,
            "outputTokens": 10, "totalTokens": 110, "requestCount": 1,
        });
        let mut row: CodexSessionUsageBreakdownRow =
            serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(row.estimated_cost_usd, None);
        assert_eq!(serde_json::to_value(&row).unwrap(), payload);

        row.estimated_cost_usd = Some(0.125);
        let priced = serde_json::to_value(&row).unwrap();
        assert_eq!(priced["estimatedCostUsd"], 0.125);
        assert_eq!(priced["totalTokens"], 110);
        let restored: CodexSessionUsageBreakdownRow = serde_json::from_value(priced).unwrap();
        assert_eq!(restored.estimated_cost_usd, Some(0.125));
    }

    fn write_jsonl(path: &Path, values: &[JsonValue]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let contents = values
            .iter()
            .map(JsonValue::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(path, contents).unwrap();
    }

    fn rollout_path(dir: &Path, thread_id: &str) -> PathBuf {
        dir.join(format!("rollout-2026-07-10T03-00-00-{thread_id}.jsonl"))
    }

    fn session_meta_at(
        thread_id: &str,
        forked_from_id: Option<&str>,
        timestamp: &str,
    ) -> JsonValue {
        json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "forked_from_id": forked_from_id,
            }
        })
    }

    fn session_meta(thread_id: &str) -> JsonValue {
        session_meta_at(thread_id, None, "2026-07-10T03:00:00Z")
    }

    fn turn_context(model: &str) -> JsonValue {
        json!({
            "timestamp": "2026-07-10T03:00:01Z",
            "type": "turn_context",
            "payload": { "model": model }
        })
    }

    fn token_count_total(input: u64, cached: u64, output: u64, timestamp: &str) -> JsonValue {
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": cached,
                        "output_tokens": output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": input + output
                    }
                }
            }
        })
    }

    fn token_count_with_last(
        total_input: u64,
        total_cached: u64,
        total_output: u64,
        last_input: u64,
        last_cached: u64,
        last_output: u64,
        limit_id: &str,
        timestamp: &str,
    ) -> JsonValue {
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": total_input,
                        "cached_input_tokens": total_cached,
                        "output_tokens": total_output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": total_input + total_output
                    },
                    "last_token_usage": {
                        "input_tokens": last_input,
                        "cached_input_tokens": last_cached,
                        "output_tokens": last_output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": last_input + last_output
                    }
                },
                "rate_limits": { "limit_id": limit_id }
            }
        })
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), unique));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn nonzero_deltas(parsed: &ParsedCodexFile) -> Vec<(u64, u64, u64)> {
        parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| {
                (
                    event.delta.input,
                    event.delta.cached_input,
                    event.delta.output,
                )
            })
            .collect()
    }

    #[test]
    fn normalize_model_strips_provider_and_date() {
        assert_eq!(
            normalize_codex_model("OpenAI/GPT-5.4-2026-03-05"),
            "gpt-5.4"
        );
        assert_eq!(normalize_codex_model("gpt-5.4-20260305"), "gpt-5.4");
    }

    #[test]
    fn prefers_last_token_usage_and_skips_duplicate_snapshots() {
        let dir = make_temp_dir("codex-usage-last");
        let file = rollout_path(&dir, PARENT_ID);
        let replay = token_count_with_last(
            87_709_262,
            83_563_008,
            240_919,
            151_258,
            147_200,
            87,
            "codex_bengalfox",
            "2026-07-10T03:00:03Z",
        );
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context("openai/gpt-5.4"),
                token_count_with_last(
                    76_780_408,
                    73_010_432,
                    243_036,
                    175_074,
                    169_728,
                    6_827,
                    "codex",
                    "2026-07-10T03:00:02Z",
                ),
                replay.clone(),
                token_count_with_last(
                    76_962_538,
                    73_180_160,
                    243_258,
                    182_130,
                    169_728,
                    222,
                    "codex",
                    "2026-07-10T03:00:04Z",
                ),
                replay,
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string())).unwrap();
        assert_eq!(
            nonzero_deltas(&parsed),
            vec![
                (175_074, 169_728, 6_827),
                (151_258, 147_200, 87),
                (182_130, 169_728, 222),
            ]
        );
        assert!(parsed.token_events[3].delta.is_zero());
        assert_eq!(parsed.token_events[0].model, "gpt-5.4");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cross_limit_identical_snapshot_is_not_double_counted() {
        let dir = make_temp_dir("codex-usage-cross-limit");
        let file = rollout_path(&dir, PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context("gpt-5.4"),
                token_count_with_last(1_000, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
                token_count_with_last(
                    1_000,
                    0,
                    10,
                    100,
                    0,
                    10,
                    "codex_bengalfox",
                    "2026-07-10T03:00:03Z",
                ),
            ],
        );
        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string())).unwrap();
        assert_eq!(nonzero_deltas(&parsed), vec![(100, 0, 10)]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn falls_back_to_cumulative_delta_when_last_missing() {
        let dir = make_temp_dir("codex-usage-delta");
        let file = rollout_path(&dir, PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context("gpt-5.4"),
                token_count_total(17_934, 9_600, 454, "2026-07-10T03:00:02Z"),
                token_count_total(36_722, 27_904, 804, "2026-07-10T03:00:03Z"),
                token_count_total(36_722, 27_904, 804, "2026-07-10T03:00:04Z"),
            ],
        );
        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string())).unwrap();
        assert_eq!(
            nonzero_deltas(&parsed),
            vec![(17_934, 9_600, 454), (18_788, 18_304, 350)]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spawned_agent_usage_does_not_require_its_owner_timeline() {
        let dir = make_temp_dir("codex-usage-spawn");
        let home = dir.join("home");
        let child = home
            .join("sessions")
            .join(format!("rollout-{CHILD_ID}.jsonl"));
        let mut meta = session_meta(CHILD_ID);
        meta["payload"]["source"] =
            json!({"subagent": {"thread_spawn": {"parent_thread_id": PARENT_ID}}});
        write_jsonl(
            &child,
            &[
                meta,
                token_count_with_last(100, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
            ],
        );
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        let instances = vec![UsageInstance {
            id: DEFAULT_INSTANCE_ID.into(),
            name: DEFAULT_INSTANCE_NAME.into(),
            data_dir: home,
        }];
        let first = store.sync(false, &instances).unwrap();
        assert_eq!(first.deferred_files, 0);
        assert_eq!(first.imported, 1);
        let second = store.sync(false, &instances).unwrap();
        assert_eq!(second.imported, 0);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(report.totals.input_tokens, 100);
        assert_eq!(report.totals.output_tokens, 10);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_fork_without_source_remains_deferred() {
        let dir = make_temp_dir("codex-usage-missing-fork-source");
        let home = dir.join("home");
        write_jsonl(
            &home
                .join("sessions")
                .join(format!("rollout-{CHILD_ID}.jsonl")),
            &[
                session_meta_at(CHILD_ID, Some(PARENT_ID), "2026-07-10T03:10:00Z"),
                token_count_with_last(100, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:10:02Z"),
            ],
        );
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        let instances = [UsageInstance {
            id: DEFAULT_INSTANCE_ID.into(),
            name: DEFAULT_INSTANCE_NAME.into(),
            data_dir: home,
        }];
        let result = store.sync(false, &instances).unwrap();
        assert_eq!(result.deferred_files, 1);
        assert_eq!(result.imported, 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_fork_source_is_independent_of_spawn_owner() {
        let payload = json!({
            "forked_from_id": PARENT_ID,
            "source": {"subagent": {"thread_spawn": {"parent_thread_id": CHILD_ID}}}
        });
        assert_eq!(
            replay_parent_from_meta(&payload).unwrap().as_deref(),
            Some(PARENT_ID)
        );
    }

    #[test]
    fn parent_fork_replay_is_skipped_and_only_new_usage_is_stored() {
        let dir = make_temp_dir("codex-usage-fork");
        let db_path = dir.join("usage.sqlite");
        let home = dir.join("home");
        let sessions = home.join("sessions").join("2026").join("07").join("10");
        let parent = sessions.join(format!("rollout-2026-07-10T03-00-00-{PARENT_ID}.jsonl"));
        let child = sessions.join(format!("rollout-2026-07-10T03-10-00-{CHILD_ID}.jsonl"));
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                turn_context("gpt-5.4"),
                token_count_with_last(1_000, 0, 20, 1_000, 0, 20, "codex", "2026-07-10T03:00:02Z"),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_ID, Some(PARENT_ID), "2026-07-10T03:10:00Z"),
                turn_context("gpt-5.4"),
                token_count_with_last(1_000, 0, 20, 1_000, 0, 20, "codex", "2026-07-10T03:00:02Z"),
                token_count_with_last(1_400, 0, 35, 400, 0, 15, "codex", "2026-07-10T03:10:05Z"),
            ],
        );

        let store = SessionUsageStore::open_path(db_path);
        let instances = vec![UsageInstance {
            id: DEFAULT_INSTANCE_ID.to_string(),
            name: DEFAULT_INSTANCE_NAME.to_string(),
            data_dir: home,
        }];
        let sync = store.sync(false, &instances).unwrap();
        assert_eq!(sync.imported, 2);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(report.totals.input_tokens, 1_400);
        assert_eq!(report.totals.output_tokens, 35);
        assert_eq!(report.totals.request_count, 2);
        assert_eq!(report.by_model.len(), 1);
        assert_eq!(report.by_model[0].key, "gpt-5.4");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_sync_does_not_double_count_unchanged_files() {
        let dir = make_temp_dir("codex-usage-incr");
        let db_path = dir.join("usage.sqlite");
        let home = dir.join("home");
        let file = home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("10")
            .join(format!("rollout-2026-07-10T03-00-00-{PARENT_ID}.jsonl"));
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context("gpt-5.4"),
                token_count_with_last(100, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
            ],
        );
        let store = SessionUsageStore::open_path(db_path);
        let instances = vec![UsageInstance {
            id: DEFAULT_INSTANCE_ID.to_string(),
            name: DEFAULT_INSTANCE_NAME.to_string(),
            data_dir: home,
        }];
        assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
        assert_eq!(store.sync(false, &instances).unwrap().imported, 0);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(report.totals.request_count, 1);
        assert_eq!(report.totals.input_tokens, 100);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn append_and_rebuild_update_report_and_session_tokens() {
        let dir = make_temp_dir("codex-usage-append");
        let home = dir.join("home");
        let file = home
            .join("sessions")
            .join(format!("rollout-{PARENT_ID}.jsonl"));
        let first = token_count_total(100, 20, 10, "2026-07-10T03:00:02Z");
        let second = token_count_total(150, 30, 15, "2026-07-10T03:00:03Z");
        write_jsonl(&file, &[session_meta(PARENT_ID), first.clone()]);
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        let instances = vec![UsageInstance {
            id: "remote".into(),
            name: "Remote".into(),
            data_dir: home,
        }];

        assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
        assert_eq!(store.sync(false, &instances).unwrap().imported, 0);
        write_jsonl(&file, &[session_meta(PARENT_ID), first, second]);
        assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(report.totals.input_tokens, 150);
        assert_eq!(report.totals.request_count, 2);
        let tokens = report.session_tokens.as_ref().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].input_tokens, 150);
        assert_eq!(tokens[0].total_tokens, 165);
        let encoded = serde_json::to_string(&report).unwrap();
        let decoded: CodexSessionUsageReport = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.by_instance[0].key, "remote");
        assert_eq!(decoded.session_tokens.unwrap()[0].session_id, PARENT_ID);

        fs::remove_file(file).unwrap();
        assert_eq!(store.sync(true, &instances).unwrap().imported, 0);
        let empty = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(empty.totals.input_tokens, 0);
        assert!(empty.session_tokens.unwrap().is_empty());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn continuations_carry_counters_deduplicate_and_survive_archiving() {
        const SEGMENT_ONE: &str = "10000000-0000-4000-8000-000000000001";
        const SEGMENT_TWO: &str = "10000000-0000-4000-8000-000000000002";
        const SEGMENT_THREE: &str = "10000000-0000-4000-8000-000000000003";
        let dir = make_temp_dir("codex-usage-segments");
        let home = dir.join("home");
        let sessions = home.join("sessions").join("2026").join("07").join("10");
        let base = sessions.join(format!("rollout-2026-07-10T03-00-00-{PARENT_ID}.jsonl"));
        let first = sessions.join(format!(
            "rollout-2026-07-10T03-10-00-{PARENT_ID}_{SEGMENT_ONE}.jsonl"
        ));
        let second = sessions.join(format!(
            "rollout-2026-07-10T03-20-00-{PARENT_ID}_{SEGMENT_TWO}.jsonl"
        ));
        let third = sessions.join(format!(
            "rollout-2026-07-10T03-30-00-{PARENT_ID}_{SEGMENT_THREE}.jsonl"
        ));
        write_jsonl(
            &base,
            &[
                session_meta(PARENT_ID),
                turn_context("gpt-5.4"),
                token_count_with_last(100, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
            ],
        );
        write_jsonl(
            &first,
            &[
                session_meta_at(PARENT_ID, None, "2026-07-10T03:10:00Z"),
                token_count_with_last(100, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:10:01Z"),
                token_count_with_last(140, 0, 14, 40, 0, 4, "codex", "2026-07-10T03:10:02Z"),
            ],
        );
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        let instances = vec![UsageInstance {
            id: "remote".into(),
            name: "Remote".into(),
            data_dir: home.clone(),
        }];
        assert_eq!(store.sync(false, &instances).unwrap().imported, 2);
        assert_eq!(store.sync(false, &instances).unwrap().imported, 0);

        write_jsonl(
            &second,
            &[
                session_meta_at(PARENT_ID, None, "2026-07-10T03:20:00Z"),
                token_count_total(140, 0, 14, "2026-07-10T03:20:01Z"),
                token_count_total(200, 0, 20, "2026-07-10T03:20:02Z"),
            ],
        );
        assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(
            (
                report.totals.input_tokens,
                report.totals.output_tokens,
                report.totals.request_count
            ),
            (200, 20, 3)
        );
        assert_eq!(report.session_tokens.as_ref().unwrap().len(), 1);
        assert_eq!(report.session_tokens.unwrap()[0].session_id, PARENT_ID);

        let archived = home
            .join("archived_sessions")
            .join(first.file_name().unwrap());
        fs::create_dir_all(archived.parent().unwrap()).unwrap();
        fs::rename(first, archived).unwrap();
        assert_eq!(store.sync(false, &instances).unwrap().imported, 0);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(report.totals.input_tokens, 200);
        assert_eq!(report.files_tracked, 3);

        write_jsonl(
            &third,
            &[
                session_meta_at(PARENT_ID, None, "2026-07-10T03:30:00Z"),
                token_count_total(30, 0, 3, "2026-07-10T03:30:01Z"),
            ],
        );
        assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(
            (report.totals.input_tokens, report.totals.output_tokens),
            (230, 23)
        );
        let card = &report.session_tokens.as_ref().unwrap()[0];
        assert_eq!(card.input_tokens, report.totals.input_tokens);
        assert_eq!(card.output_tokens, report.totals.output_tokens);
        assert_eq!(card.total_tokens, 253);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn fork_replay_matches_parent_across_segments() {
        const SEGMENT: &str = "10000000-0000-4000-8000-000000000004";
        let dir = make_temp_dir("codex-usage-segmented-parent");
        let home = dir.join("home");
        let sessions = home.join("sessions").join("2026").join("07").join("10");
        let parent = sessions.join(format!("rollout-2026-07-10T03-00-00-{PARENT_ID}.jsonl"));
        let continuation = sessions.join(format!(
            "rollout-2026-07-10T03-10-00-{PARENT_ID}_{SEGMENT}.jsonl"
        ));
        let child = sessions.join(format!("rollout-2026-07-10T03-30-00-{CHILD_ID}.jsonl"));
        let first = token_count_with_last(100, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z");
        let second = token_count_with_last(150, 0, 15, 50, 0, 5, "codex", "2026-07-10T03:10:02Z");
        write_jsonl(&parent, &[session_meta(PARENT_ID), first.clone()]);
        write_jsonl(
            &continuation,
            &[
                session_meta_at(PARENT_ID, None, "2026-07-10T03:10:00Z"),
                second.clone(),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_ID, Some(PARENT_ID), "2026-07-10T03:30:00Z"),
                first,
                second,
                token_count_with_last(170, 0, 17, 20, 0, 2, "codex", "2026-07-10T03:30:02Z"),
            ],
        );
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        let instances = vec![UsageInstance {
            id: "remote".into(),
            name: "Remote".into(),
            data_dir: home,
        }];
        let sync = store.sync(false, &instances).unwrap();
        assert_eq!(sync.deferred_files, 0);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(
            (
                report.totals.input_tokens,
                report.totals.output_tokens,
                report.totals.request_count
            ),
            (170, 17, 3)
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn mismatched_meta_id_is_deferred_without_replacing_usage() {
        const SEGMENT: &str = "10000000-0000-4000-8000-000000000005";
        let dir = make_temp_dir("codex-usage-mismatched-meta");
        let home = dir.join("home");
        let base = home
            .join("sessions")
            .join(format!("rollout-2026-07-10T03-00-00-{PARENT_ID}.jsonl"));
        let continuation = home.join("sessions").join(format!(
            "rollout-2026-07-10T03-10-00-{PARENT_ID}_{SEGMENT}.jsonl"
        ));
        write_jsonl(
            &base,
            &[
                session_meta(PARENT_ID),
                token_count_total(100, 0, 10, "2026-07-10T03:00:02Z"),
            ],
        );
        write_jsonl(
            &continuation,
            &[
                session_meta_at(PARENT_ID, None, "2026-07-10T03:10:00Z"),
                token_count_total(150, 0, 15, "2026-07-10T03:10:02Z"),
            ],
        );
        let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
        let instances = vec![UsageInstance {
            id: "remote".into(),
            name: "Remote".into(),
            data_dir: home,
        }];
        store.sync(false, &instances).unwrap();
        assert_eq!(
            store
                .query(&CodexSessionUsageQuery::default(), &instances)
                .unwrap()
                .totals
                .input_tokens,
            150
        );
        store
            .open_conn()
            .unwrap()
            .execute(
                "INSERT INTO session_token_stats VALUES (?1, 1, 1, 2)",
                params![SEGMENT],
            )
            .unwrap();

        write_jsonl(
            &continuation,
            &[
                session_meta_at(CHILD_ID, None, "2026-07-10T03:10:00Z"),
                token_count_total(250, 0, 25, "2026-07-10T03:10:02Z"),
            ],
        );
        assert!(store.sync(false, &instances).unwrap().deferred_files > 0);
        assert_eq!(
            store
                .query(&CodexSessionUsageQuery::default(), &instances)
                .unwrap()
                .totals
                .input_tokens,
            150
        );
        let legacy_stat: i64 = store
            .open_conn()
            .unwrap()
            .query_row(
                "SELECT input_tokens FROM session_token_stats WHERE session_id = ?1",
                params![SEGMENT],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_stat, 1);
        fs::remove_dir_all(dir).ok();
    }
}
