use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value as JsonValue};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use tokio_tungstenite::tungstenite::{self, Message, WebSocket};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "macos")]
const CODEX_APP_SERVER_MACOS_EXECUTABLES: &[&str] = &[
    "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
    "/Applications/ChatGPT.app/Contents/Resources/codex",
    "/Applications/Codex.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
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
    rebuild_imported_thread_metadata(codex_home, &[])
}

pub fn rebuild_imported_thread_metadata(
    codex_home: &Path,
    mapped_threads: &[(String, String)],
) -> Result<(), String> {
    // The official list call repairs rollout metadata before project assignment.
    let mut server = AppServerSession::start(codex_home)?;
    list_threads_from(&mut server, false, None)?;
    if !mapped_threads.is_empty() {
        assign_imported_threads_to_projects(mapped_threads, |method, params, deadline| {
            server.request_until(method, params, deadline)
        })?;
    }
    drop(server);
    crate::modules::codex_session_visibility::normalize_official_thread_cwds(codex_home)?;
    Ok(())
}

pub(crate) fn list_threads(
    codex_home: &Path,
    archived: bool,
    ancestor_thread_id: Option<&str>,
) -> Result<Vec<JsonValue>, String> {
    let mut server = AppServerSession::start(codex_home)?;
    list_threads_from(&mut server, archived, ancestor_thread_id)
}

