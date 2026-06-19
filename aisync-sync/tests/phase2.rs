use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aisync_core::{
    AisyncError, DeviceId, DeviceInfo, Direction, Discoverer, FileEntry, OsType,
    PeerChangeCallback, ProjectMapping, Result, Session, SessionParser, SyncManifest, Transporter,
};
use aisync_session::{ClaudeCodeParser, ParsedSession, PathRule, RuleBasedRewriter, SessionIndex};
use aisync_sync::{SyncConfig, SyncCoordinator, SyncModeConfig, WorkspaceConfig};

#[test]
fn scenario_01_one_way_push_makes_code_and_session_consistent() {
    let env = TestEnv::new("one-way");
    fs::write(env.a_code.join("main.rs"), "fn main() {}\n").unwrap();
    write_claude_session(&env.a_session, "enc", "s1", &env.a_code, "main.rs");

    let report = env
        .coordinator_with_claude()
        .push_to(&env.peer.id, &env.project())
        .unwrap();

    assert_eq!(report.direction, Direction::LocalToRemote);
    assert_eq!(
        fs::read_to_string(env.b_code.join("main.rs")).unwrap(),
        "fn main() {}\n"
    );
    let sessions = ClaudeCodeParser::parse_sessions(&env.b_session).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].original_project_path,
        env.b_code.to_string_lossy()
    );
}

#[test]
fn scenario_02_incremental_sync_transfers_only_changed_file() {
    let env = TestEnv::new("incremental");
    fs::write(env.a_code.join("a.txt"), "a1").unwrap();
    fs::write(env.a_code.join("b.txt"), "b1").unwrap();
    let logs = TransportLog::default();
    env.coordinator_with_parser(logs.clone(), Box::new(NoSessionParser))
        .push_to(&env.peer.id, &env.project())
        .unwrap();
    logs.clear();

    fs::write(env.a_code.join("b.txt"), "b2").unwrap();
    let report = env
        .coordinator_with_parser(logs.clone(), Box::new(NoSessionParser))
        .push_to(&env.peer.id, &env.project())
        .unwrap();

    assert_eq!(report.code_files_transferred, 1);
    assert_eq!(logs.file_counts(), vec![1, 0]);
}

#[test]
fn scenario_03_split_brain_returns_conflict_warning() {
    let env = TestEnv::new("conflict");
    fs::write(env.a_code.join("app.txt"), "base").unwrap();
    let mut coordinator = env.coordinator_with_no_sessions();
    coordinator.push_to(&env.peer.id, &env.project()).unwrap();

    fs::write(env.a_code.join("app.txt"), "local").unwrap();
    fs::write(env.b_code.join("app.txt"), "remote").unwrap();
    let error = coordinator
        .push_to(&env.peer.id, &env.project())
        .unwrap_err();

    match error {
        AisyncError::ConflictDetected(details) => {
            assert_eq!(details.project_id, "app");
            assert!(details.summary.contains("both changed"));
        }
        other => panic!("expected ConflictDetected, got {other:?}"),
    }
}

#[test]
fn scenario_04_structured_path_fields_are_rewritten() {
    let root = temp_dir("structured");
    let projects = root.join(".claude").join("projects");
    write_claude_session(
        &root.join(".claude"),
        "enc",
        "s",
        &PathBuf::from("/Users/alice/app"),
        "src/lib.rs",
    );
    let mut sessions = ClaudeCodeParser::parse_sessions(&projects).unwrap();
    let rewriter = RuleBasedRewriter::new(vec![PathRule::unix_to_unix(
        "/Users/alice/app",
        "/home/bob/app",
    )])
    .unwrap();
    let report = ClaudeCodeParser::rewrite_structured_paths(
        &mut sessions[0],
        &rewriter,
        aisync_core::RewriteDirection::SourceToTarget,
    );

    assert_eq!(report.applied.len(), 2);
    let refs = ClaudeCodeParser::list_path_references(&sessions[0]);
    assert!(refs
        .iter()
        .any(|reference| reference.value == "/home/bob/app"));
    assert!(refs
        .iter()
        .any(|reference| reference.value == "/home/bob/app/src/lib.rs"));
}

