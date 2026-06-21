//! Sync-history persistence: append/read JSONL records, build per-project /
//! per-workspace file summaries, and record sender/receiver/auto sync events.
//! Extracted from `mod.rs`; methods stay on `Backend` via a second impl block.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use codebaton_core::SyncManifest;
use codebaton_sync::{load_config, ProjectConfig, SyncConfig, WorkspaceConfig};

use super::{
    app_log, claude_mtime_paths, codex_session_file_matches_project,
    codex_session_file_matches_workspace, collect_jsonl_files, local_codex_sessions_dir,
    manifest_file_type, mark_incoming_session_roots, mark_incoming_sync_root,
    refresh_and_save_workspaces, safe_relative_path, should_skip_hash_path, Backend,
    HISTORY_FILE_LIMIT,
};

impl Backend {
    /// Append one sync record to the persisted history (`~/.aisync/history.jsonl`,
    /// next to the config). One JSON object per line, newest appended last.
    pub fn record_sync(
        &self,
        project_id: &str,
        direction: &str,
        success: bool,
        files: u32,
        bytes: u64,
        detail: Option<String>,
        timestamp: String,
    ) {
        self.record_sync_scoped(
            project_id, direction, success, files, bytes, detail, timestamp, None, None,
        );
    }

    pub fn record_sync_scoped(
        &self,
        project_id: &str,
        direction: &str,
        success: bool,
        files: u32,
        bytes: u64,
        detail: Option<String>,
        timestamp: String,
        workspace_name: Option<&str>,
        child_name: Option<&str>,
    ) {
        let (path, summary) = {
            let g = self.inner.lock().unwrap();
            let summary = if success {
                history_summary_from_config(
                    &g.config,
                    project_id,
                    workspace_name,
                    child_name,
                    "mixed",
                )
            } else {
                HistoryFileSummary::default()
            };
            (g.config_path.with_file_name("history.jsonl"), summary)
        };
        let bytes = if success && bytes == 0 {
            summary.bytes
        } else {
            bytes
        };
        let file_path = summary.file_paths.first().cloned();
        let file_name = file_path.as_deref().and_then(|path| {
            Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        let file_names: Vec<String> = summary
            .file_paths
            .iter()
            .filter_map(|path| {
                Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect();
        let event_id = codebaton_discovery::new_pairing_request_id();
        let entry = serde_json::json!({
            "eventId": event_id,
            "timestamp": timestamp,
            "projectId": project_id,
            "direction": direction,
            "success": success,
            "files": files,
            "bytes": bytes,
            "detail": detail,
            "workspaceName": workspace_name,
            "childName": child_name,
            "trigger": "manual",
            "role": "sender",
            "fileType": "mixed",
            "file_path": file_path,
            "file_paths": summary.file_paths,
            "file_name": file_name,
            "file_names": file_names,
        });
        match append_json_line(&path, &entry) {
            Ok(()) => app_log(
                "sender_sync_history_recorded",
                &[
                    ("project", project_id.to_string()),
                    ("event_id", event_id.clone()),
                    ("role", "sender".to_string()),
                    ("path", path.display().to_string()),
                ],
            ),
            Err(error) => app_log(
                "history_write_failed",
                &[
                    ("project", project_id.to_string()),
                    ("event_id", event_id),
                    ("path", path.display().to_string()),
                    ("error", error.to_string()),
                ],
            ),
        }
    }

    /// Read persisted sync history (newest first). When `project_id` is given,
    /// only that project's records are returned.
    pub fn sync_history(&self, project_id: Option<&str>) -> Vec<serde_json::Value> {
        let path = self
            .inner
            .lock()
            .unwrap()
            .config_path
            .with_file_name("history.jsonl");
        let Ok(text) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut rows: Vec<serde_json::Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| {
                project_id
                    .map(|pid| {
                        v.get("projectId").and_then(|p| p.as_str()) == Some(pid)
                            || v.get("workspaceName").and_then(|p| p.as_str()) == Some(pid)
                    })
                    .unwrap_or(true)
            })
            .collect();
        rows.reverse(); // newest first
        rows
    }
}

pub(crate) fn append_json_line(path: &Path, entry: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    use std::io::Write;
    writeln!(file, "{entry}")?;
    Ok(())
}

pub(crate) fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HistoryFileSummary {
    pub(crate) bytes: u64,
    pub(crate) file_paths: Vec<String>,
}

impl HistoryFileSummary {
    fn add_file(&mut self, path: &Path) {
        if let Ok(metadata) = fs::metadata(path) {
            if metadata.is_file() {
                self.bytes = self.bytes.saturating_add(metadata.len());
                if self.file_paths.len() < HISTORY_FILE_LIMIT {
                    self.file_paths.push(path.display().to_string());
                }
            }
        }
    }
}

pub(crate) fn history_summary_from_config(
    config: &SyncConfig,
    project_id: &str,
    workspace_name: Option<&str>,
    child_name: Option<&str>,
    file_type: &str,
) -> HistoryFileSummary {
    if let Some(workspace_name) = workspace_name {
        if let Some(workspace) = config
            .workspaces
            .iter()
            .find(|workspace| workspace.name == workspace_name)
        {
            return workspace_history_summary(config, workspace, child_name, file_type);
        }
    }
    if let Some(workspace) = config
        .workspaces
        .iter()
        .find(|workspace| workspace.name == project_id)
    {
        return workspace_history_summary(config, workspace, child_name, file_type);
    }
    if let Some(workspace) = config.workspaces.iter().find(|workspace| {
        workspace
            .children
            .iter()
            .any(|child| child.name == project_id)
    }) {
        return workspace_history_summary(config, workspace, Some(project_id), file_type);
    }
    if let Some(project) = config
        .projects
        .iter()
        .find(|project| project.name == project_id)
    {
        return project_history_summary(config, project, file_type);
    }
    HistoryFileSummary::default()
}

pub(crate) fn project_history_summary(
    config: &SyncConfig,
    project: &ProjectConfig,
    file_type: &str,
) -> HistoryFileSummary {
    let mut summary = HistoryFileSummary::default();
    if matches!(file_type, "code" | "mixed") {
        add_tree_history_summary(&mut summary, &project.local);
    }
    if matches!(file_type, "session" | "mixed") {
        for path in claude_mtime_paths(config, std::slice::from_ref(&project.local)) {
            add_tree_history_summary(&mut summary, &path);
        }
        add_codex_history_summary(&mut summary, |file| {
            codex_session_file_matches_project(file, &project.local)
        });
    }
    summary
}

pub(crate) fn workspace_history_summary(
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
    child_name: Option<&str>,
    file_type: &str,
) -> HistoryFileSummary {
    let mut summary = HistoryFileSummary::default();
    let roots: Vec<PathBuf> = if let Some(child_name) = child_name {
        workspace
            .children
            .iter()
            .find(|child| child.name == child_name)
            .map(|child| vec![child.local_dir.clone()])
            .unwrap_or_default()
    } else {
        vec![workspace.effective_local_root().to_path_buf()]
    };
    if matches!(file_type, "code" | "mixed") {
        for root in &roots {
            add_tree_history_summary(&mut summary, root);
        }
    }
    if matches!(file_type, "session" | "mixed") {
        for path in claude_mtime_paths(config, &roots) {
            add_tree_history_summary(&mut summary, &path);
        }
        let excluded = workspace
            .children
            .iter()
            .filter(|child| child.conflicted || !child.enabled)
            .map(|child| child.name.clone())
            .collect::<HashSet<_>>();
        add_codex_history_summary(&mut summary, |file| {
            if let Some(child_root) = roots.first().filter(|_| child_name.is_some()) {
                codex_session_file_matches_project(file, child_root)
            } else {
                codex_session_file_matches_workspace(
                    file,
                    workspace.effective_local_root(),
                    &excluded,
                )
            }
        });
    }
    summary
}

pub(crate) fn add_tree_history_summary(summary: &mut HistoryFileSummary, root: &Path) {
    if !root.exists() || should_skip_hash_path(root) {
        return;
    }
    let Ok(metadata) = fs::metadata(root) else {
        return;
    };
    if metadata.is_file() {
        summary.add_file(root);
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        add_tree_history_summary(summary, &path);
    }
}

pub(crate) fn add_codex_history_summary(
    summary: &mut HistoryFileSummary,
    mut matches: impl FnMut(&Path) -> bool,
) {
    let Some(root) = local_codex_sessions_dir() else {
        return;
    };
    let mut files = Vec::new();
    if collect_jsonl_files(&root, &mut files).is_err() {
        return;
    }
    files.retain(|file| matches(file));
    files.sort();
    for file in files {
        summary.add_file(&file);
    }
}

pub(crate) fn record_auto_sync_history(
    config_path: &Path,
    project_id: &str,
    success: bool,
    files: u32,
    detail: Option<String>,
    workspace_name: Option<&str>,
    child_name: Option<&str>,
    file_type: &str,
) {
    let path = config_path.with_file_name("history.jsonl");
    let summary = if success {
        load_config(config_path)
            .ok()
            .map(|config| {
                history_summary_from_config(
                    &config,
                    project_id,
                    workspace_name,
                    child_name,
                    file_type,
                )
            })
            .unwrap_or_default()
    } else {
        HistoryFileSummary::default()
    };
    let bytes = summary.bytes;
    let file_paths = summary.file_paths.clone();
    let file_path = summary.file_paths.first().cloned();
    let file_name = file_path.as_deref().and_then(|path| {
        Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    let file_names: Vec<String> = summary
        .file_paths
        .iter()
        .filter_map(|path| {
            Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    let event_id = codebaton_discovery::new_pairing_request_id();
    let entry = serde_json::json!({
        "eventId": event_id,
        "timestamp": super::epoch_millis_now(),
        "projectId": project_id,
        "direction": "push",
        "success": success,
        "files": files,
        "bytes": bytes,
        "detail": detail,
        "workspaceName": workspace_name,
        "childName": child_name,
        "trigger": "auto",
        "role": "sender",
        "fileType": file_type,
        "file_path": file_path,
        "file_paths": file_paths,
        "file_name": file_name,
        "file_names": file_names,
    });
    app_log(
        "record_sync_started",
        &[
            ("project", project_id.to_string()),
            ("trigger", "auto".to_string()),
            ("success", success.to_string()),
            ("bytes", bytes.to_string()),
            (
                "file_path",
                entry
                    .get("file_path")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ),
        ],
    );
    let result = (|| -> std::io::Result<()> {
        append_json_line(&path, &entry)?;
        Ok(())
    })();
    match result {
        Ok(()) => app_log(
            "sender_sync_history_recorded",
            &[
                ("project", project_id.to_string()),
                ("event_id", event_id.clone()),
                ("role", "sender".to_string()),
                ("path", path.display().to_string()),
            ],
        ),
        Err(error) => app_log(
            "history_write_failed",
            &[
                ("project", project_id.to_string()),
                ("event_id", event_id),
                ("path", path.display().to_string()),
                ("error", error.to_string()),
            ],
        ),
    }
}

pub(crate) fn record_receiver_sync_history(
    config_path: &Path,
    manifest: &SyncManifest,
    receive_dir: &Path,
) {
    let _ = refresh_and_save_workspaces(config_path);
    if manifest.files.is_empty() {
        return;
    }
    let path = config_path.with_file_name("history.jsonl");
    let bytes: u64 = manifest.files.iter().map(|file| file.size).sum();
    let file_type = manifest_file_type(manifest);
    let (project_id, workspace_name, child_name, suppress_root) =
        receiver_history_scope(config_path, manifest);
    if let Some(root) = suppress_root.as_deref().or(Some(receive_dir)) {
        mark_incoming_sync_root(root);
    }
    if matches!(file_type, "session" | "mixed") {
        mark_incoming_session_roots(config_path);
    }
    let event_id = codebaton_discovery::new_pairing_request_id();
    let entry = serde_json::json!({
        "eventId": event_id,
        "timestamp": super::epoch_millis_now(),
        "projectId": project_id,
        "direction": "receive",
        "success": true,
        "files": manifest.files.len() as u32,
        "bytes": bytes,
        "detail": format!("received into {}", receive_dir.display()),
        "workspaceName": workspace_name,
        "childName": child_name,
        "trigger": "auto",
        "role": "receiver",
        "fileType": file_type,
    });
    let result = (|| -> std::io::Result<()> {
        append_json_line(&path, &entry)?;
        Ok(())
    })();
    match result {
        Ok(()) => app_log(
            "receiver_sync_history_recorded",
            &[
                ("project", project_id),
                ("event_id", event_id.clone()),
                ("file_count", manifest.files.len().to_string()),
                ("file_type", file_type.to_string()),
                ("path", path.display().to_string()),
            ],
        ),
        Err(error) => app_log(
            "receiver_sync_history_failed",
            &[
                ("project", project_id),
                ("event_id", event_id),
                ("path", path.display().to_string()),
                ("error", error.to_string()),
            ],
        ),
    }
}

pub(crate) fn receiver_history_scope(
    config_path: &Path,
    manifest: &SyncManifest,
) -> (String, Option<String>, Option<String>, Option<PathBuf>) {
    let Ok(config) = load_config(config_path) else {
        return ("incoming".to_string(), None, None, None);
    };

    for workspace in &config.workspaces {
        let root = workspace.effective_local_root();
        let matched = manifest.files.iter().any(|file| {
            safe_relative_path(&file.relative_path)
                .map(|rel| root.join(rel).exists())
                .unwrap_or(false)
        });
        if !matched {
            continue;
        }
        let child_names: HashSet<String> = manifest
            .files
            .iter()
            .filter_map(|file| Path::new(&file.relative_path).components().next())
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .filter(|name| workspace.children.iter().any(|child| child.name == *name))
            .collect();
        let child_name = (child_names.len() == 1)
            .then(|| child_names.iter().next().cloned())
            .flatten();
        let suppress_root = child_name
            .as_ref()
            .and_then(|name| workspace.children.iter().find(|child| child.name == *name))
            .map(|child| child.local_dir.clone())
            .unwrap_or_else(|| root.to_path_buf());
        let project_id = child_name.clone().unwrap_or_else(|| workspace.name.clone());
        return (
            project_id,
            Some(workspace.name.clone()),
            child_name,
            Some(suppress_root),
        );
    }

    for project in &config.projects {
        let matched = manifest.files.iter().any(|file| {
            safe_relative_path(&file.relative_path)
                .map(|rel| project.local.join(rel).exists())
                .unwrap_or(false)
        });
        if matched {
            return (
                project.name.clone(),
                None,
                None,
                Some(project.local.clone()),
            );
        }
    }

    ("incoming".to_string(), None, None, None)
}

pub(crate) fn record_auto_workspace_child_history(
    config_path: &Path,
    workspace: &WorkspaceConfig,
    success: bool,
    detail: Option<&str>,
    file_type: &str,
    child_file_counts: Option<&HashMap<String, u32>>,
) {
    for child in &workspace.children {
        if !child.enabled || child.conflicted {
            continue;
        }
        let files = match (success, child_file_counts) {
            (true, Some(counts)) => {
                let files = counts.get(&child.name).copied().unwrap_or(0);
                if files == 0 {
                    continue;
                }
                files
            }
            (true, None) => {
                let Ok(config) = load_config(config_path) else {
                    continue;
                };
                let summary = history_summary_from_config(
                    &config,
                    &child.name,
                    Some(&workspace.name),
                    Some(&child.name),
                    file_type,
                );
                let files = summary.file_paths.len() as u32;
                if files == 0 {
                    continue;
                }
                files
            }
            (false, _) => 0,
        };
        record_auto_sync_history(
            config_path,
            &child.name,
            success,
            files,
            detail.map(str::to_string),
            Some(&workspace.name),
            Some(&child.name),
            file_type,
        );
    }
}