fn list_threads_from(
    server: &mut AppServerSession,
    archived: bool,
    ancestor_thread_id: Option<&str>,
) -> Result<Vec<JsonValue>, String> {
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

fn assign_imported_threads_to_projects(
    mapped_threads: &[(String, String)],
    mut request: impl FnMut(&str, JsonValue, Instant) -> Result<JsonValue, AppServerWaitFailure>,
) -> Result<(), String> {
    let mut cursor = JsonValue::Null;
    let mut projects = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen_cursors = std::collections::HashSet::new();
    loop {
        if Instant::now() >= deadline {
            return Err("更新项目归属超时，已导入的会话将保留".into());
        }
        let result = match request(
            "project/list",
            json!({ "cursor": cursor, "limit": 100 }),
            deadline,
        ) {
            Ok(response) => response,
            Err(error) => {
                let message = error.message();
                // Older Codex versions group by cwd and do not expose project identities.
                if message.contains("-32601") || message.contains("unknown variant `project/list`")
                {
                    return Ok(());
                }
                return Err(message.to_string());
            }
        };
        projects.extend(
            result
                .get("data")
                .and_then(JsonValue::as_array)
                .ok_or("project/list 响应缺少项目列表")?
                .iter()
                .cloned(),
        );
        cursor = result.get("nextCursor").cloned().unwrap_or(JsonValue::Null);
        if cursor.is_null() {
            break;
        }
        if !seen_cursors.insert(cursor.to_string()) || projects.len() >= 10_000 {
            return Err("官方项目列表分页异常，已导入的会话将保留".into());
        }
    }
    let mut warnings = Vec::new();
    for (thread_id, cwd) in mapped_threads {
        if Instant::now() >= deadline {
            return Err("更新项目归属超时，已导入的会话将保留".into());
        }
        let project_id =
            match crate::modules::codex_session_import_paths::project_id_for_cwd(&projects, cwd) {
                Ok(Some(project_id)) => project_id,
                Ok(None) => {
                    warnings.push(format!(
                        "未找到目标目录对应的 Codex 项目，请先在 Codex 中添加该项目: {}",
                        cwd
                    ));
                    continue;
                }
                Err(error) => {
                    warnings.push(error);
                    continue;
                }
            };
        if let Err(error) = request(
            "thread/metadata/update",
            json!({ "threadId": thread_id, "projectId": project_id }),
            deadline,
        ) {
            if error.is_timeout() {
                return Err(error.message().to_string());
            }
            warnings.push(error.message().to_string());
        }
    }
    warnings.sort();
    warnings.dedup();
    if warnings.is_empty() {
        Ok(())
    } else {
        Err(warnings.join("；"))
    }
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
    let mut server = AppServerSession::for_deletion(codex_home)?;
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
    let error = format!("未找到官方 Codex app-server 可执行文件: {}", searched_paths);
    crate::modules::logger::log_warn(&format!("[Codex Official AppServer] {}", error));
    Err(error)
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
        // Current macOS bundles contain a signed CLI helper. Preserve the old
        // executable as a fallback for installations that have not upgraded.
        if parent_file_name_eq(&app_server_path, "resources")
            && path_file_name_eq(&app_server_path, "codex")
            && app_server_path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .is_some_and(|root| {
                    path_file_name_eq(root, "chatgpt.app") || path_file_name_eq(root, "codex.app")
                })
        {
            push_candidate(
                candidates,
                app_server_path
                    .parent()
                    .unwrap()
                    .join("codex-cli/CodexCLI.app/Contents/MacOS/codex"),
            );
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
    if path_file_name_eq(path, "codex") && parent_file_name_eq(path, "macos") {
        if path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .is_some_and(|root| path_file_name_eq(root, "codexcli.app"))
        {
            return true;
        }
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

enum AppServerTransport {
    Stdio(AppServerProcess),
    #[cfg(unix)]
    Shared(Box<WebSocket<UnixStream>>),
}

struct AppServerProcess {
    child: Child,
    stdin: ChildStdin,
    receiver: mpsc::Receiver<String>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
}

struct AppServerSession {
    transport: AppServerTransport,
    next_id: i64,
}

impl AppServerSession {
    fn for_deletion(codex_home: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        if let Some(socket) = connect_shared_app_server(codex_home)? {
            return Self::initialize(AppServerTransport::Shared(Box::new(socket)));
        }
        Self::start(codex_home)
    }

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
        Self::initialize(AppServerTransport::Stdio(AppServerProcess {
            child,
            stdin,
            receiver,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
        }))
    }

    fn initialize(transport: AppServerTransport) -> Result<Self, String> {
        let mut session = Self {
            transport,
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
        session.send(json!({ "method": "initialized" }))?;
        Ok(session)
    }

    fn send(&mut self, request: JsonValue) -> Result<(), String> {
        match &mut self.transport {
            AppServerTransport::Stdio(process) => send_request(&mut process.stdin, request),
            #[cfg(unix)]
            AppServerTransport::Shared(socket) => socket
                .send(Message::Text(request.to_string().into()))
                .map_err(|error| format!("写入官方共享 app-server 失败: {}", error)),
        }
    }

    fn request(&mut self, method: &str, params: JsonValue) -> Result<JsonValue, String> {
        self.request_until(method, params, Instant::now() + APP_SERVER_RESPONSE_TIMEOUT)
            .map_err(|error| error.message().to_string())
    }

    fn request_until(
        &mut self,
        method: &str,
        params: JsonValue,
        deadline: Instant,
    ) -> Result<JsonValue, AppServerWaitFailure> {
        let deadline = deadline.min(Instant::now() + APP_SERVER_RESPONSE_TIMEOUT);
        if Instant::now() >= deadline {
            return Err(AppServerWaitFailure::Timeout(
                "更新项目归属超时，已导入的会话将保留".into(),
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "method": method, "id": id, "params": params }))
            .map_err(AppServerWaitFailure::Response)?;
        let response = match &mut self.transport {
            AppServerTransport::Stdio(process) => {
                wait_for_response_value_until(&process.receiver, id, deadline)?
            }
            #[cfg(unix)]
            AppServerTransport::Shared(socket) => {
                wait_for_socket_response_until(socket, id, deadline)?
            }
        };
        response.get("result").cloned().ok_or_else(|| {
            AppServerWaitFailure::Response(format!("官方 app-server {} 响应缺少 result", method))
        })
    }
}

impl Drop for AppServerSession {
    fn drop(&mut self) {
        match &mut self.transport {
            AppServerTransport::Stdio(process) => {
                finish_child(&mut process.child);
                if let Some(reader) = process.stdout_reader.take() {
                    let _ = reader.join();
                }
                if let Some(reader) = process.stderr_reader.take() {
                    let _ = reader.join();
                }
            }
            #[cfg(unix)]
            AppServerTransport::Shared(socket) => {
                let _ = socket.close(None);
            }
        }
    }
}

#[cfg(unix)]
fn connect_shared_app_server(codex_home: &Path) -> Result<Option<WebSocket<UnixStream>>, String> {
    let path = codex_home.join("app-server-control/app-server-control.sock");
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None)
        }
        Err(error) => {
            return Err(format!(
                "连接官方 app-server 失败 ({}): {}",
                path.display(),
                error
            ))
        }
    };
    stream
        .set_read_timeout(Some(APP_SERVER_RESPONSE_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(APP_SERVER_RESPONSE_TIMEOUT)))
        .map_err(|error| format!("设置官方 app-server 连接超时失败: {}", error))?;
    let config =
        tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(32 * 1024 * 1024));
    // The control socket speaks WebSocket, not the stdio JSONL protocol.
    // A listening service's handshake failure must not start a second server.
    let (socket, _) =
        tungstenite::client::client_with_config("ws://localhost/", stream, Some(config)).map_err(
            |error| {
                format!(
                    "官方 app-server WebSocket 握手失败 ({}): {}",
                    path.display(),
                    error
                )
            },
        )?;
    Ok(Some(socket))
}

