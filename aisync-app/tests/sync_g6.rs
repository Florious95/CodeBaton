//! App-layer integration tests for backend sync boundaries.

use std::collections::HashMap;
use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use aisync_app_lib::backend::Backend;
use aisync_core::DeviceId;
use aisync_core::{Direction, OsType};
use aisync_sync::{
    ClaudeConfig, DeviceConfig, PeerConfig, ProjectConfig, SyncConfig, SyncModeConfig,
};
use aisync_transport::{generate_tls_identity, ReceiveService, TlsConfig};
use uuid::Uuid;

fn write(path: &Path, name: &str, contents: &str) {
    fs::create_dir_all(path).unwrap();
    fs::write(path.join(name), contents).unwrap();
}

fn claude_project_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// Build a config with one project mapping local→remote, a paired peer, and
/// explicit session dirs so nothing depends on the host environment.
fn make_config(root: &Path, local_code: &Path, remote_code: &Path) -> SyncConfig {
    let peer_id = DeviceId(Uuid::new_v4());
    let mut peers = HashMap::new();
    peers.insert(
        "peer".to_string(),
        PeerConfig {
            id: peer_id,
            name: "peer".to_string(),
            endpoint: None,
            server_cert: None,
            server_name: None,
            last_seen: None,
        },
    );

    let mut project_peers = HashMap::new();
    project_peers.insert("peer".to_string(), remote_code.to_path_buf());

    let mut claude_peers = HashMap::new();
    claude_peers.insert("peer".to_string(), root.join("remote-claude"));

    SyncConfig {
        device: DeviceConfig {
            id: DeviceId(Uuid::new_v4()),
            name: "local".to_string(),
        },
        onboarded: false,
        peers,
        claude_config: ClaudeConfig {
            local: root.join("local-claude"),
            peers: claude_peers,
        },
        projects: vec![ProjectConfig {
            name: "proj".to_string(),
            local: local_code.to_path_buf(),
            peers: project_peers,
            sync_mode: SyncModeConfig::OneWayPush,
            enabled: true,
            exclude_rules: Vec::new(),
        }],
        workspaces: Vec::new(),
        exclude_rules: aisync_sync::default_exclude_rules(),
        default_sync_mode: SyncModeConfig::OneWayPush,
        refresh_interval_secs: 30,
        receive_port: 52000,
        default_file_receive_dir: None,
        state_path: Some(root.join("state.toml")),
    }
}

#[test]
fn run_sync_requires_remote_transport_and_does_not_write_local_remote_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let local_code = root.join("local");
    let remote_code = root.join("remote");

    // A normal file + a sensitive file (matches `.env*`).
    write(&local_code, "main.rs", "fn main() {}\n");
    write(&local_code, ".env.local", "SECRET=42\n");
    fs::create_dir_all(&remote_code).unwrap();
    fs::create_dir_all(root.join("local-claude")).unwrap();

    let config = make_config(root, &local_code, &remote_code);
    let backend = Backend::with_config(config, root.join("config.toml")).unwrap();

    let error = backend
        .run_sync("proj", "peer", Direction::LocalToRemote, &[])
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("has no endpoint"),
        "unexpected error: {error}"
    );
    assert!(
        !remote_code.join("main.rs").exists() && !remote_code.join(".env.local").exists(),
        "backend must not fake success by writing the local remote path"
    );
}

