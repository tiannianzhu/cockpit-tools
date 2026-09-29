use std::io::{self, Read};
use std::path::PathBuf;

use cockpit_session_usage::{CodexSessionUsageQuery, SessionUsageStore, UsageInstance};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    action: String,
    codex_home: PathBuf,
    db_path: PathBuf,
    instance_id: String,
    instance_name: String,
    #[serde(default)]
    rebuild: bool,
    #[serde(default)]
    query: CodexSessionUsageQuery,
}

fn run() -> Result<(), String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| error.to_string())?;
    let request: Request = serde_json::from_str(&input).map_err(|error| error.to_string())?;
    let instance = UsageInstance {
        id: request.instance_id,
        name: request.instance_name,
        data_dir: request.codex_home,
    };
    let instances = [instance];
    let store = SessionUsageStore::open_path(request.db_path);
    match request.action.as_str() {
        "query" => {
            let report = store.query(&request.query, &instances)?;
            println!(
                "{}",
                serde_json::to_string(&report).map_err(|error| error.to_string())?
            );
        }
        "sync" => {
            let result = store.sync(request.rebuild, &instances)?;
            println!(
                "{}",
                serde_json::to_string(&result).map_err(|error| error.to_string())?
            );
        }
        action => return Err(format!("Unknown action: {action}")),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
