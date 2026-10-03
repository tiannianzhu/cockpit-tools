use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeSettings {
    pub path: String,
    pub content: String,
    pub revision: String,
    pub account: Option<ClaudeCodeSettingsAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaudeCodeSettingsAccount {
    pub id: String,
    pub name: String,
}

pub(crate) const MODEL_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_CUSTOM_MODEL_OPTION",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
];

/// Capture only the model fields this account owns, including native picker metadata.
pub(crate) fn model_settings(value: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for key in ["model", "modelPicker", "autoCompactWindow"] {
        if let Some(value) = value.get(key) {
            result.insert(key.into(), value.clone());
        }
    }
    let mut env = serde_json::Map::new();
    for key in MODEL_ENV_KEYS {
        if let Some(value) = value.get("env").and_then(|env| env.get(key)) {
            env.insert((*key).into(), value.clone());
        }
    }
    if !env.is_empty() {
        result.insert("env".into(), Value::Object(env));
    }
    Value::Object(result)
}

/// Replace the complete account-owned model subset. Missing fields deliberately clear old values.
pub(crate) fn apply_model_settings(value: &mut Value, profile: &Value) -> Result<(), String> {
    if !profile.is_object() || profile.get("env").is_some_and(|env| !env.is_object()) {
        return Err("账号的 Claude Code 模型配置无效。".into());
    }
    let profile = model_settings(profile);
    let object = value
        .as_object_mut()
        .ok_or("Claude settings 顶层必须是 JSON object。")?;
    for key in ["model", "modelPicker", "autoCompactWindow"] {
        object.remove(key);
        if let Some(value) = profile.get(key) {
            object.insert(key.into(), value.clone());
        }
    }
    let env = object
        .entry("env")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("Claude settings 的 env 必须是 JSON object。")?;
    for key in MODEL_ENV_KEYS {
        env.remove(*key);
        if let Some(value) = profile.get("env").and_then(|env| env.get(key)) {
            env.insert((*key).into(), value.clone());
        }
    }
    Ok(())
}

pub(crate) fn account_model_settings(account: &crate::models::claude::ClaudeAccount) -> Value {
    if let Some(profile) = account.claude_code_model_settings.as_ref() {
        return profile.clone();
    }
    // Compatibility for accounts created before per-account model settings existed.
    let mut value = serde_json::json!({"env": account.api_extra_env});
    if let Some(models) = account
        .api_model_catalog
        .as_ref()
        .filter(|models| !models.is_empty())
    {
        value["modelPicker"] = serde_json::json!({"options": models.iter()
            .map(|model| serde_json::json!({"model":model})).collect::<Vec<_>>()});
    }
    model_settings(&value)
}

pub(crate) fn bind_account(
    settings: &mut ClaudeCodeSettings,
    account: Option<&crate::models::claude::ClaudeAccount>,
) {
    settings.account = account.map(|account| ClaudeCodeSettingsAccount {
        id: account.id.clone(),
        name: account.email.clone(),
    });
}

pub(crate) fn apply_api_account_settings(
    value: &mut Value,
    account: &crate::models::claude::ClaudeAccount,
    previously_managed: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeSet<String>, String> {
    let projected_env = crate::modules::claude_account::build_api_key_cli_env_map(account)?;
    let managed_keys = projected_env.keys().cloned().collect();
    let object = value
        .as_object_mut()
        .ok_or("Claude settings 顶层必须是 JSON object。")?;
    let env = object
        .entry("env")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("Claude settings 的 env 必须是 JSON object。")?;
    for key in previously_managed {
        env.remove(key);
    }
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
    ] {
        env.remove(key);
    }
    for (key, value) in projected_env {
        env.insert(key, Value::String(value));
    }
    apply_model_settings(value, &account_model_settings(account))?;
    Ok(managed_keys)
}