#[test]
fn run_sync_uses_configured_tcp_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let local_code = root.join("local");
    let remote_code = root.join("remote");
    let cert_path = root.join("receiver.der");
    write(&local_code, "main.rs", "fn main() {}\n");
    fs::create_dir_all(&remote_code).unwrap();
    fs::create_dir_all(root.join("local-claude")).unwrap();

    let (addr_rx, server) = start_receive_once(remote_code.clone());
    let addr = addr_rx.recv().unwrap();
    let mut config = make_config(root, &local_code, &remote_code);
    let peer = config.peers.get_mut("peer").unwrap();
    peer.endpoint = Some(addr);
    peer.server_cert = Some(cert_path.clone());
    peer.server_name = Some("aisync-receiver".to_string());
    fs::write(&cert_path, server.cert_der.clone()).unwrap();

    let backend = Backend::with_config(config, root.join("config.toml")).unwrap();
    let report = backend
        .run_sync("proj", "peer", Direction::LocalToRemote, &[])
        .unwrap();

    assert_eq!(report.code_files_transferred, 1);
    assert_eq!(
        fs::read_to_string(remote_code.join("main.rs")).unwrap(),
        "fn main() {}\n"
    );
    server.handle.join().unwrap().unwrap();
}

#[test]
fn sensitive_file_scan_still_finds_env_local() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let local_code = root.join("local");
    let remote_code = root.join("remote");
    write(&local_code, "main.rs", "fn main() {}\n");
    write(&local_code, ".env.local", "SECRET=42\n");
    fs::create_dir_all(&remote_code).unwrap();
    fs::create_dir_all(root.join("local-claude")).unwrap();

    let config = make_config(root, &local_code, &remote_code);
    let backend = Backend::with_config(config, root.join("config.toml")).unwrap();

    let sensitive = backend.sensitive_files("proj").unwrap();
    assert!(
        sensitive
            .iter()
            .any(|file| file.relative_path == ".env.local"),
        ".env.local must be classified as sensitive"
    );
    assert!(
        !remote_code.join(".env.local").exists(),
        "sensitive scan must not write remote paths"
    );
}

#[test]
fn paired_peer_surfaces_from_config() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let local_code = root.join("local");
    write(&local_code, "main.rs", "fn main() {}\n");

    let config = make_config(root, &local_code, &root.join("remote"));
    let backend = Backend::with_config(config, root.join("config.toml")).unwrap();

    let peers = backend.paired_peers();
    assert!(
        peers
            .iter()
            .any(|(d, _)| d.name == "peer" && matches!(d.os, OsType::Other(_))),
        "configured peer should surface in paired_peers()"
    );
}

/// BUG-UI-002: clicking 配对 must still yield a D4 pairing code even when the
/// peer is not in the live mDNS list (it flickered out, or is a config-declared
/// peer). The backend falls back to the config peer and derives a stable code.
#[test]
fn pairing_code_falls_back_to_config_peer_when_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let local_code = root.join("local");
    write(&local_code, "main.rs", "fn main() {}\n");

    let config = make_config(root, &local_code, &root.join("remote"));
    // The config peer's id — not advertised live by any discoverer here.
    let peer_id = config.peers.get("peer").unwrap().id;
    let backend = Backend::with_config(config, root.join("config.toml")).unwrap();

    let pairing = backend
        .pairing_code(&peer_id)
        .expect("pairing_code should fall back to the config peer, not fail");
    assert_eq!(pairing.peer.name, "peer");
    assert_eq!(pairing.code.len(), 6, "pairing code is six digits");
    assert!(pairing.code.chars().all(|c| c.is_ascii_digit()));
}

/// confirm_pairing must persist the peer to config.peers even when the peer is
/// offline (not in the live mDNS list) — that on-disk entry is what moves a
/// device from 发现 to 已配对. Previously persist was gated on live connection
/// info, so [peers] stayed empty (the field bug).
#[test]
fn confirm_pairing_persists_config_peer_when_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let config_path = root.join("config.toml");

    // Start from a config with NO peers; add one as a config peer (offline) then
    // confirm it.
    let mut config = SyncConfig::new("local");
    config.state_path = Some(root.join("state.toml"));
    let peer_id = DeviceId(Uuid::new_v4());
    config.peers.insert(
        "mac-mini".to_string(),
        PeerConfig {
            id: peer_id,
            name: "mac-mini".to_string(),
            endpoint: None,
            server_cert: None,
            server_name: None,
            last_seen: None,
        },
    );
    let backend = Backend::with_config(config, config_path.clone()).unwrap();

    backend
        .confirm_pairing(&peer_id)
        .expect("confirm should persist even when peer is offline");

    // Reload config from disk and assert the peer survived.
    let reloaded = aisync_sync::load_config(&config_path).unwrap();
    assert!(
        reloaded.peers.values().any(|p| p.id == peer_id),
        "confirmed peer must be persisted to config.peers on disk"
    );
}

