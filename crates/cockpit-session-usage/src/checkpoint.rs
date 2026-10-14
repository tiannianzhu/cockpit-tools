// Parser checkpoints and events commit in the same transaction. Offsets refer to
// decoded bytes so representation switches preserve the logical rollout.
static LEDGER_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const PARSER_CHECKPOINT_VERSION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ParserCheckpoint {
    version: u32,
    ledger: String,
    byte_offset: u64,
    line_offset: i64,
    root_meta_seen: bool,
    root_timestamp: Option<DateTime<Utc>>,
    parent: ParentResolution,
    parent_id: Option<String>,
    deferred_reason: Option<String>,
    is_guardian: bool,
    guardian_model: Option<String>,
    guardian_prefix_end: Option<u32>,
    guardian_repair_complete: bool,
    boundary_pending: bool,
    state: TokenParseState,
    has_billable_tokens: bool,
    replay_matched: usize,
    replay_segment: usize,
    replay_closed: bool,
    anchor: String,
    complete: bool,
}
impl Default for ParserCheckpoint {
    fn default() -> Self {
        Self {
            version: PARSER_CHECKPOINT_VERSION,
            ledger: format!(
                "{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                LEDGER_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            byte_offset: 0,
            line_offset: 0,
            root_meta_seen: false,
            root_timestamp: None,
            parent: ParentResolution::None,
            parent_id: None,
            deferred_reason: None,
            is_guardian: false,
            guardian_model: None,
            guardian_prefix_end: None,
            guardian_repair_complete: false,
            boundary_pending: true,
            state: TokenParseState::default(),
            has_billable_tokens: false,
            replay_matched: 0,
            replay_segment: 0,
            replay_closed: false,
            anchor: String::new(),
            complete: false,
        }
    }
}
fn ensure_checkpoint_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS session_response_ids(ledger TEXT NOT NULL,response_id TEXT NOT NULL,PRIMARY KEY(ledger,response_id));
        CREATE TABLE IF NOT EXISTS session_parser_checkpoints(file_path TEXT PRIMARY KEY,state TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS session_parent_sync(file_path TEXT PRIMARY KEY,state TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS session_parent_signatures(file_path TEXT NOT NULL,generation INTEGER NOT NULL,line INTEGER NOT NULL,timestamp INTEGER NOT NULL,signature TEXT NOT NULL,PRIMARY KEY(file_path,generation,line));
        CREATE INDEX IF NOT EXISTS idx_session_checkpoint_ledger ON session_parser_checkpoints(json_extract(state,'$.ledger'));
        CREATE INDEX IF NOT EXISTS idx_session_parent_ledger ON session_parent_sync(json_extract(state,'$.parser.ledger'));
        CREATE INDEX IF NOT EXISTS idx_session_parent_match ON session_parent_signatures(file_path,generation,signature,line);")
        .map_err(|e|e.to_string())
}
#[derive(Clone, PartialEq, Eq)]
struct ReaderStamp {
    modified: i64,
    size: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}
impl ReaderStamp {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            modified: metadata_modified_nanos(metadata),
            size: metadata.len(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
    fn same_file(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode
        }
        #[cfg(not(unix))]
        {
            self.modified == other.modified && self.size == other.size
        }
    }
}
struct CheckpointReader {
    reader: Box<dyn BufRead + Send>,
    handle: File,
    stamp: ReaderStamp,
    physical: PathBuf,
}
impl std::ops::Deref for CheckpointReader {
    type Target = dyn BufRead + Send;
    fn deref(&self) -> &Self::Target {
        &*self.reader
    }
}
impl std::ops::DerefMut for CheckpointReader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.reader
    }
}
struct CachedDecodedReader {
    offset: u64,
    opened: CheckpointReader,
}
static DECODED_READERS: OnceLock<Mutex<HashMap<PathBuf, CachedDecodedReader>>> = OnceLock::new();
fn decoded_readers() -> &'static Mutex<HashMap<PathBuf, CachedDecodedReader>> {
    DECODED_READERS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn cache_checkpoint_reader(_path: &Path, offset: u64, opened: CheckpointReader) {
    if !rollout::is_compressed(&opened.physical) {
        return;
    }
    if let Ok(mut cache) = decoded_readers().lock() {
        if cache.len() >= 8 {
            cache.clear();
        }
        cache.insert(
            opened.physical.clone(),
            CachedDecodedReader { offset, opened },
        );
    }
}
fn open_checkpoint_reader(path: &Path, offset: u64) -> Result<CheckpointReader, String> {
    let physical = rollout::resolve_path(path).map_err(|e| e.to_string())?;
    if let Ok(mut cache) = decoded_readers().lock() {
        if let Some(cached) = cache.remove(&physical) {
            let current = fs::metadata(&physical).map_err(|e| e.to_string())?;
            if cached.offset == offset
                && cached.opened.stamp == ReaderStamp::from_metadata(&current)
            {
                return Ok(cached.opened);
            }
        }
    }
    let mut file = File::open(&physical).map_err(|e| e.to_string())?;
    let stamp = ReaderStamp::from_metadata(&file.metadata().map_err(|e| e.to_string())?);
    let handle = file.try_clone().map_err(|e| e.to_string())?;
    let reader: Box<dyn BufRead + Send> = if rollout::is_compressed(&physical) {
        let mut decoded =
            BufReader::new(zstd::stream::read::Decoder::new(file).map_err(|e| e.to_string())?);
        let copied = std::io::copy(&mut decoded.by_ref().take(offset), &mut std::io::sink())
            .map_err(|e| e.to_string())?;
        if copied != offset {
            return Err("checkpoint exceeds decoded rollout size".into());
        }
        Box::new(decoded)
    } else {
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        Box::new(BufReader::new(file))
    };
    Ok(CheckpointReader {
        reader,
        handle,
        stamp,
        physical,
    })
}
// Anchor descriptors are opened independently and identity checked before use.
// Never seek on the retained identity handle shared with an active decoder.
fn handle_anchor(file: &mut File, offset: u64, compressed: bool) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let original = file.stream_position().map_err(|e| e.to_string())?;
    let result = (|| {
        let offset = if compressed {
            file.metadata().map_err(|e| e.to_string())?.len()
        } else {
            offset
        };
        let len = offset.min(1024) as usize;
        let mut bytes = [0u8; 1024];
        let mut hash = Sha256::new();
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        file.read_exact(&mut bytes[..len])
            .map_err(|e| e.to_string())?;
        hash.update(&bytes[..len]);
        file.seek(SeekFrom::Start(offset - len as u64))
            .map_err(|e| e.to_string())?;
        file.read_exact(&mut bytes[..len])
            .map_err(|e| e.to_string())?;
        hash.update(&bytes[..len]);
        Ok(format!(
            "{}{:x}",
            if compressed { "zst:" } else { "" },
            hash.finalize()
        ))
    })();
    file.seek(SeekFrom::Start(original))
        .map_err(|e| e.to_string())?;
    result
}
fn checkpoint_anchor(path: &Path, offset: u64) -> Result<String, String> {
    let physical = rollout::resolve_path(path).map_err(|e| e.to_string())?;
    let mut file = File::open(&physical).map_err(|e| e.to_string())?;
    handle_anchor(&mut file, offset, rollout::is_compressed(&physical))
}
fn finish_checkpoint_reader(
    path: &Path,
    offset: u64,
    reader: &mut CheckpointReader,
) -> Result<String, String> {
    let current = rollout::resolve_path(path).map_err(|e| e.to_string())?;
    let stamp = ReaderStamp::from_metadata(&fs::metadata(&current).map_err(|e| e.to_string())?);
    let compressed = rollout::is_compressed(&reader.physical);
    if current != reader.physical
        || !reader.stamp.same_file(&stamp)
        || (compressed && stamp != reader.stamp)
        || stamp.size < reader.stamp.size
    {
        return Err("rollout changed while parsing batch".into());
    }
    let mut probe = File::open(&reader.physical).map_err(|e| e.to_string())?;
    let probe_stamp = ReaderStamp::from_metadata(&probe.metadata().map_err(|e| e.to_string())?);
    let opened_stamp =
        ReaderStamp::from_metadata(&reader.handle.metadata().map_err(|e| e.to_string())?);
    if !reader.stamp.same_file(&probe_stamp) || !reader.stamp.same_file(&opened_stamp) {
        return Err("rollout changed while opening anchor descriptor".into());
    }
    let anchor = handle_anchor(&mut probe, offset, compressed)?;
    if checkpoint_anchor(path, offset)? != anchor {
        return Err("rollout anchor changed while parsing batch".into());
    }
    Ok(anchor)
}
fn load_parser_checkpoint(
    conn: &Connection,
    path: &Path,
    size: u64,
    previous_size: i64,
) -> Result<Option<ParserCheckpoint>, String> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT state FROM session_parser_checkpoints WHERE file_path=?1",
            [rollout::logical_path(path).to_string_lossy().as_ref()],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(raw
        .and_then(|v| serde_json::from_str::<ParserCheckpoint>(&v).ok())
        .filter(|c| {
            c.version == PARSER_CHECKPOINT_VERSION
                && size >= previous_size.max(0) as u64
                && checkpoint_anchor(path, c.byte_offset).is_ok_and(|a| a == c.anchor)
        }))
}
fn save_parser_checkpoint(
    conn: &Connection,
    path: &Path,
    state: &ParserCheckpoint,
) -> Result<(), String> {
    let persisted = persist_response_ids(conn, state)?;
    let json = serde_json::to_string(&persisted).map_err(|e| e.to_string())?;
    conn.execute("INSERT INTO session_parser_checkpoints VALUES(?1,?2) ON CONFLICT(file_path) DO UPDATE SET state=excluded.state",params![rollout::logical_path(path).to_string_lossy().as_ref(),json]).map_err(|e|e.to_string())?;
    Ok(())
}
// A complete JSON record is the atomic parsing unit. Byte/time limits are
// soft scheduling budgets checked between records, never record-size limits.
fn read_usage_line(reader: &mut dyn BufRead) -> Result<Option<(String, u64, bool)>, String> {
    let mut bytes = Vec::new();
    let count = reader
        .read_until(b'\n', &mut bytes)
        .map_err(|e| e.to_string())?;
    if count == 0 {
        return Ok(None);
    }
    let terminated = bytes.last() == Some(&b'\n');
    Ok(Some((
        String::from_utf8(bytes).map_err(|e| e.to_string())?,
        count as u64,
        terminated,
    )))
}

