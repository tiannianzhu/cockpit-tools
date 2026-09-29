// Stage standalone history once for both local and remote packages.
fn snapshot_export_source(instance: CodexSyncInstance, snapshot: ThreadSnapshot) -> SessionPackageSource {
    SessionPackageSource {
        session_id: snapshot.id.clone(),
        title: snapshot.title.clone(),
        cwd: snapshot.cwd.clone(),
        updated_at: snapshot.updated_at,
        relative_rollout_path: snapshot_relative_rollout_path(&snapshot),
        rollout_path: snapshot.rollout_path,
        session_index_entry: snapshot.session_index_entry,
        source_instance_id: instance.id,
        source_instance_name: instance.name,
        source_root: Some(snapshot.source_root),
    }
}

fn prepare_export_session_sources(
    mut sources: Vec<SessionPackageSource>,
) -> Result<(tempfile::TempDir, Vec<SessionPackageSource>), String> {
    let staging = tempfile::tempdir().map_err(|error| history_read_error(&error.to_string()))?;
    let mut catalog = portable_history::Catalog::default();
    for (number, source) in sources.iter_mut().enumerate() {
        prepare_export_session_source(source, staging.path().join(number.to_string()), &mut catalog)?;
    }
    Ok((staging, sources))
}

fn prepare_export_session_source(
    source: &mut SessionPackageSource,
    root: PathBuf,
    catalog: &mut portable_history::Catalog,
) -> Result<(), String> {
    source.rollout_path = rollout::resolve_path(&source.rollout_path)
        .map_err(|error| history_read_error(&error.to_string()))?;
    let relative = source.relative_rollout_path.strip_suffix(".zst")
        .unwrap_or(&source.relative_rollout_path);
    let relative = normalize_package_entry_path(relative)
        .ok_or_else(|| history_read_error("invalid rollout path"))?;
    let mut relative = PathBuf::from(relative);
    let mut prepare = || -> Result<PathBuf, String> {
        let before = fs::metadata(&source.rollout_path).map_err(|e| e.to_string())?;
        let metadata = rollout_reader::metadata(&source.rollout_path)
            .map_err(|e| e.to_string())?.ok_or("missing session metadata")?;
        if portable_history::is_paginated(&metadata) {
            // Official standalone filenames must carry the logical thread ID.
            if let Some((logical, physical)) =
                modules::codex_history_projection_repair::rollout_file_ids(&relative)
            {
                if logical != physical {
                    let filename = relative.file_name().and_then(|name| name.to_str())
                        .ok_or("invalid rollout filename")?;
                    relative.set_file_name(format!("{}-{logical}.jsonl", &filename[..27]));
                }
            }
        }
        let path = root.join(&relative);
        fs::create_dir_all(path.parent().ok_or("missing staging directory")?)
            .map_err(|e| e.to_string())?;
        if portable_history::is_paginated(&metadata) {
            let source_root = source.source_root.as_deref()
                .ok_or("paginated history source root is unavailable")?;
            portable_history::write_portable_rollout(
                source_root, &source.rollout_path, metadata, &path, catalog,
            )?;
        } else if rollout_reader::is_compressed(&source.rollout_path) {
            rollout_reader::materialize_bounded(&source.rollout_path, &path)
                .map_err(|e| e.to_string())?;
        } else {
            let mut input = File::open(&source.rollout_path).map_err(|e| e.to_string())?
                .take(before.len());
            let mut output = File::create(&path).map_err(|e| e.to_string())?;
            if std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())? != before.len() {
                return Err("rollout changed during export".into());
            }
        }
        let after = fs::metadata(&source.rollout_path).map_err(|e| e.to_string())?;
        if after.len() != before.len() || after.modified().ok() != before.modified().ok() {
            return Err("rollout changed during export".into());
        }
        modules::codex_session_import_paths::ensure_standalone_rollout_reader(BufReader::new(
            File::open(&path).map_err(|e| e.to_string())?,
        ))?;
        Ok(path)
    };
    let path = prepare().map_err(|error| history_read_error(&error))?;
    source.rollout_path = path;
    source.relative_rollout_path = path_to_package_path(&relative)
        .ok_or_else(|| history_read_error("invalid rollout path"))?;
    source.source_root = Some(root);
    Ok(())
}