static SETTINGS_PATH_LOCKS: LazyLock<Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn settings_lock(path: &Path) -> Result<Arc<Mutex<()>>, String> {
    let mut locks = SETTINGS_PATH_LOCKS
        .lock()
        .map_err(|_| "Claude settings 写入锁损坏。".to_string())?;
    Ok(locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

fn revision_for_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_revision(path: &Path) -> Result<(Vec<u8>, String), String> {
    match fs::read(path) {
        Ok(bytes) => {
            let revision = revision_for_bytes(&bytes);
            Ok((bytes, revision))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((Vec::new(), "missing".to_string()))
        }
        Err(_) => Err("无法读取 Claude settings 文件。".to_string()),
    }
}

fn parse_settings(content: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(content)
        .map_err(|_| "Claude settings 必须是有效 JSON。".to_string())?;
    let Some(object) = value.as_object() else {
        return Err("Claude settings 顶层必须是 JSON object。".to_string());
    };
    if let Some(env) = object.get("env") {
        if !env.is_object() {
            return Err("Claude settings 的 env 必须是 JSON object。".to_string());
        }
    }
    Ok(value)
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "无法定位 Claude settings 目录。".to_string())?;
    fs::create_dir_all(parent).map_err(|_| "无法创建 Claude settings 目录。".to_string())?;
    let file_name = path
        .file_name()
        .ok_or_else(|| "无法解析 Claude settings 文件名。".to_string())?;
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options
            .open(&temp_path)
            .map_err(|_| "无法创建 Claude settings 临时文件。".to_string())?;
        file.write_all(bytes)
            .map_err(|_| "无法写入 Claude settings 临时文件。".to_string())?;
        file.sync_all()
            .map_err(|_| "无法同步 Claude settings 临时文件。".to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))
                .map_err(|_| "无法设置 Claude settings 文件权限。".to_string())?;
        }
        fs::rename(&temp_path, path)
            .map_err(|_| "无法原子替换 Claude settings 文件。".to_string())?;
        if let Ok(directory) = OpenOptions::new().read(true).open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub(crate) fn read_settings_at(path: &Path) -> Result<ClaudeCodeSettings, String> {
    let (bytes, revision) = read_revision(path)?;
    let content = if revision == "missing" {
        "{}\n".to_string()
    } else {
        let content = String::from_utf8(bytes)
            .map_err(|_| "Claude settings 文件不是有效 UTF-8。".to_string())?;
        parse_settings(&content)?;
        content
    };
    Ok(ClaudeCodeSettings {
        path: path.to_string_lossy().into_owned(),
        content,
        revision,
        account: None,
    })
}

pub(crate) fn save_settings_at(
    path: &Path,
    content: &str,
    expected_revision: &str,
) -> Result<ClaudeCodeSettings, String> {
    let lock = settings_lock(path)?;
    let _guard = lock
        .lock()
        .map_err(|_| "Claude settings 写入锁损坏。".to_string())?;
    save_settings_unlocked(path, content, expected_revision)
}

fn save_settings_unlocked(
    path: &Path,
    content: &str,
    expected_revision: &str,
) -> Result<ClaudeCodeSettings, String> {
    parse_settings(content)?;
    let (current_bytes, current_revision) = read_revision(path)?;
    if current_revision != expected_revision {
        return Err("Claude settings 已被其他操作修改，请重新读取后再保存。".to_string());
    }

    if current_revision != "missing" && current_bytes == content.as_bytes() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|_| "无法设置 Claude settings 文件权限。".to_string())?;
        }
        return read_settings_at(path);
    }

    if current_revision != "missing" {
        let backup_path = path.with_file_name(format!(
            "{}.bak",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("settings.json")
        ));
        atomic_write_private(&backup_path, &current_bytes)?;
    }
    atomic_write_private(path, content.as_bytes())?;
    let revision = revision_for_bytes(content.as_bytes());
    Ok(ClaudeCodeSettings {
        path: path.to_string_lossy().into_owned(),
        content: content.to_string(),
        revision,
        account: None,
    })
}

pub fn read_settings() -> Result<ClaudeCodeSettings, String> {
    let path =
        crate::modules::claude_account::get_default_claude_code_config_dir()?.join("settings.json");
    read_settings_at(&path)
}

fn normalize_settings_base_url(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_string())
}

pub(crate) fn api_account_matches_document(
    account: &crate::models::claude::ClaudeAccount,
    value: &Value,
) -> Result<bool, String> {
    use crate::models::claude::ClaudeAuthMode;

    if account.auth_mode != ClaudeAuthMode::ApiKey {
        return Ok(false);
    }
    let api_key = account
        .api_key
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if api_key.is_empty() {
        return Ok(false);
    }
    let Some(env_value) = value.get("env") else {
        return Ok(false);
    };
    let Some(env) = env_value.as_object() else {
        return Err("Claude settings 的 env 必须是 JSON object。".to_string());
    };
    let settings_base_url = match env.get("ANTHROPIC_BASE_URL") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Ok(false),
    };
    if normalize_settings_base_url(account.api_base_url.as_deref())
        != normalize_settings_base_url(settings_base_url)
    {
        return Ok(false);
    }

    let key_field = match account.api_key_field.as_deref().map(str::trim) {
        Some("ANTHROPIC_API_KEY") => "ANTHROPIC_API_KEY",
        Some("ANTHROPIC_AUTH_TOKEN") => "ANTHROPIC_AUTH_TOKEN",
        Some(_) => return Ok(false),
        None if is_official_settings_base_url(account.api_base_url.as_deref()) => {
            "ANTHROPIC_API_KEY"
        }
        None => "ANTHROPIC_AUTH_TOKEN",
    };
    let other_field = if key_field == "ANTHROPIC_API_KEY" {
        "ANTHROPIC_AUTH_TOKEN"
    } else {
        "ANTHROPIC_API_KEY"
    };
    let configured_key = env
        .get(key_field)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let other_field_is_nonempty = match env.get(other_field) {
        None | Some(Value::Null) => false,
        Some(Value::String(value)) => !value.trim().is_empty(),
        Some(_) => true,
    };
    Ok(configured_key == api_key && !other_field_is_nonempty)
}