#[cfg(unix)]
fn wait_for_socket_response_until(
    socket: &mut WebSocket<UnixStream>,
    request_id: i64,
    deadline: Instant,
) -> Result<JsonValue, AppServerWaitFailure> {
    let deadline = deadline.min(Instant::now() + APP_SERVER_RESPONSE_TIMEOUT);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AppServerWaitFailure::Timeout(format!(
                "等待官方 app-server 响应超时 (id={})",
                request_id
            )));
        }
        socket
            .get_mut()
            .set_read_timeout(Some(remaining))
            .map_err(|error| {
                AppServerWaitFailure::Response(format!(
                    "设置官方 app-server 连接超时失败: {}",
                    error
                ))
            })?;
        match socket.read().map_err(|error| match &error {
            tungstenite::Error::Io(io)
                if matches!(
                    io.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                AppServerWaitFailure::Timeout(format!(
                    "等待官方 app-server 响应超时 (id={})",
                    request_id
                ))
            }
            _ => AppServerWaitFailure::Response(format!("读取官方共享 app-server 失败: {}", error)),
        })? {
            Message::Text(line) => {
                if let Some(response) = parse_response_value(&line, request_id)
                    .map_err(AppServerWaitFailure::Response)?
                {
                    return Ok(response);
                }
            }
            Message::Close(_) => {
                return Err(AppServerWaitFailure::Response(
                    "官方共享 app-server 已关闭连接".into(),
                ))
            }
            _ => {}
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

#[derive(Debug)]
enum AppServerWaitFailure {
    Timeout(String),
    Response(String),
}

impl AppServerWaitFailure {
    fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }

    fn message(&self) -> &str {
        match self {
            Self::Timeout(message) | Self::Response(message) => message,
        }
    }
}

fn wait_for_response_value_until(
    receiver: &mpsc::Receiver<String>,
    request_id: i64,
    deadline: Instant,
) -> Result<JsonValue, AppServerWaitFailure> {
    let deadline = deadline.min(Instant::now() + APP_SERVER_RESPONSE_TIMEOUT);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AppServerWaitFailure::Timeout(format!(
                "等待官方 app-server 响应超时 (id={})",
                request_id
            )));
        }
        let line = receiver.recv_timeout(remaining).map_err(|_| {
            AppServerWaitFailure::Timeout(format!(
                "等待官方 app-server 响应超时 (id={})",
                request_id
            ))
        })?;
        if let Some(value) =
            parse_response_value(&line, request_id).map_err(AppServerWaitFailure::Response)?
        {
            return Ok(value);
        }
    }
}

