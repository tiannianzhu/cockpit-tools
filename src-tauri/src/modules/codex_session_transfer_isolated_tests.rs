// Real ZIP/file/index round trips. Only the external official app-server RPC
// is replaced, so no installed client or real instance is started.
fn isolated_export_fixture(
    base: &Path,
    session_id: &str,
) -> (SessionPackageSource, SessionExportManifest) {
    let root = base.join("source");
    let rollout = root.join(format!("sessions/2026/10/11/rollout-{session_id}.jsonl"));
    write_rollout(&rollout, session_id, "中文🙂 round trip");
    let index = json!({"id": session_id, "thread_name": "QA export", "pinned": true, "cwd": "/tmp/project"});
    fs::write(root.join(SESSION_INDEX_FILE), format!("{index}\n")).unwrap();
    let source = SessionPackageSource {
        session_id: session_id.into(),
        title: "QA export".into(),
        cwd: "/tmp/project".into(),
        updated_at: Some(1_780_362_123),
        relative_rollout_path: path_to_package_path(rollout.strip_prefix(&root).unwrap()).unwrap(),
        rollout_path: rollout.clone(),
        session_index_entry: index.clone(),
        source_instance_id: "qa-source".into(),
        source_instance_name: "QA source".into(),
    };
    let (size_bytes, sha256) = sha256_file(&rollout).unwrap();
    let manifest = SessionExportManifest {
        kind: SESSION_EXPORT_KIND.into(),
        package_version: SESSION_EXPORT_VERSION,
        exported_at: "2026-10-11T00:00:00Z".into(),
        sessions: vec![SessionExportManifestItem {
            session_id: session_id.into(),
            title: "QA export".into(),
            cwd: "/tmp/project".into(),
            updated_at: source.updated_at,
            relative_rollout_path: "sessions/2026/10/11/rollout-imported.jsonl".into(),
            file_entry: format!("files/{session_id}.jsonl"),
            size_bytes,
            sha256,
            session_index_entry: index,
            source_instance: SessionExportInstance {
                id: source.source_instance_id.clone(),
                name: source.source_instance_name.clone(),
            },
        }],
    };
    (source, manifest)
}

