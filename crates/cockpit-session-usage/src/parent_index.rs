// Parent replay evidence is indexed on disk in the same bounded parser batches.
// A large parent no longer produces a whole-file Vec or an unbounded RAM cache.
#[derive(Default, Serialize, Deserialize)]
struct ParentScanState {
    generation: i64,
    modified: i64,
    size: u64,
    offset: u64,
    line: i64,
    complete: bool,
    missing_timestamp: bool,
    anchor: String,
    #[serde(default)]
    parser: ParserCheckpoint,
}

fn prepare_parent_index(conn: &Connection, path: &Path) -> Result<Option<ParentScanState>, String> {
    let path_text = path.to_string_lossy();
    let raw: Option<String> = conn
        .query_row(
            "SELECT state FROM session_parent_sync WHERE file_path=?1",
            [path_text.as_ref()],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let mut state = raw
        .and_then(|s| serde_json::from_str::<ParentScanState>(&s).ok())
        .unwrap_or_default();
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    let modified = metadata_modified_nanos(&meta);
    let size = meta.len();
    if state.complete && state.modified == modified && state.size == size {
        return Ok(Some(state));
    }
    if state.offset > 0
        && (size < state.size
            || (size == state.size && modified != state.modified)
            || !checkpoint_anchor(path, state.offset).is_ok_and(|v| v == state.anchor))
    {
        state = ParentScanState {
            generation: state.generation + 1,
            ..Default::default()
        };
    }
    let initial = state.offset;
    let parsed = parse_codex_file_batch_on_conn(
        conn,
        path,
        thread_id_from_filename(path),
        Some(state.parser.clone()),
    )?;
    let mut events = Vec::new();
    for event in parsed.token_events {
        let Some(timestamp) = event.timestamp else {
            state.missing_timestamp = true;
            continue;
        };
        events.push((
            event.line_offset,
            timestamp.timestamp_millis(),
            serde_json::to_string(&event.signature).map_err(|e| e.to_string())?,
        ));
    }
    state.parser = parsed.checkpoint;
    state.offset = state.parser.byte_offset;
    state.line = state.parser.line_offset;
    state.complete = state.parser.complete;
    state.size = size;
    state.modified = modified;
    state.anchor = checkpoint_anchor(path, state.offset)?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for (line, timestamp, signature) in events {
        tx.execute("INSERT OR REPLACE INTO session_parent_signatures(file_path,generation,line,timestamp,signature) VALUES(?1,?2,?3,?4,?5)",
            params![path_text.as_ref(),state.generation,line,timestamp,signature]).map_err(|e|e.to_string())?;
    }
    state.parser = persist_response_ids(&tx, &state.parser)?;
    tx.execute("INSERT INTO session_parent_sync(file_path,state) VALUES(?1,?2) ON CONFLICT(file_path) DO UPDATE SET state=excluded.state",
        params![path_text.as_ref(),serde_json::to_string(&state).map_err(|e|e.to_string())?]).map_err(|e|e.to_string())?;
    // Rewritten generations are reclaimed gradually, never by one huge DELETE.
    tx.execute("DELETE FROM session_parent_signatures WHERE rowid IN (SELECT rowid FROM session_parent_signatures WHERE file_path=?1 AND generation!=?2 LIMIT 32)",params![path_text.as_ref(),state.generation]).map_err(|e|e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    if !state.complete && state.offset == initial {
        return Err("父 rollout 尾部尚未写完".into());
    }
    Ok(state.complete.then_some(state))
}

enum ParentReplay {
    Pending,
    Matched(usize),
}
fn resolve_parent_replay(
    conn: &Connection,
    parent_id: &str,
    cutoff: DateTime<Utc>,
    index: &RolloutIndex,
    child: &[ParsedTokenEvent],
    checkpoint: &mut ParserCheckpoint,
) -> Result<ParentReplay, String> {
    let paths = index
        .get(parent_id)
        .ok_or_else(|| format!("找不到父 rollout: {parent_id}"))?;
    let mut snapshots = Vec::new();
    let selected = select_rollout_copies(paths)?;
    for path in &selected {
        let Some(state) = prepare_parent_index(conn, path)? else {
            return Ok(ParentReplay::Pending);
        };
        if state.missing_timestamp {
            return Err("父 rollout 的 token_count 缺少有效 timestamp".into());
        }
        let count:i64=conn.query_row("SELECT COUNT(*) FROM session_parent_signatures WHERE file_path=?1 AND generation=?2",params![path.to_string_lossy().as_ref(),state.generation],|r|r.get(0)).map_err(|e|e.to_string())?;
        if count == 0 {
            return Err("父 rollout 尚无可用 token_count".into());
        }
        snapshots.push((path, state));
    }
    let mut matched = 0;
    for event in child {
        let signature = serde_json::to_string(&event.signature).map_err(|e| e.to_string())?;
        let mut found = None;
        for (segment, (path, state)) in snapshots.iter().enumerate().skip(checkpoint.replay_segment)
        {
            let after = if segment == checkpoint.replay_segment {
                checkpoint.replay_matched as i64
            } else {
                0
            };
            let line:Option<i64>=conn.query_row("SELECT line FROM session_parent_signatures WHERE file_path=?1 AND generation=?2 AND signature=?3 AND line>?4 AND timestamp<=?5 ORDER BY line LIMIT 1",
                params![path.to_string_lossy().as_ref(),state.generation,signature,after,cutoff.timestamp_millis()],|r|r.get(0)).optional().map_err(|e|e.to_string())?;
            if let Some(line) = line {
                found = Some((segment, line));
                break;
            }
        }
        let Some((segment, line)) = found else { break };
        checkpoint.replay_segment = segment;
        checkpoint.replay_matched = line as usize;
        matched += 1;
    }
    Ok(ParentReplay::Matched(matched))
}