/// BUG 248-250: 添加工作区时扫描必须能列出本机根目录下的第一级子目录，
/// 不依赖配置里已存在的 workspace（旧逻辑按名字查 config 永远查不到 → 空）。
#[test]
fn scan_workspace_path_lists_first_level_subdirs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let ws = root.join("测试sync项目");
    // 三个子项目 + 一个 dotfolder（应被跳过）+ 一个文件（应被跳过）。
    write(&ws.join("第一一个"), "a.rs", "fn a() {}\n");
    write(&ws.join("proj-b"), "b.rs", "fn b() {}\n");
    write(&ws.join("proj-c"), "c.rs", "fn c() {}\n");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(ws.join("README.md"), "x").unwrap();

    let config = make_config(root, &root.join("local"), &root.join("remote"));
    let backend = Backend::with_config(config, root.join("config.toml")).unwrap();

    let found = backend
        .scan_workspace_path(&ws, &root.join("remote-root"))
        .expect("scan should succeed on a real dir");
    let names: Vec<String> = found.iter().map(|d| d.name.clone()).collect();
    assert_eq!(
        names,
        vec!["proj-b", "proj-c", "第一一个"],
        "subdirs sorted, dotfolder/file skipped"
    );
}

/// Problem B: the peer (remote) dir defaulting to the local dir is intentional
/// and must NOT be rejected as "same path" — both ends commonly share a path.
#[test]
fn add_project_allows_remote_equal_to_local() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let local = root.join("proj");
    write(&local, "main.rs", "fn main() {}\n");

    let mut config = SyncConfig::new("local");
    config.state_path = Some(root.join("state.toml"));
    let backend = Backend::with_config(config, root.join("config.toml")).unwrap();

    // remote == local — used to error with "maps peer to the same path".
    backend
        .add_project(
            "proj".into(),
            local.clone(),
            "peer".into(),
            local.clone(),
            SyncModeConfig::TwoWayAuto,
            false,
        )
        .expect("remote == local must be allowed (it's the reference default)");
    assert!(backend.config().projects.iter().any(|p| p.name == "proj"));
}

/// Problem C: a failed add_project must NOT leave a phantom project in config,
/// otherwise a retry wrongly fails with "already exists". Here the local dir is
/// missing and create_local_dir=false, so the add fails — and the project must
/// be absent afterwards so a corrected retry can succeed.
#[test]
fn failed_add_project_does_not_leave_phantom() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let missing = root.join("does-not-exist");

    let mut config = SyncConfig::new("local");
    config.state_path = Some(root.join("state.toml"));
    let backend = Backend::with_config(config, root.join("config.toml")).unwrap();

    let err = backend
        .add_project(
            "proj".into(),
            missing.clone(),
            "peer".into(),
            missing.clone(),
            SyncModeConfig::TwoWayAuto,
            false,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("local-dir-missing:"),
        "unexpected error: {err}"
    );
    // No phantom left behind.
    assert!(
        !backend.config().projects.iter().any(|p| p.name == "proj"),
        "failed add must not persist the project"
    );

    // Retry with create_local_dir=true now succeeds (creates the dir).
    backend
        .add_project(
            "proj".into(),
            missing.clone(),
            "peer".into(),
            missing.clone(),
            SyncModeConfig::TwoWayAuto,
            true,
        )
        .expect("retry with create_local_dir should succeed");
    assert!(missing.exists(), "local dir should have been created");
    assert!(backend.config().projects.iter().any(|p| p.name == "proj"));
}