#[test]
fn scenario_05_path_rewrite_round_trip_is_reversible() {
    let rewriter = RuleBasedRewriter::new(vec![PathRule::unix_to_windows(
        "/Users/alice/app",
        "C:\\Users\\bob\\app",
    )])
    .unwrap();
    let before = "open /Users/alice/app/src/lib.rs";
    let rewritten = aisync_core::PathRewriter::rewrite(
        &rewriter,
        before,
        aisync_core::RewriteDirection::SourceToTarget,
    )
    .unwrap();
    let round_trip = aisync_core::PathRewriter::rewrite(
        &rewriter,
        &rewritten,
        aisync_core::RewriteDirection::TargetToSource,
    )
    .unwrap();

    assert_eq!(round_trip, before);
}

#[test]
fn scenario_06_chinese_directory_session_mapping_uses_original_path() {
    let env = TestEnv::new("中文项目");
    fs::write(env.a_code.join("说明.txt"), "内容").unwrap();
    write_claude_session(
        &env.a_session,
        "-Users-alice-code---",
        "s",
        &env.a_code,
        "说明.txt",
    );

    env.coordinator_with_claude()
        .push_to(&env.peer.id, &env.project())
        .unwrap();

    let sessions = ClaudeCodeParser::parse_sessions(&env.b_session).unwrap();
    assert_eq!(sessions[0].encoded_dir_name, "-Users-alice-code---");
    assert_eq!(
        sessions[0].original_project_path,
        env.b_code.to_string_lossy()
    );
}

#[test]
fn scenario_07_encoding_conflict_detects_warning() {
    let s1 = ParsedSession::from_parts(
        "a".into(),
        "/Users/alice/code/项目一".into(),
        "-Users-alice-code---".into(),
        Vec::new(),
        true,
    );
    let s2 = ParsedSession::from_parts(
        "b".into(),
        "/Users/alice/code/项目二".into(),
        "-Users-alice-code---".into(),
        Vec::new(),
        true,
    );

    let conflicts = SessionIndex::from_sessions(&[s1, s2]).conflicts();

    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].original_paths.len(), 2);
}

#[test]
fn scenario_08_workspace_mode_discovers_child_projects() {
    let env = TestEnv::new("workspace");
    let local_ws = env.root.join("local-workspace");
    let remote_ws = env.root.join("remote-workspace");
    fs::create_dir_all(local_ws.join("frontend")).unwrap();
    fs::create_dir_all(local_ws.join("backend")).unwrap();
    fs::create_dir_all(remote_ws.join("frontend")).unwrap();
    let workspace = WorkspaceConfig {
        name: "all".into(),
        local_root: local_ws.clone(),
        remote_root: remote_ws.clone(),
        peer: "peer".into(),
        children: Vec::new(),
        local: local_ws,
        peers: std::collections::HashMap::from([("peer".into(), remote_ws)]),
        scan_depth: 1,
        auto_enable_new: true,
        sync_mode: SyncModeConfig::TwoWayAuto,
        enabled: true,
        exclude_rules: Vec::new(),
    };

    let projects = env
        .coordinator_with_no_sessions()
        .scan_workspace(&workspace, "peer")
        .unwrap();

    assert_eq!(projects.len(), 2);
    assert!(projects
        .iter()
        .any(|project| project.name == "frontend" && project.matched_remote && project.enabled));
    assert!(projects
        .iter()
        .any(|project| project.name == "backend" && !project.matched_remote));
}

#[test]
fn scenario_09_interrupted_transfer_leaves_target_without_half_finished_files() {
    let env = TestEnv::new("interrupted");
    fs::write(env.a_code.join("new.txt"), "new").unwrap();
    fs::write(env.b_code.join("old.txt"), "old").unwrap();
    let logs = TransportLog::default().fail_on_send_files_call(1);

    let result = env
        .coordinator_with_parser(logs, Box::new(NoSessionParser))
        .push_to(&env.peer.id, &env.project());

    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(env.b_code.join("old.txt")).unwrap(),
        "old"
    );
    assert!(!env.b_code.join("new.txt").exists());
}

