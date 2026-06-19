pub mod config;
pub mod watcher;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aisync_core::{
    AisyncError, ConflictDetails, DeviceId, DeviceInfo, Direction, Discoverer, FileEntry,
    PathRewriter, ProjectMapping, Result, RewriteDirection, SessionParser, SyncManifest,
    Transporter,
};
use aisync_transport::{diff_manifests, scan_manifest_with_patterns, FileDiff};
use serde::{Deserialize, Serialize};

pub use config::{
    default_config_path, default_refresh_interval_secs, default_state_path, load_config,
    save_config, ClaudeConfig, ConfigStore, DeviceConfig, PeerConfig, ProjectConfig, SyncConfig,
    SyncModeConfig, WorkspaceChildConfig, WorkspaceConfig,
};
pub use watcher::{
    default_exclude_rules, expand_exclude_rules, ChangeBatch, FileChange, FileChangeKind,
    FsWatcher, WatchConfig, DEFAULT_DEBOUNCE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub project_id: String,
    pub peer_id: DeviceId,
    pub direction: Direction,
    pub code_files_transferred: usize,
    pub session_files_transferred: usize,
    pub deleted_files: usize,
    pub rewritten_sessions: usize,
    pub local_version: u64,
    pub remote_version: u64,
    pub stages: Vec<SyncStage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStage {
    pub name: &'static str,
    pub percent: u8,
    pub current_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredProject {
    pub name: String,
    pub local_code_dir: PathBuf,
    pub remote_code_dir: PathBuf,
    pub enabled: bool,
    pub matched_remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoSyncOutcome {
    Idle,
    Synced(SyncReport),
}

pub struct SyncCoordinator {
    discoverer: Box<dyn Discoverer>,
    transporter: Box<dyn Transporter>,
    session_parser: Box<dyn SessionParser>,
    path_rewriter: Box<dyn PathRewriter>,
    config: SyncConfig,
    state: SyncState,
}

impl SyncCoordinator {
    pub fn new(
        discoverer: Box<dyn Discoverer>,
        transporter: Box<dyn Transporter>,
        session_parser: Box<dyn SessionParser>,
        path_rewriter: Box<dyn PathRewriter>,
        config: SyncConfig,
    ) -> Result<Self> {
        let state = SyncState::load(&config.state_path())?;
        Ok(Self {
            discoverer,
            transporter,
            session_parser,
            path_rewriter,
            config,
            state,
        })
    }

    pub fn config(&self) -> &SyncConfig {
        &self.config
    }

    pub fn push_to(&mut self, peer: &DeviceId, project: &ProjectMapping) -> Result<SyncReport> {
        self.sync_one_way(peer, project, Direction::LocalToRemote)
    }

    pub fn pull_from(&mut self, peer: &DeviceId, project: &ProjectMapping) -> Result<SyncReport> {
        self.sync_one_way(peer, project, Direction::RemoteToLocal)
    }

    pub fn run_auto_sync_once(
        &mut self,
        peer: &DeviceId,
        project: &ProjectMapping,
        local_changed: bool,
        remote_changed: bool,
    ) -> Result<AutoSyncOutcome> {
        match (local_changed, remote_changed) {
            (false, false) => Ok(AutoSyncOutcome::Idle),
            (true, false) => self.push_to(peer, project).map(AutoSyncOutcome::Synced),
            (false, true) => self.pull_from(peer, project).map(AutoSyncOutcome::Synced),
            (true, true) => Err(AisyncError::ConflictDetected(ConflictDetails {
                project_id: project.project_id.clone(),
                local_version: self
                    .state
                    .project(&project.project_id)
                    .map(|state| state.local_version + 1)
                    .unwrap_or(1),
                remote_version: self
                    .state
                    .project(&project.project_id)
                    .map(|state| state.remote_version + 1)
                    .unwrap_or(1),
                summary: "local and remote both changed since last sync".to_string(),
            })),
        }
    }

    pub fn scan_workspace(
        &self,
        workspace: &WorkspaceConfig,
        peer_name: &str,
    ) -> Result<Vec<DiscoveredProject>> {
        if workspace.scan_depth != 1 {
            return Err(AisyncError::Config(
                "workspace scan_depth other than 1 is not supported".to_string(),
            ));
        }
        let remote_base = workspace.effective_remote_root(peer_name).ok_or_else(|| {
            AisyncError::Config(format!(
                "workspace '{}' has no mapping for peer '{}'",
                workspace.name, peer_name
            ))
        })?;
        let remote_names = first_level_dir_names(&remote_base)?;
        let mut projects = Vec::new();

        for entry in fs::read_dir(workspace.effective_local_root())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            projects.push(DiscoveredProject {
                name: name.clone(),
                local_code_dir: entry.path(),
                remote_code_dir: remote_base.join(&name),
                enabled: workspace.auto_enable_new && remote_names.contains(&name),
                matched_remote: remote_names.contains(&name),
            });
        }

        projects.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(projects)
    }

    fn sync_one_way(
        &mut self,
        peer_id: &DeviceId,
        project: &ProjectMapping,
        direction: Direction,
    ) -> Result<SyncReport> {
        let remote_dir = match direction {
            Direction::LocalToRemote => &project.remote_code_dir,
            Direction::RemoteToLocal => &project.local_code_dir,
        };
        let bytes = project_source_bytes(project, direction).unwrap_or(0);
        sync_log(
            "sync_started",
            &[
                ("project", project.project_id.clone()),
                ("peer", peer_id.0.to_string()),
                ("remote_dir", remote_dir.display().to_string()),
                ("file_count", "0".to_string()),
                ("bytes", bytes.to_string()),
            ],
        );
        let result = self.sync_one_way_impl(peer_id, project, direction);
        match &result {
            Ok(report) => sync_log(
                "sync_complete",
                &[
                    ("project", project.project_id.clone()),
                    ("peer", peer_id.0.to_string()),
                    ("remote_dir", remote_dir.display().to_string()),
                    (
                        "file_count",
                        (report.code_files_transferred + report.session_files_transferred)
                            .to_string(),
                    ),
                    ("bytes", bytes.to_string()),
                ],
            ),
            Err(error) => sync_log(
                "sync_failed",
                &[
                    ("project", project.project_id.clone()),
                    ("peer", peer_id.0.to_string()),
                    ("remote_dir", remote_dir.display().to_string()),
                    ("file_count", "0".to_string()),
                    ("bytes", bytes.to_string()),
                    ("error", error.to_string()),
                ],
            ),
        }
        result
    }

    fn sync_one_way_impl(
        &mut self,
        peer_id: &DeviceId,
        project: &ProjectMapping,
        direction: Direction,
    ) -> Result<SyncReport> {
        if !project.enabled {
            return Err(AisyncError::InvalidInput(format!(
                "project '{}' is disabled",
                project.project_id
            )));
        }

        let mut stages = Vec::new();
        stages.push(stage("discover", 5, None));
        let peer = self.find_peer(peer_id)?;

        stages.push(stage("connect", 10, None));
        self.transporter.connect(&peer)?;

        let excludes = self.config.exclude_rules_for_project(&project.project_id);
        let pattern_refs = as_pattern_refs(&excludes);
        let local_snapshot = project_snapshot(
            &project.local_code_dir,
            &project.local_session_dir,
            &pattern_refs,
        )?;
        let remote_snapshot = project_snapshot(
            &project.remote_code_dir,
            &project.remote_session_dir,
            &pattern_refs,
        )?;
        self.detect_conflict(project, &local_snapshot, &remote_snapshot)?;

        let paths = DirectionalPaths::new(project, direction);
        stages.push(stage("scan_manifest", 20, None));
        let code_source_manifest = scan_manifest_with_patterns(paths.source_code, &pattern_refs)?;
        let code_target_manifest = scan_manifest_with_patterns(paths.target_code, &pattern_refs)?;
        let session_source_manifest =
            scan_manifest_with_patterns(paths.source_session, &pattern_refs)?;
        let session_target_manifest =
            scan_manifest_with_patterns(paths.target_session, &pattern_refs)?;

        stages.push(stage("exchange_manifest", 35, None));
        self.transporter.send_manifest(&code_source_manifest)?;
        self.transporter.send_manifest(&session_source_manifest)?;

        stages.push(stage("calculate_diff", 45, None));
        let code_diff = diff_manifests(&code_source_manifest, &code_target_manifest);
        let session_diff = diff_manifests(&session_source_manifest, &session_target_manifest);
        let changed_code = changed_entries(&code_diff);
        let changed_sessions = changed_entries(&session_diff);

        stages.push(stage("transfer_code", 60, first_path(&changed_code)));
        self.transporter
            .send_files(paths.source_code, &changed_code)?;
        let staged_code = unique_sibling(paths.target_code, ".aisync-stage-code")?;
        copy_manifest_files(
            paths.source_code,
            &staged_code,
            &code_source_manifest,
            None,
            self.path_rewriter.as_ref(),
        )?;

        stages.push(stage("rewrite_session", 75, first_path(&changed_sessions)));
        let staged_session = unique_staging_path(
            paths.target_session,
            ".aisync-stage-session",
            Some(paths.target_code),
        )?;
        let rewritten_sessions = self.stage_sessions(
            paths.source_session,
            &staged_session,
            direction,
            &pattern_refs,
        )?;

        stages.push(stage("transfer_session", 85, first_path(&changed_sessions)));
        self.transporter
            .send_files(paths.source_session, &changed_sessions)?;

        stages.push(stage("atomic_commit", 95, None));
        commit_two_dirs(
            paths.target_code,
            &staged_code,
            paths.target_session,
            &staged_session,
        )?;

        stages.push(stage("update_version", 100, None));
        let local_after = project_snapshot(
            &project.local_code_dir,
            &project.local_session_dir,
            &pattern_refs,
        )?;
        let remote_after = project_snapshot(
            &project.remote_code_dir,
            &project.remote_session_dir,
            &pattern_refs,
        )?;
        let versions = self
            .state
            .record_success(&project.project_id, &local_after, &remote_after);
        self.state.save(&self.config.state_path())?;

        Ok(SyncReport {
            project_id: project.project_id.clone(),
            peer_id: *peer_id,
            direction,
            code_files_transferred: changed_code.len(),
            session_files_transferred: changed_sessions.len(),
            deleted_files: code_diff.deleted.len() + session_diff.deleted.len(),
            rewritten_sessions,
            local_version: versions.local_version,
            remote_version: versions.remote_version,
            stages,
        })
    }

    fn find_peer(&self, peer_id: &DeviceId) -> Result<DeviceInfo> {
        self.discoverer
            .peers()?
            .into_iter()
            .find(|peer| &peer.id == peer_id)
            .ok_or_else(|| AisyncError::Discovery(format!("peer '{:?}' is not online", peer_id)))
    }

    fn detect_conflict(
        &self,
        project: &ProjectMapping,
        local_snapshot: &ProjectSnapshot,
        remote_snapshot: &ProjectSnapshot,
    ) -> Result<()> {
        let Some(state) = self.state.project(&project.project_id) else {
            return Ok(());
        };
        if !state.has_synced {
            return Ok(());
        }

        let local_changed = state.local_fingerprint != local_snapshot.fingerprint;
        let remote_changed = state.remote_fingerprint != remote_snapshot.fingerprint;
        if local_changed && remote_changed {
            return Err(AisyncError::ConflictDetected(ConflictDetails {
                project_id: project.project_id.clone(),
                local_version: state.local_version + 1,
                remote_version: state.remote_version + 1,
                summary: "local and remote both changed since last sync".to_string(),
            }));
        }

        Ok(())
    }

    fn stage_sessions(
        &mut self,
        source_session: &Path,
        staged_session: &Path,
        direction: Direction,
        patterns: &[&str],
    ) -> Result<usize> {
        recreate_empty_dir(staged_session)?;
        if !source_session.exists() {
            return Ok(0);
        }
        let mut sessions = self.session_parser.parse(source_session)?;
        if sessions.is_empty() {
            let manifest = scan_manifest_with_patterns(source_session, patterns)?;
            copy_manifest_files(
                source_session,
                staged_session,
                &manifest,
                Some(rewrite_direction(direction)),
                self.path_rewriter.as_ref(),
            )?;
            return Ok(0);
        }

        let mut rewritten = 0;
        for session in &mut sessions {
            self.session_parser
                .rewrite_paths(session, self.path_rewriter.as_ref())?;
            self.session_parser.write_session(session, staged_session)?;
            rewritten += 1;
        }
        Ok(rewritten)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SyncState {
    projects: HashMap<String, ProjectVersionState>,
}

impl SyncState {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                projects: HashMap::new(),
            });
        }
        let text = fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|error| AisyncError::Config(format!("parse state: {error}")))
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|error| AisyncError::Config(format!("encode state: {error}")))?;
        fs::write(path, text)?;
        Ok(())
    }

    fn project(&self, project_id: &str) -> Option<&ProjectVersionState> {
        self.projects.get(project_id)
    }

    fn record_success(
        &mut self,
        project_id: &str,
        local: &ProjectSnapshot,
        remote: &ProjectSnapshot,
    ) -> ProjectVersionState {
        let entry = self
            .projects
            .entry(project_id.to_string())
            .or_insert_with(ProjectVersionState::default);
        if !entry.has_synced || entry.local_fingerprint != local.fingerprint {
            entry.local_version += 1;
        }
        if !entry.has_synced || entry.remote_fingerprint != remote.fingerprint {
            entry.remote_version += 1;
        }
        entry.local_fingerprint = local.fingerprint.clone();
        entry.remote_fingerprint = remote.fingerprint.clone();
        entry.last_synced_at_unix_secs = unix_secs();
        entry.has_synced = true;
        entry.clone()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ProjectVersionState {
    local_version: u64,
    remote_version: u64,
    local_fingerprint: String,
    remote_fingerprint: String,
    last_synced_at_unix_secs: u64,
    has_synced: bool,
}

#[derive(Debug, Clone)]
struct ProjectSnapshot {
    fingerprint: String,
}

struct DirectionalPaths<'a> {
    source_code: &'a Path,
    source_session: &'a Path,
    target_code: &'a Path,
    target_session: &'a Path,
}

