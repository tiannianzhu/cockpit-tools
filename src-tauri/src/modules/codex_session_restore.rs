// Publish durable history, confirm official registration, then append its index.
fn restore_trashed_session_entry(entry: &TrashedSessionEntry) -> Result<(), String> {
    restore_trashed_session_entry_with_registration(entry, |root, id, path, archived, title| {
        modules::codex_official_app_server::register_restored_thread(
            root,
            id,
            path,
            archived,
            Some(title),
        )?;
        let thread = modules::codex_official_app_server::read_thread(root, id)?;
        let final_path = thread
            .get("path")
            .and_then(JsonValue::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| format!("官方恢复后缺少会话文件路径: {}", id))?;
        let canonical_root = fs::canonicalize(root)
            .map_err(|error| format!("无法确认 Codex 实例目录: {}", error))?;
        let canonical_path = rollout::resolve_path(&final_path)
            .and_then(fs::canonicalize)
            .map_err(|error| format!("无法确认恢复后的会话文件: {}", error))?;
        let expected_dir = if archived {
            "archived_sessions"
        } else {
            "sessions"
        };
        if !canonical_path.starts_with(canonical_root.join(expected_dir))
            || !canonical_path.is_file()
            || rollout_session_id(&canonical_path)?.as_deref() != Some(id)
        {
            return Err(format!("恢复后的会话文件与官方记录不一致: {}", id));
        }
        Ok(())
    })
}

fn restore_trashed_session_entry_with_registration<F>(
    entry: &TrashedSessionEntry,
    register: F,
) -> Result<(), String>
where
    F: FnOnce(&Path, &str, &Path, bool, &str) -> Result<(), String>,
{
    if !entry.trashed_rollout_path.exists() {
        return Err(format!(
            "废纸篓中的会话文件不存在，无法恢复 ({}): {}",
            entry.manifest.session_id,
            entry.trashed_rollout_path.display()
        ));
    }

    let session_id = entry.manifest.session_id.clone();
    if let Some(trashed_session_id) = rollout_session_id(&entry.trashed_rollout_path)? {
        if trashed_session_id != session_id {
            return Err(format!(
                "废纸篓中的会话文件与清单不一致，无法恢复 (清单: {}, 文件: {}): {}",
                session_id,
                trashed_session_id,
                entry.trashed_rollout_path.display()
            ));
        }
    }

    let target_rollout_path = match rollout::resolve_path(&entry.manifest.original_rollout_path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            entry.manifest.original_rollout_path.clone()
        }
        Err(error) => return Err(format!("无法定位会话恢复目标: {error}")),
    };
    let published = restore_publish::copy_if_absent(
        &entry.trashed_rollout_path,
        &target_rollout_path,
        |staged| {
            if rollout_session_id(staged)?.as_deref() != Some(session_id.as_str()) {
                return Err(format!(
                    "废纸篓中的会话文件与清单不一致，无法恢复 ({}): {}",
                    session_id,
                    entry.trashed_rollout_path.display()
                ));
            }
            Ok(())
        },
    )
    .map_err(|error| {
        format!(
            "恢复会话文件失败 ({} -> {}): {}",
            entry.trashed_rollout_path.display(),
            target_rollout_path.display(),
            error
        )
    })?;

    if !published {
        match rollout_session_id(&target_rollout_path)? {
            Some(existing_session_id) if existing_session_id == session_id => {}
            Some(existing_session_id) => {
                return Err(format!(
                    "目标位置已存在不同会话文件，为避免覆盖，无法恢复 (待恢复: {}, 已存在: {}): {}",
                    session_id,
                    existing_session_id,
                    target_rollout_path.display()
                ));
            }
            None => {
                return Err(format!(
                    "目标位置已存在无法确认会话 ID 的文件，为避免覆盖，无法恢复 ({}): {}",
                    session_id,
                    target_rollout_path.display()
                ));
            }
        }
    }

    let session_index_entry = build_restored_session_index_entry(entry, &target_rollout_path);
    let archived =
        Path::new(&entry.manifest.relative_rollout_path).starts_with("archived_sessions");
    register(
        &entry.manifest.instance_root,
        &session_id,
        &target_rollout_path,
        archived,
        &entry.manifest.title,
    )
    .map_err(|error| {
        format!(
            "会话文件已复制，但官方恢复未完成: {}。废纸篓备份已保留",
            error
        )
    })?;
    upsert_session_index_with_entry(
        &entry.manifest.instance_root,
        &session_id,
        &session_index_entry,
    )
    .map_err(|error| {
        modules::i18n::translate(
            &modules::config::get_user_config().language,
            "codex.sessionManager.messages.restoreIndexRetry",
            &[("error", &error)],
        )
    })?;
    // Never roll back a published rollout or index snapshot: Codex may already
    // have appended history. Both the durable file and backup survive any error.

    if let Err(error) = fs::remove_dir_all(&entry.entry_dir) {
        modules::logger::log_warn(&format!(
            "会话已恢复，但清理废纸篓条目失败 ({}): {}",
            entry.entry_dir.display(),
            error
        ));
    } else {
        cleanup_empty_trash_ancestors(&entry.entry_dir);
    }

    Ok(())
}

fn build_restored_session_index_entry(
    entry: &TrashedSessionEntry,
    rollout_path: &Path,
) -> JsonValue {
    let mut restored = entry.manifest.session_index_entry.clone();
    if !restored.is_object() {
        restored = json!({});
    }
    let object = restored
        .as_object_mut()
        .expect("restored index is an object");
    object.insert(
        "id".to_string(),
        JsonValue::String(entry.manifest.session_id.clone()),
    );
    if !entry.manifest.title.trim().is_empty() {
        object
            .entry("thread_name".to_string())
            .or_insert_with(|| JsonValue::String(entry.manifest.title.clone()));
    }
    if let Some(updated_at) = rollout_file_activity_seconds(rollout_path)
        .or_else(|| rollout_file_modified_seconds(rollout_path))
    {
        object.insert(
            "updated_at".to_string(),
            JsonValue::String(format_session_index_updated_at(updated_at)),
        );
    }
    restored
}
