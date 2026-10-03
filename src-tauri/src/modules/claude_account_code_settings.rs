// The settings editor and account switch share the same account-owned model projection.
fn account_matching_code_settings<'a>(
    accounts: &'a [ClaudeAccount],
    value: &Value,
) -> Option<&'a ClaudeAccount> {
    let current_id =
        crate::modules::provider_current_state::get_current_account_id("claude_code_account")
            .ok()
            .flatten();
    accounts
        .iter()
        .filter(|account| {
            crate::modules::claude_code_config::api_account_matches_document(account, value)
                .unwrap_or(false)
        })
        .min_by_key(|account| Some(&account.id) != current_id.as_ref())
}

pub fn read_code_settings() -> Result<crate::modules::claude_code_config::ClaudeCodeSettings, String>
{
    let mut settings = crate::modules::claude_code_config::read_settings()?;
    let value: Value =
        serde_json::from_str(&settings.content).map_err(|_| "Claude settings 必须是有效 JSON。")?;
    let accounts = list_accounts_checked()?;
    crate::modules::claude_code_config::bind_account(
        &mut settings,
        account_matching_code_settings(&accounts, &value),
    );
    Ok(settings)
}

pub fn account_for_code_switch(account_id: &str) -> Result<ClaudeAccount, String> {
    let account = load_account(account_id).ok_or("Claude 账号不存在")?;
    if account.auth_mode == ClaudeAuthMode::ApiKey && account.claude_code_model_settings.is_none() {
        let settings = crate::modules::claude_code_config::read_settings()?;
        if let Some(updated) = crate::modules::claude_code_config::capture_legacy_model_settings(
            &account,
            &settings.content,
        )? {
            return save_account_and_index(updated);
        }
    }
    Ok(account)
}

pub fn save_code_settings(
    content: &str,
    expected_revision: &str,
    account_id: Option<&str>,
) -> Result<crate::modules::claude_code_config::ClaudeCodeSettings, String> {
    let value: Value =
        serde_json::from_str(content).map_err(|_| "Claude settings 必须是有效 JSON。")?;
    let accounts = list_accounts_checked()?;
    let owner = if let Some(id) = account_id {
        Some(
            accounts
                .iter()
                .find(|account| account.id == id)
                .ok_or("账号已不存在，请重新读取配置。")?,
        )
    } else {
        account_matching_code_settings(&accounts, &value)
    };
    if let Some(owner) = owner {
        let updated =
            crate::modules::claude_code_config::update_account_from_settings(owner, content)?;
        let identity = build_api_key_account_id(
            updated.api_key.as_deref().unwrap_or_default(),
            updated.api_base_url.as_deref(),
        );
        if accounts.iter().any(|account| {
            account.id != owner.id
                && account.auth_mode == ClaudeAuthMode::ApiKey
                && build_api_key_account_id(
                    account.api_key.as_deref().unwrap_or_default(),
                    account.api_base_url.as_deref(),
                ) == identity
        }) {
            return Err("该 Base URL 和 API Key 已属于另一个账号，请切换到对应账号后编辑。".into());
        }
    }
    let path = get_default_claude_code_config_dir()?.join(CLAUDE_CODE_SETTINGS_FILE);
    let _lock = CLAUDE_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "无法获取 Claude 账号锁")?;
    crate::modules::claude_code_config::save_settings_and_account_at(
        &path,
        content,
        expected_revision,
        owner,
        |account| save_account_and_index_locked(account.clone()).map(|_| ()),
    )
}

/// Capture an outgoing default account before replacing its model fields, including legacy accounts.
/// Dedicated instance configurations never overwrite the default account's saved profile.
fn capture_outgoing_code_account(
    settings_path: &Path,
    incoming_id: Option<&str>,
) -> Result<(), String> {
    if settings_path != get_default_claude_code_config_dir()?.join(CLAUDE_CODE_SETTINGS_FILE) {
        return Ok(());
    }
    let settings = crate::modules::claude_code_config::read_settings_at(settings_path)?;
    let value: Value =
        serde_json::from_str(&settings.content).map_err(|_| "Claude settings 必须是有效 JSON。")?;
    let accounts = list_accounts_checked()?;
    let Some(account) = account_matching_code_settings(&accounts, &value)
        .filter(|account| Some(account.id.as_str()) != incoming_id)
    else {
        return Ok(());
    };
    let updated = crate::modules::claude_code_config::update_account_from_settings(
        account,
        &settings.content,
    )?;
    if updated.claude_code_model_settings != account.claude_code_model_settings
        || updated.api_extra_env != account.api_extra_env
        || updated.api_model_catalog != account.api_model_catalog
    {
        save_account_and_index(updated)?;
    }
    Ok(())
}
