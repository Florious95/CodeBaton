use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use codebaton_core::{AisyncError, Result, RewriteDirection};
use codebaton_session::{ClaudeCodeParser, RuleBasedRewriter};
use codebaton_sync::SyncConfig;

use super::unix_nanos_now;
use super::{
    app_log, codex_session_child_name, codex_session_file_matches_project,
    codex_session_file_matches_workspace, collect_jsonl_files, directory_bytes,
    local_claude_projects_dir, local_codex_sessions_dir, path_rule_for_project,
    remote_claude_projects_dir, same_mapping_path, same_project_path, session_child_name,
    session_path_under,
};

pub(crate) fn increment_child_file_count(
    counts: &mut HashMap<String, u32>,
    child_name: &str,
    files: usize,
) {
    if files == 0 {
        return;
    }
    let entry = counts.entry(child_name.to_string()).or_insert(0);
    *entry = entry.saturating_add(files as u32);
}

pub(crate) struct SessionSyncPlan {
    pub(crate) staging_root: PathBuf,
    pub(crate) staged_project_dir: PathBuf,
    pub(crate) remote_project_dir: PathBuf,
    pub(crate) bytes: u64,
    pub(crate) rewritten_sessions: usize,
}

pub(crate) struct WorkspaceSessionSyncPlan {
    pub(crate) tool: &'static str,
    pub(crate) staging_root: PathBuf,
    pub(crate) remote_projects_dir: PathBuf,
    pub(crate) transfers: Vec<WorkspaceSessionTransfer>,
    pub(crate) child_file_counts: HashMap<String, u32>,
    pub(crate) bytes: u64,
    pub(crate) rewritten_sessions: usize,
}

pub(crate) struct WorkspaceSessionTransfer {
    pub(crate) staged_dir: PathBuf,
    pub(crate) remote_dir: PathBuf,
}