fn parse_response_value(line: &str, request_id: i64) -> Result<Option<JsonValue>, String> {
    let Ok(value) = serde_json::from_str::<JsonValue>(line) else {
        return Ok(None);
    };
    if value.get("id").and_then(JsonValue::as_i64) != Some(request_id) {
        return Ok(None);
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
    if value.get("result").is_none() {
        return Err(format!(
            "官方 app-server 响应缺少 result (id={}): {}",
            request_id, value
        ));
    }
    Ok(Some(value))
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

    fn assign_projects_with_responses(
        stdin: &mut impl Write,
        receiver: &mpsc::Receiver<String>,
        mapped_threads: &[(String, String)],
    ) -> Result<(), String> {
        let mut request_id = 3;
        assign_imported_threads_to_projects(mapped_threads, |method, params, deadline| {
            let id = request_id;
            request_id += 1;
            send_request(stdin, json!({"method": method, "id": id, "params": params}))
                .map_err(AppServerWaitFailure::Response)?;
            let response = wait_for_response_value_until(receiver, id, deadline)?;
            Ok(response["result"].clone())
        })
    }

    #[test]
    fn imported_project_binding_rejects_repeated_pagination_cursor() {
        let (sender, receiver) = mpsc::channel();
        for id in [3, 4] {
            sender
                .send(json!({"id":id,"result":{"data":[],"nextCursor":"repeat"}}).to_string())
                .unwrap();
        }
        let mut stdin = Vec::new();
        assert!(assign_projects_with_responses(
            &mut stdin,
            &receiver,
            &[("thread".into(), "/new/project".into())]
        )
        .unwrap_err()
        .contains("分页异常"));
        assert_eq!(String::from_utf8(stdin).unwrap().lines().count(), 2);
    }

    #[test]
    fn unrelated_notifications_do_not_extend_the_response_deadline() {
        let (sender, receiver) = mpsc::channel();
        for _ in 0..100 {
            sender
                .send(json!({"method":"notification"}).to_string())
                .unwrap();
        }
        let error =
            wait_for_response_value_until(&receiver, 3, Instant::now() + Duration::from_millis(10))
                .err()
                .unwrap();
        assert!(error.is_timeout());
    }

    #[test]
    fn imported_project_binding_paginates_and_updates_matching_project() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(json!({"id":3,"result":{"data":[],"nextCursor":"page-2"}}).to_string())
            .unwrap();
        sender.send(json!({"id":4,"result":{"data":[{"id":"destination","roots":[{"path":"/b/project"}]}],"nextCursor":null}}).to_string()).unwrap();
        sender
            .send(json!({"id":5,"result":{}}).to_string())
            .unwrap();
        let mut stdin = Vec::new();
        assign_projects_with_responses(
            &mut stdin,
            &receiver,
            &[("thread-1".into(), "/b/project".into())],
        )
        .unwrap();
        let requests = String::from_utf8(stdin)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<JsonValue>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(requests[1]["params"]["cursor"], "page-2");
        assert_eq!(requests[2]["method"], "thread/metadata/update");
        assert_eq!(requests[2]["params"]["projectId"], "destination");
        assert_eq!(requests[2]["params"]["threadId"], "thread-1");
    }

    #[test]
    fn older_servers_without_project_api_keep_cwd_fallback() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(
                json!({"id":3,"error":{"code":-32600,"message":"unknown variant `project/list`"}})
                    .to_string(),
            )
            .unwrap();
        let mut stdin = Vec::new();
        assign_projects_with_responses(
            &mut stdin,
            &receiver,
            &[("thread-1".into(), "/b/project".into())],
        )
        .unwrap();
        assert_eq!(String::from_utf8(stdin).unwrap().lines().count(), 1);
    }

    #[test]
    fn unmatched_project_returns_warning_without_assigning_arbitrary_project() {
        let (sender, receiver) = mpsc::channel();
        sender.send(json!({"id":3,"result":{"data":[{"id":"other","roots":[{"path":"/other"}]}],"nextCursor":null}}).to_string()).unwrap();
        let mut stdin = Vec::new();
        let result = assign_projects_with_responses(
            &mut stdin,
            &receiver,
            &[("thread-1".into(), "/b/project".into())],
        );
        assert!(result
            .unwrap_err()
            .contains("未找到目标目录对应的 Codex 项目"));
        assert_eq!(String::from_utf8(stdin).unwrap().lines().count(), 1);
    }

    #[cfg(unix)]
    struct SocketHome(PathBuf);

    #[cfg(unix)]
    impl SocketHome {
        fn new() -> Self {
            // Keep the control socket path below macOS's Unix socket limit.
            let home =
                PathBuf::from("/tmp").join(format!("ctws-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(home.join("app-server-control")).unwrap();
            Self(home)
        }

        fn socket_path(&self) -> PathBuf {
            self.0.join("app-server-control/app-server-control.sock")
        }
    }

    #[cfg(unix)]
    impl Drop for SocketHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn shared_deletion_uses_the_existing_service_and_continues_after_rejection() {
        use std::os::unix::net::UnixListener;
        let home = SocketHome::new();
        let listener = UnixListener::bind(home.socket_path()).unwrap();
        let service = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let initialize: JsonValue =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialize["method"], "initialize");
            assert_eq!(
                initialize["params"]["capabilities"]["experimentalApi"],
                true
            );
            socket
                .send(Message::Text(
                    json!({"id": initialize["id"], "result": {}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            let initialized: JsonValue =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");

            for (thread_id, rejected) in [("blocked", true), ("idle", false)] {
                let request: JsonValue =
                    serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(request["method"], "thread/delete");
                assert_eq!(request["params"]["threadId"], thread_id);
                let response = if rejected {
                    json!({"id": request["id"], "error": {"message": "delete rejected"}})
                } else {
                    // Notifications must not be mistaken for the RPC response.
                    socket
                        .send(Message::Text(
                            json!({"method": "thread/deleted", "params": {"threadId": thread_id}})
                                .to_string()
                                .into(),
                        ))
                        .unwrap();
                    json!({"id": request["id"], "result": {}})
                };
                socket
                    .send(Message::Text(response.to_string().into()))
                    .unwrap();
            }
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
            // Dropping the Cockpit client leaves the official service available.
            let (next, _) = listener.accept().unwrap();
            drop(next);
        });
        let result = delete_threads(&home.0, &["blocked".into(), "idle".into()]).unwrap();
        assert_eq!(result.deleted, ["idle"]);
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].contains("delete rejected"));
        drop(UnixStream::connect(home.socket_path()).unwrap());
        service.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_absent_or_refused_shared_sockets_allow_a_temporary_service() {
        use std::os::unix::net::{UnixDatagram, UnixListener};
        let home = SocketHome::new();
        assert!(connect_shared_app_server(&home.0).unwrap().is_none());
        let listener = UnixListener::bind(home.socket_path()).unwrap();
        drop(listener);
        assert!(connect_shared_app_server(&home.0).unwrap().is_none());
        // A stale socket is left for Codex to manage, not removed by Cockpit.
        assert!(home.socket_path().exists());
        std::fs::remove_file(home.socket_path()).unwrap();
        // A mismatched socket type fails consistently on macOS and Linux.
        let _datagram = UnixDatagram::bind(home.socket_path()).unwrap();
        assert!(connect_shared_app_server(&home.0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn shared_handshake_failure_does_not_start_a_temporary_service() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;
        let home = SocketHome::new();
        let listener = UnixListener::bind(home.socket_path()).unwrap();
        let service = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0; 4096];
            stream.read(&mut buffer).unwrap();
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let error = match AppServerSession::for_deletion(&home.0) {
            Err(error) => error,
            Ok(_) => panic!("failed handshake must not start a different service"),
        };
        assert!(error.contains("WebSocket 握手失败"), "{error}");
        service.join().unwrap();
    }

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
    fn prioritizes_nested_macos_cli_and_preserves_legacy_fallback() {
        for root in ["/Applications/ChatGPT.app", "/Volumes/Apps Disk/Codex.app"] {
            for launch in [
                PathBuf::from(root),
                PathBuf::from(root).join("Contents/MacOS/ChatGPT"),
            ] {
                let mut candidates = Vec::new();
                push_candidate_from_codex_launch_path(&mut candidates, &launch);
                assert_eq!(
                    candidates,
                    vec![
                        PathBuf::from(root)
                            .join("Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex"),
                        PathBuf::from(root).join("Contents/Resources/codex"),
                    ]
                );
            }
        }
    }

    #[test]
    fn preserves_direct_nested_macos_cli_executable() {
        let path = PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex");
        let mut candidates = Vec::new();
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
