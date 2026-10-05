use crate::models::codex::{CodexAccount, CodexApiProviderMode};
use crate::models::ssh_server::{
    SshAuthConfig, SshCodexSyncResult, SshCodexSyncStatus, SshRemoteAccountSummary, SshServer,
    SshServerStore, SshSyncStage,
};
use crate::modules::{account, atomic_write, codex_account, logger};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tauri::Emitter;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;

const SSH_SERVERS_FILE: &str = "ssh_servers.json";
const STORE_VERSION: &str = "2";
static STORE_LOCK: Mutex<()> = Mutex::new(());
static SYNC_STATUSES: LazyLock<Mutex<HashMap<String, SshCodexSyncStatus>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// TCP/SSH 握手超时（传给 OpenSSH ConnectTimeout）
const CONNECTION_TIMEOUT_SECS: u64 = 12;
/// 测连整段命令墙钟超时
const TEST_COMMAND_TIMEOUT_SECS: u64 = 20;
/// 读写同步脚本墙钟超时
const SYNC_TIMEOUT_SECS: u64 = 45;
const APPLY_TIMEOUT_SECS: u64 = 120;
const REMOTE_PYTHON_TIMEOUT_SECS: u64 = 30;
const REMOTE_PYTHON_MAX_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshServerList {
    pub selected_server_ids: Vec<String>,
    pub servers: Vec<SshServer>,
}

fn now_timestamp() -> i64 {
    chrono::Utc::now().timestamp()
}

fn store_path() -> Result<PathBuf, String> {
    Ok(account::get_data_dir()?.join(SSH_SERVERS_FILE))
}

fn default_codex_home() -> String {
    "~/.codex".to_string()
}

fn contains_control_separator(value: &str) -> bool {
    value.contains('\n') || value.contains('\r') || value.contains('\0')
}

fn normalize_text(value: &str) -> String {
    value.trim().to_string()
}

fn sanitize_error(error: impl ToString) -> String {
    let mut value = error.to_string();
    for marker in [
        "OPENAI_API_KEY",
        "access_token",
        "refresh_token",
        "id_token",
    ] {
        value = redact_secret_values(&value, marker);
    }
    value
}

fn redact_secret_values(value: &str, marker: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(index) = remaining.find(marker) {
        let (before, matched_and_after) = remaining.split_at(index);
        output.push_str(before);
        output.push_str(marker);

        let after_marker = &matched_and_after[marker.len()..];
        let Some((delimiter_end, quote)) = secret_value_start(after_marker) else {
            remaining = after_marker;
            continue;
        };
        output.push_str(&after_marker[..delimiter_end]);

        let value_start = delimiter_end;
        let value_end = secret_value_end(&after_marker[value_start..], quote);
        output.push_str("[redacted]");
        remaining = &after_marker[value_start + value_end..];
    }
    output.push_str(remaining);
    output
}

