use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value as JsonValue};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "macos")]
const CODEX_APP_SERVER_MACOS_EXECUTABLES: &[&str] = &[
    "/Applications/ChatGPT.app/Contents/Resources/codex",
    "/Applications/Codex.app/Contents/Resources/codex",
];
const CODEX_APP_SERVER_EXECUTABLE_ENV: &str = "CODEX_APP_SERVER_EXECUTABLE";
const APP_SERVER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
const ALL_SOURCE_KINDS: &[&str] = &[
    "cli",
    "vscode",
    "exec",
    "appServer",
    "subAgent",
    "subAgentReview",
    "subAgentCompact",
    "subAgentThreadSpawn",
    "subAgentOther",
    "unknown",
];

pub fn rebuild_thread_metadata(codex_home: &Path) -> Result<(), String> {
    // Retain the historical visibility normalization for existing callers. The
    // official list call performs the server's own rollout metadata repair.
    list_threads(codex_home, false, None)?;
    crate::modules::codex_session_visibility::normalize_official_thread_cwds(codex_home)?;
    Ok(())
}

pub(crate) fn list_threads(
    codex_home: &Path,
    archived: bool,
    ancestor_thread_id: Option<&str>,
) -> Result<Vec<JsonValue>, String> {
    let mut server = AppServerSession::start(codex_home)?;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    let mut threads = Vec::new();
    loop {
        let result = server.request(
            "thread/list",
            json!({
                "cursor": cursor,
                "limit": 100,
                "sortKey": "updated_at",
                "sortDirection": "desc",
                "modelProviders": [],
                "sourceKinds": ALL_SOURCE_KINDS,
                "archived": archived,
                "ancestorThreadId": ancestor_thread_id,
            }),
        )?;
        let page = result
            .get("data")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| format!("官方 thread/list 响应缺少 data 数组: {}", result))?;
        threads.extend(page.iter().cloned());
        let next = result.get("nextCursor").and_then(JsonValue::as_str);
        match next {
            Some(next) if seen_cursors.insert(next.to_string()) => cursor = Some(next.to_string()),
            Some(_) => return Err("官方 thread/list 重复返回分页游标".into()),
            None => break,
        }
    }
    Ok(threads)
}

pub(crate) fn read_thread(codex_home: &Path, id: &str) -> Result<JsonValue, String> {
    let mut server = AppServerSession::start(codex_home)?;
    read_thread_from(&mut server, id)
}

fn read_thread_from(server: &mut AppServerSession, id: &str) -> Result<JsonValue, String> {
    let result = server.request(
        "thread/read",
        json!({ "threadId": id, "includeTurns": false }),
    )?;
    let thread = result
        .get("thread")
        .cloned()
        .ok_or_else(|| format!("官方 thread/read 响应缺少 thread: {}", result))?;
    if thread.get("id").and_then(JsonValue::as_str) != Some(id) {
        return Err(format!(
            "官方 thread/read 返回了错误的会话 ID: expected={}",
            id
        ));
    }
    Ok(thread)
}

pub(crate) fn archive_thread(codex_home: &Path, id: &str, archived: bool) -> Result<(), String> {
    let mut server = AppServerSession::start(codex_home)?;
    server.request(
        if archived {
            "thread/archive"
        } else {
            "thread/unarchive"
        },
        json!({ "threadId": id }),
    )?;
    Ok(())
}

/// Register a restored rollout through the official API, then set its archive state.
/// The caller retains its backup until this returns successfully.
pub(crate) fn register_restored_thread(
    codex_home: &Path,
    id: &str,
    path: &Path,
    archived: bool,
    name: Option<&str>,
) -> Result<(), String> {
    let mut server = AppServerSession::start(codex_home)?;
    let resume_path = if archived {
        // The server refuses to resume a rollout while it lives in
        // archived_sessions. Unarchive moves it to sessions and returns the
        // new path; after registration we archive it again.
        let unarchived = server.request("thread/unarchive", json!({ "threadId": id }))?;
        if unarchived.pointer("/thread/id").and_then(JsonValue::as_str) != Some(id) {
            return Err(format!(
                "官方 thread/unarchive 返回了错误的会话 ID: expected={}",
                id
            ));
        }
        unarchived
            .pointer("/thread/path")
            .and_then(JsonValue::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| format!("官方 thread/unarchive 响应缺少 thread.path: {}", unarchived))?
    } else {
        path.to_path_buf()
    };
    let result = server.request(
        "thread/resume",
        json!({
            "threadId": id,
            "path": resume_path.to_string_lossy(),
            "excludeTurns": true,
        }),
    )?;
    let returned_id = result
        .pointer("/thread/id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| format!("官方 thread/resume 响应缺少 thread.id: {}", result))?;
    if returned_id != id {
        return Err(format!(
            "恢复会话 ID 不匹配: expected={}, returned={}",
            id, returned_id
        ));
    }
    if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
        server.request("thread/name/set", json!({ "threadId": id, "name": name }))?;
    }
    if archived {
        server.request("thread/archive", json!({ "threadId": id }))?;
    }
    let thread = read_thread_from(&mut server, id)?;
    if thread.get("id").and_then(JsonValue::as_str) != Some(id) {
        return Err(format!("恢复后无法验证会话 ID: {}", id));
    }
    Ok(())
}

