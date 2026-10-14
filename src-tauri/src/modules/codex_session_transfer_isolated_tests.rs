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
fn compressed_trash_backup_roundtrip_preserves_original_and_recovery() {
    let base = make_temp_dir("codex-compressed-trash-roundtrip");
    for directory in ["sessions", "archived_sessions"] {
        let root = base.join(directory);
        let logical = root.join(directory).join("rollout-compressed.jsonl");
        write_rollout(&logical, "compressed", "recoverable");
        let content = fs::read(&logical).unwrap();
        let compressed = logical.with_extension("jsonl.zst");
        let encoded = zstd::stream::encode_all(content.as_slice(), 0).unwrap();
        fs::write(&compressed, &encoded).unwrap();
        fs::remove_file(&logical).unwrap();
        let instance = CodexSyncInstance {
            id: "fixture".into(), name: "Fixture".into(), data_dir: root.clone(), last_pid: None,
        };
        let snapshot = ThreadSnapshot {
            archived: directory == "archived_sessions", id: "compressed".into(),
            session_kind: default_session_kind(), parent_thread_id: None,
            title: "Compressed".into(), cwd: "/project".into(), project_name: None,
            updated_at: None, rollout_path: logical.clone(),
            session_index_entry: json!({"id":"compressed"}), source_root: root.clone(),
        };
        let trash = root.join("trash");
        let batch = trash.join("batch");
        let entry_dir = batch.join("fixture--compressed");
        copy_snapshot_rollout_to_trash(&instance, &batch, &snapshot).unwrap();
        // A rejected official deletion must leave the original and remove only its temporary backup.
        finish_delete_backup(&entry_dir, false).unwrap();
        assert_eq!(fs::read(&compressed).unwrap(), encoded);
        assert!(!logical.exists());
        assert!(!entry_dir.exists());
        copy_snapshot_rollout_to_trash(&instance, &batch, &snapshot).unwrap();
        assert!(load_trash_entries_from_roots(&[TrashRoot { path: trash.clone(), optional: false }]).unwrap().is_empty());
        fs::remove_file(&compressed).unwrap(); // Official thread/delete filesystem effect.
        finish_delete_backup(&entry_dir, true).unwrap();
        let entries = load_trash_entries_from_roots(&[TrashRoot { path: trash, optional: false }]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(fs::read(&entries[0].trashed_rollout_path).unwrap(), encoded);
        restore_trashed_session_entry_with_registration(&entries[0], |_, id, path, archived, _| {
            assert_eq!(id, "compressed");
            assert_eq!(archived, directory == "archived_sessions");
            assert_eq!(rollout_session_id(path).unwrap().as_deref(), Some("compressed"));
            let mut restored = Vec::new();
            rollout::open_reader(path).unwrap().read_to_end(&mut restored).unwrap();
            assert_eq!(restored, content);
            Ok(())
        }).unwrap();
        assert_eq!(fs::read(&compressed).unwrap(), encoded);
        assert!(!entry_dir.exists());
    }
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn rollout_deletion_requires_both_representations_to_be_absent() {
    let base = make_temp_dir("codex-compressed-delete-confirmation");
    let plain = base.join("rollout-fixture.jsonl");
    let compressed = plain.with_extension("jsonl.zst");
    fs::write(&plain, "plain").unwrap();
    fs::write(&compressed, "compressed").unwrap();
    assert!(!rollout_is_missing(&plain));
    fs::remove_file(&plain).unwrap();
    assert!(!rollout_is_missing(&plain));
    fs::remove_file(&compressed).unwrap();
    assert!(rollout_is_missing(&plain));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn compressed_trash_restore_checks_existing_plain_sibling() {
    for existing_id in ["fixture", "other-session"] {
        let base = make_temp_dir("codex-compressed-restore-conflict");
        let compressed = base.join("codex-home/sessions/2026/06/02/rollout-fixture.jsonl.zst");
        let mut entry = make_trash_entry(&base, "fixture", compressed.clone());
        let encoded = zstd::stream::encode_all(fs::read(&entry.trashed_rollout_path).unwrap().as_slice(), 0).unwrap();
        fs::remove_file(&entry.trashed_rollout_path).unwrap();
        entry.trashed_rollout_path = entry.trashed_rollout_path.with_extension("jsonl.zst");
        fs::write(&entry.trashed_rollout_path, encoded).unwrap();
        let plain = compressed.with_extension("");
        write_rollout(&plain, existing_id, "newer live content");
        let original = fs::read(&plain).unwrap();
        let result = restore_trashed_session_entry_with_registration(&entry, |_, _, path, _, _| {
            assert_eq!(existing_id, "fixture", "must not register a conflicting sibling");
            assert_eq!(path, plain);
            Ok(())
        });
        assert_eq!(result.is_ok(), existing_id == "fixture");
        assert_eq!(fs::read(&plain).unwrap(), original);
        assert!(!compressed.exists());
        assert_eq!(entry.entry_dir.exists(), existing_id != "fixture");
        fs::remove_dir_all(base).unwrap();
    }
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
    upsert_session_index_with_entry(&target_root, &None, "qa-roundtrip", &index).unwrap();
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
                &None,
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
fn isolated_import_second_index_failure_keeps_first_commit_and_removes_only_second_rollout() {
    let base = make_temp_dir("codex-session-import-checkpoint");
    let first = base.join("sessions/first.jsonl");
    let second = base.join("sessions/second.jsonl");
    write_rollout(&first, "first", "first successful import");
    let initial = Some("{\"id\":\"existing\",\"pinned\":true}\n".to_owned());
    restore_session_index_content(&base, initial.as_deref()).unwrap();
    let first_entry = json!({"id":"first", "thread_name":"First import"});
    commit_imported_session_index_with(&base, &initial, &first, || {
        upsert_session_index_with_entry(&base, &initial, "first", &first_entry)
    })
    .unwrap();
    let checkpoint = read_session_index_content(&base).unwrap();
    write_rollout(&second, "second", "second staged import");
    let index_path = base.join(SESSION_INDEX_FILE);
    let error = commit_imported_session_index_with(&base, &checkpoint, &second, || {
        // Inject a real, transient filesystem failure after the first commit:
        // a directory occupies the index filename during atomic publication.
        fs::remove_file(&index_path).unwrap();
        fs::create_dir(&index_path).unwrap();
        let result =
            upsert_session_index_with_entry(&base, &checkpoint, "second", &json!({"id":"second"}));
        assert!(result.is_err());
        fs::remove_dir(&index_path).unwrap();
        result
    })
    .unwrap_err();
    assert!(error.contains("写入 session_index.jsonl 失败"));
    assert_eq!(read_session_index_content(&base).unwrap(), checkpoint);
    let index = read_session_index_map(&base).unwrap();
    assert!(index.contains_key("existing"));
    assert!(index.contains_key("first"));
    assert!(!index.contains_key("second"));
    assert!(first.exists());
    assert!(!second.exists());
    fs::remove_dir_all(base).unwrap();
}
