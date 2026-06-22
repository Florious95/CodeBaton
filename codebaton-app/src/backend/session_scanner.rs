use std::path::PathBuf;

use codebaton_sync::SyncConfig;

use super::{app_log, claude_mtime_paths, dedupe_mtime_targets, local_codex_sessions_dir,
    refresh_workspace_children};

pub(crate) fn refresh_workspaces_in_config(config: &SyncConfig) -> (SyncConfig, bool) {
    let mut changed = false;
    let mut refreshed = Vec::with_capacity(config.workspaces.len());
    for workspace in &config.workspaces {
        let peer = workspace.effective_peer().unwrap_or_default();
        let remote_root = workspace
            .effective_remote_root(peer)
            .unwrap_or_else(|| workspace.remote_root.clone());
        match refresh_workspace_children(workspace, &remote_root) {
            Ok(next) => {
                if next.children != workspace.children {
                    for child in &next.children {
                        if !workspace.children.iter().any(|old| old.name == child.name) {
                            app_log(
                                "workspace_new_child_detected",
                                &[
                                    ("workspace", workspace.name.clone()),
                                    ("child", child.name.clone()),
                                    ("local_dir", child.local_dir.display().to_string()),
                                    ("auto_enabled", child.enabled.to_string()),
                                ],
                            );
                        }
                    }
                    app_log(
                        "workspace_children_persisted",
                        &[
                            ("workspace", workspace.name.clone()),
                            ("child_count", next.children.len().to_string()),
                        ],
                    );
                    changed = true;
                }
                refreshed.push(next);
            }
            Err(error) => {
                app_log(
                    "workspace_children_refresh_failed",
                    &[
                        ("workspace", workspace.name.clone()),
                        ("error", error.to_string()),
                    ],
                );
                refreshed.push(workspace.clone());
            }
        }
    }

    if !changed {
        return (config.clone(), false);
    }

    let mut next = config.clone();
    next.workspaces = refreshed;
    (next, true)
}

#[derive(Clone)]
pub(crate) struct SessionMtimeTarget {
    pub(crate) scope: &'static str,
    pub(crate) name: String,
    pub(crate) peer: String,
    pub(crate) tool: &'static str,
    pub(crate) path: PathBuf,
}

pub(crate) fn session_mtime_targets(config: &SyncConfig) -> Vec<SessionMtimeTarget> {
    let mut targets = Vec::new();
    for project in &config.projects {
        let Some(peer) = project.peers.keys().next().cloned() else {
            continue;
        };
        for path in claude_mtime_paths(config, std::slice::from_ref(&project.local)) {
            targets.push(SessionMtimeTarget {
                scope: "project",
                name: project.name.clone(),
                peer: peer.clone(),
                tool: "claude",
                path,
            });
        }
        if let Some(path) = local_codex_sessions_dir() {
            targets.push(SessionMtimeTarget {
                scope: "project",
                name: project.name.clone(),
                peer,
                tool: "codex",
                path,
            });
        }
    }

    for workspace in &config.workspaces {
        let Some(peer) = workspace.effective_peer().map(str::to_string) else {
            continue;
        };
        let mut roots = vec![workspace.effective_local_root().to_path_buf()];
        roots.extend(
            workspace
                .children
                .iter()
                .map(|child| child.local_dir.clone()),
        );
        for path in claude_mtime_paths(config, &roots) {
            targets.push(SessionMtimeTarget {
                scope: "workspace",
                name: workspace.name.clone(),
                peer: peer.clone(),
                tool: "claude",
                path,
            });
        }
        if let Some(path) = local_codex_sessions_dir() {
            targets.push(SessionMtimeTarget {
                scope: "workspace",
                name: workspace.name.clone(),
                peer,
                tool: "codex",
                path,
            });
        }
    }

    dedupe_mtime_targets(targets)
}