/// `thread/delete` removes a thread and its spawned descendants. Any partial
/// failure returns an error with the IDs already acknowledged by the server.
pub struct ThreadDeleteResult {
    pub deleted: Vec<String>,
    pub failures: Vec<String>,
}

pub fn delete_threads(
    codex_home: &Path,
    session_ids: &[String],
) -> Result<ThreadDeleteResult, String> {
    let mut server = AppServerSession::start(codex_home)?;
    Ok(delete_threads_with(session_ids, |id| {
        server
            .request("thread/delete", json!({ "threadId": id }))
            .map(|_| ())
    }))
}

fn delete_threads_with(
    session_ids: &[String],
    mut delete: impl FnMut(&str) -> Result<(), String>,
) -> ThreadDeleteResult {
    let mut result = ThreadDeleteResult {
        deleted: Vec::new(),
        failures: Vec::new(),
    };
    for id in dedupe_session_ids(session_ids) {
        match delete(&id) {
            Ok(()) => result.deleted.push(id),
            Err(error) => result.failures.push(format!("{}: {}", id, error)),
        }
    }
    result
}

fn dedupe_session_ids(session_ids: &[String]) -> Vec<String> {
    let mut unique = Vec::new();
    for session_id in session_ids {
        let trimmed = session_id.trim();
        if trimmed.is_empty() || unique.iter().any(|existing| existing == trimmed) {
            continue;
        }
        unique.push(trimmed.to_string());
    }
    unique
}

pub(crate) fn official_app_server_executable() -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Some(executable) = std::env::var_os(CODEX_APP_SERVER_EXECUTABLE_ENV) {
        if !executable.as_os_str().is_empty() {
            push_candidate(&mut candidates, PathBuf::from(executable));
        }
    }
    add_codex_app_server_candidates(&mut candidates);

    for executable in &candidates {
        if executable.is_file() {
            return Ok(executable.clone());
        }
    }

    let searched_paths = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "未找到官方 Codex app-server 可执行文件: {}",
        searched_paths
    ))
}

fn add_codex_app_server_candidates(candidates: &mut Vec<PathBuf>) {
    let configured_path = crate::modules::config::get_user_config().codex_app_path;
    if !configured_path.trim().is_empty() {
        push_candidate_from_codex_launch_path(candidates, Path::new(configured_path.trim()));
    }

    if let Some(detected_path) = crate::modules::process::detect_codex_exec_path() {
        push_candidate_from_codex_launch_path(candidates, &detected_path);
    }

    #[cfg(target_os = "macos")]
    for executable in CODEX_APP_SERVER_MACOS_EXECUTABLES {
        push_candidate_from_codex_launch_path(candidates, Path::new(executable));
    }
}

fn push_candidate_from_codex_launch_path(candidates: &mut Vec<PathBuf>, launch_path: &Path) {
    if let Some(app_server_path) = app_server_executable_from_codex_launch_path(launch_path) {
        // New macOS bundles embed the CLI in CodexCLI.app; keep the old layout
        // as a fallback for installations that have not migrated yet.
        if path_file_name_eq(&app_server_path, "codex")
            && parent_file_name_eq(&app_server_path, "resources")
        {
            if let Some(resources) = app_server_path.parent() {
                push_candidate(
                    candidates,
                    resources.join("codex-cli/CodexCLI.app/Contents/MacOS/codex"),
                );
            }
        }
        push_candidate(candidates, app_server_path);
    }
}

fn push_candidate(candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if path.as_os_str().is_empty() || candidates.iter().any(|candidate| candidate == &path) {
        return;
    }
    candidates.push(path);
}

