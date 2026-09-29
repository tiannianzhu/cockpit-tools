//! Read-only session inventory. Empty previews are valid, particularly for subagents.
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

pub(crate) struct IndexedSession {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub rollout_path: PathBuf,
    pub updated_at: i64,
    pub source: Value,
    pub archived: bool,
}

fn index_path(home: &Path) -> Result<Option<PathBuf>, String> {
    if !home.exists() {
        return Ok(None);
    }
    let mut latest = None;
    for entry in fs::read_dir(home).map_err(|error| format!("读取 Codex 目录失败: {error}"))?
    {
        let entry = entry.map_err(|error| format!("读取 Codex 目录项失败: {error}"))?;
        let name = entry.file_name();
        let Some(version) = name
            .to_str()
            .and_then(|name| name.strip_prefix("state_"))
            .and_then(|name| name.strip_suffix(".sqlite"))
            .and_then(|version| version.parse::<u64>().ok())
        else {
            continue;
        };
        if entry.path().is_file()
            && latest
                .as_ref()
                .is_none_or(|(current, _)| version > *current)
        {
            latest = Some((version, entry.path()));
        }
    }
    Ok(latest.map(|(_, path)| path))
}

pub(crate) fn read_sessions(home: &Path) -> Result<Vec<IndexedSession>, String> {
    let Some(path) = index_path(home)? else {
        return Ok(Vec::new());
    };
    read_index(&path)
        .map_err(|error| format!("读取 Codex 会话索引失败 ({}): {error}", path.display()))
}

fn read_index(path: &Path) -> Result<Vec<IndexedSession>, String> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|error| error.to_string())?;
    let columns = connection
        .prepare("PRAGMA table_info(threads)")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|error| error.to_string())?;
    let name = if columns.contains("name") {
        "NULLIF(TRIM(name), '')"
    } else {
        "NULL"
    };
    let sql = format!("SELECT id, COALESCE({name}, NULLIF(TRIM(title), ''), id), cwd, rollout_path, updated_at, source, archived FROM threads ORDER BY updated_at DESC, id");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let source: String = row.get(5)?;
            Ok(IndexedSession {
                id: row.get(0)?,
                title: row.get(1)?,
                cwd: row.get(2)?,
                rollout_path: PathBuf::from(row.get::<_, String>(3)?),
                updated_at: row.get(4)?,
                // Interactive sources are plain text; subagent sources are serialized JSON.
                source: serde_json::from_str(&source).unwrap_or(Value::String(source)),
                archived: row.get(6)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "cockpit-index-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&home).unwrap();
        home
    }
    fn schema(path: &Path, with_name: bool) -> Connection {
        let db = Connection::open(path).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, cwd TEXT, rollout_path TEXT, updated_at INTEGER, source TEXT, archived INTEGER, preview TEXT, has_user_event INTEGER);").unwrap();
        if with_name {
            db.execute_batch("ALTER TABLE threads ADD COLUMN name TEXT;")
                .unwrap();
        }
        db
    }
    #[test]
    fn includes_empty_preview_agents_and_preserves_source_and_archive() {
        let home = fixture();
        let path = home.join("state_1.sqlite");
        let db = schema(&path, true);
        db.execute(
            "INSERT INTO threads VALUES ('child','Agent','/work','/file',12,?1,1,'',0,NULL)",
            [r#"{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}"#],
        )
        .unwrap();
        db.execute_batch("INSERT INTO threads VALUES ('root','Old title','/work','/root',11,'vscode',0,'',0,'Renamed');").unwrap();
        drop(db);
        let before = fs::read(&path).unwrap();
        let rows = read_sessions(&home).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].archived);
        assert_eq!(
            rows[0].source["subagent"]["thread_spawn"]["parent_thread_id"],
            "root"
        );
        assert_eq!(rows[1].title, "Renamed");
        assert_eq!(rows[1].cwd, "/work");
        assert_eq!(rows[1].rollout_path, PathBuf::from("/root"));
        assert_eq!(rows[1].updated_at, 11);
        assert_eq!(before, fs::read(&path).unwrap());
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn chooses_numeric_latest_index_and_supports_older_title_schema() {
        let home = fixture();
        schema(&home.join("state_2.sqlite"), false);
        let db = schema(&home.join("state_10.sqlite"), false);
        db.execute_batch("INSERT INTO threads VALUES ('id','  ','/work','/file',1,'cli',0,'',0);")
            .unwrap();
        drop(db);
        let rows = read_sessions(&home).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "id");
        assert_eq!(rows[0].id, "id");
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn missing_index_is_empty_but_corrupt_index_is_an_error() {
        let home = fixture();
        assert!(read_sessions(&home).unwrap().is_empty());
        fs::write(home.join("state_1.sqlite"), "invalid").unwrap();
        assert!(read_sessions(&home).is_err());
        fs::remove_dir_all(home).unwrap();
    }
}