fn secret_value_start(value: &str) -> Option<(usize, Option<char>)> {
    let mut chars = value.char_indices().peekable();
    let mut end = 0;
    while let Some((index, ch)) = chars.peek().copied() {
        if ch.is_whitespace() || ch == '"' || ch == '\'' {
            end = index + ch.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    let (_, delimiter) = chars.next()?;
    if delimiter != '=' && delimiter != ':' {
        return None;
    }
    end += delimiter.len_utf8();
    while let Some((index, ch)) = chars.peek().copied() {
        if ch.is_whitespace() {
            end = index + ch.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    if let Some((index, quote @ ('"' | '\''))) = chars.peek().copied() {
        return Some((index + quote.len_utf8(), Some(quote)));
    }
    Some((end, None))
}

fn secret_value_end(value: &str, quote: Option<char>) -> usize {
    match quote {
        Some(quote) => value.find(quote).unwrap_or(value.len()),
        None => value
            .find(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';' || ch == '}')
            .unwrap_or(value.len()),
    }
}

fn validate_server(server: &SshServer) -> Result<(), String> {
    if server.name.trim().is_empty() {
        return Err("SSH server name is required".to_string());
    }
    for (label, value) in [
        ("host", server.host.as_str()),
        ("username", server.username.as_str()),
        ("codex_home", server.codex_home.as_str()),
    ] {
        if value.trim().is_empty() && label != "username" {
            return Err(format!("SSH server {} is required", label));
        }
        if contains_control_separator(value) {
            return Err(format!(
                "SSH server {} contains unsupported characters",
                label
            ));
        }
    }
    if server.host.starts_with('-')
        || server.host.chars().any(char::is_whitespace)
        || server.username.starts_with('-')
        || server.username.chars().any(char::is_whitespace)
    {
        return Err("SSH host and username contain unsupported characters".to_string());
    }
    match &server.auth {
        SshAuthConfig::Agent => {}
        SshAuthConfig::PrivateKeyFile { path } => {
            if path.trim().is_empty() {
                return Err("SSH private key path is required".to_string());
            }
            if contains_control_separator(path) {
                return Err("SSH private key path contains unsupported characters".to_string());
            }
        }
    }
    Ok(())
}

fn normalize_server(
    mut server: SshServer,
    existing: Option<&SshServer>,
) -> Result<SshServer, String> {
    let now = now_timestamp();
    if server.id.trim().is_empty() {
        server.id = Uuid::new_v4().to_string();
    } else {
        server.id = normalize_text(&server.id);
    }
    server.name = normalize_text(&server.name);
    server.host = normalize_text(&server.host);
    if server.name.is_empty() { server.name = server.host.clone(); }
    server.username = normalize_text(&server.username);
    server.codex_home = normalize_text(&server.codex_home);
    if server.codex_home.is_empty() {
        server.codex_home = default_codex_home();
    }

    if server.created_at <= 0 {
        server.created_at = existing.map(|item| item.created_at).unwrap_or(now);
    }
    server.updated_at = now;
    if let Some(existing) = existing {
        server.last_sync = existing.last_sync.clone();
        server.sync_on_codex_switch = existing.sync_on_codex_switch;
    }
    validate_server(&server)?;
    Ok(server)
}

pub fn load_store() -> Result<SshServerStore, String> {
    let path = store_path()?;
    if !path.exists() {
        return Ok(SshServerStore::default());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read SSH servers store: {}", e))?;
    let mut store: SshServerStore = atomic_write::parse_json_with_auto_restore(&path, &content)
        .map_err(|e| format!("Failed to parse SSH servers store: {}", e))?;
    migrate_selection(&mut store);
    let statuses = SYNC_STATUSES.lock().unwrap_or_else(|e| e.into_inner());
    for server in &mut store.servers {
        server.last_sync = statuses.get(&server.id).cloned();
    }
    Ok(store)
}

fn save_store(store: &SshServerStore) -> Result<(), String> {
    let path = store_path()?;
    let mut settings = store.clone();
    for server in &mut settings.servers { server.last_sync = None; }
    let content = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("Failed to serialize SSH servers store: {}", e))?;
    atomic_write::write_string_atomic(&path, &content)
}

pub fn list_servers() -> Result<SshServerList, String> {
    let store = load_store()?;
    Ok(SshServerList {
        selected_server_ids: store.selected_server_ids,
        servers: store.servers,
    })
}

pub fn upsert_server(server: SshServer) -> Result<SshServerList, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_store()?;
    store.version = STORE_VERSION.to_string();
    let existing_index = store.servers.iter().position(|item| item.id == server.id);
    let existing = existing_index.and_then(|index| store.servers.get(index));
    let server = normalize_server(server, existing)?;
    if let Some(index) = existing_index {
        let existing = &store.servers[index];
        if existing.host != server.host
            || existing.username != server.username
            || existing.port != server.port
            || existing.codex_home != server.codex_home
            || existing.auth != server.auth
            || (existing.sync_on_codex_switch && !server.sync_on_codex_switch)
        {
            cancel_server_jobs(existing);
        }
        store.servers[index] = server;
    } else {
        store.servers.push(server);
    }
    save_store(&store)?;
    list_servers()
}

pub fn delete_server(server_id: &str) -> Result<SshServerList, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_store()?;
    let server_id = server_id.trim();
    if let Some(server) = store.servers.iter().find(|server| server.id == server_id) {
        cancel_server_jobs(server);
    }
    SYNC_STATUSES.lock().unwrap_or_else(|e| e.into_inner()).remove(server_id);
    store.servers.retain(|server| server.id != server_id);
    store.selected_server_ids.retain(|id| id != server_id);
    store.selected_server_id = store.selected_server_ids.first().cloned();
    save_store(&store)?;
    list_servers()
}

fn migrate_selection(store: &mut SshServerStore) {
    // Migrate only an explicitly enabled legacy recipient. An empty v2 list
    // stays empty; a legacy target with automatic sync disabled is not opt-in.
    if store.version != STORE_VERSION && store.selected_server_ids.is_empty() {
        if let Some(id) = store.selected_server_id.as_ref() {
            if store
                .servers
                .iter()
                .any(|server| &server.id == id && server.sync_on_codex_switch)
            {
                store.selected_server_ids.push(id.clone());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    store.selected_server_ids.retain(|id| {
        store.servers.iter().any(|server| &server.id == id) && seen.insert(id.clone())
    });
    store.selected_server_id = store.selected_server_ids.first().cloned();
    store.version = STORE_VERSION.to_string();
}

pub fn select_servers(server_ids: Vec<String>) -> Result<SshServerList, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_store()?;
    let mut selected = Vec::new();
    for id in server_ids {
        let id = id.trim().to_string();
        if id.is_empty() {
            continue;
        }
        if !store.servers.iter().any(|server| server.id == id) {
            return Err(format!("SSH server not found: {}", id));
        }
        if !selected.contains(&id) {
            selected.push(id);
        }
    }
    for server in &store.servers {
        if store.selected_server_ids.contains(&server.id) && !selected.contains(&server.id) {
            cancel_server_jobs(server);
        }
    }
    apply_selection(&mut store, selected);
    save_store(&store)?;
    list_servers()
}

fn apply_selection(store: &mut SshServerStore, selected: Vec<String>) {
    for server in &mut store.servers {
        server.sync_on_codex_switch = selected.contains(&server.id);
    }
    store.selected_server_ids = selected;
    store.selected_server_id = store.selected_server_ids.first().cloned();
}

fn selected_servers(store: &SshServerStore, automatic: bool) -> Vec<SshServer> {
    store
        .servers
        .iter()
        .filter(|server| {
            store.selected_server_ids.contains(&server.id)
                && (!automatic || server.sync_on_codex_switch)
        })
        .cloned()
        .collect()
}

/// OpenSSH 参数：非交互、握手超时与私钥 IdentitiesOnly，避免 agent 里一堆 key 拖慢/超时。
fn build_ssh_args(server: &SshServer, connect_timeout_secs: u64) -> Vec<String> {
    let connect_timeout = connect_timeout_secs.clamp(3, 30);
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "NumberOfPasswordPrompts=0".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", connect_timeout),
        "-o".to_string(),
        "ServerAliveInterval=5".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=2".to_string(),
    ];
    if server.port != 0 {
        args.extend(["-p".to_string(), server.port.to_string()]);
    }
    if let SshAuthConfig::PrivateKeyFile { path } = &server.auth {
        // 与手动 `ssh -o IdentitiesOnly=yes -i key` 对齐：只用指定私钥，不试 agent 其它身份
        args.push("-o".to_string());
        args.push("IdentitiesOnly=yes".to_string());
        args.push("-i".to_string());
        args.push(path.clone());
    }
    args.push("--".to_string());
    args.push(if server.username.is_empty() {
        server.host.clone()
    } else {
        format!("{}@{}", server.username, server.host)
    });
    args
}

async fn run_ssh(
    server: &SshServer,
    timeout_secs: u64,
    remote_args: &[&str],
    stdin_payload: Option<String>,
) -> Result<String, String> {
    // 握手超时与整段墙钟分开：ConnectTimeout 用连接上限，命令本身可更长
    let connect_timeout = CONNECTION_TIMEOUT_SECS.min(timeout_secs);
    let mut command = Command::new("ssh");
    command.args(build_ssh_args(server, connect_timeout));
    command.args(remote_args);
    command.kill_on_drop(true);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    if stdin_payload.is_some() {
        command.stdin(Stdio::piped());
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("ssh_binary_missing: {}", e))?;
    if let Some(payload) = stdin_payload {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "ssh_connection_failed: stdin unavailable".to_string())?;
        stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| format!("ssh_connection_failed: {}", e))?;
        // 尽快关闭 stdin，避免远端 sh -s 一直等 EOF
        drop(stdin);
    }

    let output = timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| "ssh_connection_failed: SSH command timed out".to_string())?
        .map_err(|e| format!("ssh_connection_failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let category = if stderr.to_ascii_lowercase().contains("permission denied") {
            "ssh_auth_failed"
        } else {
            "ssh_connection_failed"
        };
        return Err(format!(
            "{}: {}",
            category,
            sanitize_error(if stderr.is_empty() {
                format!("exit status {}", output.status)
            } else {
                stderr
            })
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub async fn test_connection(server_id: &str) -> Result<String, String> {
    let store = load_store()?;
    let server = store
        .servers
        .iter()
        .find(|server| server.id == server_id)
        .cloned()
        .ok_or_else(|| format!("SSH server not found: {}", server_id))?;
    let output = run_ssh(
        &server,
        TEST_COMMAND_TIMEOUT_SECS,
        &["printf", "cockpit-tools-ssh-ok"],
        None,
    )
    .await?;
    if output.trim() == "cockpit-tools-ssh-ok" {
        Ok(output)
    } else {
        Err("ssh_connection_failed: unexpected SSH test output".to_string())
    }
}

/// Run a small Python request on a saved host. The caller cannot substitute a
/// different CODEX_HOME; stderr and remote exception text are never returned.
pub async fn run_remote_python(
    server_id: &str,
    script: &str,
    payload: &serde_json::Value,
) -> Result<String, String> {
    let server = load_store()?
        .servers
        .into_iter()
        .find(|server| server.id == server_id)
        .ok_or_else(|| format!("SSH server not found: {}", server_id))?;
    run_remote_python_on_server(&server, script, payload, REMOTE_PYTHON_TIMEOUT_SECS).await
}

pub(crate) async fn run_remote_usage_python(
    server_id: &str,
    script: &str,
    payload: &serde_json::Value,
) -> Result<String, String> {
    let server = load_store()?.servers.into_iter().find(|server| server.id == server_id)
        .ok_or_else(|| format!("SSH server not found: {}", server_id))?;
    // First-use compilation and a full remote rebuild can exceed normal RPC timeouts.
    run_remote_python_on_server(&server, script, payload, 1200).await
}

async fn run_remote_python_on_server(
    server: &SshServer,
    script: &str,
    payload: &serde_json::Value,
    timeout_secs: u64,
) -> Result<String, String> {
    validate_server(&server)?;
    let mut request = payload
        .as_object()
        .cloned()
        .ok_or("Remote Python payload must be a JSON object")?;
    request.insert(
        "codex_home".into(),
        serde_json::Value::String(server.codex_home.clone()),
    );
    let bytes = serde_json::to_vec(&request).map_err(|_| "Cannot encode remote request")?;
    if bytes.len() as u64 > REMOTE_PYTHON_MAX_BYTES {
        return Err("Remote request exceeds size limit".into());
    }
    let mut command = Command::new("ssh");
    command.args(build_ssh_args(&server, CONNECTION_TIMEOUT_SECS));
    command.arg(format!("python3 -c {}", shell_quote(script)));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|_| "Cannot start SSH")?;
    let mut stdin = child.stdin.take().ok_or("SSH stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("SSH stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("SSH stderr unavailable")?;
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let operation = async {
        stdin
            .write_all(&bytes)
            .await
            .map_err(|_| "SSH request write failed")?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|_| "SSH request write failed")?;
        drop(stdin);
        let mut bounded_stdout = stdout.take(REMOTE_PYTHON_MAX_BYTES + 1);
        let mut bounded_stderr = stderr.take(REMOTE_PYTHON_MAX_BYTES + 1);
        let reads = tokio::try_join!(
            bounded_stdout.read_to_end(&mut output),
            bounded_stderr.read_to_end(&mut diagnostics),
        );
        reads.map_err(|_| "SSH response read failed")?;
        if output.len() as u64 > REMOTE_PYTHON_MAX_BYTES
            || diagnostics.len() as u64 > REMOTE_PYTHON_MAX_BYTES
        {
            return Err("Remote response exceeds size limit");
        }
        let status = child.wait().await.map_err(|_| "SSH wait failed")?;
        if !status.success() {
            return Err("Remote Python request failed; check SSH access and remote Python 3");
        }
        String::from_utf8(output).map_err(|_| "Remote response is not UTF-8")
    };
    timeout(Duration::from_secs(timeout_secs), operation)
        .await
        .map_err(|_| "Remote Python request timed out".to_string())?
        .map_err(str::to_string)
}

/// Official-account sync touches only auth.json. API sync applies a validated bundle.
/// A directory flock covers
/// transfer and app-server reload, including independent SSH aliases. Its
/// stdin lease cancels a pending stop if the desktop job disappears.
/// Python 3 and POSIX flock are required on the remote Linux host.
const REMOTE_SYNC_SCRIPT: &str = r#"
import base64, fcntl, hashlib, json, os, select, signal, socket, stat, struct, sys, tempfile, threading, time

def emit(stage):
    print(stage, flush=True)

def stop(signum, frame):
    raise InterruptedError('SSH command cancelled')

for sig in (signal.SIGHUP, signal.SIGTERM, signal.SIGINT):
    signal.signal(sig, stop)

def atomic_file(home, name, content, mode=0o600):
    fd, temporary = tempfile.mkstemp(prefix='.cockpit-api-', dir=home)
    try:
        with os.fdopen(fd, 'wb') as output:
            os.fchmod(output.fileno(), mode)
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, os.path.join(home, name))
    finally:
        if os.path.exists(temporary): os.unlink(temporary)

def restore_bundle(home, originals):
    for name, original in originals.items():
        if name not in ('auth.json', 'config.toml', 'cockpit-model-catalog.json'):
            raise ValueError('invalid rollback target')
        if original is None:
            try: os.unlink(os.path.join(home, name))
            except FileNotFoundError: pass
        else:
            atomic_file(home, name, base64.b64decode(original['data'], validate=True), original['mode'])

def process_identity(pid):
    try:
        with open('/proc/'+str(pid)+'/stat') as source:
            fields = source.read().rsplit(')', 1)[1].split()
        # A zombie has already exited; its parent may reap it later.
        return None if fields[0] == 'Z' else fields[19]
    except FileNotFoundError:
        return None

def stop_desktop_server(home, alive):
    # The selected CODEX_HOME socket identifies exactly one service. Never scan
    # command-line text or signal proxies, other homes, or other users.
    socket_path = os.path.join(home, 'app-server-control', 'app-server-control.sock')
    try:
        os.stat(socket_path)
    except FileNotFoundError:
        return
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(5)
        try:
            connection.connect(os.path.realpath(socket_path))
        except (FileNotFoundError, ConnectionRefusedError):
            return  # No listener: Desktop will load the saved files on connection.
        pid, uid, _ = struct.unpack('3i', connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if pid <= 1 or uid != os.getuid():
            raise RuntimeError('Control socket is not owned by the current user app-server')
        identity = process_identity(pid)
        if identity is None:
            return
        try:
            executable = os.path.basename(os.readlink('/proc/'+str(pid)+'/exe'))
            with open('/proc/'+str(pid)+'/cmdline', 'rb') as source:
                args = source.read().split(b'\0')
        except FileNotFoundError:
            return
        if executable != 'codex' or b'app-server' not in args or b'--listen' not in args:
            raise RuntimeError('Control socket owner is not a listening Codex app-server')
        if not alive.is_set():
            raise RuntimeError('Remote restart cancelled')
        if process_identity(pid) != identity:
            return
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            return
    deadline = time.monotonic() + 15
    while process_identity(pid) == identity:
        if not alive.is_set():
            raise RuntimeError('Remote restart cancelled')
        if time.monotonic() >= deadline:
            raise RuntimeError('App-server did not exit within 15 seconds after SIGTERM; no force kill performed')
        time.sleep(0.1)

def main():
    request = json.loads(sys.stdin.buffer.readline())
    home = os.path.expanduser(request['home'])
    if not os.path.isdir(home):
        raise RuntimeError('existing CODEX_HOME directory required')
    payload = base64.b64decode(request['auth'], validate=True)
    expected = request['sha256']
    if hashlib.sha256(payload).hexdigest() != expected:
        raise RuntimeError('input SHA256 mismatch')
    alive = threading.Event()
    alive.set()
    def lease():
        while alive.is_set():
            # The sender sends one heartbeat every two seconds. A disconnected
            # or partitioned SSH session cannot leave a deferred reset running.
            ready, _, _ = select.select([sys.stdin.buffer], [], [], 10)
            if not ready or not sys.stdin.buffer.readline():
                alive.clear()
                return
    threading.Thread(target=lease, daemon=True).start()
    lock = os.open(home, os.O_RDONLY | os.O_DIRECTORY)
    temp = None
    originals = None
    committed = False
    api = request.get("api_bundle")
    try:
        while alive.is_set():
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                time.sleep(0.1)
        if not alive.is_set():
            raise RuntimeError('superseded or disconnected')
        prepared = None
        if api:
            try:
                catalog_prepared = prepare_api_bundle(home, api)
            except (ValueError, RuntimeError) as error:
                emit('api_error:' + json.dumps(str(error)))
                raise
            prepared = dict(catalog_prepared)
            prepared['auth.json'] = payload
        originals = {}
        for name in prepared or {'auth.json': payload}:
            target = os.path.join(home, name)
            if os.path.islink(target): raise ValueError('symlink bundle target')
            if os.path.exists(target):
                with open(target, 'rb') as source: content = source.read(10 * 1024 * 1024 + 1)
                if len(content) > 10 * 1024 * 1024: raise ValueError('oversize bundle target')
                originals[name] = {'data': base64.b64encode(content).decode(), 'mode': stat.S_IMODE(os.stat(target).st_mode)}
            else:
                originals[name] = None
        emit('transferring')
        if prepared:
            for name, content in catalog_prepared.items():
                if not alive.is_set(): raise RuntimeError('superseded or disconnected')
                atomic_file(home, name, content)
            os.fsync(lock)
            try:
                validate_api_bundle(home, api, catalog_prepared)
            except (ValueError, RuntimeError) as error:
                emit('api_error:' + json.dumps(str(error)))
                raise
        fd, temp = tempfile.mkstemp(prefix='.auth-sync-', dir=home)
        with os.fdopen(fd, 'wb') as output:
            os.fchmod(output.fileno(), 0o600)
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        if not alive.is_set():
            raise RuntimeError('superseded or disconnected')
        target = os.path.join(home, 'auth.json')
        os.replace(temp, target)
        temp = None
        os.fsync(lock)
        with open(target, 'rb') as source:
            actual = hashlib.sha256(source.read()).hexdigest()
        if actual != expected or stat.S_IMODE(os.stat(target).st_mode) != 0o600:
            raise RuntimeError('remote auth verification failed')
        emit('credentials_synced')
        if not alive.is_set():
            raise RuntimeError('superseded or disconnected')
        # Credentials remain committed even if the old service cannot be stopped.
        committed = True
        emit('reloading')
        try:
            stop_desktop_server(home, alive)
        except (OSError, RuntimeError) as error:
            detail = str(error) if isinstance(error, RuntimeError) else 'Cannot stop remote app-server (OS error '+str(error.errno)+')'
            emit('reload_error:' + json.dumps(detail))
            raise
        if not alive.is_set():
            raise RuntimeError('superseded or disconnected')
        # Retire sidecars from earlier releases; new syncs keep state in memory only.
        for obsolete in ('cockpit-model-definition.json', '.cockpit-api-previous.json',
                         '.cockpit-api-rollback.json', '.cockpit-auth-sync-generation'):
            try: os.unlink(os.path.join(home, obsolete))
            except FileNotFoundError: pass
        os.fsync(lock)
        committed = True
        # Files are verified and no old service remains. Desktop owns reconnection;
        # this is not a claim that a new client has connected or authenticated.
        emit('applied')
    finally:
        alive.clear()
        if originals is not None and not committed:
            restore_bundle(home, originals)
            os.fsync(lock)
            emit('rolled_back')
        if temp is not None:
            os.unlink(temp)
        os.close(lock)

try:
    main()
except Exception as error:
    # No exception details: OS errors must never echo auth payloads.
    print('remote auth sync or reload failed: ' + type(error).__name__, file=sys.stderr)
    sys.exit(1)
"#;

/// Only identity metadata and one-way credential digests leave the remote host.
const REMOTE_INSPECT_SCRIPT: &str = r#"
import base64, hashlib, json, os, sys

def response(identity):
    metadata = {}
    try:
        import tomllib
        from urllib.parse import urlsplit, urlunsplit
        with open(os.path.join(home, 'config.toml'), 'rb') as source:
            raw = source.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024: raise ValueError('oversize config')
        config = tomllib.loads(raw.decode())
        provider = config.get('model_provider', 'openai')
        metadata['model_provider'] = provider if isinstance(provider, str) else None
        metadata['model'] = config.get('model') if isinstance(config.get('model'), str) else None
        settings = config.get('model_providers', {}).get(provider, {})
        name = settings.get('name')
        metadata['model_provider_name'] = name.strip() if isinstance(name, str) and name.strip() else None
        url = settings.get('base_url') or config.get('openai_base_url')
        if isinstance(url, str):
            parsed = urlsplit(url)
            metadata['base_url'] = urlunsplit((parsed.scheme, parsed.netloc.split('@')[-1], parsed.path, '', ''))
        catalog = config.get('model_catalog_json')
        if isinstance(catalog, str):
            metadata['model_catalog_path'] = catalog
            path = os.path.expanduser(catalog)
            if not os.path.isabs(path): path = os.path.join(home, path)
            metadata['model_catalog_exists'] = os.path.isfile(path)
            if metadata['model_catalog_exists']:
                with open(path, 'rb') as source: raw = source.read(10 * 1024 * 1024 + 1)
                if len(raw) <= 10 * 1024 * 1024:
                    models = json.loads(raw).get('models')
                    if isinstance(models, list): metadata['catalog_model_count'] = len(models)
    except Exception:
        pass
    return dict(identity, **metadata)

def digest(value):
    return hashlib.sha256(value.encode('utf-8')).hexdigest() if isinstance(value, str) and value else None

def jwt_claim(token, key):
    try:
        encoded = token.split('.')[1]
        payload = json.loads(base64.urlsafe_b64decode(encoded + '=' * (-len(encoded) % 4)))
        value = payload.get(key)
        return value if isinstance(value, str) and value else None
    except (AttributeError, IndexError, ValueError, TypeError):
        return None

try:
    request = json.loads(sys.stdin.buffer.readline())
    home = os.path.expanduser(request['codex_home'])
    with open(os.path.join(home, 'auth.json'), 'rb') as source:
        data = source.read(10 * 1024 * 1024 + 1)
    if len(data) > 10 * 1024 * 1024:
        raise ValueError('oversize')
    auth = json.loads(data)
    if not isinstance(auth, dict):
        raise ValueError('invalid auth')
    tokens = auth.get('tokens') if isinstance(auth.get('tokens'), dict) else {}
    identity = auth.get('agent_identity') if isinstance(auth.get('agent_identity'), dict) else {}
    api_key = auth.get('OPENAI_API_KEY')
    personal = auth.get('personal_access_token')
    access = tokens.get('access_token')
    if isinstance(api_key, str) and api_key:
        mode, secret = 'api_key', api_key
    elif isinstance(identity.get('agent_private_key'), str) and identity['agent_private_key']:
        mode, secret = 'agent_identity', identity['agent_private_key']
    elif isinstance(personal, str) and personal:
        mode, secret = 'personal_access_token', personal
    elif isinstance(access, str) and access:
        mode, secret = 'oauth', access
    else:
        mode, secret = 'unknown', None
    account_id = identity.get('account_id') or tokens.get('account_id') or jwt_claim(tokens.get('id_token'), 'chatgpt_account_id')
    email = identity.get('email') or jwt_claim(tokens.get('id_token'), 'email') or jwt_claim(access, 'email')
    if not isinstance(account_id, str): account_id = None
    if not isinstance(email, str): email = None
    print(json.dumps(response({'auth_mode': mode, 'account_id': account_id, 'email': email, 'fingerprint': digest(secret)})))
except FileNotFoundError:
    print(json.dumps(response({'auth_mode': 'none', 'account_id': None, 'email': None, 'fingerprint': None})))
except Exception:
    print(json.dumps(response({'auth_mode': 'unknown', 'account_id': None, 'email': None, 'fingerprint': None})))
"#;

const REMOTE_AUTH_COMPATIBILITY_SCRIPT: &str = r#"
import json, os, re, sys

try:
    request = json.loads(sys.stdin.buffer.readline())
    home = os.path.expanduser(request['codex_home'])
    with open(os.path.join(home, 'config.toml'), 'rb') as source:
        data = source.read(1024 * 1024 + 1)
    if len(data) > 1024 * 1024:
        raise ValueError('oversize')
    text = data.decode('utf-8')
    try:
        import tomllib
    except ImportError:
        # Python 3.10 lacks tomllib. Fail closed for every provider/auth
        # declaration; a plain model/settings config remains compatible.
        risky = r'^\s*(model_provider|openai_base_url|profile|cli_auth_credentials_store|forced_login_method|forced_chatgpt_workspace_id)\s*='
        custom_openai = r'^\s*\[\s*model_providers\.openai(?:\.|\s*\])'
        compatible = not re.search(risky, text, re.M) and not re.search(custom_openai, text, re.M)
    else:
        config = tomllib.loads(text)
        compatible = (
            config.get('model_provider') in (None, 'openai')
            and not config.get('openai_base_url')
            and not config.get('profile')
            and not config.get('forced_chatgpt_workspace_id')
            and not (isinstance(config.get('model_providers'), dict) and 'openai' in config['model_providers'])
            and config.get('cli_auth_credentials_store') in (None, 'file')
            and config.get('forced_login_method') in (None, 'chatgpt' if request['target_mode'] == 'oauth' else 'api')
        )
    print(json.dumps({'compatible': compatible}))
except FileNotFoundError:
    print(json.dumps({'compatible': True}))
except Exception:
    print(json.dumps({'compatible': False}))
"#;

async fn ensure_remote_auth_compatible(
    server: &SshServer,
    snapshot: &AuthSnapshot,
) -> Result<(), String> {
    if snapshot.api_bundle.is_some() { return Ok(()); }
    let auth: serde_json::Value = serde_json::from_slice(&snapshot.bytes).map_err(|_| "Invalid auth snapshot")?;
    let target_mode = if auth["OPENAI_API_KEY"].as_str().is_some_and(|key| !key.is_empty()) { "apikey" } else { "oauth" };
    let output = run_remote_python_on_server(
        server,
        REMOTE_AUTH_COMPATIBILITY_SCRIPT,
        &serde_json::json!({"target_mode": target_mode}),
        REMOTE_PYTHON_TIMEOUT_SECS,
    )
    .await?;
    if serde_json::from_str::<serde_json::Value>(&output)
        .ok()
        .and_then(|value| value.get("compatible").and_then(|item| item.as_bool()))
        != Some(true)
    {
        return Err("Remote config.toml uses provider, profile, or credential settings incompatible with auth-only switching".into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct RemoteInspectResponse {
    auth_mode: String,
    account_id: Option<String>,
    email: Option<String>,
    fingerprint: Option<String>,
    #[serde(default)]
    model_provider: Option<String>,
    #[serde(default)]
    model_provider_name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_catalog_path: Option<String>,
    #[serde(default)]
    model_catalog_exists: bool,
    #[serde(default)]
    catalog_model_count: Option<usize>,
}

fn account_fingerprint(account: &CodexAccount) -> Option<String> {
    let secret = if account.is_api_key_auth() {
        account.openai_api_key.as_deref()
    } else if let Some(identity) = &account.agent_identity {
        Some(identity.agent_private_key.as_str())
    } else {
        Some(account.tokens.access_token.as_str())
    }?;
    if secret.is_empty() {
        None
    } else {
        Some(format!("{:x}", Sha256::digest(secret.as_bytes())))
    }
}

fn remote_matches_account(remote: &RemoteInspectResponse, account: &CodexAccount) -> bool {
    let mode_matches = match remote.auth_mode.as_str() {
        "api_key" => account.is_api_key_auth(),
        "agent_identity" => account.agent_identity.is_some(),
        "oauth" | "personal_access_token" => {
            !account.is_api_key_auth() && account.agent_identity.is_none()
        }
        _ => false,
    };
    let provider_matches = remote.auth_mode != "api_key" || match remote.base_url.as_deref() {
        Some(url) => account.api_base_url.as_deref().is_some_and(|base| base.trim().trim_end_matches('/') == url.trim().trim_end_matches('/')),
        None => account.api_provider_mode == CodexApiProviderMode::OpenaiBuiltin && account.api_base_url.as_deref().unwrap_or("").trim().is_empty(),
    };
    mode_matches && provider_matches
        && remote
            .fingerprint
            .as_ref()
            .is_some_and(|fingerprint| account_fingerprint(account).as_ref() == Some(fingerprint))
}

fn resolved_ssh_address(output: &str) -> Option<String> {
    let fields: HashMap<&str, &str> = output.lines().filter_map(|line| line.split_once(' '))
        .map(|(key, value)| (key, value.trim())).collect();
    let host = *fields.get("hostname")?;
    let user = *fields.get("user")?;
    let port = fields.get("port")?.parse::<u16>().ok()?;
    if host.is_empty() || user.is_empty() || port == 0 { return None; }
    let host = if host.contains(':') { format!("[{}]", host) } else { host.to_string() };
    Some(format!("{}@{}:{}", user, host, port))
}

async fn inspect_ssh_address(server_id: &str) -> Option<String> {
    let server = load_store().ok()?.servers.into_iter().find(|server| server.id == server_id)?;
    let mut command = Command::new("ssh");
    command.arg("-G").args(build_ssh_args(&server, 5))
        .stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    let output = timeout(Duration::from_secs(5), command.output()).await.ok()?.ok()?;
    if !output.status.success() { return None; }
    resolved_ssh_address(&String::from_utf8_lossy(&output.stdout))
}

pub async fn inspect_account(server_id: &str) -> Result<SshRemoteAccountSummary, String> {
    let output =
        run_remote_python(server_id, REMOTE_INSPECT_SCRIPT, &serde_json::json!({})).await?;
    let remote: RemoteInspectResponse = serde_json::from_str(output.trim())
        .map_err(|_| "Invalid remote account response".to_string())?;
    let accounts = codex_account::list_accounts();
    let matched_account_id = accounts
        .iter()
        .find(|account| remote_matches_account(&remote, account))
        .map(|account| account.id.clone())
        .or_else(|| {
            // A remote OAuth token may have rotated since the library copy.
            // An official account ID is useful only when it identifies one
            // unambiguous local entry; email alone is not identity proof.
            let official_id = remote.account_id.as_deref()?;
            if !matches!(
                remote.auth_mode.as_str(),
                "oauth" | "personal_access_token" | "agent_identity"
            ) {
                return None;
            }
            let mut matches = accounts.iter().filter(|account| {
                account.account_id.as_deref() == Some(official_id)
                    || account
                        .agent_identity
                        .as_ref()
                        .is_some_and(|identity| identity.account_id == official_id)
            });
            let only = matches.next()?;
            matches.next().is_none().then(|| only.id.clone())
        });
    let connection_address = inspect_ssh_address(server_id).await;
    Ok(SshRemoteAccountSummary {
        connection_address,
        server_id: server_id.to_string(),
        auth_mode: remote.auth_mode,
        account_id: remote.account_id,
        email: remote.email,
        matched_account_id,
        model_provider: remote.model_provider,
        model_provider_name: remote.model_provider_name,
        base_url: remote.base_url,
        model: remote.model,
        model_catalog_path: remote.model_catalog_path,
        model_catalog_exists: remote.model_catalog_exists,
        catalog_model_count: remote.catalog_model_count,
    })
}

/// Read the shared supplier definition at apply time; do not duplicate large templates
/// or model instructions in every stored account.
fn api_bundle_for_account(account: &CodexAccount) -> Result<Option<serde_json::Value>, String> {
    if !account.is_api_key_auth() || (account.api_provider_mode == CodexApiProviderMode::OpenaiBuiltin
        && account.api_base_url.as_deref().unwrap_or("").trim().is_empty()) {
        return Ok(None);
    }
    let path = account::get_data_dir()?.join("codex_model_providers.json");
    let providers: Vec<serde_json::Value> = serde_json::from_slice(
        &std::fs::read(path).map_err(|_| "Cannot read model provider repository")?
    ).map_err(|_| "Invalid model provider repository")?;
    api_bundle_from_providers(account, &providers).map(Some)
}

fn api_bundle_from_providers(account: &CodexAccount, providers: &[serde_json::Value]) -> Result<serde_json::Value, String> {
    let base_url = account.api_base_url.as_deref().unwrap_or("").trim().trim_end_matches('/');
    let matching: Vec<_> = providers.iter().filter(|provider| {
        provider["baseUrl"].as_str().is_some_and(|url| url.trim().trim_end_matches('/') == base_url)
    }).collect();
    if matching.len() != 1 { return Err("Select one linked model provider before remote API apply".into()); }
    let provider = matching[0];
    let definition = provider.get("modelCatalogDefinition")
        .filter(|definition| definition["models"].as_array().is_some_and(|m| !m.is_empty()))
        .ok_or("Configure and save the model parameters in the linked provider before remote API apply")?;
    let url = provider["baseUrl"].as_str().filter(|s| !s.trim().is_empty())
        .ok_or("Model provider has no base URL")?;
    let wire = provider["wireApi"].as_str().unwrap_or("responses");
    if wire != "responses" { return Err("Remote direct API apply requires a Responses provider".into()); }
    Ok(serde_json::json!({
        "base_url": url, "wire_api": wire,
        "provider_name": provider["name"].as_str().map(str::trim).filter(|name| !name.is_empty()).unwrap_or("Custom API"),
        "supports_websockets": provider["supportsWebsockets"].as_bool().unwrap_or(false),
        "model_catalog_definition": { "models": definition["models"] },
    }))
}

#[derive(Clone)]
struct AuthSnapshot {
    account_id: String,
    account_email: String,
    token_generation: u64,
    bytes: Vec<u8>,
    hash: String,
    api_bundle: Option<serde_json::Value>,
}

impl AuthSnapshot {
    fn read(account: &CodexAccount) -> Result<Self, String> {
        let bytes = std::fs::read(codex_account::get_codex_home().join("auth.json"))
            .map_err(|_| "Cannot read current local Codex auth.json".to_string())?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Current local Codex auth.json is invalid JSON".to_string())?;
        if !value.is_object() {
            return Err("Current local Codex auth.json must be an object".to_string());
        }
        let matches = if account.is_api_key_auth() {
            account
                .openai_api_key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .is_some_and(|key| {
                    value.get("OPENAI_API_KEY").and_then(|v| v.as_str()) == Some(key)
                })
        } else if let Some(identity) = &account.agent_identity {
            value.get("agent_identity") == serde_json::to_value(identity).ok().as_ref()
        } else {
            let token = value
                .pointer("/tokens/access_token")
                .or_else(|| value.get("personal_access_token"))
                .and_then(|v| v.as_str());
            !account.tokens.access_token.is_empty()
                && token == Some(account.tokens.access_token.as_str())
        };
        if !matches {
            return Err(
                "Current official auth.json does not match the selected account; SSH sync skipped"
                    .to_string(),
            );
        }
        Ok(Self {
            account_id: account.id.clone(),
            account_email: account.email.clone(),
            token_generation: account.token_generation,
            hash: format!("{:x}", Sha256::digest(&bytes)),
            api_bundle: api_bundle_for_account(account)?,
            bytes,
        })
    }

    fn from_stored_account(account: &CodexAccount) -> Result<Self, String> {
        if account.is_web_session_auth() {
            return Err("Web session accounts cannot be used for Codex SSH authentication".into());
        }
        let auth = if account.is_api_key_auth() {
            let key = account
                .openai_api_key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .ok_or("Stored API key account has no credential")?;
            serde_json::json!({"auth_mode": "apikey", "OPENAI_API_KEY": key})
        } else if let Some(identity) = &account.agent_identity {
            if identity.agent_private_key.is_empty() {
                return Err("Stored agent identity has no private key".into());
            }
            serde_json::json!({"auth_mode": "agentIdentity", "agent_identity": identity})
        } else {
            let access = account.tokens.access_token.trim();
            if access.is_empty() {
                return Err("Stored OAuth account has no access token".into());
            }
            let refresh = account.tokens.refresh_token.as_deref().unwrap_or("").trim();
            if account.tokens.id_token.trim().is_empty() && refresh.is_empty() {
                serde_json::json!({"OPENAI_API_KEY": null, "personal_access_token": access})
            } else {
                let last_refresh = account
                    .token_updated_at
                    .and_then(|timestamp| {
                        chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
                    })
                    .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                serde_json::json!({
                    "auth_mode": "chatgpt",
                    "OPENAI_API_KEY": null,
                    "tokens": {
                        "id_token": account.tokens.id_token,
                        "access_token": access,
                        "refresh_token": refresh,
                        "account_id": account.account_id,
                    },
                    "last_refresh": last_refresh,
                })
            }
        };
        let bytes =
            serde_json::to_vec(&auth).map_err(|_| "Cannot encode stored account credentials")?;
        Ok(Self {
            account_id: account.id.clone(),
            account_email: account.email.clone(),
            token_generation: account.token_generation,
            hash: format!("{:x}", Sha256::digest(&bytes)),
            api_bundle: api_bundle_for_account(account)?,
            bytes,
        })
    }
}

struct HostJobs {
    latest: watch::Sender<HostReservation>,
    gate: tokio::sync::Mutex<()>,
}

#[derive(Clone, Copy)]
struct HostReservation {
    generation: u64,
    tied_to_local_account: bool,
}

static JOB_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static JOB_SESSION_PREFIX: LazyLock<String> = LazyLock::new(|| format!("{}:", Uuid::new_v4()));

fn next_generation() -> u64 {
    let now = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_default()
        .max(0) as u64;
    let previous = JOB_SEQUENCE
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |previous| {
            Some(now.max(previous + 1))
        })
        .expect("generation update cannot fail");
    now.max(previous + 1)
}

pub fn cancel_pending_syncs_on_account_switch() {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let hosts = HOST_JOBS.lock().unwrap_or_else(|e| e.into_inner());
    for host in hosts.values() {
        cancel_local_follow_job(host);
    }
}

// Caller holds STORE_LOCK while checking and updating the latest reservation.
fn cancel_local_follow_job(host: &HostJobs) {
    if host.latest.borrow().tied_to_local_account {
        host.latest.send_replace(HostReservation {
            generation: next_generation(),
            tied_to_local_account: false,
        });
    }
}

static HOST_JOBS: LazyLock<Mutex<HashMap<String, Arc<HostJobs>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn host_key(server: &SshServer) -> String {
    format!("{}@{}:{}", server.username, server.host, server.port)
}

// Caller holds STORE_LOCK, matching reservation and publication ordering.
fn cancel_server_jobs(server: &SshServer) {
    if let Some(host) = HOST_JOBS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&host_key(server))
    {
        host.latest.send_replace(HostReservation {
            generation: next_generation(),
            tied_to_local_account: false,
        });
    }
}

fn host_jobs(server: &SshServer) -> Arc<HostJobs> {
    let key = host_key(server);
    HOST_JOBS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key)
        .or_insert_with(|| {
            let (latest, _) = watch::channel(HostReservation {
                generation: 0,
                tied_to_local_account: false,
            });
            Arc::new(HostJobs {
                latest,
                gate: tokio::sync::Mutex::new(()),
            })
        })
        .clone()
}

struct SyncJob {
    server: SshServer,
    snapshot: AuthSnapshot,
    host: Arc<HostJobs>,
    generation: u64,
    status: SshCodexSyncStatus,
}

fn result_from_status(server: &SshServer, status: SshCodexSyncStatus) -> SshCodexSyncResult {
    SshCodexSyncResult {
        server_id: server.id.clone(),
        server_name: server.name.clone(),
        job_id: status.job_id,
        stage: status.stage,
        account_id: status.account_id,
        account_email: status.account_email,
        token_generation: status.token_generation,
        bundle_hash: status.bundle_hash,
        verified: status.verified,
        error: status.error,
        synced_at: status.synced_at,
    }
}

fn emit_result(result: &SshCodexSyncResult) {
    if let Some(app) = crate::get_app_handle() {
        if let Err(error) = app.emit("codex:ssh-sync-result", result) {
            logger::log_warn(&format!("[Codex SSH] Cannot emit sync status: {}", error));
        }
    }
}

impl SyncJob {
    fn reserve(server: SshServer, snapshot: AuthSnapshot, tied_to_local_account: bool) -> Self {
        let host = host_jobs(&server);
        // Reserve and publish under the same lock as in-memory status updates, so a
        // previous job can never replace a newer job's status.
        let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let generation = next_generation();
        host.latest.send_replace(HostReservation {
            generation,
            tied_to_local_account,
        });
        let job = Self {
            status: SshCodexSyncStatus {
                job_id: format!("{}{}", *JOB_SESSION_PREFIX, Uuid::new_v4()),
                stage: SshSyncStage::Pending,
                account_id: snapshot.account_id.clone(),
                account_email: snapshot.account_email.clone(),
                token_generation: snapshot.token_generation,
                bundle_hash: snapshot.api_bundle.as_ref().map(|api| {
                    let mut digest = Sha256::new();
                    digest.update(&snapshot.bytes);
                    digest.update(api.to_string().as_bytes());
                    format!("{:x}", digest.finalize())
                }).unwrap_or_else(|| snapshot.hash.clone()),
                synced_at: now_timestamp(),
                verified: false,
                error: None,
            },
            server,
            snapshot,
            host,
            generation,
        };
        job.cache_and_emit();
        job
    }

    fn is_current(&self) -> bool {
        self.host.latest.borrow().generation == self.generation
    }

    // Caller holds STORE_LOCK.
    fn cache_and_emit(&self) {
        let result = result_from_status(&self.server, self.status.clone());
        let mut statuses = SYNC_STATUSES.lock().unwrap_or_else(|e| e.into_inner());
        let same_job = statuses.get(&self.server.id).is_some_and(|status| status.job_id == self.status.job_id);
        if self.is_current() || (same_job && self.status.stage == SshSyncStage::Superseded) {
            statuses.insert(self.server.id.clone(), self.status.clone());
        }
        drop(statuses);
        emit_result(&result);
    }

    fn publish(&mut self, stage: SshSyncStage, error: Option<String>) {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.status.stage = if self.is_current() {
            stage
        } else {
            SshSyncStage::Superseded
        };
        if stage == SshSyncStage::CredentialsSynced {
            self.status.verified = true;
        }
        self.status.error = error.map(sanitize_error);
        self.status.synced_at = now_timestamp();
        self.cache_and_emit();
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn transfer_and_apply(job: &mut SyncJob) -> Result<(), String> {
    validate_server(&job.server)?;
    let mut command = Command::new("ssh");
    command.args(build_ssh_args(&job.server, CONNECTION_TIMEOUT_SECS));
    command.arg(format!("python3 -c {}", shell_quote(&format!("{}\n{}", include_str!("remote_api_bundle.py"), REMOTE_SYNC_SCRIPT))));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("ssh_binary_missing: {}", e))?;
    let mut stdin = child.stdin.take().ok_or("SSH stdin unavailable")?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("SSH stdout unavailable")?).lines();
    let stderr = child.stderr.take().ok_or("SSH stderr unavailable")?;
    // Drain stderr concurrently to prevent SSH diagnostics blocking the process.
    // Deliberately do not expose raw remote output (which could contain secrets).
    let stderr_task = tauri::async_runtime::spawn(async move {
        let mut stderr = stderr;
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });
    let payload = serde_json::json!({
        "home": job.server.codex_home, "auth": STANDARD.encode(&job.snapshot.bytes),
        "sha256": job.snapshot.hash,
        "api_bundle": job.snapshot.api_bundle,
    })
    .to_string()
        + "\n";
    let mut changed = job.host.latest.subscribe();
    let exchange = async {
        stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|_| "SSH transfer failed".to_string())?;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
        let mut applied = false;
        let mut reload_error = None;
        let mut rolled_back = false;
        let mut api_error: Option<String> = None;
        let transfer_deadline = tokio::time::sleep(Duration::from_secs(if job.snapshot.api_bundle.is_some() { 90 } else { SYNC_TIMEOUT_SECS }));
        tokio::pin!(transfer_deadline);
        loop {
            if !job.is_current() {
                return Err("superseded".to_string());
            }
            tokio::select! {
                biased;
                _ = changed.changed() => return Err("superseded".to_string()),
                _ = &mut transfer_deadline, if !job.status.verified => return Err("SSH credentials transfer timed out".to_string()),
                line = stdout.next_line() => {
                    match line.map_err(|_| "SSH result read failed".to_string())?.as_deref() {
                        Some("superseded") => return Err("superseded".to_string()),
                        Some("transferring") => job.publish(SshSyncStage::Transferring, None),
                        Some("rolled_back") => { job.status.verified = false; rolled_back = true; },
                        Some(line) if line.starts_with("api_error:") => {
                            api_error = serde_json::from_str::<String>(&line[10..]).ok();
                        },
                        Some("credentials_synced") => job.publish(SshSyncStage::CredentialsSynced, None),
                        Some("reloading") if job.status.verified => job.publish(SshSyncStage::Reloading, None),
                        Some(line) if line.starts_with("reload_error:") => {
                            reload_error = serde_json::from_str::<String>(&line[13..]).ok();
                        },
                        Some("applied") if job.status.verified => { applied = true; },
                        Some(_) => {},
                        None => break,
                    }
                },
                _ = heartbeat.tick() => {
                    stdin.write_all(b"alive\n").await.map_err(|_| "SSH heartbeat failed".to_string())?;
                },
            }
        }
        let exit = child
            .wait()
            .await
            .map_err(|_| "SSH wait failed".to_string())?;
        if exit.success() && applied {
            Ok(())
        } else if let Some(error) = reload_error {
            Err(format!("认证及配置已同步，但旧的远端服务未确认退出：{}。", error))
        } else if job.snapshot.api_bundle.is_some() {
            let detail = api_error.unwrap_or_else(|| "Check remote Codex, Python 3.11, catalog definition".into());
            Err(format!("Remote API apply failed: {}. {}", detail, if rolled_back { "Original files restored" } else { "Remote apply not confirmed" }))
        } else if job.status.verified {
            Err("Credentials verified; remote reload was interrupted. Reconnect remote Codex to use the saved configuration.".to_string())
        } else {
            Err("SSH credentials transfer failed. Check SSH access, remote Python 3, and existing CODEX_HOME.".to_string())
        }
    };
    let outcome = timeout(Duration::from_secs(APPLY_TIMEOUT_SECS), exchange)
        .await
        .unwrap_or_else(|_| Err("SSH sync or reload timed out".to_string()));
    // Close the remote lease first. Even if transport is partitioned, the remote
    // wrapper expires its lease and cancels pending work before releasing flock.
    drop(stdin);
    let _ = child.kill().await;
    stderr_task.abort();
    outcome
}

async fn run_job(mut job: SyncJob) -> SshCodexSyncResult {
    let host = job.host.clone();
    let _gate = host.gate.lock().await;
    if !job.is_current() {
        job.publish(SshSyncStage::Superseded, None);
    } else if let Err(error) = ensure_remote_auth_compatible(&job.server, &job.snapshot).await {
        job.publish(SshSyncStage::Failed, Some(error));
    } else {
        match transfer_and_apply(&mut job).await {
            Ok(()) => job.publish(SshSyncStage::Applied, None),
            Err(error) if !job.is_current() || error == "superseded" => {
                job.publish(SshSyncStage::Superseded, None)
            }
            Err(error) => job.publish(SshSyncStage::Failed, Some(error)),
        }
    }
    result_from_status(&job.server, job.status)
}

/// Called after the successful local auth projection while the local switch lock
/// still protects the snapshot. Network and idle waits run only in spawned jobs.
pub fn dispatch_selected_servers_after_codex_switch(account: &CodexAccount) -> Result<(), String> {
    let store = load_store()?;
    let servers = selected_servers(&store, true);
    if servers.is_empty() {
        return Ok(());
    }
    let snapshot = match AuthSnapshot::read(account) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let snapshot = AuthSnapshot {
                account_id: account.id.clone(),
                account_email: account.email.clone(),
                token_generation: account.token_generation,
                bytes: Vec::new(),
                hash: String::new(),
                api_bundle: None,
            };
            for server in servers {
                let mut job = SyncJob::reserve(server, snapshot.clone(), true);
                job.publish(SshSyncStage::Failed, Some(error.clone()));
            }
            return Err(error);
        }
    };
    for server in servers {
        let job = SyncJob::reserve(server, snapshot.clone(), true);
        tauri::async_runtime::spawn(run_job(job));
    }
    Ok(())
}