fn app_server_executable_from_codex_launch_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }

    if is_existing_app_server_path_shape(path) {
        return Some(path.to_path_buf());
    }

    if path_file_name_eq(path, "codex.app") {
        return Some(path.join("Contents").join("Resources").join("codex"));
    }

    if path_file_name_eq(path, "chatgpt.app") {
        return Some(path.join("Contents").join("Resources").join("codex"));
    }

    if path_file_name_eq(path, "codex") && parent_file_name_eq(path, "macos") {
        let contents_dir = path.parent()?.parent()?;
        return Some(contents_dir.join("Resources").join("codex"));
    }

    if path_file_name_eq(path, "chatgpt") && parent_file_name_eq(path, "macos") {
        let contents_dir = path.parent()?.parent()?;
        return Some(contents_dir.join("Resources").join("codex"));
    }

    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if path_file_name_eq(&resolved, "chatgpt") && parent_file_name_eq(&resolved, "chatgpt") {
        return Some(resolved.parent()?.join("resources").join("codex"));
    }

    if path_file_name_eq(path, "codex.exe") {
        return Some(path.parent()?.join("resources").join("codex.exe"));
    }

    if path_file_name_eq(path, "chatgpt.exe") {
        return Some(path.parent()?.join("resources").join("codex.exe"));
    }

    None
}

fn is_existing_app_server_path_shape(path: &Path) -> bool {
    if path.ends_with("CodexCLI.app/Contents/MacOS/codex") {
        return true;
    }
    if path_file_name_eq(path, "codex") && parent_file_name_eq(path, "resources") {
        return true;
    }
    path_file_name_eq(path, "codex.exe") && parent_file_name_eq(path, "resources")
}

fn path_file_name_eq(path: &Path, expected: &str) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

fn parent_file_name_eq(path: &Path, expected: &str) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

fn build_app_server_command(executable: &Path, codex_home: &Path) -> Command {
    let mut command = Command::new(executable);
    crate::modules::process::apply_managed_proxy_env_to_command(&mut command);
    command
        .args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command
}

struct AppServerSession {
    child: Child,
    stdin: ChildStdin,
    receiver: mpsc::Receiver<String>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    next_id: i64,
}

impl AppServerSession {
    fn start(codex_home: &Path) -> Result<Self, String> {
        let executable = official_app_server_executable()?;
        let mut child = build_app_server_command(&executable, codex_home)
            .spawn()
            .map_err(|error| {
                format!(
                    "启动官方 Codex app-server 失败 ({} / CODEX_HOME={}): {}",
                    executable.display(),
                    codex_home.display(),
                    error
                )
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or("无法读取官方 app-server stdout")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("无法读取官方 app-server stderr")?;
        let stdin = child.stdin.take().ok_or("无法写入官方 app-server stdin")?;
        let (sender, receiver) = mpsc::channel();
        let stdout_reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                crate::modules::logger::log_warn(&format!(
                    "[Codex Official AppServer][stderr] {}",
                    line
                ));
            }
        });
        let mut session = Self {
            child,
            stdin,
            receiver,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            next_id: 1,
        };
        session.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "cockpit-tools",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            }),
        )?;
        Ok(session)
    }

    fn request(&mut self, method: &str, params: JsonValue) -> Result<JsonValue, String> {
        let id = self.next_id;
        self.next_id += 1;
        send_request(
            &mut self.stdin,
            json!({
                "method": method, "id": id, "params": params,
            }),
        )?;
        let response = wait_for_response_value(&self.receiver, id)?;
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("官方 app-server {} 响应缺少 result", method))
    }
}

