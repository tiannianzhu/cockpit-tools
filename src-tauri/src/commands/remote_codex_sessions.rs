use crate::modules::remote_codex_sessions::{
    self, RemoteCodexSession, RemoteCodexSessionList, RemoteCodexSessionPurgeResult,
    RemoteCodexSessionTrashEntry,
};

#[tauri::command]
pub async fn list_remote_codex_sessions(
    server_id: String,
) -> Result<RemoteCodexSessionList, String> {
    remote_codex_sessions::list(&server_id).await
}

#[tauri::command]
pub async fn preview_remote_codex_session_export(
    server_id: String,
    session_ids: Vec<String>,
) -> Result<crate::modules::codex_session_manager::CodexSessionExportPreview, String> {
    remote_codex_sessions::preview_package_export(&server_id, session_ids).await
}

#[tauri::command]
pub async fn export_remote_codex_sessions(
    app: tauri::AppHandle,
    server_id: String,
    session_ids: Vec<String>,
    export_path: String,
    transfer_id: Option<String>,
) -> Result<crate::modules::codex_session_manager::CodexSessionExportSummary, String> {
    remote_codex_sessions::export_package(&server_id, session_ids, export_path, transfer_id, app)
        .await
}

#[tauri::command]
pub async fn preview_remote_codex_session_import(
    server_id: String,
    import_file_path: String,
) -> Result<crate::modules::codex_session_manager::CodexSessionImportPreview, String> {
    remote_codex_sessions::preview_package_import(&server_id, import_file_path).await
}

#[tauri::command]
pub async fn import_remote_codex_sessions(
    app: tauri::AppHandle,
    server_id: String,
    import_file_path: String,
    session_ids: Vec<String>,
    transfer_id: Option<String>,
) -> Result<crate::modules::codex_session_manager::CodexSessionImportSummary, String> {
    remote_codex_sessions::import_package(
        &server_id,
        import_file_path,
        session_ids,
        transfer_id,
        app,
    )
    .await
}

#[tauri::command]
pub async fn trash_remote_codex_session(
    server_id: String,
    session_id: String,
) -> Result<RemoteCodexSessionTrashEntry, String> {
    remote_codex_sessions::trash(&server_id, &session_id).await
}

#[tauri::command]
pub async fn list_remote_codex_session_trash(
    server_id: String,
) -> Result<Vec<RemoteCodexSessionTrashEntry>, String> {
    remote_codex_sessions::list_trash(&server_id).await
}

#[tauri::command]
pub async fn restore_remote_codex_session(
    server_id: String,
    session_id: String,
) -> Result<RemoteCodexSession, String> {
    remote_codex_sessions::restore(&server_id, &session_id).await
}

#[tauri::command]
pub async fn purge_remote_codex_session(
    server_id: String,
    session_id: String,
) -> Result<RemoteCodexSessionPurgeResult, String> {
    remote_codex_sessions::purge(&server_id, &session_id).await
}

#[tauri::command]
pub async fn clear_remote_codex_session_trash(
    server_id: String,
) -> Result<RemoteCodexSessionPurgeResult, String> {
    remote_codex_sessions::clear_trash(&server_id).await
}

#[tauri::command]
pub async fn query_remote_codex_session_usage(
    server_id: String,
    query: crate::modules::codex_session_usage::CodexSessionUsageQuery,
) -> Result<crate::modules::codex_session_usage::CodexSessionUsageReport, String> {
    remote_codex_sessions::query_usage(&server_id, query).await
}

#[tauri::command]
pub async fn sync_remote_codex_session_usage(
    server_id: String,
    rebuild: Option<bool>,
    query: crate::modules::codex_session_usage::CodexSessionUsageQuery,
) -> Result<crate::modules::codex_session_usage::CodexSessionUsageSyncResult, String> {
    crate::modules::remote_codex_sessions::sync_usage(&server_id, rebuild.unwrap_or(false), query)
        .await
}

#[tauri::command]
pub async fn open_remote_codex_session_target(
    server_id: String,
    session_id: String,
    folder: bool,
) -> Result<(), String> {
    crate::modules::remote_codex_sessions::open_target(&server_id, &session_id, folder).await
}
