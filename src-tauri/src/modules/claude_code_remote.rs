use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex as StdMutex};
use tokio::sync::Mutex;

const PREFERENCES_FILE: &str = "claude_code_sync.json";
const REMOTE_INSTALL_SCRIPT: &str = include_str!("claude_code_remote_settings.py");

static SYNC_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static PREFERENCES_LOCK: StdMutex<()> = StdMutex::new(());

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeSyncPreferences {
    #[serde(default)]
    pub server_ids: Vec<String>,
    #[serde(default)]
    pub last_results: Vec<ClaudeCodeSyncResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeSyncResult {
    pub server_id: String,
    pub server_name: String,
    pub synced_at: i64,
    pub revision: String,
    pub verified: bool,
    pub error: Option<String>,
}

fn valid_server_ids(server_ids: Vec<String>) -> Result<Vec<String>, String> {
    let servers = crate::modules::ssh_server::list_servers()?;
    let known: HashSet<String> = servers
        .servers
        .into_iter()
        .map(|server| server.id)
        .collect();
    let mut seen = HashSet::new();
    Ok(server_ids
        .into_iter()
        .filter(|id| known.contains(id) && seen.insert(id.clone()))
        .collect())
}

fn preferences_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::modules::account::get_data_dir()?.join(PREFERENCES_FILE))
}

pub fn read_sync_preferences() -> Result<ClaudeCodeSyncPreferences, String> {
    let _guard = PREFERENCES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    read_preferences_unlocked()
}

fn read_preferences_unlocked() -> Result<ClaudeCodeSyncPreferences, String> {
    let path = preferences_path()?;
    if !path.exists() {
        return Ok(ClaudeCodeSyncPreferences::default());
    }
    let bytes = std::fs::read(path)
        .map_err(|_| "Could not read Claude Code sync preferences".to_string())?;
    let mut preferences: ClaudeCodeSyncPreferences = serde_json::from_slice(&bytes)
        .map_err(|_| "Could not parse Claude Code sync preferences".to_string())?;
    preferences.server_ids = valid_server_ids(preferences.server_ids)?;
    Ok(preferences)
}

pub fn save_sync_preferences(server_ids: Vec<String>) -> Result<ClaudeCodeSyncPreferences, String> {
    let _guard = PREFERENCES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = read_preferences_unlocked()?;
    let preferences = ClaudeCodeSyncPreferences {
        server_ids: valid_server_ids(server_ids)?,
        last_results: previous.last_results,
    };
    write_preferences_unlocked(&preferences)?;
    Ok(preferences)
}

fn write_preferences_unlocked(preferences: &ClaudeCodeSyncPreferences) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&preferences)
        .map_err(|_| "Could not encode Claude Code sync preferences".to_string())?;
    crate::modules::atomic_write::write_bytes_atomic(&preferences_path()?, &bytes)?;
    Ok(())
}

fn persist_results(results: &[ClaudeCodeSyncResult]) -> Result<(), String> {
    let _guard = PREFERENCES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut preferences = read_preferences_unlocked()?;
    // Keep server_ids as currently saved; the user may change selection while
    // the network batch is running.
    preferences.last_results = results.to_vec();
    write_preferences_unlocked(&preferences)
}

fn sha256(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Copy the complete local settings snapshot byte-for-byte to each selected
/// SSH login user's ~/.claude/settings.json. Each remote failure is isolated.
pub async fn sync_settings(
    settings: &crate::modules::claude_code_config::ClaudeCodeSettings,
    server_ids: &[String],
) -> Result<Vec<ClaudeCodeSyncResult>, String> {
    let content = settings.content.as_ref();
    let revision = &settings.revision;
    if revision == "missing" {
        return Err("Local Claude Code settings file is missing".into());
    }
    if sha256(content) != *revision {
        return Err("Local Claude Code settings snapshot is inconsistent".into());
    }
    let parsed: Value = serde_json::from_str(content)
        .map_err(|_| "Local Claude Code settings must be a JSON object".to_string())?;
    if !parsed.is_object() {
        return Err("Local Claude Code settings must be a JSON object".into());
    }

    let _guard = SYNC_LOCK.lock().await;
    // The local file may have changed while this request waited for an earlier
    // batch. Re-read it before any remote write so stale snapshots cannot win.
    let current = crate::modules::claude_code_config::read_settings()?;
    if current.revision != *revision {
        return Err("Local Claude Code settings changed; retry synchronization".into());
    }

    let inventory = crate::modules::ssh_server::list_servers()?;
    let by_id: std::collections::HashMap<String, String> = inventory
        .servers
        .into_iter()
        .map(|server| (server.id, server.name))
        .collect();
    let ids = valid_server_ids(server_ids.to_vec())?;
    let mut results = Vec::with_capacity(ids.len());
    let mut superseded = false;
    for server_id in ids {
        let server_name = by_id.get(&server_id).cloned().unwrap_or_default();
        let now = chrono::Utc::now().timestamp();
        if !superseded {
            superseded = crate::modules::claude_code_config::read_settings()
                .map(|latest| latest.revision != *revision)
                .unwrap_or(true);
        }
        let (verified, error) = if superseded {
            (
                false,
                Some("Local settings changed; sync superseded".to_string()),
            )
        } else {
            let response = crate::modules::ssh_server::run_remote_python(
                &server_id,
                REMOTE_INSTALL_SCRIPT,
                &serde_json::json!({ "content": content, "revision": revision }),
            )
            .await;
            match response {
                Ok(output) => match serde_json::from_str::<Value>(&output) {
                    Ok(value)
                        if value.get("verified").and_then(Value::as_bool) == Some(true)
                            && value.get("revision").and_then(Value::as_str)
                                == Some(revision.as_str()) =>
                    {
                        (true, None)
                    }
                    _ => (
                        false,
                        Some("Remote settings verification failed".to_string()),
                    ),
                },
                Err(_) => (false, Some("Remote settings sync failed".to_string())),
            }
        };
        results.push(ClaudeCodeSyncResult {
            server_id,
            server_name,
            synced_at: now,
            revision: revision.clone(),
            verified,
            error,
        });
    }
    persist_results(&results)?;
    Ok(results)
}