pub async fn sync_current_account_to_server(
    server_id: String,
) -> Result<SshCodexSyncResult, String> {
    let job = {
        let home = codex_account::get_codex_home();
        let _lease = codex_account::try_acquire_profile_mutation_lease(&home, "ssh-auth-snapshot")?;
        let account = codex_account::get_current_account().ok_or("No current Codex account")?;
        let snapshot = AuthSnapshot::read(&account)?;
        let store = load_store()?;
        let id = server_id;
        let server = store
            .servers
            .into_iter()
            .find(|server| server.id == id)
            .ok_or_else(|| format!("SSH server not found: {}", id))?;
        SyncJob::reserve(server, snapshot, true)
    };
    Ok(run_job(job).await)
}

/// Switch only the selected remote host. The local official auth file and
/// current account selection are neither read nor changed.
pub async fn switch_account(
    server_id: &str,
    account_id: &str,
) -> Result<SshCodexSyncResult, String> {
    let account = codex_account::list_accounts_checked()?
        .into_iter()
        .find(|account| account.id == account_id)
        .ok_or("Stored Codex account not found")?;
    let snapshot = AuthSnapshot::from_stored_account(&account)?;
    let server = load_store()?
        .servers
        .into_iter()
        .find(|server| server.id == server_id)
        .ok_or_else(|| format!("SSH server not found: {}", server_id))?;
    let job = SyncJob::reserve(server, snapshot, false);
    Ok(run_job(job).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::codex::CodexTokens;

    #[test]
    fn resolved_address_uses_ssh_config_fields_and_formats_ipv6() {
        assert_eq!(resolved_ssh_address("hostname host.example\nuser alice\nport 2202\n"), Some("alice@host.example:2202".into()));
        assert_eq!(resolved_ssh_address("hostname 2001:db8::1\nuser alice\nport 22\n"), Some("alice@[2001:db8::1]:22".into()));
        assert!(resolved_ssh_address("hostname example\n").is_none());
    }

    #[test]
    fn blank_display_name_defaults_to_host_without_replacing_custom_name() {
        let mut draft = server("test");
        draft.name = " ".into();
        draft.host = "my-host".into();
        assert_eq!(normalize_server(draft.clone(), None).unwrap().name, "my-host");
        draft.name = "My workstation".into();
        assert_eq!(normalize_server(draft, None).unwrap().name, "My workstation");
    }

    fn server(id: &str) -> SshServer {
        SshServer {
            id: id.into(),
            name: id.into(),
            host: "fixture-alias".into(),
            port: 0,
            username: String::new(),
            codex_home: "~/.codex".into(),
            auth: SshAuthConfig::Agent,
            sync_on_codex_switch: true,
            created_at: 1,
            updated_at: 1,
            last_sync: None,
        }
    }

    #[test]
    fn api_bundle_uses_unique_provider_definition_by_base_url() {
        let mut account = CodexAccount::new("fixture-api".into(), "fixture".into(), CodexTokens {
            id_token: String::new(), access_token: String::new(), refresh_token: None,
        });
        account.api_provider_id = Some("preset-id".into());
        account.api_base_url = Some(" https://provider.example/v1/ ".into());
        let provider = serde_json::json!({
            "id": "provider-id", "name": "Example Provider", "baseUrl": "https://provider.example/v1", "wireApi": "responses",
            "modelCatalogDefinition": {"base_model": "official-template", "models": [{"slug": "custom-model"}]},
        });
        let result = api_bundle_from_providers(&account, &[provider.clone()]).unwrap();
        assert!(result["model_catalog_definition"].get("base_model").is_none());
        assert_eq!(result["model_catalog_definition"]["models"], provider["modelCatalogDefinition"]["models"]);
        assert_eq!(result["provider_name"], "Example Provider");
        assert!(result.get("api_key").is_none());
        let mut duplicate = provider.clone();
        duplicate["id"] = serde_json::json!("another-provider-id");
        duplicate["baseUrl"] = serde_json::json!(" https://provider.example/v1/ ");
        assert!(api_bundle_from_providers(&account, &[provider.clone(), duplicate]).is_err());
        let mut unrelated = provider.clone();
        unrelated["id"] = serde_json::json!("preset-id");
        unrelated["baseUrl"] = serde_json::json!("https://other.example/v1");
        assert_eq!(
            api_bundle_from_providers(&account, &[unrelated.clone(), provider.clone()]).unwrap(),
            result,
        );
        assert!(api_bundle_from_providers(&account, &[unrelated]).is_err());
        let mut missing = provider.clone();
        missing.as_object_mut().unwrap().remove("modelCatalogDefinition");
        assert!(api_bundle_from_providers(&account, &[missing]).is_err());
        let mut incompatible = provider;
        incompatible["wireApi"] = serde_json::json!("chat");
        assert!(api_bundle_from_providers(&account, &[incompatible]).is_err());
    }

    #[test]
    fn stored_oauth_snapshot_uses_library_credentials_without_local_profile() {
        let mut account = CodexAccount::new(
            "library-entry".into(),
            "person@example.test".into(),
            CodexTokens {
                id_token: "id-fixture".into(),
                access_token: "access-fixture".into(),
                refresh_token: Some("refresh-fixture".into()),
            },
        );
        account.account_id = Some("official-id".into());
        let snapshot = AuthSnapshot::from_stored_account(&account).unwrap();
        let auth: serde_json::Value = serde_json::from_slice(&snapshot.bytes).unwrap();
        assert_eq!(auth["auth_mode"], "chatgpt");
        assert_eq!(auth["tokens"]["account_id"], "official-id");
        assert_eq!(auth["tokens"]["access_token"], "access-fixture");
        assert_eq!(snapshot.account_id, "library-entry");
    }

    #[test]
    fn migrates_only_explicit_selection_and_respects_empty_v2() {
        let mut store: SshServerStore = serde_json::from_value(serde_json::json!({
            "version": "1", "selected_server_id": "a", "servers": [server("a"), server("b")]
        }))
        .unwrap();
        migrate_selection(&mut store);
        assert_eq!(store.selected_server_ids, vec!["a"]);
        store.selected_server_ids.clear();
        migrate_selection(&mut store);
        assert!(store.selected_server_ids.is_empty());
        assert!(store.selected_server_id.is_none());
        store.selected_server_ids = vec!["a".into(), "b".into()];
        store.servers[1].sync_on_codex_switch = false;
        assert_eq!(selected_servers(&store, true).len(), 1);
        assert_eq!(selected_servers(&store, false).len(), 2);
    }

    #[test]
    fn selection_is_the_single_opt_in_and_disabled_legacy_target_stays_disabled() {
        let mut store = SshServerStore::default();
        let mut disabled = server("a");
        disabled.sync_on_codex_switch = false;
        store.servers = vec![disabled, server("b")];
        store.version = "1".into();
        store.selected_server_id = Some("a".into());
        migrate_selection(&mut store);
        assert!(store.selected_server_ids.is_empty());
        assert!(selected_servers(&store, true).is_empty());
        apply_selection(&mut store, vec!["a".into(), "b".into()]);
        assert_eq!(selected_servers(&store, true).len(), 2);
        apply_selection(&mut store, vec!["b".into()]);
        assert!(!store.servers[0].sync_on_codex_switch);
        assert_eq!(selected_servers(&store, true)[0].id, "b");
        let mut edited = store.servers[1].clone();
        edited.sync_on_codex_switch = false;
        assert!(
            normalize_server(edited, Some(&store.servers[1]))
                .unwrap()
                .sync_on_codex_switch
        );
        apply_selection(&mut store, vec![]);
        assert!(selected_servers(&store, true).is_empty());
    }

    #[test]
    fn aliases_preserve_ssh_config_and_quote_remote_code() {
        let server = server("a");
        validate_server(&server).unwrap();
        let args = build_ssh_args(&server, 12);
        assert_eq!(args.last().unwrap(), "fixture-alias");
        assert!(!args.contains(&"-p".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(!args.iter().any(|a| a.contains("StrictHostKeyChecking=no")));
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn generations_are_monotonic_and_shared_per_host() {
        let a = server("ordering-a");
        let b = server("ordering-b");
        let first = host_jobs(&a);
        let second = host_jobs(&b);
        assert!(Arc::ptr_eq(&first, &second));
        let old = next_generation();
        let new = next_generation();
        assert!(new > old);
        first.latest.send_replace(HostReservation {
            generation: old,
            tied_to_local_account: true,
        });
        second.latest.send_replace(HostReservation {
            generation: new,
            tied_to_local_account: true,
        });
        assert_eq!(first.latest.borrow().generation, new);
    }

    #[test]
    fn local_account_change_cancels_only_local_follow_jobs() {
        let mut independent_server = server("independent");
        independent_server.host = format!("independent-{}", Uuid::new_v4());
        let mut following_server = server("following");
        following_server.host = format!("following-{}", Uuid::new_v4());
        let independent = host_jobs(&independent_server);
        let following = host_jobs(&following_server);
        let independent_generation = next_generation();
        let following_generation = next_generation();
        independent.latest.send_replace(HostReservation {
            generation: independent_generation,
            tied_to_local_account: false,
        });
        following.latest.send_replace(HostReservation {
            generation: following_generation,
            tied_to_local_account: true,
        });
        cancel_local_follow_job(&independent);
        cancel_local_follow_job(&following);
        assert_eq!(
            independent.latest.borrow().generation,
            independent_generation
        );
        assert!(following.latest.borrow().generation > following_generation);
        assert!(!following.latest.borrow().tied_to_local_account);
    }

    #[cfg(unix)]
    struct Fixture {
        root: PathBuf,
        home: PathBuf,
    }

    #[cfg(unix)]
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("cockpit-ssh-fixture-{}", Uuid::new_v4()));
            let home = root.join("existing profile");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(
                home.join("config.toml"),
                "# fixture must remain byte-identical\n",
            )
            .unwrap();
            Self { root, home }
        }

        fn start(&self, generation: u64, payload: &[u8]) -> std::process::Child {
            use std::io::Write;
            let mut child = std::process::Command::new("python3")
                .args(["-c", REMOTE_SYNC_SCRIPT])
                .env("HOME", &self.root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let request = serde_json::json!({
                "home": self.home, "generation": generation.to_string(),
                "auth": STANDARD.encode(payload), "sha256": format!("{:x}", Sha256::digest(payload)),
            });
            writeln!(child.stdin.as_mut().unwrap(), "{}", request).unwrap();
            child
        }

        fn complete(&self, generation: u64, payload: &[u8]) -> std::process::Output {
            let mut child = self.start(generation, payload);
            // Keep the stdin lease open until the transaction exits.
            let stdin = child.stdin.take().unwrap();
            let output = child.wait_with_output().unwrap();
            drop(stdin);
            output
        }
    }

    #[cfg(unix)]
    fn run_fixture_python(
        script: &str,
        home: &std::path::Path,
        target_mode: &str,
    ) -> serde_json::Value {
        use std::io::Write;
        let mut child = std::process::Command::new("python3")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let request = serde_json::json!({"codex_home": home, "target_mode": target_mode});
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn inspect_reports_digest_without_credential_and_checks_provider_config() {
        let fixture = Fixture::new();
        let auth = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": "fixture-private-access", "account_id": "official-id"}
        });
        std::fs::write(fixture.home.join("auth.json"), auth.to_string()).unwrap();
        let inspected = run_fixture_python(REMOTE_INSPECT_SCRIPT, &fixture.home, "oauth");
        assert_eq!(inspected["auth_mode"], "oauth");
        assert_eq!(inspected["account_id"], "official-id");
        assert_eq!(
            inspected["fingerprint"],
            format!("{:x}", Sha256::digest(b"fixture-private-access"))
        );
        assert!(!inspected.to_string().contains("fixture-private-access"));
        let compatible =
            run_fixture_python(REMOTE_AUTH_COMPATIBILITY_SCRIPT, &fixture.home, "oauth");
        assert_eq!(compatible["compatible"], true);
        std::fs::write(
            fixture.home.join("config.toml"),
            "[model_providers.openai]\nbase_url = 'https://example.test'\n",
        )
        .unwrap();
        let incompatible =
            run_fixture_python(REMOTE_AUTH_COMPATIBILITY_SCRIPT, &fixture.home, "oauth");
        assert_eq!(incompatible["compatible"], false);
        std::fs::write(fixture.home.join("config.toml"), "forced_login_method = 'api'\n").unwrap();
        let api = run_fixture_python(REMOTE_AUTH_COMPATIBILITY_SCRIPT, &fixture.home, "apikey");
        assert_eq!(api["compatible"], true);
        let oauth = run_fixture_python(REMOTE_AUTH_COMPATIBILITY_SCRIPT, &fixture.home, "oauth");
        assert_eq!(oauth["compatible"], false);
    }

    #[cfg(unix)]
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn remote_auth_only_atomic_secure_write_leaves_no_sidecars() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let output = fixture.complete(20, br#"{"fixture":"new"}"#);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "transferring\ncredentials_synced\nreloading\napplied\n"
        );
        assert_eq!(
            std::fs::read(fixture.home.join("auth.json")).unwrap(),
            br#"{"fixture":"new"}"#
        );
        assert_eq!(
            std::fs::metadata(fixture.home.join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_to_string(fixture.home.join("config.toml")).unwrap(),
            "# fixture must remain byte-identical\n"
        );
        let files: Vec<_> = std::fs::read_dir(&fixture.home)
            .unwrap()
            .map(|p| p.unwrap().file_name())
            .collect();
        assert_eq!(files.len(), 2); // only the existing config and auth
    }

    #[cfg(unix)]
    #[test]
    fn no_listener_syncs_files_but_missing_home_is_not_created() {
        let fixture = Fixture::new();
        let failed = fixture.complete(1, br#"{"fixture":true}"#);
        assert!(failed.status.success());
        let stages = String::from_utf8_lossy(&failed.stdout);
        assert!(stages.contains("credentials_synced"));
        assert!(stages.contains("applied"));
        std::fs::remove_dir_all(&fixture.home).unwrap();
        let missing = fixture.complete(2, br#"{"fixture":true}"#);
        assert!(!missing.status.success());
        assert!(!fixture.home.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_remote_waiter_cannot_apply_after_new_credentials() {
        use std::io::{BufRead, BufReader as StdBufReader};
        let fixture = Fixture::new();
        let mut holder = std::process::Command::new("python3")
            .args(["-u", "-c", "import fcntl,os,sys; fd=os.open(sys.argv[1],os.O_RDONLY); fcntl.flock(fd,fcntl.LOCK_EX); print('locked',flush=True); sys.stdin.read()"])
            .arg(&fixture.home)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut line = String::new();
        StdBufReader::new(holder.stdout.take().unwrap()).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "locked");
        let mut old = fixture.start(1, br#"{"fixture":"old"}"#);
        drop(old.stdin.take());
        let output = old.wait_with_output().unwrap();
        drop(holder.stdin.take());
        holder.wait().unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("credentials_synced"));
        let new = fixture.complete(2, br#"{"fixture":"new"}"#);
        assert!(new.status.success());
        assert_eq!(std::fs::read(fixture.home.join("auth.json")).unwrap(), br#"{"fixture":"new"}"#);
    }
}
