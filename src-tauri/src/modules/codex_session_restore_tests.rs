#[test]
fn restore_index_failure_keeps_durable_target_and_trash_for_retry() {
    let base = make_temp_dir("codex-session-restore-index-retry");
    let root = base.join("codex-home");
    fs::create_dir_all(&root).unwrap();
    let target = root.join("sessions/2026/10/24/rollout-session-1.jsonl");
    let entry = make_trash_entry(&base, "session-1", target.clone());
    let source = fs::read(&entry.trashed_rollout_path).unwrap();
    fs::create_dir(root.join(SESSION_INDEX_FILE)).unwrap();
    assert!(
        restore_trashed_session_entry_with_registration(&entry, |_, _, _, _, _| Ok(())).is_err()
    );
    assert_eq!(fs::read(&target).unwrap(), source);
    assert_eq!(fs::read(&entry.trashed_rollout_path).unwrap(), source);
    fs::remove_dir(root.join(SESSION_INDEX_FILE)).unwrap();
    fs::write(
        root.join(SESSION_INDEX_FILE),
        b"{\"id\":\"other\",\"thread_name\":\"Concurrent record\"}\n",
    )
    .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&target)
        .unwrap()
        .write_all(b"new concurrent history\n")
        .unwrap();
    let mut appended = source.clone();
    appended.extend_from_slice(b"new concurrent history\n");
    restore_trashed_session_entry_with_registration(&entry, |_, _, path, _, _| {
        assert_eq!(path, target);
        assert_eq!(fs::read(path).unwrap(), appended);
        Ok(())
    })
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), appended);
    assert!(read_session_index_map(&root).unwrap().contains_key("other"));
    assert!(!entry.entry_dir.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn restore_registration_failure_keeps_concurrently_appended_history_and_index() {
    let base = make_temp_dir("codex-session-restore-registration-append");
    let target = base.join("codex-home/sessions/rollout-session-1.jsonl");
    let entry = make_trash_entry(&base, "session-1", target.clone());
    let original = fs::read(&entry.trashed_rollout_path).unwrap();
    let index_path = entry.manifest.instance_root.join(SESSION_INDEX_FILE);
    let mut expected = original.clone();
    expected.extend_from_slice(b"new client history\n");
    let concurrent_index = b"{\"id\":\"concurrent\",\"thread_name\":\"Client record\"}\n";
    let error = restore_trashed_session_entry_with_registration(&entry, |_, _, path, _, _| {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"new client history\n")
            .unwrap();
        fs::write(&index_path, concurrent_index).unwrap();
        Err("official confirmation failed".into())
    })
    .unwrap_err();
    assert!(error.contains("official confirmation failed"));
    assert_eq!(fs::read(&target).unwrap(), expected);
    assert_eq!(fs::read(&index_path).unwrap(), concurrent_index);
    assert_eq!(fs::read(&entry.trashed_rollout_path).unwrap(), original);
    restore_trashed_session_entry_with_registration(&entry, |_, _, _, _, _| Ok(())).unwrap();
    assert_eq!(fs::read(&target).unwrap(), expected);
    assert!(fs::read(&index_path).unwrap().starts_with(concurrent_index));
    assert!(!entry.entry_dir.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn restored_session_index_is_append_only_and_uses_the_latest_existing_record() {
    let root = make_temp_dir("codex-session-restore-index-append");
    let before = b"{ \"id\":\"session\",\"pinned\":false }\r\n{\"id\":\"other\",\"thread_name\":\"Keep me\"}\n{\"id\":\"session\",\"pinned\":true}\n{unfinished old tail";
    fs::write(root.join(SESSION_INDEX_FILE), before).unwrap();
    upsert_session_index_with_entry(
        &root,
        "session",
        &json!({"id":"session", "thread_name":"Restored"}),
    )
    .unwrap();
    assert!(fs::read(root.join(SESSION_INDEX_FILE))
        .unwrap()
        .starts_with(before));
    let index = read_session_index_map(&root).unwrap();
    assert_eq!(index["session"]["pinned"], true);
    assert_eq!(index["session"]["thread_name"], "Restored");
    assert_eq!(index["other"]["thread_name"], "Keep me");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn restored_session_index_concurrent_appends_keep_every_entry() {
    let root = make_temp_dir("codex-session-restore-index-concurrent");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let workers = (0..8)
        .map(|index| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let id = format!("session-{index}");
                upsert_session_index_with_entry(&root, &id, &json!({"id":id,"thread_name":index}))
                    .unwrap();
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
    let index = read_session_index_map(&root).unwrap();
    for number in 0..8 {
        assert_eq!(index[&format!("session-{number}")]["thread_name"], number);
    }
    fs::remove_dir_all(root).unwrap();
}