impl<'a> DirectionalPaths<'a> {
    fn new(project: &'a ProjectMapping, direction: Direction) -> Self {
        match direction {
            Direction::LocalToRemote => Self {
                source_code: &project.local_code_dir,
                source_session: &project.local_session_dir,
                target_code: &project.remote_code_dir,
                target_session: &project.remote_session_dir,
            },
            Direction::RemoteToLocal => Self {
                source_code: &project.remote_code_dir,
                source_session: &project.remote_session_dir,
                target_code: &project.local_code_dir,
                target_session: &project.local_session_dir,
            },
        }
    }
}

fn project_source_bytes(project: &ProjectMapping, direction: Direction) -> Result<u64> {
    let paths = DirectionalPaths::new(project, direction);
    Ok(directory_bytes(paths.source_code)? + directory_bytes(paths.source_session)?)
}

fn directory_bytes(root: &Path) -> Result<u64> {
    let mut total = 0;
    if !root.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total += directory_bytes(&entry.path())?;
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

fn sync_log(event: &str, fields: &[(&str, String)]) {
    let mut line = format!("[aisync-sync] event={event}");
    for (key, value) in fields {
        let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"<encode-error>\"".into());
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&encoded);
    }
    eprintln!("{line}");
}

fn project_snapshot(
    code_dir: &Path,
    session_dir: &Path,
    patterns: &[&str],
) -> Result<ProjectSnapshot> {
    let code = scan_manifest_with_patterns(code_dir, patterns)?;
    let session = scan_manifest_with_patterns(session_dir, patterns)?;
    Ok(ProjectSnapshot {
        fingerprint: fingerprint_pair(&code, &session),
    })
}

