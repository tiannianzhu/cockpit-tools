//! Codex session usage adapter for desktop instance discovery and pricing.
//! Parsing, incremental cursors, and reporting live in `cockpit-session-usage`.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use cockpit_session_usage::SessionUsageStore;
pub use cockpit_session_usage::{
    CodexSessionUsageQuery, CodexSessionUsageReport, CodexSessionUsageSyncResult, UsageInstance,
};

use crate::modules::{account, codex_instance};

const DEFAULT_INSTANCE_ID: &str = "__default__";
const DEFAULT_INSTANCE_NAME: &str = "默认实例";
const DB_FILE_NAME: &str = "codex_session_usage.sqlite";
static SYNC_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn usage_store() -> Result<SessionUsageStore, String> {
    let path = account::get_data_dir()?.join(DB_FILE_NAME);
    #[cfg(not(test))]
    {
        let maintenance_path = path.clone();
        crate::modules::background_maintenance::schedule(
            format!("session-rollup:{}", path.display()),
            move || {
                let Ok(_guard) = SYNC_LOCK.get_or_init(|| Mutex::new(())).try_lock() else {
                    return Ok(false);
                };
                SessionUsageStore::open_path(maintenance_path.clone()).maintenance_batch()
            },
        );
    }
    Ok(SessionUsageStore::open_path(path))
}

fn collect_usage_instances() -> Result<Vec<UsageInstance>, String> {
    let mut instances = Vec::new();
    let default_dir = codex_instance::get_default_codex_home()?;
    let store = codex_instance::load_instance_store()?;
    instances.push(UsageInstance {
        id: DEFAULT_INSTANCE_ID.to_string(),
        name: DEFAULT_INSTANCE_NAME.to_string(),
        data_dir: default_dir,
    });
    for instance in store.instances {
        let user_data_dir = instance.user_data_dir.trim();
        if user_data_dir.is_empty() {
            continue;
        }
        instances.push(UsageInstance {
            id: instance.id,
            name: instance.name,
            data_dir: PathBuf::from(user_data_dir),
        });
    }
    Ok(instances)
}

pub fn apply_report_cost(report: &mut CodexSessionUsageReport) {
    let price = |model: &str, usage: &cockpit_session_usage::CodexSessionUsagePricingGroup| {
        crate::modules::codex_local_access::try_estimate_model_token_cost_usd_for_service_tier(
            model,
            usage.service_tier.as_deref(),
            usage.context_input_tokens,
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
        )
    };
    for session in report.session_tokens.iter_mut().flatten() {
        session.apply_cost(price);
    }
    for day in &mut report.by_day {
        day.apply_cost(price);
    }
    report.totals.estimated_cost_usd = report.by_model.iter_mut().fold(0.0, |sum, row| {
        row.apply_cost(price);
        sum + row.estimated_cost_usd.unwrap_or(0.0)
    });
}

pub fn query_session_usage(
    query: CodexSessionUsageQuery,
) -> Result<CodexSessionUsageReport, String> {
    let store = usage_store()?;
    let instances = collect_usage_instances()?;
    let mut report = store.query(&query, &instances)?;
    apply_report_cost(&mut report);
    Ok(report)
}

pub fn sync_session_usage(
    rebuild: bool,
    query: CodexSessionUsageQuery,
) -> Result<CodexSessionUsageSyncResult, String> {
    let _guard = SYNC_LOCK
        .get_or_init(|| Mutex::new(()))
        .try_lock()
        .map_err(|_| crate::modules::background_maintenance::busy_message())?;
    let store = usage_store()?;
    let instances = collect_usage_instances()?;
    let mut result = store.sync_with_batch_control(rebuild, &instances, || loop {
        if crate::modules::app_lifecycle::is_shutdown_started() {
            return None;
        }
        if let Some(resource) = crate::modules::background_maintenance::try_resource() {
            return Some(resource);
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    })?;
    let mut report = store.query(&query, &instances)?;
    apply_report_cost(&mut report);
    result.report = Some(report);
    Ok(result)
}