#[test]
fn scenario_10_large_directory_performance_stays_within_budget() {
    let env = TestEnv::new("large");
    for index in 0..10_000 {
        fs::write(env.a_code.join(format!("f-{index:05}.txt")), "x").unwrap();
    }

    let started = Instant::now();
    let report = env
        .coordinator_with_no_sessions()
        .push_to(&env.peer.id, &env.project())
        .unwrap();

    assert_eq!(report.code_files_transferred, 10_000);
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "large directory sync took {:?}",
        started.elapsed()
    );
}

#[test]
fn scenario_11_sensitive_files_are_excluded() {
    let env = TestEnv::new("exclude");
    fs::write(env.a_code.join(".env"), "SECRET=1").unwrap();
    fs::write(env.a_code.join(".env.local"), "SECRET=2").unwrap();
    fs::create_dir_all(env.a_code.join("node_modules/pkg")).unwrap();
    fs::write(env.a_code.join("node_modules/pkg/index.js"), "module").unwrap();
    fs::create_dir_all(env.a_code.join(".git/objects/aa")).unwrap();
    fs::write(env.a_code.join(".git/objects/aa/object"), "git").unwrap();
    fs::create_dir_all(env.a_code.join("target/debug")).unwrap();
    fs::write(env.a_code.join("target/debug/app"), "bin").unwrap();
    fs::write(env.a_code.join("visible.txt"), "ok").unwrap();

    env.coordinator_with_no_sessions()
        .push_to(&env.peer.id, &env.project())
        .unwrap();

    assert_eq!(
        fs::read_to_string(env.b_code.join("visible.txt")).unwrap(),
        "ok"
    );
    assert!(!env.b_code.join(".env").exists());
    assert!(!env.b_code.join(".env.local").exists());
    assert!(!env.b_code.join("node_modules/pkg/index.js").exists());
    assert!(!env.b_code.join(".git/objects/aa/object").exists());
    assert!(!env.b_code.join("target/debug/app").exists());
}

#[test]
fn scenario_12_two_way_auto_sync_pushes_then_pulls() {
    let env = TestEnv::new("auto");
    fs::write(env.a_code.join("state.txt"), "from-a").unwrap();
    let mut coordinator = env.coordinator_with_no_sessions();

    let pushed = coordinator
        .run_auto_sync_once(&env.peer.id, &env.project(), true, false)
        .unwrap();
    assert!(matches!(pushed, aisync_sync::AutoSyncOutcome::Synced(_)));
    assert_eq!(
        fs::read_to_string(env.b_code.join("state.txt")).unwrap(),
        "from-a"
    );

    fs::write(env.b_code.join("state.txt"), "from-b").unwrap();
    let pulled = coordinator
        .run_auto_sync_once(&env.peer.id, &env.project(), false, true)
        .unwrap();
    assert!(matches!(pulled, aisync_sync::AutoSyncOutcome::Synced(_)));
    assert_eq!(
        fs::read_to_string(env.a_code.join("state.txt")).unwrap(),
        "from-b"
    );
}

#[derive(Clone)]
struct TestEnv {
    root: PathBuf,
    a_code: PathBuf,
    a_session: PathBuf,
    b_code: PathBuf,
    b_session: PathBuf,
    peer: DeviceInfo,
}

impl TestEnv {
    fn new(name: &str) -> Self {
        let root = temp_dir(name);
        let a_code = root.join("mac-a").join("code").join("app");
        let a_session = root.join("mac-a").join(".claude");
        let b_code = root.join("mac-b").join("code").join("app");
        let b_session = root.join("mac-b").join(".claude");
        for path in [&a_code, &a_session, &b_code, &b_session] {
            fs::create_dir_all(path).unwrap();
        }
        Self {
            root,
            a_code,
            a_session,
            b_code,
            b_session,
            peer: DeviceInfo {
                id: DeviceId::new(),
                name: "mac-b".into(),
                os: OsType::Darwin,
                addresses: Vec::new(),
                protocol_version: 1,
            },
        }
    }

    fn project(&self) -> ProjectMapping {
        ProjectMapping {
            project_id: "app".into(),
            local_code_dir: self.a_code.clone(),
            local_session_dir: self.a_session.clone(),
            remote_code_dir: self.b_code.clone(),
            remote_session_dir: self.b_session.clone(),
            original_source_path: self.a_code.to_string_lossy().into_owned(),
            enabled: true,
        }
    }