mod signature_map {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &HashMap<Option<String>, TokenUsageSignature>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.iter().collect::<Vec<_>>().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<Option<String>, TokenUsageSignature>, D::Error> {
        Ok(
            Vec::<(Option<String>, TokenUsageSignature)>::deserialize(deserializer)?
                .into_iter()
                .collect(),
        )
    }
}

fn persist_response_ids(
    conn: &Connection,
    state: &ParserCheckpoint,
) -> Result<ParserCheckpoint, String> {
    for id in &state.state.usage_record_ids {
        conn.execute(
            "INSERT OR IGNORE INTO session_response_ids VALUES(?1,?2)",
            params![state.ledger, id],
        )
        .map_err(|e| e.to_string())?;
    }
    let mut persisted = state.clone();
    persisted.state.usage_record_ids.clear();
    Ok(persisted)
}

fn cleanup_response_ids_batch(conn: &Connection) -> Result<bool, String> {
    let cursor = get_meta_i64(conn, "response_gc_cursor").unwrap_or(0);
    let next:Option<i64>=conn.query_row("SELECT MAX(id) FROM (SELECT rowid AS id FROM session_response_ids WHERE rowid>?1 ORDER BY rowid LIMIT 32)",[cursor],|r|r.get(0)).map_err(|e|e.to_string())?;
    let Some(next) = next else {
        set_meta(conn, "response_gc_cursor", "0")?;
        return Ok(true);
    };
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM session_response_ids WHERE rowid>?1 AND rowid<=?2 AND NOT EXISTS(SELECT 1 FROM session_parser_checkpoints c WHERE json_extract(c.state,'$.ledger')=session_response_ids.ledger) AND NOT EXISTS(SELECT 1 FROM session_parent_sync p WHERE json_extract(p.state,'$.parser.ledger')=session_response_ids.ledger) AND NOT EXISTS(SELECT 1 FROM session_group_sync g WHERE json_extract(g.state,'$.parser.ledger')=session_response_ids.ledger)",params![cursor,next]).map_err(|e|e.to_string())?;
    set_meta(&tx, "response_gc_cursor", &next.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(false)
}

fn backfill_guardian_batch(
    conn: &Connection,
    table: &str,
    path: &str,
    prefix: &str,
    state: &mut ParserCheckpoint,
) -> Result<bool, String> {
    let Some((model, end)) = state.guardian_model.as_ref().zip(state.guardian_prefix_end) else {
        return Ok(false);
    };
    if !state.is_guardian || state.guardian_repair_complete {
        return Ok(false);
    }
    let condition="file_path=?1 AND model='unknown' AND substr(request_id,1,length(?2))=?2 AND CAST(substr(request_id,length(?2)+1) AS INTEGER)<=?3";
    conn.execute(&format!("UPDATE {table} SET model=?4 WHERE rowid IN(SELECT rowid FROM {table} WHERE {condition} LIMIT 32)"),params![path,prefix,end,model]).map_err(|e|e.to_string())?;
    let pending: bool = conn
        .query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {condition})"),
            params![path, prefix, end],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    state.guardian_repair_complete = !pending;
    Ok(pending)
}
