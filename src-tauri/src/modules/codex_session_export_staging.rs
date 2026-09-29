// Stage decoded standalone history once for both local and remote packages.
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
    }
}

fn prepare_export_session_sources(
    mut sources: Vec<SessionPackageSource>,
) -> Result<(tempfile::TempDir, Vec<SessionPackageSource>), String> {
    let staging = tempfile::tempdir().map_err(|error| history_read_error(&error.to_string()))?;
    for (number, source) in sources.iter_mut().enumerate() {
        source.rollout_path = cockpit_session_usage::rollout::resolve_path(&source.rollout_path)
            .map_err(|error| history_read_error(&error.to_string()))?;
        source.relative_rollout_path = source.relative_rollout_path.strip_suffix(".zst")
            .unwrap_or(&source.relative_rollout_path).to_owned();
        if rollout_reader::is_compressed(&source.rollout_path) {
            let path = staging.path().join(format!("{number}.jsonl"));
            rollout_reader::materialize_bounded(&source.rollout_path, &path)
                .map_err(|error| history_read_error(&error.to_string()))?;
            source.rollout_path = path;
        }
        modules::codex_session_import_paths::ensure_standalone_rollout_reader(BufReader::new(
            File::open(&source.rollout_path).map_err(|error| error.to_string())?,
        ))?;
    }
    Ok((staging, sources))
}