/// Full GUI-to-GUI path: instance B runs the real receive daemon (as the GUI
/// does on startup); instance A registers B as a peer endpoint, adds a project,
/// and pushes. Proves flows 4 (add project) + 5 (real TCP push to a peer GUI).
#[test]
fn gui_instance_pushes_to_peer_serve_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // ── instance B: receiver GUI ──
    let b_root = root.join("B");
    let b_recv = b_root.join("received");
    fs::create_dir_all(&b_recv).unwrap();
    let port = free_port();
    std::env::set_var("AISYNC_RECEIVE_DIR", b_recv.to_string_lossy().to_string());
    let b_cfg = SyncConfig {
        device: DeviceConfig {
            id: DeviceId(Uuid::new_v4()),
            name: "B".into(),
        },
        onboarded: false,
        peers: HashMap::new(),
        claude_config: ClaudeConfig::default(),
        projects: vec![],
        workspaces: vec![],
        exclude_rules: aisync_sync::default_exclude_rules(),
        default_sync_mode: SyncModeConfig::OneWayPush,
        refresh_interval_secs: 30,
        receive_port: port,
        default_file_receive_dir: None,
        state_path: Some(b_root.join("state.toml")),
    };
    fs::create_dir_all(&b_root).unwrap();
    let backend_b = Backend::with_config_serving(b_cfg, b_root.join("config.toml")).unwrap();
    let serve = backend_b.serve_info().expect("B serve daemon must start");
    std::env::remove_var("AISYNC_RECEIVE_DIR");

    // ── instance A: sender GUI ──
    let a_root = root.join("A");
    let a_proj = a_root.join("myproj");
    let a_claude = a_root.join(".claude");
    let b_claude = b_root.join(".claude");
    let b_remote_project = b_recv.join("myproj");
    write(&a_proj, "main.rs", "fn main() {}\n");
    let session_id = "session-a";
    let local_session_dir = a_claude
        .join("projects")
        .join(claude_project_dir_name(&a_proj));
    fs::create_dir_all(&local_session_dir).unwrap();
    let session_line = serde_json::json!({
        "type": "user",
        "cwd": a_proj.to_string_lossy(),
        "message": {
            "content": [{
                "type": "tool_use",
                "input": {
                    "file_path": a_proj.join("main.rs").to_string_lossy()
                }
            }]
        },
        "sessionId": session_id
    })
    .to_string();
    fs::write(
        local_session_dir.join(format!("{session_id}.jsonl")),
        format!("{session_line}\n"),
    )
    .unwrap();
    let mut a_claude_peers = HashMap::new();
    a_claude_peers.insert("B".to_string(), b_claude.clone());
    let a_cfg = SyncConfig {
        device: DeviceConfig {
            id: DeviceId(Uuid::new_v4()),
            name: "A".into(),
        },
        onboarded: false,
        peers: HashMap::new(),
        claude_config: ClaudeConfig {
            local: a_claude.clone(),
            peers: a_claude_peers,
        },
        projects: vec![],
        workspaces: vec![],
        exclude_rules: aisync_sync::default_exclude_rules(),
        default_sync_mode: SyncModeConfig::OneWayPush,
        refresh_interval_secs: 30,
        receive_port: 52000,
        default_file_receive_dir: None,
        state_path: Some(a_root.join("state.toml")),
    };
    fs::create_dir_all(&a_root).unwrap();
    let backend_a = Backend::with_config(a_cfg, a_root.join("config.toml")).unwrap();

    // A registers B's endpoint + pinned cert, then adds a project mapping to B.
    backend_a
        .add_peer_endpoint(
            "B".into(),
            DeviceId(Uuid::new_v4()),
            SocketAddr::from(([127, 0, 0, 1], serve.port)),
            Some(serve.cert_path.clone()),
            Some("aisync-receiver".into()),
        )
        .unwrap();
    backend_a
        .add_project(
            "myproj".into(),
            a_proj.clone(),
            "B".into(),
            b_remote_project.clone(),
            SyncModeConfig::OneWayPush,
            false,
        )
        .unwrap();

    // A pushes to B over real TCP/TLS.
    let report = backend_a
        .run_sync("myproj", "B", Direction::LocalToRemote, &[])
        .expect("push to peer GUI should succeed");
    assert!(report.code_files_transferred >= 1);
    assert!(report.session_files_transferred >= 1);

    // The file must land in B's configured remote mapping directory.
    let received = b_remote_project.join("main.rs");
    for _ in 0..50 {
        if received.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        received.exists(),
        "pushed file must arrive in B's mapped remote dir at {}",
        received.display()
    );
    assert_eq!(fs::read_to_string(&received).unwrap(), "fn main() {}\n");
    assert!(
        !b_recv.join("main.rs").exists(),
        "pushed file must not land in the default receive root"
    );

    let remote_session = b_claude
        .join("projects")
        .join(claude_project_dir_name(&b_remote_project))
        .join(format!("{session_id}.jsonl"));
    for _ in 0..50 {
        if remote_session.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        remote_session.exists(),
        "rewritten Claude session must arrive at {}",
        remote_session.display()
    );
    let session = fs::read_to_string(&remote_session).unwrap();
    assert!(session.contains(&b_remote_project.to_string_lossy().to_string()));
    assert!(!session.contains(&a_proj.to_string_lossy().to_string()));
}