fn api_account_matches_settings_at(
    account: &crate::models::claude::ClaudeAccount,
    path: &Path,
) -> Result<bool, String> {
    let settings = read_settings_at(path)?;
    let value: Value =
        serde_json::from_str(&settings.content).map_err(|_| "Claude settings 必须是有效 JSON。")?;
    Ok(api_account_matches_document(account, &value)?
        && model_settings(&value) == model_settings(&account_model_settings(account)))
}

pub(crate) fn capture_legacy_model_settings(
    account: &crate::models::claude::ClaudeAccount,
    content: &str,
) -> Result<Option<crate::models::claude::ClaudeAccount>, String> {
    let value: Value =
        serde_json::from_str(content).map_err(|_| "Claude settings 必须是有效 JSON。")?;
    if account.claude_code_model_settings.is_none()
        && api_account_matches_document(account, &value)?
    {
        Ok(Some(update_account_from_settings(account, content)?))
    } else {
        Ok(None)
    }
}

pub(crate) fn update_account_from_settings(
    account: &crate::models::claude::ClaudeAccount,
    content: &str,
) -> Result<crate::models::claude::ClaudeAccount, String> {
    use crate::models::claude::ClaudeAuthMode;
    if account.auth_mode != ClaudeAuthMode::ApiKey {
        return Err("仅 API Key 账号支持保存 Claude Code 配置。".into());
    }
    let value = parse_settings(content)?;
    let env = value
        .get("env")
        .and_then(Value::as_object)
        .ok_or("账号配置必须包含 API Key。")?;
    let auth_fields = ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"];
    if auth_fields.iter().any(|key| {
        env.get(*key)
            .is_some_and(|value| !value.is_null() && !value.is_string())
    }) {
        return Err("API Key 认证环境变量必须是字符串。".into());
    }
    let keys: Vec<_> = auth_fields
        .iter()
        .filter_map(|field| {
            env.get(*field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(|key| (*field, key))
        })
        .collect();
    if keys.len() != 1 {
        return Err("账号配置必须且只能填写一种 API Key 认证环境变量。".into());
    }
    let base_url = match env.get("ANTHROPIC_BASE_URL") {
        None | Some(Value::Null) => None,
        Some(Value::String(base)) => normalize_settings_base_url(Some(base)),
        _ => return Err("供应商 Base URL 必须是字符串。".into()),
    };
    if let Some(base) = base_url.as_ref() {
        let url = Url::parse(base).map_err(|_| "供应商 Base URL 不是有效 URL。")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err("供应商 Base URL 仅支持有效的 HTTP / HTTPS 地址。".into());
        }
    }
    let mut updated = account.clone();
    if normalize_settings_base_url(account.api_base_url.as_deref()) != base_url {
        // The account name/ID remain stable; old provider metadata must not describe a new endpoint.
        updated.api_provider_id = None;
        updated.api_provider_name = base_url
            .as_ref()
            .and_then(|base| Url::parse(base).ok())
            .and_then(|url| url.host_str().map(str::to_string))
            .or(Some("Anthropic Official".into()));
        updated.api_provider_source_tag = None;
        updated.api_provider_website = None;
        updated.api_provider_api_key_url = None;
        updated.plan_type = updated.api_provider_name.clone();
    }
    updated.api_key = Some(keys[0].1.into());
    updated.api_key_field = Some(keys[0].0.into());
    updated.api_base_url = base_url;
    updated.claude_code_model_settings = Some(model_settings(&value));
    updated.api_model_catalog = value
        .get("modelPicker")
        .and_then(|picker| picker.get("options"))
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    option
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        });
    let mut extra_env = updated.api_extra_env.take().unwrap_or_default();
    for key in MODEL_ENV_KEYS {
        extra_env.remove(*key);
        if let Some(value) = env.get(*key).and_then(Value::as_str) {
            extra_env.insert((*key).into(), value.into());
        }
    }
    updated.api_extra_env = (!extra_env.is_empty()).then_some(extra_env);
    let provider = serde_json::json!({"id":updated.api_provider_id,"name":updated.api_provider_name,
        "baseUrl":updated.api_base_url,"sourceTag":updated.api_provider_source_tag,"website":updated.api_provider_website,
        "apiKeyUrl":updated.api_provider_api_key_url,"keyField":updated.api_key_field,
        "modelCatalog":updated.api_model_catalog,"extraEnv":updated.api_extra_env});
    updated.claude_credentials_raw = Some(
        serde_json::json!({"authMode":"api_key","anthropicApiKey":updated.api_key,
        "apiKeyField":updated.api_key_field,"apiProvider":provider}),
    );
    updated.claude_config_raw = Some(
        serde_json::json!({"hasCompletedOnboarding":true,"apiKeyAccount":{
        "label":updated.email,"keyHash":format!("{:x}",md5::compute(keys[0].1.as_bytes())),"provider":provider}}),
    );
    Ok(updated)
}

