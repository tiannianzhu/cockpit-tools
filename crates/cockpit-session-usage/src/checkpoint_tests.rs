#[test]
fn parser_checkpoint_resumes_append_and_retains_partial_tail() {
    use std::io::Write;
    let dir = make_temp_dir("usage-checkpoint-append");
    let file = rollout_path(&dir, PARENT_ID);
    write_jsonl(
        &file,
        &[
            session_meta(PARENT_ID),
            turn_context("gpt-5.4"),
            token_count_total(100, 20, 10, "2026-07-10T03:00:02Z"),
        ],
    );
    let first = parse_codex_file_batch(&file, Some(PARENT_ID.into()), None).unwrap();
    let saved = serde_json::to_string(&first.checkpoint).unwrap();
    let restored: ParserCheckpoint = serde_json::from_str(&saved).unwrap();
    let prior_offset = restored.byte_offset;
    let event = token_count_total(140, 30, 15, "2026-07-10T03:00:03Z").to_string() + "\n";
    let cut = event.len() / 2;
    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    append.write_all(event[..cut].as_bytes()).unwrap();
    let partial =
        parse_codex_file_batch(&file, Some(PARENT_ID.into()), Some(restored.clone())).unwrap();
    assert_eq!(partial.checkpoint.byte_offset, prior_offset);
    assert!(!partial.checkpoint.complete);
    assert!(partial.token_events.is_empty());
    append.write_all(event[cut..].as_bytes()).unwrap();
    let next = parse_codex_file_batch(&file, Some(PARENT_ID.into()), Some(restored)).unwrap();
    assert_eq!(nonzero_deltas(&next), vec![(40, 10, 5)]);
    assert_eq!(next.token_events[0].event_index, Some(2));
    assert_eq!(next.line_offset, first.line_offset + 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parser_batches_resume_without_replaying_history() {
    let dir = make_temp_dir("usage-checkpoint-batches");
    let file = rollout_path(&dir, PARENT_ID);
    let mut lines = vec![session_meta(PARENT_ID), turn_context("gpt-5.4")];
    for n in 1..=100 {
        lines.push(token_count_total(n * 10, 0, n, "2026-07-10T03:00:02Z"));
    }
    write_jsonl(&file, &lines);
    let mut checkpoint = None;
    let mut events = 0;
    let mut batches = 0;
    loop {
        let parsed = parse_codex_file_batch(&file, Some(PARENT_ID.into()), checkpoint).unwrap();
        assert!(parsed.token_events.len() <= INSERT_BATCH_SIZE);
        events += parsed.token_events.len();
        batches += 1;
        if parsed.checkpoint.complete {
            break;
        }
        checkpoint = Some(
            serde_json::from_str(&serde_json::to_string(&parsed.checkpoint).unwrap()).unwrap(),
        );
    }
    assert_eq!(events, 100);
    assert!(batches >= 4);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parent_index_is_bounded_and_replay_cursor_is_a_line_not_match_count() {
    let dir = make_temp_dir("usage-parent-index");
    let file = rollout_path(&dir, PARENT_ID);
    let child_file = rollout_path(&dir, CHILD_ID);
    let mut lines = vec![session_meta(PARENT_ID)];
    for n in 1..=100 {
        lines.push(token_count_total(n * 10, 0, n, "2026-07-10T03:00:02Z"));
    }
    write_jsonl(&file, &lines);
    write_jsonl(
        &child_file,
        &[session_meta(CHILD_ID), lines[20].clone(), lines[80].clone()],
    );
    let child = parse_codex_file(&child_file, Some(CHILD_ID.into())).unwrap();
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let conn = store.open_conn().unwrap();
    let index = build_rollout_index(&[file.clone()]);
    let cutoff = DateTime::parse_from_rfc3339("2026-07-10T04:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut checkpoint = ParserCheckpoint::default();
    assert!(matches!(
        resolve_parent_replay(
            &conn,
            PARENT_ID,
            cutoff,
            &index,
            &child.token_events[..1],
            &mut checkpoint
        )
        .unwrap(),
        ParentReplay::Pending
    ));
    loop {
        if let ParentReplay::Matched(n) = resolve_parent_replay(
            &conn,
            PARENT_ID,
            cutoff,
            &index,
            &child.token_events[..1],
            &mut checkpoint,
        )
        .unwrap()
        {
            assert_eq!(n, 1);
            break;
        }
    }
    assert_eq!(checkpoint.replay_matched, 21);
    assert!(matches!(
        resolve_parent_replay(
            &conn,
            PARENT_ID,
            cutoff,
            &index,
            &child.token_events[1..],
            &mut checkpoint
        )
        .unwrap(),
        ParentReplay::Matched(1)
    ));
    assert_eq!(checkpoint.replay_matched, 81);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn session_rollup_matches_report_before_and_after_backfill_and_append() {
    let dir = make_temp_dir("usage-rollup-report");
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let conn = store.open_conn().unwrap();
    for n in 1..=5 {
        conn.execute("INSERT INTO session_usage_events VALUES(?1,'i','Instance','s','gpt-5.4',NULL,?2,100,20,10,'f')",params![format!("r{n}"),1_780_000_000+n]).unwrap();
    }
    let query = CodexSessionUsageQuery {
        from_timestamp: Some(1_780_000_002),
        to_timestamp: Some(1_780_000_004),
        instance_id: Some("i".into()),
    };
    let before = store.query(&query, &[]).unwrap();
    while !SESSION_USAGE_ROLLUP.batch(&conn).unwrap() {}
    let after = store.query(&query, &[]).unwrap();
    assert_eq!(before.totals.input_tokens, 300);
    assert_eq!(after.totals.request_count, 3);
    assert_eq!(
        serde_json::to_value(&before.totals).unwrap(),
        serde_json::to_value(&after.totals).unwrap()
    );
    conn.execute("DELETE FROM session_usage_events WHERE request_id='r3'", [])
        .unwrap();
    assert_eq!(store.query(&query, &[]).unwrap().totals.request_count, 2);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rewritten_file_keeps_previous_report_until_atomic_publication() {
    let dir = make_temp_dir("usage-rewrite-snapshot");
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let conn = store.open_conn().unwrap();
    for n in 1..=80 {
        conn.execute("INSERT INTO session_usage_events VALUES(?1,'i','Instance','s','gpt-5.4',NULL,?2,100,20,10,'rewritten')",params![format!("r{n}"),1_780_000_000+n]).unwrap();
    }
    let query = CodexSessionUsageQuery::default();
    loop {
        let ready = prepare_rewrite_batch(&conn, "rewritten").unwrap();
        assert_eq!(store.query(&query, &[]).unwrap().totals.request_count, 80);
        if ready {
            break;
        }
    }
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM session_usage_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    conn.execute("INSERT INTO session_usage_events VALUES('r1','i','Instance','s','gpt-5.4',NULL,1780000001,200,20,10,'rewritten')",[]).unwrap();
    assert_eq!(
        store.query(&query, &[]).unwrap().totals.input_tokens,
        8000,
        "interrupted replacement must not publish partial statistics"
    );
    drop(conn);
    let conn = store.open_conn().unwrap();
    assert_eq!(store.query(&query, &[]).unwrap().totals.input_tokens, 8000);
    let tx = conn.unchecked_transaction().unwrap();
    publish_rewrite(&tx, "rewritten", true).unwrap();
    tx.rollback().unwrap();
    assert_eq!(store.query(&query, &[]).unwrap().totals.input_tokens, 8000);
    publish_rewrite(&conn, "rewritten", true).unwrap();
    assert_eq!(store.query(&query, &[]).unwrap().totals.input_tokens, 200);
    while !cleanup_rewrite_batch(&conn).unwrap() {}
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM session_rewrite_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn truncated_rollout_replaces_usage_and_next_append_remains_incremental() {
    use std::io::Write;
    let dir = make_temp_dir("usage-rewrite-integration");
    let home = dir.join("home");
    let file = rollout_path(&home.join("sessions"), PARENT_ID);
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = vec![UsageInstance {
        id: "i".into(),
        name: "Instance".into(),
        data_dir: home,
    }];
    write_jsonl(
        &file,
        &[
            session_meta(PARENT_ID),
            turn_context("gpt-5.4"),
            token_count_total(100, 0, 10, "2026-07-10T03:00:02Z"),
            token_count_total(200, 0, 20, "2026-07-10T03:00:03Z"),
        ],
    );
    assert!(store.sync(false, &instances).unwrap().errors.is_empty());
    assert_eq!(
        store
            .query(&Default::default(), &[])
            .unwrap()
            .totals
            .input_tokens,
        200
    );
    write_jsonl(
        &file,
        &[
            session_meta(PARENT_ID),
            turn_context("gpt-5.4"),
            token_count_total(50, 0, 5, "2026-07-10T03:00:02Z"),
        ],
    );
    assert!(store.sync(false, &instances).unwrap().errors.is_empty());
    let report = store.query(&Default::default(), &[]).unwrap();
    assert_eq!(report.totals.input_tokens, 50);
    assert_eq!(report.totals.request_count, 1);
    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    writeln!(
        append,
        "{}",
        token_count_total(75, 0, 7, "2026-07-10T03:00:03Z")
    )
    .unwrap();
    assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
    let report = store.query(&Default::default(), &[]).unwrap();
    assert_eq!(report.totals.input_tokens, 75);
    assert_eq!(report.totals.request_count, 2);
    assert_eq!(store.sync(false, &instances).unwrap().imported, 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn same_size_rewrite_replaces_previous_usage() {
    let dir = make_temp_dir("usage-same-size-rewrite");
    let home = dir.join("home");
    let file = rollout_path(&home.join("sessions"), PARENT_ID);
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "i".into(),
        name: "Instance".into(),
        data_dir: home,
    }];
    write_jsonl(
        &file,
        &[
            session_meta(PARENT_ID),
            token_count_total(100, 0, 10, "2026-07-10T03:00:02Z"),
        ],
    );
    store.sync(false, &instances).unwrap();
    let size = fs::metadata(&file).unwrap().len();
    write_jsonl(
        &file,
        &[
            session_meta(PARENT_ID),
            token_count_total(700, 0, 70, "2026-07-10T03:00:02Z"),
        ],
    );
    assert_eq!(fs::metadata(&file).unwrap().len(), size);
    let result = store.sync(false, &instances).unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let totals = store.query(&Default::default(), &instances).unwrap().totals;
    assert_eq!(
        (
            totals.input_tokens,
            totals.output_tokens,
            totals.request_count
        ),
        (700, 70, 1)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pricing_rollup_preserves_context_maximum_through_update_delete_and_edges() {
    let dir = make_temp_dir("usage-pricing-rollup-max");
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let conn = store.open_conn().unwrap();
    conn.execute_batch("INSERT INTO session_usage_events(request_id,instance_id,instance_name,session_id,model,service_tier,timestamp,input_tokens,cached_input_tokens,output_tokens,file_path) VALUES
        ('a','i','Instance','s','fixture-model',NULL,60,100000,10000,10,'f'),
        ('b','i','Instance','s','fixture-model',NULL,61,160000,20000,20,'f'),
        ('c','i','Instance','s','fixture-model',NULL,62,300000,30000,30,'f');").unwrap();
    let query = CodexSessionUsageQuery {
        from_timestamp: Some(0),
        to_timestamp: Some(179),
        instance_id: None,
    };
    let before = store.query(&query, &[]).unwrap();
    while !SESSION_USAGE_ROLLUP.batch(&conn).unwrap() {}
    let after = store.query(&query, &[]).unwrap();
    assert_eq!(
        serde_json::to_value(before.by_model).unwrap(),
        serde_json::to_value(&after.by_model).unwrap()
    );
    assert_eq!(
        after.by_model[0]
            .pricing_usage
            .iter()
            .map(|g| g.context_input_tokens)
            .collect::<Vec<_>>(),
        vec![160000, 300000]
    );
    assert!(after.by_model[0]
        .pricing_usage
        .iter()
        .all(|g| g.service_tier.is_none()));
    conn.execute(
        "UPDATE session_usage_events SET input_tokens=120000 WHERE request_id='b'",
        [],
    )
    .unwrap();
    let updated = store.query(&query, &[]).unwrap();
    assert_eq!(
        updated.by_model[0].pricing_usage[0].context_input_tokens,
        120000
    );
    conn.execute("DELETE FROM session_usage_events WHERE request_id='b'", [])
        .unwrap();
    assert_eq!(
        store.query(&query, &[]).unwrap().by_model[0].pricing_usage[0].context_input_tokens,
        100000
    );
    let edge = store
        .query(
            &CodexSessionUsageQuery {
                from_timestamp: Some(62),
                to_timestamp: Some(62),
                instance_id: None,
            },
            &[],
        )
        .unwrap();
    assert_eq!(
        (edge.totals.input_tokens, edge.totals.request_count),
        (300000, 1)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn segmented_rewrite_survives_cancellation_changed_source_and_restart() {
    const LEAF: &str = "10000000-0000-4000-8000-000000000004";
    let dir = make_temp_dir("usage-group-cancel");
    let home = dir.join("home");
    let root = home
        .join("sessions")
        .join(format!("rollout-2026-07-10T03-00-00-{PARENT_ID}.jsonl"));
    let segment = home.join("sessions").join(format!(
        "rollout-2026-07-10T03-10-00-{PARENT_ID}_{LEAF}.jsonl"
    ));
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "i".into(),
        name: "Instance".into(),
        data_dir: home,
    }];
    write_jsonl(
        &root,
        &[
            session_meta(PARENT_ID),
            token_count_total(100, 0, 10, "2026-07-10T03:00:02Z"),
        ],
    );
    write_jsonl(
        &segment,
        &[
            session_meta(PARENT_ID),
            token_count_total(200, 0, 20, "2026-07-10T03:10:02Z"),
        ],
    );
    assert!(store.sync(false, &instances).unwrap().errors.is_empty());
    let old = store.query(&Default::default(), &instances).unwrap().totals;
    assert_eq!((old.input_tokens, old.request_count), (200, 2));
    let mut lines = vec![session_meta(PARENT_ID)];
    for n in 1..=80 {
        lines.push(token_count_total(
            100 + n * 10,
            0,
            10 + n,
            "2026-07-10T03:10:02Z",
        ));
    }
    write_jsonl(&segment, &lines);
    let mut stopped = false;
    let interrupted=store.sync_with_batch_control(false,&instances,||{
        let conn=store.open_conn().unwrap();
        let copying:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM session_group_sync WHERE json_extract(state,'$.phase')=2 AND json_extract(state,'$.copy_cursor')>0)",[],|r|r.get(0)).unwrap();
        // Every pre-publication group view remains the original complete view.
        let visible=store.query(&Default::default(),&instances).unwrap();
        assert_eq!(visible.totals.request_count,2);
        assert_eq!(visible.event_count,2);
        let session=&visible.session_tokens.as_ref().unwrap()[0];
        assert_eq!((session.input_tokens,session.output_tokens,session.total_tokens),(200,20,220));
        if copying{stopped=true;None}else{Some(())}
    }).unwrap();
    assert!(stopped);
    assert!(interrupted.errors.is_empty());
    assert_eq!(
        store
            .query(&Default::default(), &instances)
            .unwrap()
            .totals
            .input_tokens,
        200
    );
    fs::write(segment.with_extension("jsonl.zst"), b"invalid zstd").unwrap();
    fs::remove_file(&segment).unwrap();
    let failed = SessionUsageStore::open_path(dir.join("usage.sqlite"))
        .sync(false, &instances)
        .unwrap();
    assert_eq!(failed.errors.len(), 1);
    let visible = store.query(&Default::default(), &instances).unwrap();
    assert_eq!(visible.totals.input_tokens, 200);
    assert_eq!(
        visible.session_tokens.as_ref().unwrap()[0].total_tokens,
        220
    );
    fs::remove_file(segment.with_extension("jsonl.zst")).unwrap();
    write_jsonl(
        &segment,
        &[
            session_meta(PARENT_ID),
            token_count_total(150, 0, 15, "2026-07-10T03:10:02Z"),
        ],
    );
    compress_fixture(&segment);
    let resumed = SessionUsageStore::open_path(dir.join("usage.sqlite"))
        .sync(false, &instances)
        .unwrap();
    assert!(resumed.errors.is_empty(), "{:?}", resumed.errors);
    let report = store.query(&Default::default(), &instances).unwrap();
    assert_eq!(
        (
            report.totals.input_tokens,
            report.totals.output_tokens,
            report.totals.request_count
        ),
        (150, 15, 2)
    );
    assert_eq!(report.session_tokens.as_ref().unwrap().len(), 1);
    assert_eq!(store.sync(false, &instances).unwrap().files_changed, 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn opened_reader_rejects_atomic_replacement_before_cache_publication() {
    let dir = make_temp_dir("usage-reader-replacement");
    for compressed in [false, true] {
        let plain = dir.join(if compressed {
            "compressed.jsonl"
        } else {
            "plain.jsonl"
        });
        fs::write(&plain, b"old record\n").unwrap();
        let path = if compressed {
            compress_fixture(&plain)
        } else {
            plain.clone()
        };
        let mut opened = open_checkpoint_reader(&path, 0).unwrap();
        let replacement = dir.join("replacement");
        let new = b"new record\n";
        fs::write(
            &replacement,
            if compressed {
                zstd::stream::encode_all(&new[..], 0).unwrap()
            } else {
                new.to_vec()
            },
        )
        .unwrap();
        fs::rename(replacement, &path).unwrap();
        let mut old = String::new();
        opened.read_to_string(&mut old).unwrap();
        assert_eq!(old, "old record\n");
        assert!(finish_checkpoint_reader(&path, old.len() as u64, &mut opened).is_err());
        let mut fresh = open_checkpoint_reader(&path, 0).unwrap();
        let mut actual = String::new();
        fresh.read_to_string(&mut actual).unwrap();
        assert_eq!(actual, "new record\n");
        assert!(finish_checkpoint_reader(&path, actual.len() as u64, &mut fresh).is_ok());
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn guardian_repair_is_bounded_to_the_original_unknown_prefix() {
    use std::io::Write;
    let dir = make_temp_dir("usage-guardian-prefix");
    let file = rollout_path(&dir.join("sessions"), PARENT_ID);
    let mut meta = session_meta(PARENT_ID);
    meta["payload"]["thread_source"] = json!("guardian_review");
    let mut lines = vec![meta];
    for n in 1..=40 {
        lines.push(usage_record(
            &format!("r{n}"),
            100,
            0,
            10,
            n * 100,
            0,
            n * 10,
            "2026-07-10T03:00:01Z",
        ));
    }
    write_jsonl(&file, &lines);
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "i".into(),
        name: "Instance".into(),
        data_dir: dir.clone(),
    }];
    store.sync(false, &instances).unwrap();
    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    for event in [
        turn_context("review-model"),
        usage_record("known", 60, 0, 6, 4060, 0, 406, "2026-07-10T03:00:02Z"),
        turn_context("unknown"),
        usage_record("later", 50, 0, 5, 4110, 0, 411, "2026-07-10T03:00:03Z"),
    ] {
        writeln!(append, "{event}").unwrap();
    }
    let result = store.sync(false, &instances).unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let report = store.query(&Default::default(), &instances).unwrap();
    let by_model = report
        .by_model
        .iter()
        .map(|r| (r.key.as_str(), r.input_tokens))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(by_model.get("review-model"), Some(&4060));
    assert_eq!(by_model.get("unknown"), Some(&50));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cancelled_rebuild_before_first_acquire_keeps_existing_publication() {
    let dir = make_temp_dir("usage-cancel-rebuild");
    let file = rollout_path(&dir.join("sessions"), PARENT_ID);
    write_jsonl(
        &file,
        &[
            session_meta(PARENT_ID),
            token_count_total(100, 0, 10, "2026-07-10T03:00:02Z"),
        ],
    );
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "i".into(),
        name: "Instance".into(),
        data_dir: dir.clone(),
    }];
    store.sync(false, &instances).unwrap();
    let before = store.query(&Default::default(), &instances).unwrap();
    let stopped = store
        .sync_with_batch_control(true, &instances, || None::<()>)
        .unwrap();
    assert!(!stopped.rebuilt);
    let after = store.query(&Default::default(), &instances).unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn complete_large_turn_context_preserves_model_and_usage() {
    let dir = make_temp_dir("usage-large-turn-context");
    let file = rollout_path(&dir.join("sessions"), PARENT_ID);
    let mut context = turn_context("large-context-model");
    // Long instructions are valid rollout data; the batch budget is a soft
    // scheduling boundary and must not change which complete records count.
    context["payload"]["developer_instructions"] = json!("x".repeat(2 * 1024 * 1024 + 1024));
    write_jsonl(
        &file,
        &[
            session_meta(PARENT_ID),
            context,
            token_count_total(100, 20, 10, "2026-07-10T03:00:02Z"),
        ],
    );
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "i".into(),
        name: "Instance".into(),
        data_dir: dir.clone(),
    }];
    let imported = store.sync(false, &instances).unwrap();
    assert!(imported.errors.is_empty(), "{:?}", imported.errors);
    let report = store.query(&Default::default(), &instances).unwrap();
    assert_eq!(
        (
            report.totals.input_tokens,
            report.totals.output_tokens,
            report.totals.request_count
        ),
        (100, 10, 1)
    );
    assert_eq!(report.by_model[0].key, "large-context-model");
    fs::remove_dir_all(dir).unwrap();
}
