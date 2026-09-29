#[test]
fn compressed_session_heads_are_listed_once_and_state_selects_current_rollout() {
    let base = make_temp_dir("codex-session-compressed-list");
    let (source, _) = isolated_export_fixture(&base, "compressed-head");
    let instance = CodexSyncInstance {
        id: source.source_instance_id.clone(), name: source.source_instance_name.clone(),
        data_dir: base.join("source"), last_pid: None,
    };
    let plain = fs::read(&source.rollout_path).unwrap();
    let compressed = source.rollout_path.with_extension("jsonl.zst");
    fs::write(&compressed, zstd::stream::encode_all(plain.as_slice(), 0).unwrap()).unwrap();
    fs::remove_file(&source.rollout_path).unwrap();
    assert_eq!(read_rollout_session_meta(&compressed).unwrap().unwrap()["payload"]["id"], "compressed-head");
    assert!(rollout_conversation_contains_query_uncached(&compressed, "round trip").unwrap());
    // The local inventory uses the newest official state schema and its current generation.
    let database = rusqlite::Connection::open(instance.data_dir.join("state_7.sqlite")).unwrap();
    database.execute_batch("CREATE TABLE threads(id TEXT, title TEXT, cwd TEXT, rollout_path TEXT, updated_at INTEGER, source TEXT, archived INTEGER)").unwrap();
    database.execute("INSERT INTO threads VALUES (?1,'Head','/project',?2,1,'cli',0)", rusqlite::params!["compressed-head", source.rollout_path.to_string_lossy()]).unwrap();
    let listed = load_thread_snapshots(&instance).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(rollout::resolve_path(&listed[0].rollout_path).unwrap(), compressed);
    let ancestor = instance.data_dir.join("archived_sessions/rollout-old.jsonl.zst");
    fs::create_dir_all(ancestor.parent().unwrap()).unwrap();
    fs::copy(&compressed, &ancestor).unwrap();
    let listed = load_thread_snapshots(&instance).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(rollout::resolve_path(&listed[0].rollout_path).unwrap(), compressed);
    fs::write(&source.rollout_path, &plain).unwrap();
    let listed = load_thread_snapshots(&instance).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(rollout::resolve_path(&listed[0].rollout_path).unwrap(), source.rollout_path);
    drop(database);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn compressed_trash_restore_validates_tmp_magic_and_preserves_encoded_bytes() {
    let base = make_temp_dir("codex-session-compressed-restore");
    let target = base.join("profile/sessions/2026/10/24/rollout-compressed.jsonl.zst");
    let mut entry = make_trash_entry(&base, "compressed", target.clone());
    let plain = fs::read(&entry.trashed_rollout_path).unwrap();
    let encoded = zstd::stream::encode_all(plain.as_slice(), 0).unwrap();
    let source = entry.trashed_rollout_path.with_extension("jsonl.zst");
    fs::write(&source, &encoded).unwrap();
    fs::remove_file(&entry.trashed_rollout_path).unwrap();
    entry.trashed_rollout_path = source;
    restore_trashed_session_entry_with_registration(&entry, |_, _, _, _, _| Ok(())).unwrap();
    assert_eq!(fs::read(&target).unwrap(), encoded);
    assert_eq!(
        rollout_session_id(&target).unwrap().as_deref(),
        Some("compressed")
    );
    assert!(!entry.entry_dir.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn compressed_standalone_export_import_roundtrip_uses_decoded_hash_and_plain_entry() {
    let base = make_temp_dir("codex-session-compressed-export");
    let (snapshot, manifest) = isolated_export_fixture(&base, "compressed-export");
    let original = fs::read(&snapshot.rollout_path).unwrap();
    let encoded = zstd::stream::encode_all(original.as_slice(), 0).unwrap();
    let compressed = snapshot.rollout_path.with_extension("jsonl.zst");
    fs::write(&compressed, &encoded).unwrap();
    fs::remove_file(&snapshot.rollout_path).unwrap();
    // Keep the logical .jsonl source pointer while only its .zst sibling exists.
    let package = base.join("package.zip");
    let summary = export_session_package(vec![snapshot], 1, package.to_string_lossy().into_owned(), None, None).unwrap();
    assert_eq!(summary.exported_session_count, 1);
    let actual_manifest = read_session_export_manifest_from_path(&package).unwrap();
    assert!(actual_manifest.sessions[0].relative_rollout_path.ends_with(".jsonl"));
    assert_eq!(actual_manifest.sessions[0].size_bytes, manifest.sessions[0].size_bytes);
    assert_eq!(actual_manifest.sessions[0].sha256, manifest.sessions[0].sha256);
    let mut archive = ZipArchive::new(File::open(package).unwrap()).unwrap();
    let imported = base.join("target/rollout-imported.jsonl");
    write_imported_rollout_from_archive(&mut archive, &actual_manifest.sessions[0], &imported, None)
        .unwrap();
    assert_eq!(fs::read(imported).unwrap(), original);
    assert_eq!(fs::read(compressed).unwrap(), encoded);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn compressed_metadata_is_bounded_and_export_rejects_missing_history_chain() {
    let base = make_temp_dir("codex-session-compressed-boundaries");
    let too_large = base.join("large.tmp");
    let bytes = vec![b' '; 1024 * 1024 + 2];
    fs::write(
        &too_large,
        zstd::stream::encode_all(bytes.as_slice(), 0).unwrap(),
    )
    .unwrap();
    assert!(read_rollout_session_meta(&too_large).is_err());
    let (mut snapshot, _) = isolated_export_fixture(&base, "chain");
    let bytes = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"chain\",\"history_base\":{\"thread_id\":\"parent\"}}}\n";
    let compressed = snapshot.rollout_path.with_extension("jsonl.zst");
    let encoded = zstd::stream::encode_all(bytes.as_slice(), 0).unwrap();
    fs::write(&compressed, &encoded).unwrap();
    fs::remove_file(&snapshot.rollout_path).unwrap();
    snapshot.rollout_path = compressed.clone();
    assert!(prepare_export_session_sources(vec![snapshot])
        .unwrap_err()
        .contains("分页会话"));
    assert_eq!(fs::read(compressed).unwrap(), encoded);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn export_prefers_plain_sibling_and_preserves_archived_relative_path() {
    let base = make_temp_dir("codex-export-plain-sibling");
    let (mut source, _) = isolated_export_fixture(&base, "plain-sibling");
    let plain = fs::read(&source.rollout_path).unwrap();
    let compressed = source.rollout_path.with_extension("jsonl.zst");
    fs::write(&compressed, zstd::stream::encode_all(plain.as_slice(), 0).unwrap()).unwrap();
    source.rollout_path = compressed;
    source.relative_rollout_path = "archived_sessions/rollout-plain-sibling.jsonl.zst".into();
    let package = base.join("package.zip");
    export_session_package(vec![source], 1, package.to_string_lossy().into_owned(), None, None).unwrap();
    let manifest = read_session_export_manifest_from_path(&package).unwrap();
    assert_eq!(manifest.sessions[0].relative_rollout_path, "archived_sessions/rollout-plain-sibling.jsonl");
    assert_eq!(read_session_export_entry_bytes(&package, &manifest.sessions[0]).unwrap(), plain);
    fs::remove_dir_all(base).unwrap();
}