/// Keep account and active file consistent, rolling back both if account persistence fails.
pub(crate) fn save_settings_and_account_at(
    path: &Path,
    content: &str,
    revision: &str,
    account: Option<&crate::models::claude::ClaudeAccount>,
    mut persist: impl FnMut(&crate::models::claude::ClaudeAccount) -> Result<(), String>,
) -> Result<ClaudeCodeSettings, String> {
    let updated = account
        .map(|account| update_account_from_settings(account, content))
        .transpose()?;
    let lock = settings_lock(path)?;
    let _guard = lock.lock().map_err(|_| "Claude settings 写入锁损坏。")?;
    let before = read_settings_at(path)?;
    let backup_path = path.with_file_name("settings.json.bak");
    let previous_backup = if updated.is_some() {
        match fs::read(&backup_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err("无法读取 Claude settings 备份文件。".into()),
        }
    } else {
        None
    };
    let mut saved = save_settings_unlocked(path, content, revision)?;
    if let Some(updated) = updated.as_ref() {
        if let Err(error) = persist(updated) {
            let account_restored =
                persist(account.expect("updated account requires original")).is_ok();
            // The path lock covers app writers; preserve a newer edit by an external CLI/editor.
            let unchanged =
                read_revision(path).is_ok_and(|(_, revision)| revision == saved.revision);
            let file_restored = if !unchanged {
                false
            } else if before.revision == "missing" {
                fs::remove_file(path).is_ok()
            } else {
                atomic_write_private(path, before.content.as_bytes()).is_ok()
            };
            let backup_restored = if !unchanged {
                false
            } else if let Some(bytes) = previous_backup {
                atomic_write_private(&backup_path, &bytes).is_ok()
            } else {
                !backup_path.exists() || fs::remove_file(&backup_path).is_ok()
            };
            if !account_restored || !file_restored || !backup_restored {
                return Err(format!(
                    "账号配置保存失败，回滚未完成，请重新读取配置后检查账号：{}",
                    error
                ));
            }
            return Err(format!("账号配置保存失败，已恢复原配置：{}", error));
        }
        bind_account(&mut saved, Some(updated));
    }
    Ok(saved)
}

fn is_official_settings_base_url(base_url: Option<&str>) -> bool {
    let Some(base_url) = normalize_settings_base_url(base_url) else {
        return true;
    };
    Url::parse(&base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "api.anthropic.com" || host == "api.claude.com")
}