impl Drop for AppServerSession {
    fn drop(&mut self) {
        finish_child(&mut self.child);
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

fn send_request(stdin: &mut impl Write, request: JsonValue) -> Result<(), String> {
    let line = serde_json::to_string(&request)
        .map_err(|error| format!("序列化官方 app-server 请求失败: {}", error))?;
    stdin
        .write_all(line.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("写入官方 app-server 请求失败: {}", error))
}

fn wait_for_response_value(
    receiver: &mpsc::Receiver<String>,
    request_id: i64,
) -> Result<JsonValue, String> {
    loop {
        let line = receiver
            .recv_timeout(APP_SERVER_RESPONSE_TIMEOUT)
            .map_err(|_| format!("等待官方 app-server 响应超时 (id={})", request_id))?;
        let Ok(value) = serde_json::from_str::<JsonValue>(&line) else {
            continue;
        };
        if value.get("id").and_then(JsonValue::as_i64) != Some(request_id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            crate::modules::logger::log_warn(&format!(
                "[Codex Official AppServer] response error: id={}, error={}",
                request_id, error
            ));
            return Err(format!(
                "官方 app-server 返回错误 (id={}): {}",
                request_id, error
            ));
        }
        if value.get("result").is_some() {
            return Ok(value);
        }
        return Err(format!(
            "官方 app-server 响应缺少 result (id={}): {}",
            request_id, value
        ));
    }
}

fn finish_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_continues_after_a_rejected_thread() {
        let mut visited = Vec::new();
        let result =
            delete_threads_with(&["first".into(), "blocked".into(), "last".into()], |id| {
                visited.push(id.to_string());
                if id == "blocked" {
                    Err("forked history still references it".into())
                } else {
                    Ok(())
                }
            });
        assert_eq!(visited, ["first", "blocked", "last"]);
        assert_eq!(result.deleted, ["first", "last"]);
        assert_eq!(
            result.failures,
            ["blocked: forked history still references it"]
        );
    }

    #[test]
    fn macos_candidates_prefer_embedded_cli_and_keep_legacy_fallback() {
        for launch in [
            "/fixture/ChatGPT.app",
            "/fixture/ChatGPT.app/Contents/MacOS/ChatGPT",
            "/fixture/ChatGPT.app/Contents/Resources/codex",
        ] {
            let mut candidates = Vec::new();
            push_candidate_from_codex_launch_path(&mut candidates, Path::new(launch));
            assert_eq!(candidates, vec![
                PathBuf::from("/fixture/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex"),
                PathBuf::from("/fixture/ChatGPT.app/Contents/Resources/codex"),
            ]);
        }
    }

    #[test]
    fn keeps_direct_embedded_cli_path() {
        let path = PathBuf::from(
            "/fixture/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
        );
        let mut candidates = Vec::new();
        push_candidate_from_codex_launch_path(&mut candidates, &path);
        push_candidate_from_codex_launch_path(&mut candidates, &path);
        assert_eq!(candidates, vec![path]);
    }

    #[test]
    fn maps_macos_launch_binary_to_resources_app_server() {
        let launch_path = PathBuf::from("/Applications/Codex.app/Contents/MacOS/Codex");
        let app_server_path = app_server_executable_from_codex_launch_path(&launch_path)
            .expect("resolve app-server path");

        assert_eq!(
            app_server_path,
            PathBuf::from("/Applications/Codex.app/Contents/Resources/codex")
        );
    }

    #[test]
    fn maps_chatgpt_macos_launch_binary_to_resources_app_server() {
        let launch_path = PathBuf::from("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT");
        let app_server_path = app_server_executable_from_codex_launch_path(&launch_path)
            .expect("resolve app-server path");

        assert_eq!(
            app_server_path,
            PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex")
        );
    }

    #[test]
    fn maps_macos_app_root_to_resources_app_server() {
        let launch_path = PathBuf::from("/Applications/Codex.app");
        let app_server_path = app_server_executable_from_codex_launch_path(&launch_path)
            .expect("resolve app-server path");

        assert_eq!(
            app_server_path,
            PathBuf::from("/Applications/Codex.app/Contents/Resources/codex")
        );
    }

    #[test]
    fn maps_chatgpt_macos_app_root_to_resources_app_server() {
        let launch_path = PathBuf::from("/Applications/ChatGPT.app");
        let app_server_path = app_server_executable_from_codex_launch_path(&launch_path)
            .expect("resolve app-server path");

        assert_eq!(
            app_server_path,
            PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex")
        );
    }

    #[test]
    fn maps_windows_launch_binary_to_resources_app_server() {
        let launch_path =
            PathBuf::from("C:/Program Files/WindowsApps/OpenAI.Codex_1.2.3/app/Codex.exe");
        let app_server_path = app_server_executable_from_codex_launch_path(&launch_path)
            .expect("resolve app-server path");

        assert_eq!(
            app_server_path,
            PathBuf::from(
                "C:/Program Files/WindowsApps/OpenAI.Codex_1.2.3/app/resources/codex.exe"
            )
        );
    }

    #[test]
    fn maps_chatgpt_windows_launch_binary_to_resources_app_server() {
        let launch_path =
            PathBuf::from("C:/Program Files/WindowsApps/OpenAI.ChatGPT_1.2.3/app/ChatGPT.exe");
        let app_server_path = app_server_executable_from_codex_launch_path(&launch_path)
            .expect("resolve app-server path");

        assert_eq!(
            app_server_path,
            PathBuf::from(
                "C:/Program Files/WindowsApps/OpenAI.ChatGPT_1.2.3/app/resources/codex.exe"
            )
        );
    }

    #[test]
    fn keeps_existing_resources_app_server_path() {
        let app_server_path = PathBuf::from(
            "C:/Program Files/WindowsApps/OpenAI.Codex_1.2.3/app/resources/codex.exe",
        );

        assert_eq!(
            app_server_executable_from_codex_launch_path(&app_server_path),
            Some(app_server_path)
        );
    }

    #[test]
    fn maps_linux_chatgpt_binary_to_resources_app_server() {
        let launch_path = PathBuf::from("/usr/lib/chatgpt/ChatGPT");
        assert_eq!(
            app_server_executable_from_codex_launch_path(&launch_path),
            Some(PathBuf::from("/usr/lib/chatgpt/resources/codex"))
        );
    }
}