fn fingerprint_pair(code: &SyncManifest, session: &SyncManifest) -> String {
    let mut hasher = blake3::Hasher::new();
    update_fingerprint(&mut hasher, "code", code);
    update_fingerprint(&mut hasher, "session", session);
    hasher.finalize().to_hex().to_string()
}

fn update_fingerprint(hasher: &mut blake3::Hasher, prefix: &str, manifest: &SyncManifest) {
    hasher.update(prefix.as_bytes());
    for file in &manifest.files {
        hasher.update(file.relative_path.as_bytes());
        hasher.update(file.blake3_hash.as_bytes());
        hasher.update(&file.size.to_le_bytes());
    }
}

fn changed_entries(diff: &FileDiff) -> Vec<FileEntry> {
    diff.added
        .iter()
        .chain(diff.modified.iter())
        .cloned()
        .collect()
}

fn first_path(entries: &[FileEntry]) -> Option<String> {
    entries.first().map(|entry| entry.relative_path.clone())
}

fn copy_manifest_files(
    source: &Path,
    target: &Path,
    manifest: &SyncManifest,
    rewrite: Option<RewriteDirection>,
    rewriter: &dyn PathRewriter,
) -> Result<()> {
    recreate_empty_dir(target)?;
    for entry in &manifest.files {
        let source_path = source.join(&entry.relative_path);
        let target_path = target.join(&entry.relative_path);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        match rewrite {
            Some(direction) => copy_with_rewrite(&source_path, &target_path, direction, rewriter)?,
            None => {
                fs::copy(&source_path, &target_path)?;
            }
        }
    }
    Ok(())
}