pub fn api_account_matches_settings(
    account: &crate::models::claude::ClaudeAccount,
) -> Result<bool, String> {
    use crate::models::claude::ClaudeAuthMode;

    if account.auth_mode != ClaudeAuthMode::ApiKey {
        return Ok(false);
    }
    let path =
        crate::modules::claude_account::get_default_claude_code_config_dir()?.join("settings.json");
    api_account_matches_settings_at(account, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "claude-code-settings-test-{}",
                uuid::Uuid::new_v4()
            ))
            .join("settings.json")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn missing_settings_can_be_created_with_missing_revision() {
        let path = temp_settings_path();
        let initial = read_settings_at(&path).expect("missing config should be readable");
        assert_eq!(initial.revision, "missing");
        assert_eq!(initial.content, "{}\n");

        let saved = save_settings_at(&path, "{\n  \"env\": {}\n}\n", "missing")
            .expect("creation should succeed");
        assert_eq!(saved.revision, revision_for_bytes(saved.content.as_bytes()));
        assert_eq!(fs::read_to_string(&path).unwrap(), saved.content);
        cleanup(&path);
    }

    #[test]
    fn unknown_fields_and_user_format_are_preserved_verbatim() {
        let path = temp_settings_path();
        let content = "{\n  \"permissions\": { \"allow\": [\"Read\"] },\n  \"customUnknown\": {\"keep\":true}\n}\n";
        let created = save_settings_at(&path, content, "missing").unwrap();
        let read = read_settings_at(&path).unwrap();
        assert_eq!(read.content, content);
        assert_eq!(read.revision, created.revision);
        cleanup(&path);
    }

    #[test]
    fn stale_revision_is_rejected_without_overwriting_current_file() {
        let path = temp_settings_path();
        let created = save_settings_at(&path, "{\"env\":{}}", "missing").unwrap();
        let before = fs::read(&path).unwrap();
        let error = save_settings_at(&path, "{\"env\":{\"A\":\"B\"}}", "missing")
            .expect_err("stale revision must conflict");
        assert!(error.contains("重新读取"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(created.revision, revision_for_bytes(&before));
        cleanup(&path);
    }

    #[test]
    fn invalid_json_or_env_shape_never_replaces_existing_file() {
        let path = temp_settings_path();
        save_settings_at(&path, "{\"custom\":true}", "missing").unwrap();
        let before = fs::read(&path).unwrap();
        let revision = revision_for_bytes(&before);
        for invalid in ["{", "[]", "null", "{\"env\":[]}"] {
            assert!(save_settings_at(&path, invalid, &revision).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        cleanup(&path);
    }

    #[test]
    fn main_and_backup_files_are_private_and_backup_preserves_previous_bytes() {
        let path = temp_settings_path();
        let previous = "{\n  \"keep\": true\n}\n";
        let created = save_settings_at(&path, previous, "missing").unwrap();
        let next = "{\n  \"keep\": false\n}\n";
        save_settings_at(&path, next, &created.revision).unwrap();
        let backup_path = path.with_file_name("settings.json.bak");
        assert_eq!(fs::read_to_string(&backup_path).unwrap(), previous);
        assert_eq!(fs::read_to_string(&path).unwrap(), next);
        let current = read_settings_at(&path).unwrap();
        save_settings_at(&path, next, &current.revision).unwrap();
        assert_eq!(fs::read_to_string(&backup_path).unwrap(), previous);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&backup_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        cleanup(&path);
    }
    fn api_key_account(
        base_url: Option<&str>,
        key_field: &str,
        key: &str,
    ) -> crate::models::claude::ClaudeAccount {
        use crate::models::claude::{ClaudeAccount, ClaudeAuthMode};
        ClaudeAccount {
            id: "fixture".into(),
            email: "Fixture".into(),
            auth_mode: ClaudeAuthMode::ApiKey,
            account_uuid: None,
            organization_uuid: None,
            organization_name: None,
            plan_type: None,
            avatar_url: None,
            profile_updated_at: None,
            quota: None,
            quota_error: None,
            usage_updated_at: None,
            status: None,
            status_reason: None,
            api_key: Some(key.into()),
            api_base_url: base_url.map(str::to_string),
            api_provider_id: None,
            api_provider_name: None,
            api_provider_source_tag: None,
            api_provider_website: None,
            api_provider_api_key_url: None,
            api_key_field: Some(key_field.into()),
            api_model_catalog: None,
            api_extra_env: None,
            claude_code_model_settings: None,
            desktop_gateway_auth_scheme: None,
            desktop_gateway_credential_kind: None,
            desktop_gateway_config_id: None,
            desktop_gateway_profile_dir: None,
            desktop_gateway_models: None,
            desktop_gateway_connection_mode: None,
            desktop_gateway_upstream_models: None,
            desktop_gateway_model_mappings: None,
            desktop_profile_dir: None,
            desktop_profile_imported_at: None,
            claude_credentials_raw: None,
            claude_config_raw: None,
            claude_usage_raw: None,
            tags: None,
            account_note: None,
            created_at: 1,
            last_used: 1,
        }
    }

    #[test]
    fn account_match_requires_same_base_url_and_selected_key_only() {
        let path = temp_settings_path();
        save_settings_at(&path, "{\"env\":{\"ANTHROPIC_BASE_URL\":\"https://relay.example/v1/\",\"ANTHROPIC_AUTH_TOKEN\":\"token\"}}", "missing").unwrap();
        let account = api_key_account(
            Some("https://relay.example/v1"),
            "ANTHROPIC_AUTH_TOKEN",
            "token",
        );
        assert!(api_account_matches_settings_at(&account, &path).unwrap());
        let different_base = api_key_account(
            Some("https://other.example/v1"),
            "ANTHROPIC_AUTH_TOKEN",
            "token",
        );
        assert!(!api_account_matches_settings_at(&different_base, &path).unwrap());
        let wrong_key_field = api_key_account(
            Some("https://relay.example/v1"),
            "ANTHROPIC_API_KEY",
            "token",
        );
        assert!(!api_account_matches_settings_at(&wrong_key_field, &path).unwrap());
        cleanup(&path);
    }

    #[test]
    fn account_match_rejects_both_auth_fields_and_supports_official_default_base() {
        let path = temp_settings_path();
        save_settings_at(
            &path,
            "{\"env\":{\"ANTHROPIC_API_KEY\":\"token\",\"ANTHROPIC_AUTH_TOKEN\":\"other\"}}",
            "missing",
        )
        .unwrap();
        let account = api_key_account(None, "ANTHROPIC_API_KEY", "token");
        assert!(!api_account_matches_settings_at(&account, &path).unwrap());
        save_settings_at(
            &path,
            "{\"env\":{\"ANTHROPIC_API_KEY\":\"token\"}}",
            &read_settings_at(&path).unwrap().revision,
        )
        .unwrap();
        assert!(api_account_matches_settings_at(&account, &path).unwrap());
        cleanup(&path);
    }

    #[test]
    fn account_save_keeps_identity_and_only_captures_api_and_model_settings() {
        let mut account = api_key_account(
            Some("https://old.example/api"),
            "ANTHROPIC_AUTH_TOKEN",
            "old-key",
        );
        account.tags = Some(vec!["work".into()]);
        account.account_note = Some("keep note".into());
        account.api_provider_id = Some("old-provider".into());
        let content = serde_json::json!({"model":"alpha", "modelPicker":{"options":[{
            "model":"alpha", "label":"Custom label", "description":"details", "behavesAs":"sonnet", "future":42
        }],"replaceBuiltInOptions":true,"futurePicker":true}, "env":{
            "ANTHROPIC_BASE_URL":"https://new.example/api/", "ANTHROPIC_API_KEY":"new-key",
            "ANTHROPIC_DEFAULT_SONNET_MODEL":"alpha", "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME":"Role name",
            "CLAUDE_CODE_SUBAGENT_MODEL":"beta", "GLOBAL_ENV":"keep"
        }, "permissions":{"allow":["Read"]}, "hooks":{"keep":true}, "futureGlobal":true}).to_string();
        let updated = update_account_from_settings(&account, &content).unwrap();
        assert_eq!(updated.id, account.id);
        assert_eq!(updated.email, account.email);
        assert_eq!(updated.tags, account.tags);
        assert_eq!(updated.account_note, account.account_note);
        assert_eq!(
            (updated.created_at, updated.last_used),
            (account.created_at, account.last_used)
        );
        assert_eq!(updated.api_key.as_deref(), Some("new-key"));
        assert_eq!(
            updated.api_base_url.as_deref(),
            Some("https://new.example/api")
        );
        assert_eq!(updated.api_key_field.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(updated.api_provider_id.is_none());
        assert_eq!(updated.api_model_catalog, Some(vec!["alpha".into()]));
        let profile = updated.claude_code_model_settings.as_ref().unwrap();
        assert_eq!(profile["modelPicker"]["options"][0]["future"], 42);
        assert_eq!(profile["modelPicker"]["replaceBuiltInOptions"], true);
        for key in ["permissions", "hooks", "futureGlobal"] {
            assert!(profile.get(key).is_none());
        }
        for key in ["GLOBAL_ENV", "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"] {
            assert!(profile["env"].get(key).is_none());
        }
        assert_eq!(
            updated.claude_credentials_raw.as_ref().unwrap()["anthropicApiKey"],
            "new-key"
        );
    }

    #[test]
    fn switching_accounts_restores_models_and_preserves_global_settings() {
        let first = api_key_account(
            Some("https://first.example/api"),
            "ANTHROPIC_AUTH_TOKEN",
            "first-key",
        );
        let mut settings = serde_json::json!({"model":"first-model", "modelPicker":{"options":[{"model":"first-model","label":"First"}]},
            "env":{"ANTHROPIC_AUTH_TOKEN":"first-key","ANTHROPIC_BASE_URL":"https://first.example/api",
                "ANTHROPIC_DEFAULT_OPUS_MODEL":"first-model", "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME":"First role", "UNRELATED":"keep"},
            "permissions":{"allow":["Read"]},"hooks":{"keep":true},"futureGlobal":{"keep":true}});
        let first = update_account_from_settings(&first, &settings.to_string()).unwrap();
        let mut second = api_key_account(None, "ANTHROPIC_API_KEY", "second-key");
        second.claude_code_model_settings = Some(
            serde_json::json!({"modelPicker":{"options":[{"model":"second-model","label":"Second"}]},
            "env":{"CLAUDE_CODE_SUBAGENT_MODEL":"second-model"}, "permissions":{"deny":["Read"]}}),
        );
        let globals = (
            settings["permissions"].clone(),
            settings["hooks"].clone(),
            settings["futureGlobal"].clone(),
        );
        apply_api_account_settings(&mut settings, &second, &Default::default()).unwrap();
        assert!(api_account_matches_document(&second, &settings).unwrap());
        assert_eq!(settings["env"]["ANTHROPIC_API_KEY"], "second-key");
        assert!(settings["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert!(settings["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert!(settings.get("model").is_none());
        assert!(settings["env"]
            .get("ANTHROPIC_DEFAULT_OPUS_MODEL_NAME")
            .is_none());
        assert_eq!(
            settings["env"]["CLAUDE_CODE_SUBAGENT_MODEL"],
            "second-model"
        );
        assert_eq!(settings["modelPicker"]["options"][0]["label"], "Second");
        apply_api_account_settings(&mut settings, &first, &Default::default()).unwrap();
        assert!(api_account_matches_document(&first, &settings).unwrap());
        assert_eq!(settings["model"], "first-model");
        assert_eq!(settings["modelPicker"]["options"][0]["label"], "First");
        assert!(settings["env"].get("CLAUDE_CODE_SUBAGENT_MODEL").is_none());
        assert_eq!(settings["env"]["UNRELATED"], "keep");
        assert_eq!(
            (
                settings["permissions"].clone(),
                settings["hooks"].clone(),
                settings["futureGlobal"].clone()
            ),
            globals
        );
    }

    #[test]
    fn switching_accounts_restores_context_and_compaction_windows() {
        let first = api_key_account(None, "ANTHROPIC_API_KEY", "first-key");
        let second = api_key_account(None, "ANTHROPIC_API_KEY", "second-key");
        let first_settings = serde_json::json!({
            "model": "first-model", "autoCompactWindow": 180000,
            "env": {"ANTHROPIC_API_KEY": "first-key", "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "200000",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "180000", "UNRELATED": "keep"},
            "permissions": {"allow": ["Read"]}
        });
        let first = update_account_from_settings(&first, &first_settings.to_string()).unwrap();
        let second = update_account_from_settings(&second, &serde_json::json!({
            "model": "second-model", "autoCompactWindow": 270000,
            "env": {"ANTHROPIC_API_KEY": "second-key", "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "300000"}
        }).to_string()).unwrap();
        let mut settings = first_settings.clone();
        apply_api_account_settings(&mut settings, &second, &Default::default()).unwrap();
        assert_eq!(settings["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "300000");
        assert_eq!(settings["autoCompactWindow"], 270000);
        assert!(settings["env"]
            .get("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
            .is_none());
        settings["autoCompactWindow"] = serde_json::json!(260000);

        let without_limits = api_key_account(None, "ANTHROPIC_API_KEY", "third-key");
        apply_api_account_settings(&mut settings, &without_limits, &Default::default()).unwrap();
        assert!(settings.get("autoCompactWindow").is_none());
        assert!(settings["env"]
            .get("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
            .is_none());
        assert!(settings["env"]
            .get("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
            .is_none());
        apply_api_account_settings(&mut settings, &first, &Default::default()).unwrap();
        assert_eq!(settings, first_settings);
    }

    #[test]
    fn saved_model_profile_participates_in_launch_matching() {
        let path = temp_settings_path();
        let mut account = api_key_account(None, "ANTHROPIC_API_KEY", "fixture-key");
        account.claude_code_model_settings = Some(serde_json::json!({
            "model":"selected", "autoCompactWindow":180000,
            "env":{"CLAUDE_CODE_MAX_CONTEXT_TOKENS":"200000"}
        }));
        let settings =
            serde_json::json!({"env":{"ANTHROPIC_API_KEY":"fixture-key"},"model":"other"});
        let saved = save_settings_at(&path, &settings.to_string(), "missing").unwrap();
        assert!(!api_account_matches_settings_at(&account, &path).unwrap());
        let mut settings = settings;
        apply_api_account_settings(&mut settings, &account, &Default::default()).unwrap();
        let mut saved = save_settings_at(&path, &settings.to_string(), &saved.revision).unwrap();
        assert!(api_account_matches_settings_at(&account, &path).unwrap());
        for (pointer, value) in [
            ("/autoCompactWindow", serde_json::json!(170000)),
            (
                "/env/CLAUDE_CODE_MAX_CONTEXT_TOKENS",
                serde_json::json!("300000"),
            ),
        ] {
            *settings.pointer_mut(pointer).unwrap() = value;
            saved = save_settings_at(&path, &settings.to_string(), &saved.revision).unwrap();
            assert!(!api_account_matches_settings_at(&account, &path).unwrap());
            apply_api_account_settings(&mut settings, &account, &Default::default()).unwrap();
            saved = save_settings_at(&path, &settings.to_string(), &saved.revision).unwrap();
            assert!(api_account_matches_settings_at(&account, &path).unwrap());
        }
        cleanup(&path);
    }

    #[test]
    fn legacy_accounts_project_presets_without_inheriting_previous_account_models() {
        let mut account = api_key_account(None, "ANTHROPIC_API_KEY", "fixture-key");
        account.api_model_catalog = Some(vec!["preset-model".into()]);
        account.api_extra_env = Some(std::collections::BTreeMap::from([(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".into(),
            "preset-model".into(),
        )]));
        let mut settings = serde_json::json!({"model":"stale", "modelPicker":{"options":[{"model":"stale"}]},
            "env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"stale", "UNRELATED":"keep"}});
        apply_api_account_settings(&mut settings, &account, &Default::default()).unwrap();
        assert_eq!(
            settings["modelPicker"]["options"][0]["model"],
            "preset-model"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
            "preset-model"
        );
        assert!(settings.get("model").is_none());
        assert!(settings["env"]
            .get("ANTHROPIC_DEFAULT_OPUS_MODEL")
            .is_none());
        assert_eq!(settings["env"]["UNRELATED"], "keep");
    }

    #[test]
    fn save_persists_native_account_profile_and_reports_its_owner() {
        let path = temp_settings_path();
        let account_path = path.with_file_name("account.json");
        let account = api_key_account(None, "ANTHROPIC_API_KEY", "fixture-key");
        let content = serde_json::json!({"env":{"ANTHROPIC_API_KEY":"rotated-key"},"model":"selected","permissions":{"allow":["Read"]}}).to_string();
        let saved =
            save_settings_and_account_at(&path, &content, "missing", Some(&account), |updated| {
                fs::write(&account_path, serde_json::to_vec(updated).unwrap())
                    .map_err(|_| "fixture save failed".into())
            })
            .unwrap();
        let persisted: crate::models::claude::ClaudeAccount =
            serde_json::from_slice(&fs::read(&account_path).unwrap()).unwrap();
        assert_eq!(persisted.id, account.id);
        assert_eq!(persisted.api_key.as_deref(), Some("rotated-key"));
        assert_eq!(
            persisted.claude_code_model_settings,
            Some(serde_json::json!({"model":"selected"}))
        );
        assert_eq!(saved.account.unwrap().id, account.id);
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        cleanup(&path);
    }

    #[test]
    fn failed_account_save_rolls_back_and_invalid_or_stale_edits_never_persist() {
        let path = temp_settings_path();
        let account = api_key_account(None, "ANTHROPIC_API_KEY", "fixture-key");
        let before = "{\"env\":{\"ANTHROPIC_API_KEY\":\"fixture-key\"},\"model\":\"old\"}";
        let initial = save_settings_at(&path, before, "missing").unwrap();
        let backup = path.with_file_name("settings.json.bak");
        fs::write(&backup, b"previous backup bytes").unwrap();
        let mut attempts = Vec::new();
        let next = "{\"env\":{\"ANTHROPIC_API_KEY\":\"rotated-key\"},\"model\":\"new\"}";
        let error = save_settings_and_account_at(
            &path,
            next,
            &initial.revision,
            Some(&account),
            |updated| {
                attempts.push(updated.api_key.clone());
                if attempts.len() == 1 {
                    Err("fixture failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(error.contains("已恢复"));
        assert_eq!(
            attempts,
            vec![Some("rotated-key".into()), Some("fixture-key".into())]
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(fs::read(&backup).unwrap(), b"previous backup bytes");
        for content in [
            "{}",
            "{\"env\":{\"ANTHROPIC_API_KEY\":\"a\",\"ANTHROPIC_AUTH_TOKEN\":\"b\"}}",
            "{\"env\":{\"ANTHROPIC_API_KEY\":\"a\",\"ANTHROPIC_AUTH_TOKEN\":42}}",
            "{\"env\":{\"ANTHROPIC_API_KEY\":\"a\",\"ANTHROPIC_BASE_URL\":\"file:///tmp\"}}",
        ] {
            assert!(save_settings_and_account_at(
                &path,
                content,
                &initial.revision,
                Some(&account),
                |_| panic!("must not persist invalid edit")
            )
            .is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), before);
        }
        assert!(
            save_settings_and_account_at(&path, next, "missing", Some(&account), |_| panic!(
                "must not persist stale edit"
            ))
            .is_err()
        );
        cleanup(&path);
    }

    #[test]
    fn legacy_current_account_is_captured_once_without_replacing_user_models() {
        let mut legacy = api_key_account(None, "ANTHROPIC_API_KEY", "fixture-key");
        legacy.api_model_catalog = Some(vec!["preset".into()]);
        let current = serde_json::json!({"env":{"ANTHROPIC_API_KEY":"fixture-key"},"model":"user-model", "permissions":{"allow":["Read"]}}).to_string();
        let migrated = capture_legacy_model_settings(&legacy, &current)
            .unwrap()
            .unwrap();
        assert_eq!(
            migrated.claude_code_model_settings,
            Some(serde_json::json!({"model":"user-model"}))
        );
        let other = current.replace("fixture-key", "different-key");
        assert!(capture_legacy_model_settings(&legacy, &other)
            .unwrap()
            .is_none());
        assert!(capture_legacy_model_settings(&migrated, &current)
            .unwrap()
            .is_none());
        let path = temp_settings_path();
        save_settings_at(&path, &current, "missing").unwrap();
        assert!(!api_account_matches_settings_at(&legacy, &path).unwrap());
        assert!(api_account_matches_settings_at(&migrated, &path).unwrap());
        cleanup(&path);
    }

    #[test]
    fn failed_account_creation_restores_missing_file_and_backup() {
        let path = temp_settings_path();
        let account = api_key_account(None, "ANTHROPIC_API_KEY", "fixture-key");
        let mut attempts = 0;
        assert!(save_settings_and_account_at(
            &path,
            "{\"env\":{\"ANTHROPIC_API_KEY\":\"fixture-key\"}}",
            "missing",
            Some(&account),
            |_| {
                attempts += 1;
                if attempts == 1 {
                    Err("fixture failure".into())
                } else {
                    Ok(())
                }
            }
        )
        .is_err());
        assert!(!path.exists());
        assert!(!path.with_file_name("settings.json.bak").exists());
        cleanup(&path);
    }

    #[test]
    fn failed_account_save_does_not_roll_back_over_an_external_file_edit() {
        let path = temp_settings_path();
        let account = api_key_account(None, "ANTHROPIC_API_KEY", "fixture-key");
        let initial = save_settings_at(
            &path,
            "{\"env\":{\"ANTHROPIC_API_KEY\":\"fixture-key\"}}",
            "missing",
        )
        .unwrap();
        let mut attempts = 0;
        let external = "{\"permissions\":{\"allow\":[\"Read\"]},\"external\":true}";
        let error = save_settings_and_account_at(
            &path,
            "{\"env\":{\"ANTHROPIC_API_KEY\":\"rotated-key\"}}",
            &initial.revision,
            Some(&account),
            |_| {
                attempts += 1;
                if attempts == 1 {
                    fs::write(&path, external).unwrap();
                    Err("fixture failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(error.contains("回滚未完成"));
        assert_eq!(fs::read_to_string(&path).unwrap(), external);
        cleanup(&path);
    }
}
