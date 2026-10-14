fn compress_fixture(path: &Path) -> PathBuf {
    let compressed = path.with_extension("jsonl.zst");
    fs::write(
        &compressed,
        zstd::stream::encode_all(fs::read(path).unwrap().as_slice(), 0).unwrap(),
    )
    .unwrap();
    fs::remove_file(path).unwrap();
    compressed
}

fn fixture_usage_ids(store: &SessionUsageStore) -> Vec<String> {
    let conn = store.open_conn().unwrap();
    let mut query = conn
        .prepare("SELECT request_id FROM session_usage_events ORDER BY request_id")
        .unwrap();
    query
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn compressed_usage_transition_preserves_ids_cursors_and_latest_totals() {
    let dir = make_temp_dir("compressed-usage-transition");
    let home = dir.join("home");
    let plain = home
        .join("sessions")
        .join(format!("rollout-{PARENT_ID}.jsonl"));
    let records = vec![
        session_meta(PARENT_ID),
        turn_context("gpt-5.4"),
        token_count_total(100, 20, 10, "2026-07-10T03:00:02Z"),
        token_count_total(150, 30, 15, "2026-07-10T03:00:03Z"),
        json!({"type":"unrelated", "padding":"x".repeat(1000)}),
    ];
    write_jsonl(&plain, &records);
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "fixture".into(),
        name: "Fixture".into(),
        data_dir: home,
    }];
    assert!(store.sync(false, &instances).unwrap().errors.is_empty());
    let ids = fixture_usage_ids(&store);
    assert_eq!(ids.len(), 2);
    let compressed = compress_fixture(&plain);
    for rebuild in [false, true, false] {
        let sync = store.sync(rebuild, &instances).unwrap();
        assert!(sync.errors.is_empty(), "{:?}", sync.errors);
        assert_eq!(sync.files_scanned, 1);
        assert_eq!(fixture_usage_ids(&store), ids);
        let report = store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap();
        assert_eq!(
            (
                report.totals.input_tokens,
                report.totals.output_tokens,
                report.totals.request_count
            ),
            (150, 15, 2)
        );
        assert_eq!(report.files_tracked, 1);
        let card = &report.session_tokens.as_ref().unwrap()[0];
        assert_eq!(
            (card.input_tokens, card.output_tokens, card.total_tokens),
            (150, 15, 165)
        );
    }
    assert!(!plain.exists());
    for path in [&plain, &compressed] {
        assert_eq!(
            read_token_stats_from_rollout_uncached(path, fs::metadata(&compressed).unwrap().len()),
            Some((150, 15, 165))
        );
    }
    let mut appended = records;
    appended.push(token_count_total(200, 40, 20, "2026-07-10T03:00:04Z"));
    write_jsonl(&plain, &appended);
    // An older compressed sibling must not shadow the append or become a second file.
    let sync = store.sync(false, &instances).unwrap();
    assert!(sync.errors.is_empty());
    assert_eq!(sync.files_scanned, 1);
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
    assert_eq!(&fixture_usage_ids(&store)[..2], &ids);
    let conn = store.open_conn().unwrap();
    assert_eq!(
        conn.query_row("SELECT file_path FROM session_log_sync", [], |row| row
            .get::<_, String>(
            0
        ))
        .unwrap(),
        plain.to_string_lossy()
    );
    assert_eq!(
        conn.query_row("SELECT last_line_offset FROM session_log_sync", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        appended.len() as i64
    );
    assert!(compressed.is_file());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn compressed_usage_segments_and_parent_replay_keep_logical_identities() {
    const SEGMENT: &str = "10000000-0000-4000-8000-000000000004";
    let dir = make_temp_dir("compressed-usage-parent");
    let home = dir.join("home");
    let sessions = home.join("sessions");
    let parent = sessions.join(format!("rollout-2026-07-10T03-00-00-{PARENT_ID}.jsonl"));
    let segment = sessions.join(format!(
        "rollout-2026-07-10T03-10-00-{PARENT_ID}_{SEGMENT}.jsonl"
    ));
    let child = sessions.join(format!("rollout-2026-07-10T03-30-00-{CHILD_ID}.jsonl"));
    let first = token_count_with_last(100, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z");
    let second = token_count_with_last(150, 0, 15, 50, 0, 5, "codex", "2026-07-10T03:10:02Z");
    write_jsonl(
        &parent,
        &[
            session_meta(PARENT_ID),
            first.clone(),
            json!({"type":"padding","value":"x".repeat(1000)}),
        ],
    );
    write_jsonl(
        &segment,
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
    let instances = [UsageInstance {
        id: "fixture".into(),
        name: "Fixture".into(),
        data_dir: home.clone(),
    }];
    store.sync(false, &instances).unwrap();
    let ids = fixture_usage_ids(&store);
    assert_eq!(ids.len(), 3);
    let parent_zst = compress_fixture(&parent);
    compress_fixture(&child);
    // A stale archived plain copy is physically larger than the complete compressed source.
    let archived = home
        .join("archived_sessions")
        .join(parent.file_name().unwrap());
    write_jsonl(
        &archived,
        &[
            session_meta(PARENT_ID),
            token_count_total(50, 0, 5, "2026-07-10T03:00:02Z"),
        ],
    );
    assert!(fs::metadata(&archived).unwrap().len() > fs::metadata(&parent_zst).unwrap().len());
    for rebuild in [false, true, false] {
        let sync = store.sync(rebuild, &instances).unwrap();
        assert!(sync.errors.is_empty(), "{:?}", sync.errors);
        assert_eq!(sync.deferred_files, 0);
        assert_eq!(fixture_usage_ids(&store), ids);
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
        assert_eq!(report.files_tracked, 3);
        assert_eq!(report.session_tokens.as_ref().unwrap().len(), 2);
        assert!(!report
            .session_tokens
            .as_ref()
            .unwrap()
            .iter()
            .any(|row| row.session_id == SEGMENT));
        if segment.exists() {
            compress_fixture(&segment);
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn compressed_usage_decode_failure_preserves_good_incremental_cache() {
    let dir = make_temp_dir("compressed-usage-decode-error");
    let home = dir.join("home");
    let plain = home
        .join("sessions")
        .join(format!("rollout-{PARENT_ID}.jsonl"));
    write_jsonl(
        &plain,
        &[
            session_meta(PARENT_ID),
            token_count_total(100, 20, 10, "2026-07-10T03:00:02Z"),
        ],
    );
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "fixture".into(),
        name: "Fixture".into(),
        data_dir: home,
    }];
    store.sync(false, &instances).unwrap();
    let ids = fixture_usage_ids(&store);
    let compressed = compress_fixture(&plain);
    let good_bytes = fs::read(&compressed).unwrap();
    let mut damaged = good_bytes.clone();
    damaged.truncate(damaged.len() - 3);
    fs::write(&compressed, damaged).unwrap();
    let sync = store.sync(false, &instances).unwrap();
    assert_eq!(sync.errors.len(), 1);
    assert_eq!(fixture_usage_ids(&store), ids);
    let report = store
        .query(&CodexSessionUsageQuery::default(), &instances)
        .unwrap();
    assert_eq!(report.totals.input_tokens, 100);
    assert_eq!(report.session_tokens.as_ref().unwrap()[0].input_tokens, 100);
    assert_eq!(report.files_tracked, 1);
    assert_eq!(
        read_token_stats_from_rollout_uncached(
            &compressed,
            fs::metadata(&compressed).unwrap().len()
        ),
        None
    );
    fs::write(&compressed, good_bytes).unwrap();
    assert!(store.sync(false, &instances).unwrap().errors.is_empty());
    assert_eq!(fixture_usage_ids(&store), ids);
    fs::write(&compressed, "invalid zstd").unwrap();
    assert_eq!(store.sync(true, &instances).unwrap().errors.len(), 1);
    assert_eq!(
        store
            .query(&CodexSessionUsageQuery::default(), &instances)
            .unwrap()
            .files_tracked,
        0
    );
    fs::remove_dir_all(dir).unwrap();
}

fn write_compressed_jsonl(path: &Path, records: &[JsonValue]) {
    let content = records
        .iter()
        .map(JsonValue::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        zstd::stream::encode_all(content.as_bytes(), 0).unwrap(),
    )
    .unwrap();
}

#[test]
fn usage_follows_a_rollout_through_compression_and_archive() {
    let dir = make_temp_dir("usage-compressed-migration");
    let home = dir.join("home");
    let plain = rollout_path(&home.join("sessions/2026/07/10"), PARENT_ID);
    let compressed = plain.with_extension("jsonl.zst");
    let archived = home
        .join("archived_sessions")
        .join(compressed.file_name().unwrap());
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "i".into(),
        name: "Instance".into(),
        data_dir: home.clone(),
    }];
    let mut records = vec![session_meta(PARENT_ID), turn_context("gpt-5.4")];
    for n in 1..=70 {
        records.push(token_count_total(
            n * 100,
            n * 20,
            n * 10,
            "2026-07-10T03:00:02Z",
        ));
    }
    write_jsonl(&plain, &records[..3]);
    write_compressed_jsonl(&compressed, &records);
    write_compressed_jsonl(&archived, &records);
    // An archive copy need not have the timestamp of the active source.
    for (path, time) in [(&compressed, 10), (&archived, 20)] {
        File::open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(time))
            .unwrap();
    }
    assert_eq!(collect_codex_session_files(&home), vec![plain.clone()]);
    assert_eq!(store.sync(false, &instances).unwrap().imported, 1);

    fs::remove_file(plain).unwrap();
    assert_eq!(collect_codex_session_files(&home), vec![compressed.clone()]);
    assert!(store.sync(false, &instances).unwrap().errors.is_empty());
    let query = CodexSessionUsageQuery::default();
    let report = store.query(&query, &instances).unwrap();
    assert_eq!(report.files_tracked, 1);
    assert_eq!(report.totals.request_count, 70);
    assert_eq!(report.totals.total_tokens, 7_700);
    assert_eq!(report.totals.cached_input_tokens, 1_400);
    let ids = fixture_usage_ids(&store);
    assert_eq!(store.sync(false, &instances).unwrap().imported, 0);

    fs::remove_file(compressed).unwrap();
    assert_eq!(collect_codex_session_files(&home), vec![archived]);
    let moved = store.sync(false, &instances).unwrap();
    assert!(moved.errors.is_empty(), "{:?}", moved.errors);
    assert_eq!(moved.imported, 0);
    assert_eq!(fixture_usage_ids(&store), ids);

    let rebuilt = store.sync(true, &instances).unwrap();
    assert!(rebuilt.errors.is_empty(), "{:?}", rebuilt.errors);
    assert_eq!(rebuilt.imported, 70);
    assert_eq!(
        store.query(&query, &instances).unwrap().totals.total_tokens,
        7_700
    );
    assert_eq!(fixture_usage_ids(&store), ids);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn truncated_compressed_replacement_keeps_previous_usage_visible() {
    let dir = make_temp_dir("usage-invalid-compressed-rewrite");
    let home = dir.join("home");
    let plain = rollout_path(&home.join("sessions"), PARENT_ID);
    let compressed = plain.with_extension("jsonl.zst");
    let store = SessionUsageStore::open_path(dir.join("usage.sqlite"));
    let instances = [UsageInstance {
        id: "i".into(),
        name: "Instance".into(),
        data_dir: home,
    }];
    write_jsonl(
        &plain,
        &[
            session_meta(PARENT_ID),
            token_count_total(100, 0, 10, "2026-07-10T03:00:02Z"),
        ],
    );
    assert_eq!(store.sync(false, &instances).unwrap().imported, 1);
    fs::remove_file(plain).unwrap();

    let mut replacement = vec![session_meta(PARENT_ID)];
    for n in 1..=100 {
        replacement.push(token_count_total(n * 200, 0, n, "2026-07-10T03:00:03Z"));
    }
    replacement.push(json!({ "type": "response_item", "payload": "a".repeat(256 * 1024) }));
    write_compressed_jsonl(&compressed, &replacement);
    let mut invalid = fs::read(&compressed).unwrap();
    invalid.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd]); // Truncated second frame.
    fs::write(&compressed, invalid).unwrap();

    let failed = store.sync(false, &instances).unwrap();
    assert_eq!(failed.errors.len(), 1);
    assert!(
        failed.imported > 0,
        "the decoder must have committed staged batches"
    );
    let query = CodexSessionUsageQuery::default();
    assert_eq!(
        store.query(&query, &instances).unwrap().totals.total_tokens,
        110
    );

    write_compressed_jsonl(
        &compressed,
        &[
            session_meta(PARENT_ID),
            token_count_total(250, 0, 25, "2026-07-10T03:00:03Z"),
        ],
    );
    let repaired = store.sync(false, &instances).unwrap();
    assert!(repaired.errors.is_empty(), "{:?}", repaired.errors);
    let report = store.query(&query, &instances).unwrap();
    assert_eq!(report.totals.request_count, 1);
    assert_eq!(report.totals.total_tokens, 275);
    fs::remove_dir_all(dir).unwrap();
}
