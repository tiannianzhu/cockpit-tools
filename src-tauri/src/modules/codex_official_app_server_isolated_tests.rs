// Explicit, offline integration only: the caller must provide a disposable
// HOME/CODEX_HOME and launch this test under an OS-level network deny policy.
#[test]
#[ignore = "requires isolated HOME, native official Codex and OS-denied networking"]
fn isolated_native_official_metadata_rebuild_import_delete_and_failure_recovery() {
    assert_eq!(
        std::env::var("COCKPIT_CODEX_OFFLINE_INTEGRATION").as_deref(),
        Ok("1")
    );
    let root = PathBuf::from(
        std::env::var_os("COCKPIT_CODEX_OFFLINE_ROOT").expect("explicit disposable root"),
    );
    let configured_home = PathBuf::from(std::env::var_os("HOME").expect("isolated HOME"));
    let configured_codex =
        PathBuf::from(std::env::var_os("CODEX_HOME").expect("isolated CODEX_HOME"));
    assert!(root.is_absolute());
    assert!(configured_home.starts_with(&root));
    assert!(configured_codex.starts_with(&root));
    let blocked = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:9".parse().unwrap(),
        Duration::from_millis(100),
    )
    .unwrap_err();
    assert!(
        matches!(blocked.raw_os_error(), Some(1 | 13)),
        "OS network deny must be active: {blocked}"
    );
    let native = PathBuf::from(
        std::env::var_os(CODEX_APP_SERVER_EXECUTABLE_ENV).expect("explicit native engine"),
    );
    assert_eq!(official_app_server_executable().unwrap(), native);
    let mut bytes = [0u8; 4];
    std::io::Read::read_exact(&mut std::fs::File::open(&native).unwrap(), &mut bytes).unwrap();
    assert_eq!(
        &bytes[..4],
        &[0xcf, 0xfa, 0xed, 0xfe],
        "never invoke a shell/Node wrapper"
    );
    let codex = root.join(format!("fixture-{}", uuid::Uuid::new_v4()));
    let project = codex.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(codex.join("config.toml"), "cli_auth_credentials_store = \"file\"\n[analytics]\nenabled = false\n[feedback]\nenabled = false\n").unwrap();
    let session_id = uuid::Uuid::new_v4().to_string();
    let rollout = codex.join(format!(
        "sessions/2026/10/11/rollout-2026-10-11T03-00-00-{session_id}.jsonl"
    ));
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let records = [
        json!({"timestamp":"2026-10-11T03:00:00Z","type":"session_meta","payload":{"id":session_id,"timestamp":"2026-10-11T03:00:00Z","cwd":project,"originator":"codex_cli_rs","cli_version":"0.162.0-alpha.17.2","source":"cli","model_provider":"openai"}}),
        json!({"timestamp":"2026-10-11T03:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"QA synthetic offline thread","images":[],"local_images":[],"text_elements":[]}}),
        json!({"timestamp":"2026-10-11T03:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"QA synthetic offline thread"}]}}),
    ];
    let original = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>()
        .into_bytes();
    std::fs::write(&rollout, &original).unwrap();
    std::fs::write(codex.join("session_index.jsonl"), format!("{}\n", json!({"id":session_id,"thread_name":"QA synthetic offline thread","updated_at":"2026-10-11T03:00:01Z"}))).unwrap();
    let started = Instant::now();
    rebuild_thread_metadata(&codex).expect("production rebuild using real official app-server");
    assert!(started.elapsed() < Duration::from_secs(25));
    assert_eq!(std::fs::read(&rollout).unwrap(), original);
    let databases = std::fs::read_dir(&codex)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("state_")
                && path
                    .extension()
                    .is_some_and(|extension| extension == "sqlite")
        })
        .collect::<Vec<_>>();
    assert!(
        !databases.is_empty(),
        "official engine must create a local state DB"
    );
    let count = || {
        databases
            .iter()
            .map(|path| {
                let db = rusqlite::Connection::open(path).unwrap();
                db.query_row(
                    "SELECT COUNT(*) FROM threads WHERE id=?1",
                    [&session_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
            })
            .sum::<i64>()
    };
    assert!(
        count() > 0,
        "thread must be indexed by the actual official engine"
    );
    let mapped = rebuild_imported_thread_metadata(
        &codex,
        &[(session_id.clone(), project.to_string_lossy().into_owned())],
    );
    if let Err(error) = mapped {
        assert!(
            error.contains("未找到目标目录对应的 Codex 项目"),
            "unexpected mapped rebuild error: {error}"
        );
        println!("Expected missing-project warning: {error}");
    }
    assert_eq!(std::fs::read(&rollout).unwrap(), original);
    assert!(count() > 0);
    let project_id = isolated_native_create_project(&codex, &project);
    rebuild_imported_thread_metadata(
        &codex,
        &[(session_id.clone(), project.to_string_lossy().into_owned())],
    )
    .expect("production mapped rebuild must assign the actual locally registered project");
    let db = rusqlite::Connection::open(&databases[0]).unwrap();
    let saved_project: Option<String> = db
        .query_row(
            "SELECT project_id FROM threads WHERE id=?1",
            [&session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(saved_project.as_deref(), Some(project_id.as_str()));
    drop(db);
    assert_eq!(std::fs::read(&rollout).unwrap(), original);
    println!(
        "Actual project/create -> mapped production rebuild -> persisted project_id: {project_id}"
    );
    let invalid = delete_threads(&codex, &["not-a-valid-thread-id".into()]).unwrap();
    assert!(invalid.deleted.is_empty());
    assert_eq!(invalid.failures.len(), 1);
    assert_eq!(std::fs::read(&rollout).unwrap(), original);
    rebuild_thread_metadata(&codex).expect("retry after failed delete");
    let deleted = delete_threads(&codex, &[session_id.clone(), session_id.clone()]).unwrap();
    assert_eq!(
        deleted.deleted,
        vec![session_id.clone()],
        "deduplicated real thread/delete"
    );
    assert!(deleted.failures.is_empty());
    assert!(!rollout.exists());
    assert_eq!(count(), 0);
    assert!(!codex.join("auth.json").exists());
    println!(
        "Official engine: {}; fixture: {}; DBs: {:?}; elapsed_ms: {}",
        native.display(),
        codex.display(),
        databases,
        started.elapsed().as_millis()
    );
    std::fs::remove_dir_all(codex).unwrap();
}

fn isolated_native_create_project(codex: &Path, project: &Path) -> String {
    let mut server = AppServerSession::start(codex).unwrap();
    // Parameters are taken from the signed engine's generated local JSON
    // schema. This registers only the fixture folder, without a desktop UI.
    let result = server.request(
        "project/create",
        json!({"idempotencyKey":uuid::Uuid::new_v4().to_string(),"name":"QA isolated project","roots":[{"path":project}],"metadata":{"qa":"offline"}}),
    ).expect("actual official local project/create");
    result["project"]["id"]
        .as_str()
        .expect("project/create missing id")
        .to_owned()
}

#[test]
fn official_fork_dependency_refusal_preserves_parent_child_and_index() {
    let base = tempfile::tempdir().unwrap();
    let parent = base.path().join("parent.jsonl");
    let child = base.path().join("child.jsonl");
    let index = base.path().join("session_index.jsonl");
    let parent_bytes = b"parent history\n";
    let child_bytes = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"history_base\":{\"thread_id\":\"parent\"}}}\n";
    let index_bytes = b"{\"id\":\"parent\"}\n{\"id\":\"child\"}\n";
    std::fs::write(&parent, parent_bytes).unwrap();
    std::fs::write(&child, child_bytes).unwrap();
    std::fs::write(&index, index_bytes).unwrap();
    let result = delete_threads_with(&["parent".into()], |_| {
        Err("cannot delete thread parent: forked history still references it".into())
    });
    assert!(result.deleted.is_empty());
    assert_eq!(result.failures.len(), 1);
    assert!(result.failures[0].contains("forked history still references it"));
    assert_eq!(std::fs::read(parent).unwrap(), parent_bytes);
    assert_eq!(std::fs::read(child).unwrap(), child_bytes);
    assert_eq!(std::fs::read(index).unwrap(), index_bytes);
}