#[test]
fn isolated_export_import_trash_restore_roundtrip_keeps_bytes_index_and_package() {
    let base = make_temp_dir("codex-session-transfer-roundtrip");
    let (source, manifest) = isolated_export_fixture(&base, "qa-roundtrip");
    let original_bytes = fs::read(&source.rollout_path).unwrap();
    let package = base.join("sessions.zip");
    write_session_export_package(&package, &manifest, &[source], None, None).unwrap();
    let original_package_hash = sha256_file(&package).unwrap();
    let disk_manifest = read_session_export_manifest_from_path(&package).unwrap();
    assert_eq!(disk_manifest.sessions[0].session_id, "qa-roundtrip");
    validate_manifest_item(&disk_manifest.sessions[0]).unwrap();
    let target_root = base.join("target");
    let target_path = resolve_import_target_rollout_path(&target_root, &disk_manifest.sessions[0]);
    let mut archive = ZipArchive::new(File::open(&package).unwrap()).unwrap();
    write_imported_rollout_from_archive(
        &mut archive,
        &disk_manifest.sessions[0],
        &target_path,
        None,
    )
    .unwrap();
    assert_eq!(fs::read(&target_path).unwrap(), original_bytes);
    let index = build_imported_session_index_entry(&disk_manifest.sessions[0], &target_path);
    upsert_session_index_with_entry(&target_root, "qa-roundtrip", &index).unwrap();
    let target = CodexSyncInstance {
        id: "qa-target".into(),
        name: "QA target".into(),
        data_dir: target_root.clone(),
        last_pid: None,
    };
    let imported = ThreadSnapshot {
        archived: false,
        id: "qa-roundtrip".into(),
        session_kind: default_session_kind(),
        parent_thread_id: None,
        title: "QA export".into(),
        cwd: "/tmp/project".into(),
        project_name: None,
        updated_at: Some(1_780_362_123),
        rollout_path: target_path.clone(),
        session_index_entry: index,
        source_root: target_root.clone(),
    };
    let trash_root = base.join("trash");
    let batch = trash_root.join("20261011-000000");
    copy_snapshot_rollout_to_trash(&target, &batch, &imported).unwrap();
    // Simulate the official thread/delete RPC's filesystem effects. Cockpit
    // only finalizes the recoverable backup after deletion is confirmed.
    fs::remove_file(&target_path).unwrap();
    fs::write(target_root.join(SESSION_INDEX_FILE), "").unwrap();
    finish_delete_backup(&batch.join("qa-target--qa-roundtrip"), true).unwrap();
    assert!(!target_path.exists());
    assert!(!read_session_index_map(&target_root)
        .unwrap()
        .contains_key("qa-roundtrip"));
    let entries = load_trash_entries_from_roots(&[TrashRoot {
        path: trash_root,
        optional: false,
    }])
    .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        fs::read(&entries[0].trashed_rollout_path).unwrap(),
        original_bytes
    );
    restore_trashed_session_entry_with_registration(
        &entries[0],
        |root, id, path, archived, title| {
            assert_eq!(root, target_root);
            assert_eq!(id, "qa-roundtrip");
            assert_eq!(path, target_path);
            assert!(!archived);
            assert_eq!(title, "QA export");
            assert_eq!(fs::read(path).unwrap(), original_bytes);
            // The official registration RPC owns index updates on restore.
            upsert_session_index_with_entry(
                root,
                id,
                &entries[0].manifest.session_index_entry,
            )
        },
    )
    .unwrap();
    assert_eq!(fs::read(&target_path).unwrap(), original_bytes);
    assert_eq!(
        read_session_index_map(&target_root).unwrap()["qa-roundtrip"]["pinned"],
        true
    );
    assert!(!entries[0].entry_dir.exists());
    assert_eq!(sha256_file(&package).unwrap(), original_package_hash);
    drop(archive);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_package_read_rejects_paginated_history_before_remote_staging() {
    let base = make_temp_dir("codex-session-import-paginated");
    let (source, mut manifest) = isolated_export_fixture(&base, "qa-paginated");
    let content = fs::read_to_string(&source.rollout_path).unwrap();
    let mut records = content
        .lines()
        .map(|line| serde_json::from_str::<JsonValue>(line).unwrap())
        .collect::<Vec<_>>();
    records[0]["payload"]["history_base"] = json!({"session_id": "ancestor", "byte_offset": 128});
    let content = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    fs::write(&source.rollout_path, content).unwrap();
    let (size_bytes, sha256) = sha256_file(&source.rollout_path).unwrap();
    manifest.sessions[0].size_bytes = size_bytes;
    manifest.sessions[0].sha256 = sha256;
    let package = base.join("sessions.zip");
    write_session_export_package(&package, &manifest, &[source], None, None).unwrap();
    let error = read_session_export_entry_bytes(&package, &manifest.sessions[0]).unwrap_err();
    assert!(error.contains("分页会话"));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_import_rejects_manifest_id_mismatch_without_publishing_file() {
    let base = make_temp_dir("codex-session-import-id-mismatch");
    let (source, mut manifest) = isolated_export_fixture(&base, "actual-id");
    manifest.sessions[0].session_id = "wrong-manifest-id".into();
    let package = base.join("sessions.zip");
    write_session_export_package(&package, &manifest, &[source], None, None).unwrap();
    let target = base.join("target/rollout.jsonl");
    let mut archive = ZipArchive::new(File::open(&package).unwrap()).unwrap();
    assert!(
        write_imported_rollout_from_archive(&mut archive, &manifest.sessions[0], &target, None)
            .is_err(),
        "matching hash must not allow manifest/rollout identity mismatch"
    );
    assert!(!target.exists());
    assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 0);
    drop(archive);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_import_disk_parent_failure_preserves_existing_file_and_package() {
    let base = make_temp_dir("codex-session-import-parent-failure");
    let (source, manifest) = isolated_export_fixture(&base, "qa-disk-failure");
    let package = base.join("sessions.zip");
    write_session_export_package(&package, &manifest, &[source], None, None).unwrap();
    let hash = sha256_file(&package).unwrap();
    let blocker = base.join("target-file");
    fs::write(&blocker, b"keep existing data").unwrap();
    let mut archive = ZipArchive::new(File::open(&package).unwrap()).unwrap();
    assert!(write_imported_rollout_from_archive(
        &mut archive,
        &manifest.sessions[0],
        &blocker.join("rollout.jsonl"),
        None
    )
    .is_err());
    assert_eq!(fs::read(&blocker).unwrap(), b"keep existing data");
    assert_eq!(sha256_file(&package).unwrap(), hash);
    drop(archive);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_import_index_failure_preserves_published_history_and_concurrent_index_entries() {
    let base = make_temp_dir("codex-session-import-checkpoint");
    let first = base.join("sessions/first.jsonl");
    let second = base.join("sessions/second.jsonl");
    write_rollout(&first, "first", "first successful import");
    let index_path = base.join(SESSION_INDEX_FILE);
    fs::write(&index_path, "{\"id\":\"existing\",\"pinned\":true}\n").unwrap();
    commit_imported_session_index_with(|| {
        upsert_session_index_with_entry(&base, "first", &json!({"id":"first", "thread_name":"First import"}))
    }).unwrap();
    write_rollout(&second, "second", "second published import");
    let held_index = base.join("held-index");
    let mut after_failure = String::new();
    let error = commit_imported_session_index_with(|| {
        // A real filesystem publication failure overlaps another writer's update.
        fs::rename(&index_path, &held_index).unwrap();
        fs::create_dir(&index_path).unwrap();
        let result = upsert_session_index_with_entry(&base, "second", &json!({"id":"second"}));
        assert!(result.is_err());
        fs::remove_dir(&index_path).unwrap();
        fs::rename(&held_index, &index_path).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&index_path).unwrap();
        file.write_all(b"{\"id\":\"concurrent\",\"thread_name\":\"Newer client record\"}\n").unwrap();
        after_failure = fs::read_to_string(&index_path).unwrap();
        result
    }).unwrap_err();
    assert!(error.contains("session_index"));
    assert_eq!(read_session_index_content(&base).unwrap().as_deref(), Some(after_failure.as_str()));
    let index = read_session_index_map(&base).unwrap();
    assert!(index.contains_key("existing"));
    assert!(index.contains_key("first"));
    assert!(index.contains_key("concurrent"));
    assert!(!index.contains_key("second"));
    assert!(first.exists());
    assert!(second.exists(), "published history must remain recoverable after index failure");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_import_retry_repairs_only_identical_unindexed_history() {
    let base = make_temp_dir("codex-session-import-index-retry");
    let (snapshot, manifest) = isolated_export_fixture(&base, "retry-index");
    let package = base.join("package.zip");
    write_session_export_package(&package, &manifest, &[snapshot.clone()], None, None).unwrap();
    let mut archive = ZipArchive::new(File::open(&package).unwrap()).unwrap();
    let item = &manifest.sessions[0];
    let root = base.join("target");
    fs::create_dir_all(&root).unwrap();
    let rollout = root.join("rollout-retry-index.jsonl");
    fs::copy(&snapshot.rollout_path, &rollout).unwrap();
    let before = fs::read(&rollout).unwrap();
    assert!(repair_imported_session_index(&root, &mut archive, item, &rollout, None).unwrap());
    assert_eq!(fs::read(&rollout).unwrap(), before);
    assert_eq!(read_session_index_map(&root).unwrap()["retry-index"]["thread_name"], "QA export");
    assert!(!repair_imported_session_index(&root, &mut archive, item, &rollout, None).unwrap());
    fs::remove_file(root.join(SESSION_INDEX_FILE)).unwrap();
    fs::OpenOptions::new().append(true).open(&rollout).unwrap().write_all(b"new client history\n").unwrap();
    assert!(!repair_imported_session_index(&root, &mut archive, item, &rollout, None).unwrap());
    assert!(!root.join(SESSION_INDEX_FILE).exists());
    let mapped = root.join("mapped.jsonl");
    modules::codex_session_import_paths::rewrite_rollout_workspace_paths(&snapshot.rollout_path, &mapped, &item.cwd, "/mapped/project").unwrap();
    assert!(repair_imported_session_index(&root, &mut archive, item, &mapped, Some("/mapped/project")).unwrap());
    assert_eq!(read_session_index_map(&root).unwrap()["retry-index"]["cwd"], "/mapped/project");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_import_retry_matches_compressed_sibling_and_rejects_appended_history() {
    let base = make_temp_dir("codex-session-import-compressed-retry");
    let (source, manifest) = isolated_export_fixture(&base, "compressed-retry");
    let package = base.join("package.zip");
    write_session_export_package(&package, &manifest, &[source.clone()], None, None).unwrap();
    let mut archive = ZipArchive::new(File::open(&package).unwrap()).unwrap();
    let item = &manifest.sessions[0];
    let root = base.join("target");
    fs::create_dir_all(&root).unwrap();
    let logical = root.join("rollout-compressed-retry.jsonl");
    let compressed = logical.with_extension("jsonl.zst");
    let original = fs::read(&source.rollout_path).unwrap();
    let encoded = zstd::stream::encode_all(original.as_slice(), 0).unwrap();
    fs::write(&compressed, &encoded).unwrap();

    assert!(repair_imported_session_index(&root, &mut archive, item, &logical, None).unwrap());
    assert_eq!(read_session_index_map(&root).unwrap()["compressed-retry"]["thread_name"], "QA export");
    assert!(!logical.exists());
    assert_eq!(fs::read(&compressed).unwrap(), encoded);

    fs::remove_file(root.join(SESSION_INDEX_FILE)).unwrap();
    let appended = zstd::stream::encode_all(&b"new concurrent client history\n"[..], 0).unwrap();
    fs::OpenOptions::new().append(true).open(&compressed).unwrap()
        .write_all(&appended).unwrap();
    let mut changed_encoded = encoded;
    changed_encoded.extend_from_slice(&appended);
    assert!(!repair_imported_session_index(&root, &mut archive, item, &logical, None).unwrap());
    assert!(!root.join(SESSION_INDEX_FILE).exists());
    assert!(!logical.exists());
    assert_eq!(fs::read(&compressed).unwrap(), changed_encoded);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_import_target_discovery_recovers_collision_without_an_official_db_record() {
    let base = make_temp_dir("codex-session-import-discovery-retry");
    let (source, manifest) = isolated_export_fixture(&base, "discovery-retry");
    let package = base.join("package.zip");
    write_session_export_package(&package, &manifest, &[source.clone()], None, None).unwrap();
    let mut archive = ZipArchive::new(File::open(&package).unwrap()).unwrap();
    let item = &manifest.sessions[0];
    let target = CodexSyncInstance {
        id: "target".into(), name: "Target".into(), data_dir: base.join("target"), last_pid: None,
    };
    let destination = resolve_import_target_rollout_path(&target.data_dir, item);
    write_rollout(&destination, "unrelated", "keep this collision");
    let unrelated = fs::read(&destination).unwrap();
    let collision = uniquify_rollout_path(&destination);
    let compressed = collision.with_extension("jsonl.zst");
    let encoded = zstd::stream::encode_all(fs::read(&source.rollout_path).unwrap().as_slice(), 0).unwrap();
    fs::write(&compressed, &encoded).unwrap();
    assert!(load_thread_snapshots(&target).unwrap().is_empty());

    let discovered = load_import_target_snapshots(&target, &manifest, &mut archive, &HashMap::new()).unwrap();
    let recovered = &discovered[&item.session_id];
    assert_eq!(recovered.rollout_path, collision);
    assert!(repair_imported_session_index(&target.data_dir, &mut archive, item, &recovered.rollout_path, None).unwrap());
    assert_eq!(read_session_index_map(&target.data_dir).unwrap()[&item.session_id]["thread_name"], "QA export");
    assert_eq!(fs::read(&destination).unwrap(), unrelated);
    assert_eq!(fs::read(&compressed).unwrap(), encoded);
    assert!(!collision.exists());

    fs::remove_file(target.data_dir.join(SESSION_INDEX_FILE)).unwrap();
    let appended = zstd::stream::encode_all(&b"new concurrent client history\n"[..], 0).unwrap();
    fs::OpenOptions::new().append(true).open(&compressed).unwrap().write_all(&appended).unwrap();
    let mut changed = encoded;
    changed.extend_from_slice(&appended);
    let discovered = load_import_target_snapshots(&target, &manifest, &mut archive, &HashMap::new()).unwrap();
    let conflict = &discovered[&item.session_id];
    assert!(!repair_imported_session_index(&target.data_dir, &mut archive, item, &conflict.rollout_path, None).unwrap());
    assert!(!target.data_dir.join(SESSION_INDEX_FILE).exists());
    assert_eq!(fs::read(&compressed).unwrap(), changed);
    assert_eq!(fs::read(&destination).unwrap(), unrelated);
    assert!(!collision.exists());
    fs::remove_dir_all(base).unwrap();
}
