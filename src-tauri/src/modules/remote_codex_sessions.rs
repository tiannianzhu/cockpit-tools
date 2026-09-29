use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use tauri::Emitter;
use uuid::Uuid;

const EXPORT_LIMIT: u64 = 64 * 1024 * 1024;
const CHUNK_SIZE: usize = 512 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCodexSession {
    pub id: String,
    pub title: String,
    pub cwd: Option<String>,
    pub updated_at: i64,
    pub session_kind: String,
    #[serde(default)]
    pub parent_thread_id: Option<String>,
    pub archived: bool,
    pub size_bytes: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCodexSessionList {
    pub sessions: Vec<RemoteCodexSession>,
    pub total: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCodexSessionTrashEntry {
    pub id: String,
    pub title: String,
    pub cwd: Option<String>,
    pub session_kind: String,
    pub parent_thread_id: Option<String>,
    pub trash_root_id: String,
    pub deleted_at: i64,
    pub archived: bool,
    pub size_bytes: u64,
    #[serde(default)]
    pub session_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCodexSessionPurgeResult {
    pub purged_roots: usize,
    pub purged_sessions: usize,
}

struct ExportStaging(PathBuf);

impl Drop for ExportStaging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn emit_package_progress(
    app: &tauri::AppHandle,
    transfer_id: Option<&str>,
    operation: &str,
    phase: &str,
    current: usize,
    total: usize,
    current_label: Option<String>,
    running: bool,
) {
    let Some(transfer_id) = transfer_id else {
        return;
    };
    let _ = app.emit(
        crate::modules::codex_session_manager::SESSION_TRANSFER_PROGRESS_EVENT,
        crate::modules::codex_session_manager::CodexSessionTransferProgress {
            transfer_id: transfer_id.to_string(),
            operation: operation.to_string(),
            phase: phase.to_string(),
            current,
            total,
            percent: if total == 0 {
                0
            } else {
                ((current.min(total) * 100) / total).min(100) as u8
            },
            current_label,
            running,
        },
    );
}

async fn selected_package_sessions(
    server_id: &str,
    session_ids: Vec<String>,
    expand_descendants: bool,
) -> Result<(usize, Vec<RemoteCodexSession>, String), String> {
    let requested: HashSet<String> = session_ids
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if requested.is_empty() {
        return Err("请至少选择一条会话".to_string());
    }
    for id in &requested {
        valid_id(id)?;
    }
    let server = crate::modules::ssh_server::list_servers()?
        .servers
        .into_iter()
        .find(|server| server.id == server_id)
        .ok_or("Remote host is no longer registered")?;
    let rows = list(server_id).await?.sessions;
    let mut included = requested.clone();
    if expand_descendants {
        loop {
            let before = included.len();
            for row in &rows {
                if row
                    .parent_thread_id
                    .as_ref()
                    .is_some_and(|parent| included.contains(parent))
                {
                    included.insert(row.id.clone());
                }
            }
            if included.len() == before {
                break;
            }
        }
    }
    let selected = rows
        .into_iter()
        .filter(|row| included.contains(&row.id))
        .collect();
    Ok((requested.len(), selected, server.name))
}

pub async fn preview_package_export(
    server_id: &str,
    session_ids: Vec<String>,
) -> Result<crate::modules::codex_session_manager::CodexSessionExportPreview, String> {
    use crate::modules::codex_session_manager::{
        CodexSessionExportPreview, CodexSessionExportPreviewItem,
    };
    let (requested_count, rows, source_name) =
        selected_package_sessions(server_id, session_ids, true).await?;
    let total_size_bytes = rows.iter().map(|row| row.size_bytes).sum();
    let items = rows
        .into_iter()
        .map(|row| CodexSessionExportPreviewItem {
            session_id: row.id,
            title: row.title,
            cwd: row.cwd.unwrap_or_default(),
            updated_at: Some(row.updated_at),
            size_bytes: row.size_bytes,
            source_instance_id: server_id.to_string(),
            source_instance_name: source_name.clone(),
        })
        .collect::<Vec<_>>();
    Ok(CodexSessionExportPreview {
        requested_session_count: requested_count,
        exportable_session_count: items.len(),
        missing_session_count: requested_count.saturating_sub(items.len()),
        total_size_bytes,
        items,
    })
}

pub async fn export_package(
    server_id: &str,
    session_ids: Vec<String>,
    output_path: String,
    transfer_id: Option<String>,
    app: tauri::AppHandle,
) -> Result<crate::modules::codex_session_manager::CodexSessionExportSummary, String> {
    use crate::modules::codex_session_manager::{export_session_package, SessionPackageSource};
    let (requested_count, rows, source_name) =
        selected_package_sessions(server_id, session_ids, false).await?;
    let directory = std::env::temp_dir().join(format!("cockpit-remote-export-{}", Uuid::new_v4()));
    std::fs::create_dir(&directory).map_err(|e| format!("Cannot stage remote export: {e}"))?;
    let staging = ExportStaging(directory);
    let mut sources = Vec::with_capacity(rows.len());
    let total = rows.len();
    for (index, row) in rows.into_iter().enumerate() {
        emit_package_progress(
            &app,
            transfer_id.as_deref(),
            "export",
            "download",
            index,
            total,
            Some(row.title.clone()),
            true,
        );
        let staged = staging.0.join(format!("rollout-{}.jsonl", row.id));
        let relative_rollout_path = download_session(
            server_id,
            &row.id,
            staged.to_str().ok_or("Invalid staging path")?,
        )
        .await?;
        sources.push(SessionPackageSource {
            session_id: row.id.clone(),
            title: row.title.clone(),
            cwd: row.cwd.unwrap_or_default(),
            updated_at: Some(row.updated_at),
            relative_rollout_path,
            rollout_path: staged,
            session_index_entry: json!({"id": row.id, "thread_name": row.title}),
            source_instance_id: server_id.to_string(),
            source_instance_name: source_name.clone(),
        });
        emit_package_progress(
            &app,
            transfer_id.as_deref(),
            "export",
            "download",
            index + 1,
            total,
            None,
            true,
        );
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _staging = staging;
        let reporter = |progress| {
            let _ = app.emit(
                crate::modules::codex_session_manager::SESSION_TRANSFER_PROGRESS_EVENT,
                progress,
            );
        };
        export_session_package(
            sources,
            requested_count,
            output_path,
            transfer_id,
            Some(&reporter),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn existing_session_ids(server_id: &str) -> Result<HashSet<String>, String> {
    let result = call(server_id, json!({"action":"existingIds"})).await?;
    let ids: Vec<String> =
        serde_json::from_value(result).map_err(|e| format!("Invalid remote ID list: {e}"))?;
    Ok(ids.into_iter().collect())
}

pub async fn preview_package_import(
    server_id: &str,
    import_file_path: String,
) -> Result<crate::modules::codex_session_manager::CodexSessionImportPreview, String> {
    use crate::modules::codex_session_manager::{
        read_session_export_manifest_from_path, validate_manifest_item, CodexSessionImportPreview,
        CodexSessionImportPreviewItem,
    };
    let path = PathBuf::from(import_file_path.trim());
    let manifest =
        tauri::async_runtime::spawn_blocking(move || read_session_export_manifest_from_path(&path))
            .await
            .map_err(|e| e.to_string())??;
    let server = crate::modules::ssh_server::list_servers()?
        .servers
        .into_iter()
        .find(|server| server.id == server_id)
        .ok_or("Remote host is no longer registered")?;
    let existing = existing_session_ids(server_id).await?;
    let items = manifest
        .sessions
        .iter()
        .map(|item| {
            let (status, reason) = if validate_manifest_item(item).is_err() {
                ("invalid", Some("会话包条目无效".to_string()))
            } else if existing.contains(&item.session_id) {
                (
                    "conflict",
                    Some("目标主机已存在相同 ID 的会话，已跳过避免覆盖".to_string()),
                )
            } else {
                ("ready", None)
            };
            CodexSessionImportPreviewItem {
                session_id: item.session_id.clone(),
                title: item.title.clone(),
                cwd: item.cwd.clone(),
                updated_at: item.updated_at,
                size_bytes: item.size_bytes,
                status: status.to_string(),
                reason,
                existing_instance_names: if existing.contains(&item.session_id) {
                    vec![server.name.clone()]
                } else {
                    Vec::new()
                },
            }
        })
        .collect::<Vec<_>>();
    let importable_session_count = items.iter().filter(|item| item.status == "ready").count();
    Ok(CodexSessionImportPreview {
        package_version: manifest.package_version,
        exported_at: Some(manifest.exported_at),
        import_file_path,
        target_instance_id: server_id.to_string(),
        target_instance_name: server.name,
        total_session_count: items.len(),
        importable_session_count,
        items,
    })
}

pub async fn import_package(
    server_id: &str,
    import_file_path: String,
    session_ids: Vec<String>,
    transfer_id: Option<String>,
    app: tauri::AppHandle,
) -> Result<crate::modules::codex_session_manager::CodexSessionImportSummary, String> {
    use crate::modules::codex_session_manager::{
        package_import_relative_path, read_session_export_entry_bytes,
        read_session_export_manifest_from_path, validate_manifest_item, CodexSessionImportSummary,
    };
    let requested: HashSet<String> = session_ids
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if requested.is_empty() {
        return Err("请至少选择一条要导入的会话".to_string());
    }
    let path = PathBuf::from(import_file_path.trim());
    let manifest =
        tauri::async_runtime::spawn_blocking(move || read_session_export_manifest_from_path(&path))
            .await
            .map_err(|e| e.to_string())??;
    let selected = manifest
        .sessions
        .iter()
        .filter(|item| requested.contains(&item.session_id))
        .cloned()
        .collect::<Vec<_>>();
    let mut selected_ids = HashSet::new();
    for item in &selected {
        validate_manifest_item(item)?;
        valid_id(&item.session_id)?;
        if !selected_ids.insert(item.session_id.clone()) {
            return Err("Duplicate session ID in package manifest".to_string());
        }
    }
    let server = crate::modules::ssh_server::list_servers()?
        .servers
        .into_iter()
        .find(|server| server.id == server_id)
        .ok_or("Remote host is no longer registered")?;
    let existing = existing_session_ids(server_id).await?;
    if let Some(item) = selected
        .iter()
        .find(|item| existing.contains(&item.session_id))
    {
        return Err(format!(
            "Remote session ID already exists: {}",
            item.session_id
        ));
    }
    if selected.is_empty() {
        return Ok(CodexSessionImportSummary {
            requested_session_count: requested.len(),
            imported_session_count: 0,
            skipped_session_count: requested.len(),
            target_instance_id: server_id.to_string(),
            target_instance_name: server.name,
            message: "没有匹配的会话可导入".to_string(),
        });
    }
    let staging_id = Uuid::new_v4().to_string();
    let mut members = Vec::with_capacity(selected.len());
    let stage_result = async {
        for (index, item) in selected.iter().enumerate() {
            emit_package_progress(&app, transfer_id.as_deref(), "import", "read", index, selected.len(), Some(item.title.clone()), true);
            let package_path = PathBuf::from(import_file_path.trim());
            let item_copy = item.clone();
            let bytes = tauri::async_runtime::spawn_blocking(move || {
                read_session_export_entry_bytes(&package_path, &item_copy)
            })
            .await
            .map_err(|e| e.to_string())??;
            if bytes.is_empty() {
                return Err("Empty session rollout in package".to_string());
            }
            for (index, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
                call(
                    server_id,
                    json!({"action":"importStage","transferId":staging_id,"id":item.session_id,"offset":index*CHUNK_SIZE,"totalBytes":bytes.len(),"sha256":item.sha256,"data":STANDARD.encode(chunk)}),
                )
                .await?;
            }
            let relative_path = package_import_relative_path(item);
            let archived = relative_path.starts_with("archived_sessions/");
            members.push(json!({"id":item.session_id,"title":item.title,"relativePath":relative_path,"archived":archived,"sha256":item.sha256,"sizeBytes":item.size_bytes}));
            emit_package_progress(&app, transfer_id.as_deref(), "import", "write", index+1, selected.len(), None, true);
        }
        emit_package_progress(&app, transfer_id.as_deref(), "import", "rebuild", selected.len(), selected.len(), None, true);
        let result = call(
            server_id,
            json!({"action":"importCommit","transferId":staging_id,"members":members}),
        )
        .await
        .map_err(|error| format!("{error}; remote import may have registered some sessions or copied files; refresh the session list before retrying"))?;
        Ok::<usize, String>(result.get("importedCount").and_then(Value::as_u64).ok_or("Missing import count")? as usize)
    }
    .await;
    if stage_result.is_err() {
        let _ = call(
            server_id,
            json!({"action":"importAbort","transferId":staging_id}),
        )
        .await;
    }
    let imported_count = stage_result?;
    emit_package_progress(
        &app,
        transfer_id.as_deref(),
        "import",
        "done",
        imported_count,
        selected.len(),
        None,
        false,
    );
    Ok(CodexSessionImportSummary {
        requested_session_count: requested.len(),
        imported_session_count: imported_count,
        skipped_session_count: requested.len().saturating_sub(imported_count),
        target_instance_id: server_id.to_string(),
        target_instance_name: server.name.clone(),
        message: format!("已导入 {} 条会话到 {}", imported_count, server.name),
    })
}

async fn call(server_id: &str, payload: Value) -> Result<Value, String> {
    let output =
        crate::modules::ssh_server::run_remote_python(server_id, REMOTE_SCRIPT, &payload).await?;
    let response: Value = serde_json::from_str(output.trim())
        .map_err(|e| format!("Invalid remote session response: {e}"))?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Remote session operation failed")
            .to_string());
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

fn valid_id(id: &str) -> Result<(), String> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| "Invalid session ID".to_string())
}

pub async fn open_target(server_id: &str, session_id: &str, folder: bool) -> Result<(), String> {
    valid_id(session_id)?;
    let server = crate::modules::ssh_server::list_servers()?
        .servers
        .into_iter()
        .find(|server| server.id == server_id)
        .ok_or("Remote host is no longer registered")?;
    let config = tokio::process::Command::new("ssh")
        .args(["-G", "--", &server.host])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !config.status.success() {
        return Err("无法读取 VS Code 使用的 SSH 配置".into());
    }
    let config = String::from_utf8_lossy(&config.stdout);
    let setting = |key: &str| {
        config.lines().find_map(|line| {
            line.split_once(' ')
                .filter(|(name, _)| *name == key)
                .map(|(_, value)| value.trim())
        })
    };
    if (!server.username.is_empty() && setting("user") != Some(server.username.as_str()))
        || (server.port != 0
            && setting("port").and_then(|port| port.parse::<u16>().ok()) != Some(server.port))
    {
        return Err("此主机的 SSH 别名配置与 Cockpit 的用户名或端口不一致，请先在 SSH 配置中保持一致后使用 VS Code 打开".into());
    }
    if let crate::models::ssh_server::SshAuthConfig::PrivateKeyFile { path } = &server.auth {
        if !config
            .lines()
            .any(|line| line.strip_prefix("identityfile ") == Some(path.as_str()))
        {
            return Err(
                "请先在此主机的 SSH 配置中设置与 Cockpit 一致的 IdentityFile，再使用 VS Code 打开"
                    .into(),
            );
        }
    }
    let result = call(
        server_id,
        json!({"action":"location", "id":session_id, "folder":folder}),
    )
    .await?;
    let path = result
        .get("path")
        .and_then(Value::as_str)
        .ok_or("Missing remote path")?;
    // VS Code Remote SSH resolves this host through the user's existing SSH config.
    let uri = format!(
        "vscode-remote://ssh-remote+{}/{}",
        urlencoding::encode(&server.host),
        path.trim_start_matches('/')
            .split('/')
            .map(|part| urlencoding::encode(part).into_owned())
            .collect::<Vec<_>>()
            .join("/")
    );
    #[cfg(target_os = "macos")]
    let executable = "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code";
    #[cfg(not(target_os = "macos"))]
    let executable = "code";
    let output = tokio::process::Command::new(executable)
        .arg(if folder { "--folder-uri" } else { "--file-uri" })
        .arg(uri)
        .output()
        .await
        .map_err(|e| format!("无法启动 VS Code（需要 Remote SSH 扩展）：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "无法打开远程会话：{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

const USAGE_SOURCES: [(&str, &str); 4] = [
    (
        "Cargo.toml",
        include_str!("../../../crates/cockpit-session-usage/Cargo.toml"),
    ),
    (
        "Cargo.lock",
        include_str!("../../../crates/cockpit-session-usage/Cargo.lock"),
    ),
    (
        "src/lib.rs",
        include_str!("../../../crates/cockpit-session-usage/src/lib.rs"),
    ),
    (
        "src/main.rs",
        include_str!("../../../crates/cockpit-session-usage/src/main.rs"),
    ),
];

async fn usage_request(
    server_id: &str,
    action: &str,
    rebuild: bool,
    query: &crate::modules::codex_session_usage::CodexSessionUsageQuery,
) -> Result<Value, String> {
    use sha2::{Digest, Sha256};
    let server = crate::modules::ssh_server::list_servers()?
        .servers
        .into_iter()
        .find(|server| server.id == server_id)
        .ok_or("Remote host is no longer registered")?;
    let mut hash = Sha256::new();
    for (name, content) in USAGE_SOURCES {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(content.as_bytes());
        hash.update([0]);
    }
    let mut request = json!({"action":action, "version":format!("{:x}", hash.finalize()),
        "instanceId":server_id, "instanceName":if server.name.is_empty() {server.host} else {server.name},
        "rebuild":rebuild, "query":query});
    for install in [false, true] {
        if install {
            request["sources"] = serde_json::to_value(
                USAGE_SOURCES
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
            )
            .map_err(|e| e.to_string())?;
        }
        let output = crate::modules::ssh_server::run_remote_usage_python(
            server_id,
            include_str!("remote_usage.py"),
            &request,
        )
        .await?;
        let response: Value = serde_json::from_str(output.trim())
            .map_err(|e| format!("Invalid remote usage response: {e}"))?;
        if response["ok"] != true {
            return Err(response["error"]
                .as_str()
                .unwrap_or("Remote usage failed")
                .to_string());
        }
        if response["result"]["needsInstall"] == true {
            continue;
        }
        return Ok(response["result"]["report"].clone());
    }
    Err("远端统计程序安装后不可用".into())
}

pub async fn query_usage(
    server_id: &str,
    query: crate::modules::codex_session_usage::CodexSessionUsageQuery,
) -> Result<crate::modules::codex_session_usage::CodexSessionUsageReport, String> {
    let mut report =
        serde_json::from_value(usage_request(server_id, "query", false, &query).await?)
            .map_err(|e| format!("Invalid remote usage report: {e}"))?;
    crate::modules::codex_session_usage::apply_report_cost(&mut report);
    Ok(report)
}

pub async fn sync_usage(
    server_id: &str,
    rebuild: bool,
    query: crate::modules::codex_session_usage::CodexSessionUsageQuery,
) -> Result<crate::modules::codex_session_usage::CodexSessionUsageSyncResult, String> {
    serde_json::from_value(usage_request(server_id, "sync", rebuild, &query).await?)
        .map_err(|e| format!("Invalid remote usage result: {e}"))
}

pub async fn list(server_id: &str) -> Result<RemoteCodexSessionList, String> {
    let result = call(server_id, json!({"action":"list"})).await?;
    serde_json::from_value(result).map_err(|e| format!("Invalid session list: {e}"))
}

pub async fn trash(
    server_id: &str,
    session_id: &str,
) -> Result<RemoteCodexSessionTrashEntry, String> {
    valid_id(session_id)?;
    let result = call(server_id, json!({"action":"trash", "id":session_id})).await?;
    serde_json::from_value(result).map_err(|e| format!("Invalid trash result: {e}"))
}

pub async fn list_trash(server_id: &str) -> Result<Vec<RemoteCodexSessionTrashEntry>, String> {
    let result = call(server_id, json!({"action":"listTrash"})).await?;
    serde_json::from_value(result).map_err(|e| format!("Invalid trash list: {e}"))
}

pub async fn restore(server_id: &str, session_id: &str) -> Result<RemoteCodexSession, String> {
    valid_id(session_id)?;
    let result = call(server_id, json!({"action":"restore", "id":session_id})).await?;
    serde_json::from_value(result).map_err(|e| format!("Invalid restore result: {e}"))
}

pub async fn purge(
    server_id: &str,
    session_id: &str,
) -> Result<RemoteCodexSessionPurgeResult, String> {
    valid_id(session_id)?;
    let result = call(server_id, json!({"action":"purge", "id":session_id})).await?;
    serde_json::from_value(result).map_err(|e| format!("Invalid purge result: {e}"))
}

pub async fn clear_trash(server_id: &str) -> Result<RemoteCodexSessionPurgeResult, String> {
    let result = call(server_id, json!({"action":"clearTrash"})).await?;
    serde_json::from_value(result).map_err(|e| format!("Invalid clear trash result: {e}"))
}

async fn download_session(
    server_id: &str,
    session_id: &str,
    output_path: &str,
) -> Result<String, String> {
    valid_id(session_id)?;
    let target = Path::new(output_path);
    if target.extension().and_then(|s| s.to_str()) != Some("jsonl")
        || !target.parent().is_some_and(Path::is_dir)
    {
        return Err("Choose an existing directory and a .jsonl output filename".to_string());
    }
    if target.exists() {
        return Err("Export target already exists".to_string());
    }
    let first = call(
        server_id,
        json!({"action":"readChunk", "id":session_id, "offset":0, "size":CHUNK_SIZE}),
    )
    .await?;
    let total = first
        .get("totalBytes")
        .and_then(Value::as_u64)
        .ok_or("Missing transcript size")?;
    let signature = first
        .get("signature")
        .and_then(Value::as_str)
        .ok_or("Missing transcript signature")?
        .to_string();
    let relative_path = first
        .get("relativePath")
        .and_then(Value::as_str)
        .ok_or("Missing remote rollout path")?
        .to_string();
    if total > EXPORT_LIMIT {
        return Err("Transcript exceeds the 64 MiB export limit".to_string());
    }
    let parent = target.parent().ok_or("Missing export directory")?;
    let (mut temp, temp_path) = tempfile_in(parent)?;
    let result = async {
        let mut offset = 0u64;
        let mut chunk = first;
        loop {
            if chunk.get("totalBytes").and_then(Value::as_u64) != Some(total) {
                return Err("Remote transcript changed during export".to_string());
            }
            if chunk.get("signature").and_then(Value::as_str) != Some(signature.as_str()) {
                return Err("Remote transcript changed during export".to_string());
            }
            let encoded = chunk
                .get("data")
                .and_then(Value::as_str)
                .ok_or("Missing export chunk")?;
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|e| format!("Invalid export chunk: {e}"))?;
            if bytes.is_empty() && offset < total {
                return Err("Remote export ended early".to_string());
            }
            temp.write_all(&bytes)
                .map_err(|e| format!("Cannot write export: {e}"))?;
            offset += bytes.len() as u64;
            if offset == total {
                break;
            }
            if offset > total {
                return Err("Remote export exceeded expected size".to_string());
            }
            chunk = call(
                server_id,
                json!({"action":"readChunk", "id":session_id, "offset":offset, "size":CHUNK_SIZE}),
            )
            .await?;
        }
        temp.sync_all()
            .map_err(|e| format!("Cannot sync export: {e}"))?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    // No overwrite: a user or another export may have created the target meanwhile.
    if let Err(error) = std::fs::hard_link(&temp_path, target) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("Cannot finalize export: {error}"));
    }
    std::fs::remove_file(&temp_path).map_err(|e| format!("Cannot remove temporary export: {e}"))?;
    Ok(relative_path)
}

fn tempfile_in(parent: &Path) -> Result<(std::fs::File, PathBuf), String> {
    // Kept separate from the final filename so the final link cannot replace an existing file.
    let path = parent.join(format!(".codex-remote-export-{}.tmp", Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|e| format!("Cannot create temporary export: {e}"))?;
    Ok((file, path))
}

const REMOTE_SCRIPT: &str = r#"
import base64, contextlib, fcntl, hashlib, json, os, pathlib, queue, re, shutil, sqlite3, subprocess, sys, threading, time, uuid

def fail(message):
    raise ValueError(message)

def session_id(value):
    try: return str(uuid.UUID(value))
    except (ValueError, AttributeError, TypeError): fail('Invalid session ID')

def safe_file(path):
    if path.is_symlink() or not path.is_file(): fail('Unsafe or missing session file')
    return path

def safe_dir(path):
    if path.is_symlink(): fail('Symlinked session directory is not allowed')
    return path

def rollout_path(root, thread):
    raw = thread.get('path')
    if not isinstance(raw, str) or not raw: fail('Official thread has no rollout path')
    path = pathlib.Path(raw)
    if not path.is_absolute(): fail('Official rollout path is not absolute')
    if path.is_symlink(): fail('Symlinked session file is not allowed')
    path = path.resolve(strict=True)
    if root not in path.parents or path.relative_to(root).parts[0] not in ('sessions','archived_sessions'):
        fail('Official rollout path is outside the registered Codex home')
    safe_file(path)
    return path

def relative_path(value):
    if not isinstance(value, str): fail('Invalid trash manifest path')
    path = pathlib.PurePosixPath(value)
    if path.is_absolute() or '..' in path.parts or len(path.parts) < 2 or path.parts[0] not in ('sessions','archived_sessions') or path.suffix != '.jsonl':
        fail('Invalid trash manifest path')
    return pathlib.Path(*path.parts)

def trash_root(root):
    data_dir = safe_dir(pathlib.Path.home() / '.antigravity_cockpit')
    return safe_dir(data_dir / 'cockpit-tools-codex-session-trash')

def import_directory(root,transfer):
    return safe_dir(safe_dir(root/'cockpit-tools-remote-session-import')/session_id(transfer))

def trash_members(root, sid):
    entry = safe_dir(trash_root(root) / sid)
    data = json.loads(safe_file(entry / 'manifest.json').read_text())
    if data.get('id') != sid: fail('Trash manifest ID mismatch')
    members = [(data,safe_file(entry / 'rollout.jsonl'),relative_path(data.get('relativePath')))]
    seen = {sid}
    for member in data.get('descendants',[]):
        mid = session_id(member.get('id'))
        if mid in seen: fail('Duplicate session in trash manifest')
        seen.add(mid)
        if member.get('dbOnly') is True:
            members.append((member,None,None))
        else:
            members.append((member,safe_file(entry / ('rollout-' + mid + '.jsonl')),relative_path(member.get('relativePath'))))
    for member,file,relative in members:
        if file is None: continue
        mid=session_id(member.get('id'))
        stem=relative.stem
        if '_' in stem:
            stem,segment=stem.rsplit('_',1)
            session_id(segment)
        if not stem.startswith('rollout-') or not stem.endswith('-'+mid): fail('Trash manifest path ID mismatch')
        if backup_session_meta(file).get('id')!=mid: fail('Trash backup session ID mismatch')
    return data,members

def backup_session_meta(file):
    with file.open('rb') as stream: first=stream.readline(1024*1024)
    try: record=json.loads(first)
    except (ValueError,UnicodeError): return {}
    payload=record.get('payload') if isinstance(record,dict) and record.get('type')=='session_meta' else None
    return payload if isinstance(payload,dict) else {}

def trash_group(root,sid):
    data,members=trash_members(root,sid)
    root_meta=backup_session_meta(members[0][1])
    if kind(root_meta)=='subagent' or parent_thread_id(root_meta):
        fail('Cannot directly mutate a subagent session; use its main conversation')
    return data,members

def backup_originals_intact(root,members):
    return all(file is not None and relative is not None and (root/relative).is_file() and same_content(file,root/relative) for _,file,relative in members)

def trash_rows(root,sid,data,members):
    session_ids=[member['id'] for member,_,_ in members]
    rows=[]
    for member,file,_ in members:
        meta=backup_session_meta(file) if file else {}
        mid=member['id']
        rows.append({'id':mid,'title':str(member.get('title') or meta.get('title') or mid),'cwd':meta.get('cwd') if isinstance(meta.get('cwd'),str) else member.get('cwd'),'sessionKind':kind(meta) if meta else member.get('sessionKind','conversation'),'parentThreadId':parent_thread_id(meta) or member.get('parentThreadId'),'trashRootId':sid,'deletedAt':int(data['deletedAt']),'archived':bool(member.get('archived')),'sizeBytes':file.stat().st_size if file else 0,'sessionIds':session_ids if mid==sid else []})
    return rows

def codex_executable():
    located = shutil.which('codex')
    if located: return located
    home = pathlib.Path.home()
    candidates = [home/'.local/bin/codex',home/'.bun/bin/codex',home/'.npm-global/bin/codex',home/'.volta/bin/codex']
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK): return str(candidate)
    fail('Official Codex CLI is unavailable on this host')

class AppServer:
    def __init__(self, root):
        self.child = subprocess.Popen([codex_executable(),'app-server'],env=dict(os.environ,CODEX_HOME=str(root)),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
        self.responses = queue.Queue(maxsize=1000)
        self.counter = 0
        threading.Thread(target=self._read,daemon=True).start()
        threading.Thread(target=self._drain_stderr,daemon=True).start()
        try:
            self.call('initialize',{'clientInfo':{'name':'cockpit-tools','version':'1.0'},'capabilities':{'experimentalApi':True}})
            self.child.stdin.write(b'{"method":"initialized"}\n')
            self.child.stdin.flush()
        except Exception:
            self.close()
            raise
    def _read(self):
        try:
            for line in self.child.stdout:
                if len(line) > 32*1024*1024: fail('Official app-server response exceeds size limit')
                try: value=json.loads(line)
                except ValueError: continue
                if isinstance(value,dict) and 'id' in value:
                    try: self.responses.put(value,timeout=1)
                    except queue.Full: break
        finally:
            try: self.responses.put(None,timeout=1)
            except queue.Full: pass
    def _drain_stderr(self):
        for _ in iter(lambda:self.child.stderr.read(4096),b''): pass
    def call(self,method,params):
        self.counter += 1
        rid = self.counter
        self.child.stdin.write((json.dumps({'id':rid,'method':method,'params':params},separators=(',',':'))+'\n').encode())
        self.child.stdin.flush()
        deadline = time.monotonic()+20
        while True:
            try: row=self.responses.get(timeout=max(.001,deadline-time.monotonic()))
            except queue.Empty: fail('Official app-server timed out during '+method)
            if row is None: fail('Official app-server closed during '+method)
            if row.get('id') != rid: continue
            if 'error' in row:
                error=row['error']
                fail('Official '+method+' failed: '+str(error.get('message',error) if isinstance(error,dict) else error))
            return row.get('result') or {}
    def close(self):
        if self.child.poll() is None:
            self.child.terminate()
            try: self.child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.child.kill(); self.child.wait(timeout=2)
        for stream in (self.child.stdin,self.child.stdout,self.child.stderr):
            try: stream.close()
            except OSError: pass
    def __enter__(self): return self
    def __exit__(self,*_): self.close()
    def read(self,sid):
        result=self.call('thread/read',{'threadId':sid,'includeTurns':False})
        thread=result.get('thread')
        if not isinstance(thread,dict) or thread.get('id') != sid: fail('Official thread/read returned wrong session')
        return thread

def kind(thread):
    source=thread.get('source')
    if isinstance(source,dict) and ('subAgent' in source or 'subagent' in source): return 'subagent'
    return 'external' if source == 'exec' else 'conversation'

def parent_thread_id(thread):
    parent=thread.get('parentThreadId') or thread.get('parent_thread_id')
    if isinstance(parent,str) and parent.strip(): return parent.strip()
    source=thread.get('source')
    if not isinstance(source,dict): return None
    agent=source.get('subAgent')
    if not isinstance(agent,dict): agent=source.get('subagent')
    if not isinstance(agent,dict): return None
    spawn=agent.get('thread_spawn',agent.get('threadSpawn'))
    if not isinstance(spawn,dict): return None
    parent=spawn.get('parent_thread_id',spawn.get('parentThreadId'))
    return parent.strip() if isinstance(parent,str) and parent.strip() else None

def index_threads(root):
    candidates=[]
    for path in root.iterdir():
        match=re.fullmatch(r'state_(\d+)\.sqlite',path.name)
        if match and path.is_file() and not path.is_symlink(): candidates.append((int(match.group(1)),path))
    if not candidates: return []
    path=max(candidates,key=lambda item:item[0])[1]
    required={'id','rollout_path','title','cwd','updated_at','source','archived'}
    try:
        with contextlib.closing(sqlite3.connect(path.as_uri()+'?mode=ro',uri=True)) as db:
            db.execute('PRAGMA query_only=ON')
            names={row[1] for row in db.execute('PRAGMA table_info(threads)')}
            missing=required-names
            if missing: fail('Codex session index has incompatible threads schema: missing '+', '.join(sorted(missing)))
            name_expr='name' if 'name' in names else 'NULL'
            records=db.execute('SELECT id, rollout_path, '+name_expr+', title, cwd, updated_at, source, archived FROM threads ORDER BY updated_at DESC, id').fetchall()
    except sqlite3.DatabaseError as exc:
        fail('Cannot read Codex session index '+path.name+': '+str(exc))
    threads=[]
    for sid,rollout,name,title,cwd,updated,source,archived in records:
        try:
            sid=session_id(sid)
            if not isinstance(rollout,str) or not rollout: raise ValueError('missing rollout path')
            if isinstance(source,str):
                try: source=json.loads(source)
                except ValueError: pass
            display=next((value.strip() for value in (name,title) if isinstance(value,str) and value.strip()),sid)
            thread={'id':sid,'path':rollout,'name':display,'cwd':cwd,'updatedAt':int(updated),'source':source,'archived':bool(archived)}
            if kind(thread)=='subagent' and not parent_thread_id(thread):
                meta=backup_session_meta(rollout_path(root,thread))
                thread['parentThreadId']=parent_thread_id(meta)
            threads.append(thread)
        except (ValueError,TypeError) as exc:
            fail('Invalid row in Codex session index '+path.name+': '+str(exc))
    return threads

def session_row(root,thread,archived=None):
    try: path=rollout_path(root,thread) if thread.get('path') else None
    except FileNotFoundError:
        if archived is None: raise
        path=None # The index can retain a session whose rollout was removed.
    if path is None and archived is None: fail('Official thread has no rollout path')
    sid=session_id(thread.get('id'))
    title=next((value for value in (thread.get('name'),thread.get('preview')) if isinstance(value,str) and value.strip()),sid)
    return {'id':sid,'title':title[:500],'cwd':thread.get('cwd') if isinstance(thread.get('cwd'),str) else None,'updatedAt':int(thread.get('updatedAt') or (path.stat().st_mtime if path else 0)),'sessionKind':kind(thread),'parentThreadId':parent_thread_id(thread),'archived':archived if archived is not None else path.relative_to(root).parts[0]=='archived_sessions','sizeBytes':path.stat().st_size if path else 0}

def raw_scan(root):
    found=[]
    for name in ('sessions','archived_sessions'):
        base=safe_dir(root/name)
        if not base.exists(): continue
        for directory,dirs,files in os.walk(base,followlinks=False):
            dirs[:]=[d for d in dirs if not pathlib.Path(directory,d).is_symlink()]
            for filename in files:
                if not filename.startswith('rollout-') or not filename.endswith('.jsonl'): continue
                path=pathlib.Path(directory,filename)
                if path.is_symlink(): continue
                try:
                    with path.open('rb') as source: row=json.loads(source.readline(1024*1024))
                    meta=row.get('payload',{}) if row.get('type')=='session_meta' else {}
                    sid=session_id(meta.get('id'))
                    found.append((sid,path,meta))
                except (OSError,ValueError,UnicodeError): continue
    return found

def same_content(left,right):
    if left.stat().st_size!=right.stat().st_size: return False
    with left.open('rb') as a,right.open('rb') as b:
        while True:
            a_chunk=a.read(1024*1024); b_chunk=b.read(1024*1024)
            if a_chunk!=b_chunk: return False
            if not a_chunk: return True

def atomic_write(path,content):
    safe_dir(path.parent)
    temp=path.with_name(path.name+'.tmp-'+str(uuid.uuid4()))
    try:
        with temp.open('xb') as stream:
            stream.write(content); stream.flush(); os.fsync(stream.fileno())
        os.replace(temp,path)
    finally:
        if temp.exists(): temp.unlink()

def main(p):
    home=p.get('codex_home')
    if not isinstance(home,str) or not home.strip(): fail('Registered Codex home is missing')
    root=pathlib.Path(os.path.expanduser(home)).resolve(strict=True)
    if not root.is_dir(): fail('Codex home is not a directory')
    action=p.get('action')
    if action=='existingIds':
        return sorted({thread['id'] for thread in index_threads(root)} | {sid for sid,_,_ in raw_scan(root)})
    if action=='importStage':
        sid=session_id(p.get('id'))
        transfer=import_directory(root,p.get('transferId'))
        offset=int(p.get('offset',-1)); total=int(p.get('totalBytes',0))
        if total<1 or total>64*1024*1024 or offset<0 or offset>=total: fail('Invalid import size or offset')
        data=base64.b64decode(p.get('data',''),validate=True)
        if not data or len(data)>512*1024 or offset+len(data)>total: fail('Invalid import chunk')
        if offset==0:
            transfer.mkdir(parents=True,exist_ok=True)
            with (transfer/('rollout-'+sid+'.jsonl')).open('xb') as stream:
                stream.write(data); stream.flush(); os.fsync(stream.fileno())
        else:
            file=safe_file(transfer/('rollout-'+sid+'.jsonl'))
            if file.stat().st_size!=offset: fail('Import chunks arrived out of order')
            with file.open('ab') as stream:
                stream.write(data); stream.flush(); os.fsync(stream.fileno())
        return {'receivedBytes':offset+len(data)}
    if action=='importAbort':
        transfer=import_directory(root,p.get('transferId'))
        if transfer.exists(): shutil.rmtree(transfer)
        if transfer.parent.exists():
            try: transfer.parent.rmdir()
            except OSError: pass
        return {'aborted':True}
    if action=='listTrash':
        base=trash_root(root)
        if not base.exists(): return []
        rows=[]
        for entry in base.iterdir():
            if entry.is_symlink() or not entry.is_dir(): continue
            try:
                sid=session_id(entry.name); data,members=trash_group(root,sid)
                if data.get('deletionPending') is not True: rows.extend(trash_rows(root,sid,data,members))
            except (ValueError,OSError,KeyError): continue
        return sorted(rows,key=lambda row:row['deletedAt'],reverse=True)
    if action in ('purge','clearTrash'):
        base=trash_root(root)
        if action=='purge':
            roots=[session_id(p.get('id'))]
        else:
            roots=[]
            if base.exists():
                for entry in base.iterdir():
                    if entry.is_symlink() or not entry.is_dir(): fail('Unexpected entry in remote session trash')
                    roots.append(session_id(entry.name))
        groups=[]
        for sid in roots:
            data,members=trash_group(root,sid)
            if data.get('deletionPending') is True:
                if action=='purge': fail('Deletion was not confirmed; backup is not a trashed session')
                continue
            groups.append((sid,len(members)))
        for sid,_ in groups: shutil.rmtree(base/sid)
        return {'purgedRoots':len(groups),'purgedSessions':sum(count for _,count in groups)}
    if action=='list':
        rows=[session_row(root,thread,thread['archived']) for thread in index_threads(root)]
        children={row['id']:row for row in rows if row['parentThreadId'] or row['sessionKind']=='subagent'}
        mains=[row for row in rows if not row['parentThreadId'] and row['sessionKind']!='subagent']
        mains.sort(key=lambda row:row['updatedAt'],reverse=True)
        main_ids={row['id'] for row in mains}
        included=set(main_ids)
        pending=set(main_ids)
        while pending:
            descendants={sid for sid,row in children.items() if row['parentThreadId'] in pending and sid not in included}
            included.update(descendants); pending=descendants
        return {'sessions':mains+[row for row in rows if row['id'] in included and row['id'] not in main_ids],'total':len(mains)}
    if action=='importCommit':
        transfer=import_directory(root,p.get('transferId'))
        raw=p.get('members')
        if not isinstance(raw,list) or not raw: fail('No session package entries to import')
        indexed={thread['id'] for thread in index_threads(root)} | {sid for sid,_,_ in raw_scan(root)}
        prepared={}
        targets=set()
        for member in raw:
            if not isinstance(member,dict): fail('Invalid import member')
            mid=session_id(member.get('id'))
            if mid in prepared: fail('Duplicate session in import')
            if mid in indexed: fail('Remote session ID already exists')
            relative=relative_path(member.get('relativePath'))
            if not relative.name.startswith('rollout-'): fail('Invalid import rollout filename')
            archived=relative.parts[0]=='archived_sessions'
            if archived!=bool(member.get('archived')): fail('Import archive state mismatch')
            target=root/pathlib.Path('sessions',*relative.parts[1:])
            parent=target.parent
            while parent!=root:
                if parent.is_symlink(): fail('Symlinked import path is not allowed')
                parent=parent.parent
            if target.is_symlink() or target.exists() or target in targets:
                fail('Import target already exists')
            targets.add(target)
            file=safe_file(transfer/('rollout-'+mid+'.jsonl'))
            size=int(member.get('sizeBytes',-1)); digest=member.get('sha256')
            if size<1 or size>64*1024*1024 or file.stat().st_size!=size:
                fail('Import rollout size mismatch')
            if not isinstance(digest,str) or not re.fullmatch('[0-9a-fA-F]{64}',digest): fail('Invalid import checksum')
            with file.open('rb') as stream:
                if hashlib.sha256(stream.read()).hexdigest().lower()!=digest.lower(): fail('Import rollout checksum mismatch')
            meta=backup_session_meta(file)
            if meta.get('id')!=mid: fail('Import rollout session ID mismatch')
            prepared[mid]=(member,file,target,parent_thread_id(meta))
        ordered=[]
        pending=dict(prepared)
        while pending:
            ready=[mid for mid,(_,_,_,parent) in pending.items() if parent not in pending]
            if not ready: fail('Cycle in imported session hierarchy')
            for mid in sorted(ready):
                ordered.append((mid,*pending.pop(mid)))
        with AppServer(root) as api:
            for mid,member,file,target,_ in ordered:
                # Register one file at a time: archiving a parent must not move
                # an active child before that child is resumed.
                target.parent.mkdir(parents=True,exist_ok=True)
                with file.open('rb') as source,target.open('xb') as destination:
                    shutil.copyfileobj(source,destination)
                    destination.flush(); os.fsync(destination.fileno())
                result=api.call('thread/resume',{'threadId':mid,'path':str(target),'excludeTurns':True})
                if not isinstance(result.get('thread'),dict) or result['thread'].get('id')!=mid:
                    fail('Official thread/resume returned wrong session')
                api.read(mid)
                title=member.get('title')
                if isinstance(title,str) and title.strip() and title!=mid:
                    api.call('thread/name/set',{'threadId':mid,'name':title})
                if member['archived']:
                    api.call('thread/archive',{'threadId':mid})
            indexed={thread['id']:thread for thread in index_threads(root)}
            for mid,member,_,_,_ in ordered:
                thread=indexed.get(mid)
                if thread is None or thread['archived']!=member['archived']:
                    fail('Imported session archive state does not match package')
                final=rollout_path(root,thread)
                if backup_session_meta(final).get('id')!=mid:
                    fail('Imported session indexed path has wrong session ID')
        shutil.rmtree(transfer)
        try: transfer.parent.rmdir()
        except OSError: pass
        return {'importedCount':len(ordered)}
    with AppServer(root) as api:
        sid=session_id(p.get('id'))
        if action in ('location','readChunk'):
            thread=api.read(sid); path=rollout_path(root,thread)
            if action=='location': return {'path':str(path.parent if p.get('folder') else path)}
            size=path.stat().st_size
            offset=int(p.get('offset',0)); cap=max(1,min(512*1024,int(p.get('size',512*1024))))
            if offset<0 or offset>size: fail('Invalid export offset')
            before=path.stat()
            with path.open('rb') as stream: stream.seek(offset); content=stream.read(cap)
            after=path.stat()
            signature=lambda stat: ':'.join(str(v) for v in (stat.st_dev,stat.st_ino,stat.st_mtime_ns,stat.st_size))
            if signature(before)!=signature(after): fail('Remote transcript changed during export')
            return {'data':base64.b64encode(content).decode(),'totalBytes':size,'signature':signature(after),'relativePath':path.relative_to(root).as_posix()}
        if action=='trash':
            root_thread=api.read(sid)
            if kind(root_thread)=='subagent' or parent_thread_id(root_thread):
                fail('Cannot directly trash a subagent session; trash its main conversation')
            selected={sid:root_thread}
            listed=index_threads(root)
            parents={sid}
            while parents:
                descendants={session_id(thread.get('id')):thread for thread in listed if parent_thread_id(thread) in parents and session_id(thread.get('id')) not in selected}
                selected.update(descendants)
                parents=set(descendants)
            ordered=[root_thread]+[selected[key] for key in sorted(selected) if key!=sid]
            members=[]
            for thread in ordered:
                path=rollout_path(root,thread)
                row=session_row(root,thread)
                members.append((dict(id=row['id'],title=row['title'],archived=row['archived'],relativePath=path.relative_to(root).as_posix(),parentThreadId=row['parentThreadId'],sessionKind=row['sessionKind'],cwd=row['cwd']),path))
            entry=trash_root(root)/sid
            if entry.exists() or entry.is_symlink(): fail('Session already has a trash entry or retained backup; inspect it before retrying')
            entry.mkdir(parents=True)
            manifest=dict(members[0][0],deletionPending=True,deletedAt=int(time.time()),descendants=[m for m,_ in members[1:]])
            try:
                for index,(member,path) in enumerate(members):
                    target=entry/('rollout.jsonl' if index==0 else 'rollout-'+member['id']+'.jsonl')
                    before=path.stat()
                    shutil.copy2(path,target)
                    after=path.stat()
                    if (before.st_dev,before.st_ino,before.st_mtime_ns,before.st_size)!=(after.st_dev,after.st_ino,after.st_mtime_ns,after.st_size) or target.stat().st_size!=after.st_size:
                        fail('Remote transcript changed during backup')
                    with target.open('rb') as stream: os.fsync(stream.fileno())
                atomic_write(entry/'manifest.json',json.dumps(manifest).encode())
            except Exception:
                shutil.rmtree(entry)
                raise
            # A single official deletion removes the parent and all spawned descendants.
            # Remove a rejected operation's backup only after verifying every original.
            try: api.call('thread/delete',{'threadId':sid})
            except Exception as exc:
                saved=[(m,entry/('rollout.jsonl' if i==0 else 'rollout-'+m['id']+'.jsonl'),relative_path(m['relativePath'])) for i,(m,_) in enumerate(members)]
                if backup_originals_intact(root,saved):
                    shutil.rmtree(entry)
                    fail(str(exc)+'; original files unchanged; temporary backup removed; no fallback was performed')
                fail(str(exc)+'; deletion outcome uncertain; backup retained for inspection (not in trash) at '+str(entry))
            # Some internal review agents are not in Codex's spawn-edge cascade.
            # Delete only surviving members of this explicitly identified family.
            errors=[]
            for member,path in reversed(members[1:]):
                if path.exists():
                    try: api.call('thread/delete',{'threadId':member['id']})
                    except Exception as exc: errors.append(str(exc))
            remaining=[member['id'] for member,path in members if path.exists()]
            if errors or remaining:
                fail('Main session deleted; family cleanup incomplete; backup retained at '+str(entry)+': '+'; '.join(errors+remaining))
            manifest['deletionPending']=False
            atomic_write(entry/'manifest.json',json.dumps(manifest).encode())
            return trash_rows(root,sid,manifest,[(m,entry/('rollout.jsonl' if i==0 else 'rollout-'+m['id']+'.jsonl'),None) for i,(m,_) in enumerate(members)])[0]
        if action=='restore':
            data,members=trash_group(root,sid)
            if data.get('deletionPending') is True: fail('Deletion was not confirmed; inspect the retained backup instead of restoring')
            targets=[]
            for member,file,relative in members:
                if file is None: continue # Legacy database-only member has no rollout to recover.
                target=root/relative
                parent=target.parent
                while parent!=root:
                    if parent.is_symlink(): fail('Symlinked restore path is not allowed')
                    parent=parent.parent
                if target.is_symlink(): fail('Symlinked restore target is not allowed')
                if target.exists() and (not target.is_file() or not same_content(file,target)):
                    fail('Restore target already exists with different content')
                targets.append((member,file,target))
            # Backups are copied, never moved, so a failed registration stays retryable.
            for member,file,target in targets:
                target.parent.mkdir(parents=True,exist_ok=True)
                if not target.exists():
                    with file.open('rb') as source, target.open('xb') as destination:
                        shutil.copyfileobj(source,destination)
                        destination.flush(); os.fsync(destination.fileno())
            # Archive restored threads before resuming active ones. Official
            # archive can cascade to descendants that are already running.
            ordered=[item for item in targets if item[0].get('archived')]+[item for item in targets if not item[0].get('archived')]
            for member,_,target in ordered:
                mid=session_id(member['id'])
                resume_path=target
                if member.get('archived'):
                    unarchived=api.call('thread/unarchive',{'threadId':mid})
                    thread=unarchived.get('thread')
                    if not isinstance(thread,dict) or thread.get('id')!=mid:
                        fail('Official thread/unarchive returned wrong session')
                    resume_path=rollout_path(root,thread)
                resumed=api.call('thread/resume',{'threadId':mid,'path':str(resume_path),'excludeTurns':True})
                if not isinstance(resumed.get('thread'),dict) or resumed['thread'].get('id')!=mid:
                    fail('Official thread/resume returned wrong session')
                api.read(mid)
                title=member.get('title')
                if isinstance(title,str) and title.strip() and title!=mid:
                    api.call('thread/name/set',{'threadId':mid,'name':title})
                if member.get('archived'):
                    api.call('thread/archive',{'threadId':mid})
            indexed={thread['id']:thread for thread in index_threads(root)}
            for member,_,_ in targets:
                thread=indexed.get(member['id'])
                if thread is None or bool(member.get('archived')) != thread['archived']:
                    fail('Restored session archive state does not match backup')
                final_path=rollout_path(root,thread)
                if backup_session_meta(final_path).get('id')!=member['id']:
                    fail('Restored session indexed path has wrong session ID')
            result=session_row(root,api.read(sid))
            shutil.rmtree(trash_root(root)/sid)
            return result
        fail('Unsupported remote session operation')

def locked_main(p):
    home=p.get('codex_home')
    if not isinstance(home,str) or not home.strip(): fail('Registered Codex home is missing')
    root=pathlib.Path(os.path.expanduser(home)).resolve(strict=True)
    if not root.is_dir(): fail('Codex home is not a directory')
    descriptor=os.open(root,os.O_RDONLY|getattr(os,'O_DIRECTORY',0))
    try:
        # Listing and usage scans run together when the session page opens.
        # Readers may coexist; mutations still exclude both readers and writers.
        read_only=p.get('action') in ('list','listTrash','existingIds','location','readChunk')
        mode=fcntl.LOCK_SH if read_only else fcntl.LOCK_EX
        try: fcntl.flock(descriptor,mode|fcntl.LOCK_NB)
        except BlockingIOError: fail('远程会话正被其他操作占用，请等待操作完成后重试')
        return main(p)
    finally: os.close(descriptor)

try:
    print(json.dumps({'ok':True,'result':locked_main(json.load(sys.stdin))},ensure_ascii=False))
except Exception as exc:
    print(json.dumps({'ok':False,'error':str(exc)},ensure_ascii=False))
"#;
