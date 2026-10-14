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