pub(crate) fn prepare_claude_session_sync(
    config_path: &Path,
    config: &SyncConfig,
    peer_name: &str,
    project: &codebaton_core::ProjectMapping,
) -> Result<Option<SessionSyncPlan>> {
    let Some(local_projects_dir) = local_claude_projects_dir(&project.local_session_dir) else {
        app_log(
            "session_scan_done",
            &[
                ("tool", "claude".to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                (
                    "local_session_dir",
                    project.local_session_dir.display().to_string(),
                ),
                ("count", "0".to_string()),
                ("reason", "session_dir_missing".to_string()),
            ],
        );
        return Ok(None);
    };

    // P0(round7-mem)：只进本项目对应的编码目录，避免把整棵 ~/.claude/projects
    // 全量读进内存后才过滤。Claude 按 cwd 编码会话目录，故本地项目的所有会话都落在
    // claude_project_dir_name(local_code_dir) 这一个编码目录下。下方 same_project_path
    // 仍作内容侧权威过滤，处理同一编码目录内多 cwd 碰撞的情况。
    let local_encoded_dir = claude_project_dir_name(&project.local_code_dir);
    let sessions = ClaudeCodeParser::parse_sessions_filtered(&local_projects_dir, |encoded| {
        encoded == local_encoded_dir
    })?;
    let mut sessions: Vec<_> = sessions
        .into_iter()
        .filter(|session| {
            same_project_path(
                &session.original_project_path,
                &project.local_code_dir,
                &project.original_source_path,
            )
        })
        .collect();

    app_log(
        "session_sync_started",
        &[
            ("tool", "claude".to_string()),
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            (
                "local_session_dir",
                local_projects_dir.display().to_string(),
            ),
            (
                "remote_dir",
                remote_claude_projects_dir(config, peer_name, project)
                    .display()
                    .to_string(),
            ),
            ("file_count", sessions.len().to_string()),
        ],
    );

    if sessions.is_empty() {
        app_log(
            "session_scan_done",
            &[
                ("tool", "claude".to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                (
                    "local_session_dir",
                    local_projects_dir.display().to_string(),
                ),
                ("count", "0".to_string()),
                ("reason", "no_matching_sessions".to_string()),
            ],
        );
        return Ok(None);
    }

    let staging_root =
        config_path.with_file_name(format!(".aisync-session-stage-{}", unix_nanos_now()));
    let staged_projects_dir = staging_root.join("projects");
    fs::create_dir_all(&staged_projects_dir)?;

    let rewriter = project_rewriter(config, peer_name, project)?;
    let target_encoded_dir = claude_project_dir_name(&project.remote_code_dir);
    let staged_project_dir = staged_projects_dir.join(&target_encoded_dir);
    let remote_project_dir =
        remote_claude_projects_dir(config, peer_name, project).join(&target_encoded_dir);
    let mut changed = 0usize;
    let mut unchanged = 0usize;
    let mut applied = 0usize;
    let mut skipped = 0usize;
    for session in &mut sessions {
        let report = ClaudeCodeParser::rewrite_structured_paths(
            session,
            &rewriter,
            RewriteDirection::SourceToTarget,
        );
        if report.applied.is_empty() {
            unchanged += 1;
        } else {
            changed += 1;
        }
        applied += report.applied.len();
        skipped += report.skipped.len();
        session.encoded_dir_name = target_encoded_dir.clone();
        ClaudeCodeParser::write_session(session, &staged_projects_dir)?;
    }

    let bytes = directory_bytes(&staged_project_dir)?;
    app_log(
        "session_rewrite_done",
        &[
            ("tool", "claude".to_string()),
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            ("changed", changed.to_string()),
            ("unchanged", unchanged.to_string()),
            ("applied", applied.to_string()),
            ("skipped", skipped.to_string()),
            ("target_dir", staged_project_dir.display().to_string()),
            ("bytes", bytes.to_string()),
        ],
    );

    Ok(Some(SessionSyncPlan {
        staging_root,
        staged_project_dir,
        remote_project_dir,
        bytes,
        rewritten_sessions: changed,
    }))
}

pub(crate) fn prepare_codex_session_sync(
    config_path: &Path,
    peer_name: &str,
    project: &codebaton_core::ProjectMapping,
) -> Result<Option<SessionSyncPlan>> {
    let Some(local_sessions_dir) = local_codex_sessions_dir() else {
        app_log(
            "session_scan_done",
            &[
                ("tool", "codex".to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                ("local_session_dir", "~/.codex/sessions".to_string()),
                ("count", "0".to_string()),
                ("reason", "session_dir_missing".to_string()),
            ],
        );
        return Ok(None);
    };

    let mut files = Vec::new();
    collect_jsonl_files(&local_sessions_dir, &mut files)?;
    let mut selected = Vec::new();
    for file in files {
        if codex_session_file_matches_project(&file, &project.local_code_dir) {
            selected.push(file);
        }
    }
    selected.sort();
    let remote_sessions_dir = PathBuf::from("~/.codex/sessions");
    app_log(
        "session_sync_started",
        &[
            ("tool", "codex".to_string()),
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            (
                "local_session_dir",
                local_sessions_dir.display().to_string(),
            ),
            ("remote_dir", remote_sessions_dir.display().to_string()),
            ("file_count", selected.len().to_string()),
        ],
    );
    if selected.is_empty() {
        app_log(
            "session_scan_done",
            &[
                ("tool", "codex".to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                (
                    "local_session_dir",
                    local_sessions_dir.display().to_string(),
                ),
                ("count", "0".to_string()),
                ("reason", "no_matching_sessions".to_string()),
            ],
        );
        return Ok(None);
    }

    let staging_root =
        config_path.with_file_name(format!(".aisync-codex-session-stage-{}", unix_nanos_now()));
    let staged_sessions_dir = staging_root.join("sessions");
    fs::create_dir_all(&staged_sessions_dir)?;
    for file in selected {
        let relative = file
            .strip_prefix(&local_sessions_dir)
            .map_err(|error| AisyncError::Session(error.to_string()))?;
        let target = staged_sessions_dir.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&file, &target)?;
    }

    let bytes = directory_bytes(&staged_sessions_dir)?;
    let file_count = count_files_recursive(&staged_sessions_dir);
    app_log(
        "session_rewrite_done",
        &[
            ("tool", "codex".to_string()),
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            ("changed", "0".to_string()),
            ("unchanged", file_count.to_string()),
            ("applied", "0".to_string()),
            ("skipped", "0".to_string()),
            ("target_dir", staged_sessions_dir.display().to_string()),
            ("bytes", bytes.to_string()),
        ],
    );

    Ok(Some(SessionSyncPlan {
        staging_root,
        staged_project_dir: staged_sessions_dir,
        remote_project_dir: remote_sessions_dir,
        bytes,
        rewritten_sessions: 0,
    }))
}

pub(crate) fn prepare_claude_workspace_session_sync(
    config_path: &Path,
    config: &SyncConfig,
    peer_name: &str,
    project: &codebaton_core::ProjectMapping,
    excluded_children: &HashSet<String>,
) -> Result<Option<WorkspaceSessionSyncPlan>> {
    let Some(local_projects_dir) = local_claude_projects_dir(&project.local_session_dir) else {
        app_log(
            "session_scan_done",
            &[
                ("tool", "claude".to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                (
                    "local_session_dir",
                    project.local_session_dir.display().to_string(),
                ),
                ("count", "0".to_string()),
                ("reason", "session_dir_missing".to_string()),
            ],
        );
        return Ok(None);
    };

    // P0(round7-mem)：workspace 的子项目会话落在不同编码目录，但都以 workspace 根的
    // 编码目录名为前缀（claude_project_dir_name 逐字符编码、长度不变，故
    // encode(root/sub) 必以 encode(root) 开头）。按前缀预过滤目录，避免全量解析；
    // 下方 session_path_under 仍作内容侧权威过滤。
    let local_encoded_prefix = claude_project_dir_name(&project.local_code_dir);
    let sessions = ClaudeCodeParser::parse_sessions_filtered(&local_projects_dir, |encoded| {
        encoded.starts_with(&local_encoded_prefix)
    })?;
    let mut sessions: Vec<_> = sessions
        .into_iter()
        .filter(|session| {
            session_path_under(&session.original_project_path, &project.local_code_dir)
                && !session_child_name(&session.original_project_path, &project.local_code_dir)
                    .as_ref()
                    .map(|name| excluded_children.contains(name))
                    .unwrap_or(false)
        })
        .collect();
    app_log(
        "session_sync_started",
        &[
            ("tool", "claude".to_string()),
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            (
                "local_session_dir",
                local_projects_dir.display().to_string(),
            ),
            (
                "remote_dir",
                remote_claude_projects_dir(config, peer_name, project)
                    .display()
                    .to_string(),
            ),
            ("file_count", sessions.len().to_string()),
        ],
    );
    if sessions.is_empty() {
        app_log(
            "session_scan_done",
            &[
                ("tool", "claude".to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                (
                    "local_session_dir",
                    local_projects_dir.display().to_string(),
                ),
                ("count", "0".to_string()),
                ("reason", "no_matching_sessions".to_string()),
            ],
        );
        return Ok(None);
    }

    let staging_root = config_path.with_file_name(format!(
        ".aisync-workspace-session-stage-{}",
        unix_nanos_now()
    ));
    let staged_projects_dir = staging_root.join("projects");
    fs::create_dir_all(&staged_projects_dir)?;
    let remote_projects_dir = remote_claude_projects_dir(config, peer_name, project);
    let rewriter = project_rewriter(config, peer_name, project)?;
    let mut changed = 0usize;
    let mut unchanged = 0usize;
    let mut applied = 0usize;
    let mut skipped = 0usize;
    let mut child_file_counts = HashMap::new();
    for session in &mut sessions {
        let report = ClaudeCodeParser::rewrite_structured_paths(
            session,
            &rewriter,
            RewriteDirection::SourceToTarget,
        );
        if report.applied.is_empty() {
            unchanged += 1;
        } else {
            changed += 1;
        }
        applied += report.applied.len();
        skipped += report.skipped.len();
        let child_name =
            session_child_name(&session.original_project_path, &project.local_code_dir);
        if let Some(child_name) = &child_name {
            increment_child_file_count(&mut child_file_counts, child_name, 1);
        }
        session.encoded_dir_name =
            claude_project_dir_name(Path::new(&session.original_project_path));
        ClaudeCodeParser::write_session(session, &staged_projects_dir)?;
    }

    let bytes = directory_bytes(&staged_projects_dir)?;
    let mut transfers = Vec::new();
    for entry in fs::read_dir(&staged_projects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        transfers.push(WorkspaceSessionTransfer {
            staged_dir: entry.path(),
            remote_dir: remote_projects_dir.join(name),
        });
    }
    transfers.sort_by(|left, right| left.staged_dir.cmp(&right.staged_dir));
    app_log(
        "session_rewrite_done",
        &[
            ("tool", "claude".to_string()),
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            ("changed", changed.to_string()),
            ("unchanged", unchanged.to_string()),
            ("applied", applied.to_string()),
            ("skipped", skipped.to_string()),
            ("target_dir", staged_projects_dir.display().to_string()),
            ("bytes", bytes.to_string()),
        ],
    );

    Ok(Some(WorkspaceSessionSyncPlan {
        tool: "claude",
        staging_root,
        remote_projects_dir,
        transfers,
        child_file_counts,
        bytes,
        rewritten_sessions: changed,
    }))
}

pub(crate) fn prepare_codex_workspace_session_sync(
    config_path: &Path,
    peer_name: &str,
    project: &codebaton_core::ProjectMapping,
    excluded_children: &HashSet<String>,
) -> Result<Option<WorkspaceSessionSyncPlan>> {
    let Some(local_sessions_dir) = local_codex_sessions_dir() else {
        app_log(
            "session_scan_done",
            &[
                ("tool", "codex".to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                ("local_session_dir", "~/.codex/sessions".to_string()),
                ("count", "0".to_string()),
                ("reason", "session_dir_missing".to_string()),
            ],
        );
        return Ok(None);
    };

    let mut files = Vec::new();
    collect_jsonl_files(&local_sessions_dir, &mut files)?;
    let mut selected = Vec::new();
    let mut child_file_counts = HashMap::new();
    for file in files {
        if !codex_session_file_matches_workspace(&file, &project.local_code_dir, excluded_children)
        {
            continue;
        }
        if let Some(child_name) =
            codex_session_child_name(&file, &project.local_code_dir, excluded_children)
        {
            increment_child_file_count(&mut child_file_counts, &child_name, 1);
        }
        selected.push(file);
    }
    selected.sort();
    let remote_sessions_dir = PathBuf::from("~/.codex/sessions");
    app_log(
        "session_sync_started",
        &[
            ("tool", "codex".to_string()),
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            (
                "local_session_dir",
                local_sessions_dir.display().to_string(),
            ),
            ("remote_dir", remote_sessions_dir.display().to_string()),
            ("file_count", selected.len().to_string()),
        ],
    );
    if selected.is_empty() {
        app_log(
            "session_scan_done",
            &[
                ("tool", "codex".to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                (
                    "local_session_dir",
                    local_sessions_dir.display().to_string(),
                ),
                ("count", "0".to_string()),
                ("reason", "no_matching_sessions".to_string()),
            ],
        );
        return Ok(None);
    }

    let staging_root =
        config_path.with_file_name(format!(".aisync-codex-session-stage-{}", unix_nanos_now()));
    let staged_sessions_dir = staging_root.join("sessions");
    fs::create_dir_all(&staged_sessions_dir)?;
    for file in selected {
        let relative = file
            .strip_prefix(&local_sessions_dir)
            .map_err(|error| AisyncError::Session(error.to_string()))?;
        let target = staged_sessions_dir.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&file, &target)?;
    }

    let bytes = directory_bytes(&staged_sessions_dir)?;
    let file_count = count_files_recursive(&staged_sessions_dir);
    app_log(
        "session_rewrite_done",
        &[
            ("tool", "codex".to_string()),
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            ("changed", "0".to_string()),
            ("unchanged", file_count.to_string()),
            ("applied", "0".to_string()),
            ("skipped", "0".to_string()),
            ("target_dir", staged_sessions_dir.display().to_string()),
            ("bytes", bytes.to_string()),
        ],
    );

    Ok(Some(WorkspaceSessionSyncPlan {
        tool: "codex",
        staging_root,
        remote_projects_dir: remote_sessions_dir.clone(),
        transfers: vec![WorkspaceSessionTransfer {
            staged_dir: staged_sessions_dir,
            remote_dir: remote_sessions_dir,
        }],
        child_file_counts,
        bytes,
        rewritten_sessions: 0,
    }))
}

pub(crate) fn count_files_recursive(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            count += count_files_recursive(&path);
        } else if file_type.is_file() {
            count += 1;
        }
    }
    count
}

pub(crate) fn project_rewriter(
    config: &SyncConfig,
    peer_name: &str,
    project: &codebaton_core::ProjectMapping,
) -> Result<RuleBasedRewriter> {
    let source_device_id = config.device.id;
    let target_device_id = config
        .peers
        .get(peer_name)
        .map(|peer| peer.id)
        .ok_or_else(|| AisyncError::Config(format!("peer '{peer_name}' not found")))?;
    let same_device = source_device_id == target_device_id;
    let same_path = same_mapping_path(&project.local_code_dir, &project.remote_code_dir);
    app_log(
        "circular_mapping_check",
        &[
            ("source_device_id", source_device_id.0.to_string()),
            ("target_device_id", target_device_id.0.to_string()),
            ("source_path", project.local_code_dir.display().to_string()),
            ("target_path", project.remote_code_dir.display().to_string()),
            ("same_device", same_device.to_string()),
            ("same_path", same_path.to_string()),
        ],
    );

    if same_path {
        if same_device {
            return Err(AisyncError::PathRewrite(format!(
                "circular mapping: source equals target ({})",
                project.local_code_dir.display()
            )));
        }
        return RuleBasedRewriter::new(Vec::new());
    }

    RuleBasedRewriter::new(vec![path_rule_for_project(project)])
}

pub(crate) fn claude_project_dir_name(path: &Path) -> String {
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