    fn config(&self) -> SyncConfig {
        let mut config = SyncConfig::new("mac-a");
        config.state_path = Some(self.root.join("state.toml"));
        config
    }

    fn coordinator_with_no_sessions(&self) -> SyncCoordinator {
        self.coordinator_with_parser(TransportLog::default(), Box::new(NoSessionParser))
    }

    fn coordinator_with_claude(&self) -> SyncCoordinator {
        self.coordinator_with_parser(TransportLog::default(), Box::new(ClaudeCodeParser::new()))
    }

    fn coordinator_with_parser(
        &self,
        transport: TransportLog,
        parser: Box<dyn SessionParser>,
    ) -> SyncCoordinator {
        let rewriter = RuleBasedRewriter::new(vec![PathRule::unix_to_unix(
            self.a_code.to_string_lossy().into_owned(),
            self.b_code.to_string_lossy().into_owned(),
        )])
        .unwrap();
        SyncCoordinator::new(
            Box::new(FakeDiscoverer {
                peers: vec![self.peer.clone()],
            }),
            Box::new(transport),
            parser,
            Box::new(rewriter),
            self.config(),
        )
        .unwrap()
    }
}

#[derive(Clone, Default)]
struct TransportLog {
    inner: Arc<Mutex<TransportLogInner>>,
}

#[derive(Default)]
struct TransportLogInner {
    file_counts: Vec<usize>,
    send_files_calls: usize,
    fail_on_call: Option<usize>,
}

impl TransportLog {
    fn file_counts(&self) -> Vec<usize> {
        self.inner.lock().unwrap().file_counts.clone()
    }

    fn clear(&self) {
        self.inner.lock().unwrap().file_counts.clear();
    }

    fn fail_on_send_files_call(self, call: usize) -> Self {
        self.inner.lock().unwrap().fail_on_call = Some(call);
        self
    }
}

impl Transporter for TransportLog {
    fn connect(&mut self, _peer: &DeviceInfo) -> Result<()> {
        Ok(())
    }

    fn send_manifest(&mut self, _manifest: &SyncManifest) -> Result<()> {
        Ok(())
    }

    fn send_files(&mut self, _root: &Path, files: &[FileEntry]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.send_files_calls += 1;
        if inner.fail_on_call == Some(inner.send_files_calls) {
            return Err(AisyncError::Transport("simulated interruption".into()));
        }
        inner.file_counts.push(files.len());
        Ok(())
    }

    fn receive_files(&mut self, _target_dir: &Path) -> Result<SyncManifest> {
        Ok(SyncManifest { files: Vec::new() })
    }
}

struct FakeDiscoverer {
    peers: Vec<DeviceInfo>,
}

impl Discoverer for FakeDiscoverer {
    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    fn peers(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self.peers.clone())
    }

    fn on_peer_change(&mut self, _callback: PeerChangeCallback) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct NoSessionParser;

impl SessionParser for NoSessionParser {
    fn tool_name(&self) -> &str {
        "none"
    }

    fn detect(&self, _path: &Path) -> bool {
        false
    }

    fn parse(&self, _config_dir: &Path) -> Result<Vec<Session>> {
        Ok(Vec::new())
    }

    fn rewrite_paths(
        &self,
        _session: &mut Session,
        _rewriter: &dyn aisync_core::PathRewriter,
    ) -> Result<()> {
        Ok(())
    }

    fn write_session(&self, _session: &Session, _target_dir: &Path) -> Result<()> {
        Ok(())
    }
}

fn write_claude_session(
    claude_dir: &Path,
    encoded_dir: &str,
    session_id: &str,
    project_path: &Path,
    relative_file: &str,
) {
    let dir = claude_dir.join("projects").join(encoded_dir);
    fs::create_dir_all(&dir).unwrap();
    let file_path = project_path.join(relative_file);
    let record = serde_json::json!({
        "type": "user",
        "cwd": project_path,
        "message": {
            "content": [{
                "type": "tool_use",
                "input": {
                    "file_path": file_path
                }
            }]
        }
    });
    fs::write(
        dir.join(format!("{session_id}.jsonl")),
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();
}

fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "aisync-phase2-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}
