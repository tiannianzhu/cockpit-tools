use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshServerStore {
    pub version: String,
    #[serde(default)]
    pub selected_server_id: Option<String>,
    #[serde(default)]
    pub selected_server_ids: Vec<String>,
    pub servers: Vec<SshServer>,
}

impl Default for SshServerStore {
    fn default() -> Self {
        Self {
            version: "2".to_string(),
            selected_server_id: None,
            selected_server_ids: Vec::new(),
            servers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshServer {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub codex_home: String,
    pub auth: SshAuthConfig,
    pub sync_on_codex_switch: bool,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<SshCodexSyncStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SshAuthConfig {
    Agent,
    PrivateKeyFile { path: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshSyncStage {
    #[default]
    Pending,
    Transferring,
    CredentialsSynced,
    Reloading,
    Applied,
    Failed,
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshCodexSyncStatus {
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub stage: SshSyncStage,
    pub account_id: String,
    pub account_email: String,
    pub token_generation: u64,
    pub bundle_hash: String,
    pub synced_at: i64,
    pub verified: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshCodexSyncResult {
    pub server_id: String,
    pub server_name: String,
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub stage: SshSyncStage,
    pub account_id: String,
    pub account_email: String,
    pub token_generation: u64,
    pub bundle_hash: String,
    pub verified: bool,
    pub error: Option<String>,
    pub synced_at: i64,
}

/// Public remote identity only; credential fingerprints stay inside the backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshRemoteAccountSummary {
    #[serde(default)]
    pub connection_address: Option<String>,
    pub server_id: String,
    pub auth_mode: String,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub matched_account_id: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub model_provider_name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_catalog_path: Option<String>,
    #[serde(default)]
    pub model_catalog_exists: bool,
    #[serde(default)]
    pub catalog_model_count: Option<usize>,
}