fn copy_with_rewrite(
    source: &Path,
    target: &Path,
    direction: RewriteDirection,
    rewriter: &dyn PathRewriter,
) -> Result<()> {
    match fs::read_to_string(source) {
        Ok(content) => {
            let rewritten = rewriter.rewrite(&content, direction)?;
            fs::write(target, rewritten)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            fs::copy(source, target)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn commit_two_dirs(
    first_target: &Path,
    first_stage: &Path,
    second_target: &Path,
    second_stage: &Path,
) -> Result<()> {
    let mut committed = Vec::new();
    for (target, stage) in [(first_target, first_stage), (second_target, second_stage)] {
        let backup = unique_sibling(target, ".aisync-backup")?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if target.exists() {
            fs::rename(target, &backup)?;
        }
        if let Err(error) = fs::rename(stage, target) {
            if backup.exists() {
                let _ = fs::rename(&backup, target);
            }
            rollback_committed(&mut committed);
            return Err(error.into());
        }
        committed.push((target.to_path_buf(), backup));
    }

    for (_, backup) in committed {
        if backup.exists() {
            fs::remove_dir_all(backup)?;
        }
    }
    Ok(())
}

fn rollback_committed(committed: &mut Vec<(PathBuf, PathBuf)>) {
    while let Some((target, backup)) = committed.pop() {
        let _ = fs::remove_dir_all(&target);
        if backup.exists() {
            let _ = fs::rename(backup, target);
        }
    }
}

fn recreate_empty_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

fn unique_sibling(path: &Path, prefix: &str) -> Result<PathBuf> {
    unique_staging_path(path, prefix, None)
}

fn unique_staging_path(
    path: &Path,
    prefix: &str,
    avoid_ancestor: Option<&Path>,
) -> Result<PathBuf> {
    let parent = avoid_ancestor
        .filter(|ancestor| path.starts_with(ancestor))
        .and_then(Path::parent)
        .or_else(|| path.parent())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("root");
    for attempt in 0..1000 {
        let candidate = parent.join(format!("{prefix}-{name}-{}-{attempt}", unix_nanos()));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(AisyncError::Io(format!(
        "could not allocate staging path for {}",
        path.display()
    )))
}

fn first_level_dir_names(root: &Path) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    if !root.exists() {
        return Ok(names);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(names)
}

fn as_pattern_refs(patterns: &[String]) -> Vec<&str> {
    patterns.iter().map(String::as_str).collect()
}

fn rewrite_direction(direction: Direction) -> RewriteDirection {
    match direction {
        Direction::LocalToRemote => RewriteDirection::SourceToTarget,
        Direction::RemoteToLocal => RewriteDirection::TargetToSource,
    }
}

fn stage(name: &'static str, percent: u8, current_file: Option<String>) -> SyncStage {
    SyncStage {
        name,
        percent,
        current_file,
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisync_core::{
        DeviceInfo, Discoverer, OsType, PeerChangeCallback, Session, SyncMode, Transporter,
    };
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn config_round_trips_as_toml() {
        let root = temp_dir("config");
        let local_project = root.join("local-project");
        let remote_project = root.join("remote-project");
        fs::create_dir_all(&local_project).unwrap();
        fs::create_dir_all(&remote_project).unwrap();

        let mut config = SyncConfig::new("MacBook");
        config.claude_config.local = root.join("claude-local");
        config
            .claude_config
            .peers
            .insert("desktop".into(), root.join("claude-remote"));
        config.peers.insert(
            "desktop".into(),
            PeerConfig {
                id: DeviceId::new(),
                name: "Desktop".into(),
                endpoint: None,
                server_cert: None,
                server_name: None,
                last_seen: Some("2026-06-19T10:00:00Z".into()),
            },
        );
        config.projects.push(ProjectConfig {
            name: "myapp".into(),
            local: local_project,
            peers: BTreeMap::from([("desktop".to_string(), remote_project)])
                .into_iter()
                .collect(),
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: vec!["dist/".into()],
        });
        config.state_path = Some(root.join("state.toml"));

        let path = root.join("config.toml");
        save_config(&path, &config).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("sync_mode = \"two_way_auto\""));

        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.device.name, "MacBook");
        assert_eq!(loaded.projects[0].exclude_rules, vec!["dist/"]);
    }

    #[test]
    fn watcher_debounces_and_filters_excluded_paths() {
        let root = temp_dir("watcher");
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = FsWatcher::start(
            WatchConfig {
                paths: vec![root.clone()],
                debounce: Duration::from_millis(120),
                exclude_rules: default_exclude_rules(),
            },
            tx,
        )
        .unwrap();

        thread::sleep(Duration::from_millis(150));
        let file = root.join("note.txt");
        fs::write(&file, "one").unwrap();
        fs::write(&file, "two").unwrap();

        assert!(recv_path_with_name(&rx, "note.txt", Duration::from_secs(3)));

        fs::write(root.join(".env"), "SECRET=1").unwrap();
        assert!(!recv_path_with_name(
            &rx,
            ".env",
            Duration::from_millis(500)
        ));
        fs::write(root.join(".env.local"), "SECRET=2").unwrap();
        assert!(!recv_path_with_name(
            &rx,
            ".env.local",
            Duration::from_millis(500)
        ));
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "module").unwrap();
        assert!(!recv_path_with_name(
            &rx,
            "index.js",
            Duration::from_millis(500)
        ));
        fs::create_dir_all(root.join(".git/objects/aa")).unwrap();
        fs::write(root.join(".git/objects/aa/object"), "git").unwrap();
        assert!(!recv_path_with_name(
            &rx,
            "object",
            Duration::from_millis(500)
        ));
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/app"), "bin").unwrap();
        assert!(!recv_path_with_name(&rx, "app", Duration::from_millis(500)));
        watcher.stop();
    }

    #[test]
    fn push_copies_code_rewrites_sessions_and_detects_split_brain() {
        let root = temp_dir("push");
        let local_code = root.join("local-code");
        let local_session = root.join("local-session");
        let remote_code = root.join("remote-code");
        let remote_session = root.join("remote-session");
        for dir in [&local_code, &local_session, &remote_code, &remote_session] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(local_code.join("src.txt"), "hello").unwrap();
        fs::write(
            local_session.join("session.txt"),
            format!("open {}", local_code.display()),
        )
        .unwrap();

        let peer = test_peer();
        let logs = Arc::new(Mutex::new(Vec::new()));
        let config = test_config(root.join("state.toml"));
        let mut coordinator = SyncCoordinator::new(
            Box::new(FakeDiscoverer {
                peers: vec![peer.clone()],
            }),
            Box::new(FakeTransporter { logs: logs.clone() }),
            Box::new(TextSessionParser),
            Box::new(ReplaceRewriter {
                source: local_code.to_string_lossy().into_owned(),
                target: remote_code.to_string_lossy().into_owned(),
            }),
            config,
        )
        .unwrap();
        let project = ProjectMapping {
            project_id: "myapp".into(),
            local_code_dir: local_code.clone(),
            local_session_dir: local_session.clone(),
            remote_code_dir: remote_code.clone(),
            remote_session_dir: remote_session.clone(),
            original_source_path: local_code.to_string_lossy().into_owned(),
            enabled: true,
        };

        let report = coordinator.push_to(&peer.id, &project).unwrap();
        assert_eq!(report.code_files_transferred, 1);
        assert_eq!(report.session_files_transferred, 1);
        assert_eq!(
            fs::read_to_string(remote_code.join("src.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(remote_session.join("session.txt")).unwrap(),
            format!("open {}", remote_code.display())
        );
        assert!(logs.lock().unwrap().contains(&"connect".to_string()));

        fs::write(local_code.join("src.txt"), "local change").unwrap();
        fs::write(remote_code.join("src.txt"), "remote change").unwrap();
        let error = coordinator.push_to(&peer.id, &project).unwrap_err();
        match error {
            AisyncError::ConflictDetected(details) => {
                assert_eq!(details.project_id, "myapp");
                assert!(details.summary.contains("both changed"));
            }
            other => panic!("expected conflict, got {other:?}"),
        }
    }

    #[test]
    fn workspace_scan_matches_first_level_directories_by_name() {
        let root = temp_dir("workspace");
        let local = root.join("local");
        let remote = root.join("remote");
        fs::create_dir_all(local.join("frontend")).unwrap();
        fs::create_dir_all(local.join("backend")).unwrap();
        fs::create_dir_all(remote.join("frontend")).unwrap();

        let workspace = WorkspaceConfig {
            name: "all".into(),
            local_root: local.clone(),
            remote_root: remote.clone(),
            peer: "desktop".to_string(),
            children: Vec::new(),
            local: local.clone(),
            peers: BTreeMap::from([("desktop".to_string(), remote.clone())])
                .into_iter()
                .collect(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        };
        let peer = test_peer();
        let coordinator = SyncCoordinator::new(
            Box::new(FakeDiscoverer { peers: vec![peer] }),
            Box::new(FakeTransporter {
                logs: Arc::new(Mutex::new(Vec::new())),
            }),
            Box::new(TextSessionParser),
            Box::new(ReplaceRewriter {
                source: String::new(),
                target: String::new(),
            }),
            test_config(root.join("state.toml")),
        )
        .unwrap();

        let projects = coordinator.scan_workspace(&workspace, "desktop").unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "backend");
        assert!(!projects[0].matched_remote);
        assert_eq!(projects[1].name, "frontend");
        assert!(projects[1].matched_remote);
        assert!(projects[1].enabled);
    }

    fn test_config(state_path: PathBuf) -> SyncConfig {
        let mut config = SyncConfig::new("test");
        config.state_path = Some(state_path);
        config
    }

    fn test_peer() -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(),
            name: "peer".into(),
            os: OsType::Darwin,
            addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            protocol_version: 1,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("aisync-sync-{name}-{}", DeviceId::new().0));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn recv_path_with_name(
        rx: &std::sync::mpsc::Receiver<ChangeBatch>,
        file_name: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Ok(batch) => {
                    if batch.changes.iter().any(|change| {
                        change.path.file_name().and_then(|name| name.to_str()) == Some(file_name)
                    }) {
                        return true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
        false
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

    struct FakeTransporter {
        logs: Arc<Mutex<Vec<String>>>,
    }

    impl Transporter for FakeTransporter {
        fn connect(&mut self, _peer: &DeviceInfo) -> Result<()> {
            self.logs.lock().unwrap().push("connect".into());
            Ok(())
        }

        fn send_manifest(&mut self, _manifest: &SyncManifest) -> Result<()> {
            self.logs.lock().unwrap().push("manifest".into());
            Ok(())
        }

        fn send_files(&mut self, _root: &Path, files: &[FileEntry]) -> Result<()> {
            self.logs
                .lock()
                .unwrap()
                .push(format!("files:{}", files.len()));
            Ok(())
        }

        fn receive_files(&mut self, _target_dir: &Path) -> Result<SyncManifest> {
            Ok(SyncManifest { files: Vec::new() })
        }
    }

    struct TextSessionParser;

    impl SessionParser for TextSessionParser {
        fn tool_name(&self) -> &str {
            "text"
        }

        fn detect(&self, path: &Path) -> bool {
            path.join("session.txt").exists()
        }

        fn parse(&self, config_dir: &Path) -> Result<Vec<Session>> {
            let path = config_dir.join("session.txt");
            if !path.exists() {
                return Ok(Vec::new());
            }
            Ok(vec![Session {
                id: "session".into(),
                tool_name: "text".into(),
                project_id: None,
                data: serde_json::json!({
                    "content": fs::read_to_string(path)?,
                }),
            }])
        }

        fn rewrite_paths(&self, session: &mut Session, rewriter: &dyn PathRewriter) -> Result<()> {
            let content = session
                .data
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            session.data["content"] = serde_json::Value::String(
                rewriter.rewrite(content, RewriteDirection::SourceToTarget)?,
            );
            Ok(())
        }

        fn write_session(&self, session: &Session, target_dir: &Path) -> Result<()> {
            fs::create_dir_all(target_dir)?;
            let content = session
                .data
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            fs::write(target_dir.join("session.txt"), content)?;
            Ok(())
        }
    }

    struct ReplaceRewriter {
        source: String,
        target: String,
    }

    impl PathRewriter for ReplaceRewriter {
        fn rewrite(&self, content: &str, direction: RewriteDirection) -> Result<String> {
            let (from, to) = match direction {
                RewriteDirection::SourceToTarget => (&self.source, &self.target),
                RewriteDirection::TargetToSource => (&self.target, &self.source),
            };
            Ok(content.replace(from, to))
        }
    }

    #[test]
    fn sync_mode_config_converts_to_core_mode() {
        assert_eq!(
            SyncModeConfig::TwoWayAuto.to_sync_mode(),
            SyncMode::TwoWayAuto
        );
    }
}