#[test]
fn add_workspace_persists_entity_and_syncs_whole_root_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let b_root = root.join("B");
    let b_id = DeviceId(Uuid::new_v4());
    fs::create_dir_all(&b_root).unwrap();
    let b_cfg = SyncConfig {
        device: DeviceConfig {
            id: b_id,
            name: "B".into(),
        },
        onboarded: false,
        peers: HashMap::new(),
        claude_config: ClaudeConfig::default(),
        projects: vec![],
        workspaces: vec![],
        exclude_rules: aisync_sync::default_exclude_rules(),
        default_sync_mode: SyncModeConfig::OneWayPush,
        refresh_interval_secs: 30,
        receive_port: free_port(),
        default_file_receive_dir: None,
        state_path: Some(b_root.join("state.toml")),
    };
    let b_config_path = b_root.join("config.toml");
    let backend_b = Backend::with_config_serving(b_cfg, b_config_path.clone()).unwrap();
    let serve = backend_b.serve_info().expect("B serve daemon must start");

    let a_root = root.join("A");
    let workspace_root = a_root.join("workspace");
    let remote_root = b_root.join("workspace-remote");
    write(
        &workspace_root.join("app-one").join("src"),
        "main.rs",
        "fn main() {}\n",
    );
    write(&workspace_root.join("app-two"), "README.md", "# app two\n");

    let a_cfg = SyncConfig {
        device: DeviceConfig {
            id: DeviceId(Uuid::new_v4()),
            name: "A".into(),
        },
        onboarded: false,
        peers: HashMap::new(),
        claude_config: ClaudeConfig::default(),
        projects: vec![],
        workspaces: vec![],
        exclude_rules: aisync_sync::default_exclude_rules(),
        default_sync_mode: SyncModeConfig::OneWayPush,
        refresh_interval_secs: 30,
        receive_port: free_port(),
        default_file_receive_dir: None,
        state_path: Some(a_root.join("state.toml")),
    };
    fs::create_dir_all(&a_root).unwrap();
    let config_path = a_root.join("config.toml");
    let backend_a = Backend::with_config_serving(a_cfg, config_path.clone()).unwrap();
    backend_a
        .add_peer_endpoint(
            "B".into(),
            b_id,
            SocketAddr::from(([127, 0, 0, 1], serve.port)),
            Some(serve.cert_path.clone()),
            Some("aisync-receiver".into()),
        )
        .unwrap();

    let request_id = backend_a
        .add_workspace(
            "workspace".into(),
            workspace_root.clone(),
            "B".into(),
            remote_root.clone(),
            SyncModeConfig::OneWayPush,
            true,
        )
        .expect("workspace add should send request");
    let request = (0..50)
        .find_map(|_| {
            let request = backend_b.take_pending_workspace_mapping_request();
            if request.is_none() {
                thread::sleep(std::time::Duration::from_millis(50));
            }
            request
        })
        .expect("B should receive workspace mapping request");
    assert_eq!(request.request_id, request_id);
    assert_eq!(request.workspace_name, "workspace");
    assert_eq!(request.source_root, workspace_root);

    backend_b
        .confirm_workspace_mapping_request(&request_id, remote_root.clone())
        .expect("B should confirm workspace mapping");
    let processed = (0..50)
        .find_map(|_| {
            let count = backend_a.process_workspace_mapping_acks().unwrap();
            if count == 0 {
                thread::sleep(std::time::Duration::from_millis(50));
                None
            } else {
                Some(count)
            }
        })
        .expect("A should receive workspace mapping ack");
    assert_eq!(processed, 1);

    let report = backend_a
        .run_workspace_sync("workspace", Direction::LocalToRemote)
        .expect("workspace sync should push whole root");
    assert_eq!(report.project_id, "workspace");
    assert!(report.code_files_transferred >= 2);

    let persisted = aisync_sync::load_config(&config_path).unwrap();
    assert!(
        persisted.projects.is_empty(),
        "workspace add must not create scattered projects"
    );
    let workspace = persisted
        .workspaces
        .iter()
        .find(|workspace| workspace.name == "workspace")
        .expect("workspace entity persisted");
    assert_eq!(workspace.local_root, workspace_root);
    assert_eq!(workspace.remote_root, remote_root);
    assert_eq!(workspace.peer, "B");
    let child_names: Vec<_> = workspace
        .children
        .iter()
        .map(|child| child.name.as_str())
        .collect();
    assert_eq!(child_names, vec!["app-one", "app-two"]);

    let persisted_b = aisync_sync::load_config(&b_config_path).unwrap();
    let receiver_workspace = persisted_b
        .workspaces
        .iter()
        .find(|workspace| workspace.name == "workspace")
        .expect("receiver workspace entity persisted");
    assert_eq!(receiver_workspace.local_root, remote_root);
    assert_eq!(receiver_workspace.remote_root, workspace_root);
    assert_eq!(receiver_workspace.peer, "A");

    let received_one = workspace
        .remote_root
        .join("app-one")
        .join("src")
        .join("main.rs");
    let received_two = workspace.remote_root.join("app-two").join("README.md");
    for _ in 0..50 {
        if received_one.exists() && received_two.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(fs::read_to_string(received_one).unwrap(), "fn main() {}\n");
    assert_eq!(fs::read_to_string(received_two).unwrap(), "# app two\n");
}

struct ReceiveThread {
    cert_der: Vec<u8>,
    handle: thread::JoinHandle<aisync_core::Result<()>>,
}

fn start_receive_once(target: std::path::PathBuf) -> (mpsc::Receiver<SocketAddr>, ReceiveThread) {
    let identity = generate_tls_identity("aisync-receiver").unwrap();
    let cert_der = identity.cert_der.clone();
    let (addr_tx, addr_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let tls = TlsConfig::new(identity, "aisync-receiver");
            let service =
                ReceiveService::bind(SocketAddr::from(([127, 0, 0, 1], 0)), target, &tls).await?;
            addr_tx.send(service.local_addr()?).unwrap();
            service.receive_once(None).await?;
            Ok(())
        })
    });
    (addr_rx, ReceiveThread { cert_der, handle })
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
