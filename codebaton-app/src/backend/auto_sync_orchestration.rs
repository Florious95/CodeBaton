//! Auto-sync orchestration: fingerprint computation for change detection and the
//! behavior-sensitive workspace sync driver (`run_workspace_auto_sync_outcome`).
//! Extracted from `mod.rs`; pinned by AUTO-* / CLI-SS-* tests.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use codebaton_core::Result;
use codebaton_discovery::PeerConnectionInfo;
use codebaton_sync::{save_config, SyncConfig, SyncReport, WorkspaceConfig};

use super::SessionMtimeTarget;
use super::{
    app_log, claude_mtime_paths, codex_session_file_matches_project,
    codex_session_file_matches_workspace, hash_codex_sessions_matching, hash_tree_contents,
    replace_workspace, run_workspace_tcp_push,
};

pub(crate) struct WorkspaceSyncOutcome {
    pub(crate) report: SyncReport,
    pub(crate) workspace: WorkspaceConfig,
    pub(crate) child_file_counts: HashMap<String, u32>,
}

pub(crate) fn hash_prefix(fingerprint: &str) -> String {
    fingerprint.chars().take(8).collect()
}

pub(crate) fn workspace_local_session_roots(workspace: &WorkspaceConfig) -> Vec<PathBuf> {
    let root = workspace.effective_local_root();
    let mut roots = vec![root.to_path_buf()];
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                roots.push(entry.path());
            }
        }
    }
    roots.extend(
        workspace
            .children
            .iter()
            .map(|child| child.local_dir.clone()),
    );
    roots.sort();
    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

pub(crate) fn workspace_auto_sync_fingerprint(
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
) -> Option<String> {
    if !workspace.enabled {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"workspace");
    hasher.update(workspace.name.as_bytes());
    hash_tree_contents(&mut hasher, "code", workspace.effective_local_root(), 16384);
    let roots = workspace_local_session_roots(workspace);
    for path in claude_mtime_paths(config, &roots) {
        hash_tree_contents(&mut hasher, "claude", &path, 4096);
    }
    Some(hasher.finalize().to_hex().to_string())
}

pub(crate) fn sync_fingerprint_for_target(config: &SyncConfig, target: &SessionMtimeTarget) -> Option<String> {
    match target.scope {
        "project" => project_sync_fingerprint_for_target(config, target),
        "workspace" => workspace_sync_fingerprint_for_target(config, target),
        _ => None,
    }
}

pub(crate) fn project_sync_fingerprint_for_target(
    config: &SyncConfig,
    target: &SessionMtimeTarget,
) -> Option<String> {
    let project = config.projects.iter().find(|project| {
        project.name == target.name && project.peers.contains_key(&target.peer) && project.enabled
    })?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"project-sync");
    hasher.update(project.name.as_bytes());
    hasher.update(target.peer.as_bytes());
    hash_tree_contents(&mut hasher, "code", &project.local, 8192);
    for path in claude_mtime_paths(config, std::slice::from_ref(&project.local)) {
        hash_tree_contents(&mut hasher, "claude", &path, 2048);
    }
    if target.tool == "codex" {
        hash_codex_sessions_matching(&mut hasher, "codex", |file| {
            codex_session_file_matches_project(file, &project.local)
        });
    }
    Some(hasher.finalize().to_hex().to_string())
}

pub(crate) fn workspace_sync_fingerprint_for_target(
    config: &SyncConfig,
    target: &SessionMtimeTarget,
) -> Option<String> {
    let workspace = config
        .workspaces
        .iter()
        .find(|workspace| workspace.name == target.name && workspace.enabled)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"workspace-sync");
    hasher.update(workspace.name.as_bytes());
    hasher.update(target.peer.as_bytes());
    hash_tree_contents(&mut hasher, "code", workspace.effective_local_root(), 16384);
    let roots = workspace_local_session_roots(workspace);
    for path in claude_mtime_paths(config, &roots) {
        hash_tree_contents(&mut hasher, "claude", &path, 4096);
    }
    if target.tool == "codex" {
        let excluded = HashSet::<String>::new();
        hash_codex_sessions_matching(&mut hasher, "codex", |file| {
            codex_session_file_matches_workspace(file, workspace.effective_local_root(), &excluded)
        });
    }
    Some(hasher.finalize().to_hex().to_string())
}

pub(crate) fn run_workspace_auto_sync_outcome(
    config_path: &Path,
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
    live_connection: Option<PeerConnectionInfo>,
) -> Result<WorkspaceSyncOutcome> {
    let outcome = run_workspace_tcp_push(config_path, config, workspace, live_connection)?;
    let mut updated = config.clone();
    replace_workspace(&mut updated, outcome.workspace.clone());
    save_config(config_path, &updated)?;
    app_log(
        "workspace_children_persisted",
        &[
            ("workspace", outcome.report.project_id.clone()),
            ("config", config_path.display().to_string()),
        ],
    );
    Ok(outcome)
}
