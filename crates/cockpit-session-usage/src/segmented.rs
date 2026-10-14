// Same-root continuations share counters and deduplication state. Parse into an
// on-disk staging table, then publish the complete logical group through the
// same bounded rewrite snapshots used by ordinary files.
#[derive(Default, Clone, Serialize, Deserialize)]
struct GroupCheckpoint {
    stamps: Vec<(String, i64, u64)>,
    paths: Vec<String>,
    segment: usize,
    parser: ParserCheckpoint,
    phase: u8,
    copy_cursor: i64,
}
fn ensure_group_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS session_group_sync(session_id TEXT PRIMARY KEY,state TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS session_group_events(request_id TEXT PRIMARY KEY,instance_id TEXT NOT NULL,instance_name TEXT NOT NULL,session_id TEXT NOT NULL,model TEXT NOT NULL,service_tier TEXT,timestamp INTEGER NOT NULL,input_tokens INTEGER NOT NULL,cached_input_tokens INTEGER NOT NULL,output_tokens INTEGER NOT NULL,file_path TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_session_group_ledger ON session_group_sync(json_extract(state,'$.parser.ledger'));
        CREATE INDEX IF NOT EXISTS idx_session_group_events ON session_group_events(session_id);")
        .map_err(|e|e.to_string())
}
fn save_group(conn: &Connection, root: &str, state: &GroupCheckpoint) -> Result<(), String> {
    let mut state = state.clone();
    state.parser = persist_response_ids(conn, &state.parser)?;
    conn.execute("INSERT INTO session_group_sync VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET state=excluded.state",params![root,serde_json::to_string(&state).map_err(|e|e.to_string())?]).map_err(|e|e.to_string())?;
    Ok(())
}
fn sync_segmented_group(
    conn: &mut Connection,
    instance: &UsageInstance,
    root_id: &str,
    paths: &[PathBuf],
    rollout_index: &RolloutIndex,
    cursors: &mut HashMap<String, (i64, i64, i64)>,
) -> Result<FileSyncResult, String> {
    let selected = select_rollout_copies(paths)?;
    let stamps = selected
        .iter()
        .map(|p| {
            let m = fs::metadata(p).map_err(|e| e.to_string())?;
            Ok((
                rollout::logical_path(p).to_string_lossy().into_owned(),
                metadata_modified_nanos(&m),
                m.len(),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let raw: Option<String> = conn
        .query_row(
            "SELECT state FROM session_group_sync WHERE session_id=?1",
            [root_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let saved = raw.and_then(|v| serde_json::from_str::<GroupCheckpoint>(&v).ok());
    let unchanged = stamps.iter().all(|(p, m, n)| {
        cursors
            .get(p)
            .is_some_and(|(oldm, oldn, line)| *m == *oldm && *n == *oldn as u64 && *line > 0)
    });
    let has_stats: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_token_stats WHERE session_id=?1)",
            [root_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if saved.is_none() && unchanged && has_stats {
        return Ok(FileSyncResult::default());
    }
    let mut state = if let Some(saved) = saved.as_ref().filter(|s| s.stamps == stamps) {
        saved.clone()
    } else {
        // Reclaim an interrupted/obsolete stage before recording a new manifest.
        let removed=conn.execute("DELETE FROM session_group_events WHERE rowid IN(SELECT rowid FROM session_group_events WHERE session_id=?1 LIMIT 32)",[root_id]).map_err(|e|e.to_string())?;
        if removed > 0 {
            return Ok(FileSyncResult {
                more: true,
                ..Default::default()
            });
        }
        let mut names = stamps
            .iter()
            .filter_map(|(p, _, _)| logical_rollout_name(Path::new(p)))
            .collect::<HashSet<_>>();
        let mut old = saved.map(|s| s.paths).unwrap_or_default();
        let mut stmt=conn.prepare("SELECT DISTINCT file_path FROM session_usage_events WHERE session_id=?1 UNION SELECT file_path FROM session_log_sync").map_err(|e|e.to_string())?;
        for p in stmt
            .query_map([root_id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
        {
            let p = p.map_err(|e| e.to_string())?;
            if logical_rollout_name(Path::new(&p)).is_some_and(|n| names.contains(&n)) {
                old.push(p)
            }
        }
        for (p, _, _) in &stamps {
            if !old.contains(p) {
                old.push(p.clone())
            }
        }
        names.clear();
        GroupCheckpoint {
            stamps: stamps.clone(),
            paths: old,
            ..Default::default()
        }
    };
    let mut result = FileSyncResult {
        changed: true,
        more: true,
        ..Default::default()
    };
    match state.phase {
        0 => {
            let Some(path) = selected.get(state.segment) else {
                let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
                for path in &state.paths {
                    tx.execute(
                        "UPDATE session_rewrite_state SET phase=1 WHERE file_path=?1 AND phase=2",
                        [path],
                    )
                    .map_err(|e| e.to_string())?;
                }
                state.phase = 1;
                save_group(&tx, root_id, &state)?;
                tx.commit().map_err(|e| e.to_string())?;
                return Ok(result);
            };
            let prior = state.parser.byte_offset;
            let mut parsed = parse_codex_file_batch_on_conn(
                conn,
                path,
                Some(root_id.to_owned()),
                Some(state.parser.clone()),
            )?;
            if parsed.has_billable_tokens
                && (!parsed.root_meta_seen || parsed.parent == ParentResolution::Deferred)
            {
                return Ok(FileSyncResult {
                    deferred: true,
                    ..Default::default()
                });
            }
            let mut replay = 0;
            if !parsed.checkpoint.replay_closed {
                let parent = match parsed.parent {
                    ParentResolution::Parent => parsed
                        .parent_id
                        .as_deref()
                        .zip(parsed.root_timestamp)
                        .map(|(p, t)| (p.to_owned(), t, None)),
                    ParentResolution::None if state.segment > 0 => parsed
                        .root_timestamp
                        .map(|t| (root_id.to_owned(), t, Some(state.segment))),
                    _ => None,
                };
                if let Some((parent, cutoff, earlier)) = parent {
                    let mut index = rollout_index.clone();
                    let child = if let Some(earlier) = earlier {
                        index.insert(root_id.to_owned(), selected[..earlier].to_vec());
                        let count = parsed
                            .token_events
                            .iter()
                            .take_while(|e| e.timestamp.is_some_and(|t| t <= cutoff))
                            .count();
                        &parsed.token_events[..count]
                    } else {
                        parsed.token_events.as_slice()
                    };
                    if !child.is_empty() {
                        match resolve_parent_replay(
                            conn,
                            &parent,
                            cutoff,
                            &index,
                            child,
                            &mut parsed.checkpoint,
                        ) {
                            Ok(ParentReplay::Pending) => return Ok(result),
                            Ok(ParentReplay::Matched(n)) => replay = n,
                            Err(_) if earlier.is_some() => {}
                            Err(_) => {
                                return Ok(FileSyncResult {
                                    deferred: true,
                                    ..Default::default()
                                })
                            }
                        }
                    }
                }
                parsed.checkpoint.replay_closed |= replay < parsed.token_events.len();
            }
            let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
            let name = logical_rollout_name(path).ok_or("invalid segment filename")?;
            for (offset, event) in parsed.token_events.iter().enumerate() {
                let Some(index) = event.event_index else {
                    continue;
                };
                if offset < replay {
                    continue;
                }
                let id = format!("{SEGMENT_REQUEST_ID_PREFIX}:{root_id}:{name}:{index}");
                let existed: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM session_usage_events WHERE request_id=?1)",
                        [&id],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                tx.execute("INSERT OR REPLACE INTO session_group_events VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![id,instance.id,instance.name,root_id,event.model,event.service_tier,event.timestamp.map(|t|t.timestamp()).unwrap_or(0),event.delta.input as i64,event.delta.cached_input as i64,event.delta.output as i64,stamps[state.segment].0]).map_err(|e|e.to_string())?;
                if existed {
                    result.skipped += 1
                } else {
                    result.imported += 1
                }
            }
            let repairing = backfill_guardian_batch(
                &tx,
                "session_group_events",
                &stamps[state.segment].0,
                &format!("{SEGMENT_REQUEST_ID_PREFIX}:{root_id}:{name}:"),
                &mut parsed.checkpoint,
            )?;
            state.parser = parsed.checkpoint;
            if state.parser.complete && !repairing {
                let carry = state.parser.state.clone();
                let ledger = state.parser.ledger.clone();
                state.segment += 1;
                state.parser = ParserCheckpoint {
                    state: carry,
                    ledger,
                    ..Default::default()
                };
            } else if !repairing && state.parser.byte_offset == prior {
                // A partially written tail cannot publish a partial group.
                return Ok(FileSyncResult {
                    deferred: true,
                    ..Default::default()
                });
            }
            save_group(&tx, root_id, &state)?;
            tx.commit().map_err(|e| e.to_string())?;
        }
        1 => {
            for path in &state.paths {
                if !prepare_rewrite_batch(conn, path)? {
                    return Ok(result);
                }
            }
            state.phase = 2;
            save_group(conn, root_id, &state)?;
        }
        2 => {
            let next:Option<i64>=conn.query_row("SELECT MAX(id) FROM(SELECT rowid AS id FROM session_group_events WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT 32)",params![root_id,state.copy_cursor],|r|r.get(0)).map_err(|e|e.to_string())?;
            let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
            if let Some(next) = next {
                tx.execute("INSERT INTO session_usage_events(request_id,instance_id,instance_name,session_id,model,service_tier,timestamp,input_tokens,cached_input_tokens,output_tokens,file_path)
                    SELECT request_id,instance_id,instance_name,session_id,model,service_tier,timestamp,input_tokens,cached_input_tokens,output_tokens,file_path FROM session_group_events WHERE session_id=?1 AND rowid>?2 AND rowid<=?3",params![root_id,state.copy_cursor,next]).map_err(|e|e.to_string())?;
                state.copy_cursor = next;
            } else {
                state.phase = 3
            }
            save_group(&tx, root_id, &state)?;
            tx.commit().map_err(|e| e.to_string())?;
        }
        3 => {
            let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
            for path in &state.paths {
                publish_rewrite(&tx, path, true)?;
                tx.execute("DELETE FROM session_log_sync WHERE file_path=?1", [path])
                    .map_err(|e| e.to_string())?;
                cursors.remove(path);
            }
            for (path, modified, size) in &stamps {
                // Group completion is separate from per-file parser progress.
                upsert_cursor_on_conn(&tx, path, &instance.id, *modified, *size, 1)?;
                cursors.insert(path.clone(), (*modified, *size as i64, 1));
            }
            for path in &selected {
                if let Some(leaf) = rollout_identity(path).and_then(|i| i.segment_id) {
                    tx.execute(
                        "DELETE FROM session_token_stats WHERE session_id=?1",
                        [leaf],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            tx.execute("INSERT OR REPLACE INTO session_token_stats SELECT ?1,SUM(input_tokens),SUM(output_tokens),SUM(input_tokens+output_tokens) FROM session_usage_events WHERE session_id=?1",[root_id]).map_err(|e|e.to_string())?;
            state.phase = 4;
            save_group(&tx, root_id, &state)?;
            tx.commit().map_err(|e| e.to_string())?;
        }
        _ => {
            let removed=conn.execute("DELETE FROM session_group_events WHERE rowid IN(SELECT rowid FROM session_group_events WHERE session_id=?1 LIMIT 32)",[root_id]).map_err(|e|e.to_string())?;
            if removed == 0 {
                conn.execute(
                    "DELETE FROM session_group_sync WHERE session_id=?1",
                    [root_id],
                )
                .map_err(|e| e.to_string())?;
                result.more = false;
            }
        }
    }
    Ok(result)
}
