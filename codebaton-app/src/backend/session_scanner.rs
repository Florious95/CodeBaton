use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use codebaton_core::AisyncError;
use codebaton_sync::{load_config, SyncConfig};

use super::{
    app_log, auto_sync_gate_key, baseline_session_target, begin_auto_sync_bypass_cooldown,
    claude_mtime_paths, dedupe_mtime_targets, enqueue_workspace_first_propagation, finish_auto_sync,
    hash_prefix, incoming_sync_recent, latest_mtime_limited, local_codex_sessions_dir,
    record_auto_sync_history, record_auto_workspace_child_history, refresh_and_save_workspaces,
    refresh_interval_secs, refresh_workspace_children, run_pending_workspace_first_propagations,
    run_project_auto_sync, run_workspace_auto_sync_outcome, session_sync_key, session_target_key,
    sync_fingerprint_for_target, target_content_fingerprint, try_begin_auto_sync,
};

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
                let mut queue_first_propagation = false;
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
                            if child.enabled {
                                queue_first_propagation = true;
                                app_log(
                                    "workspace_child_auto_enabled",
                                    &[
                                        ("workspace", workspace.name.clone()),
                                        ("child", child.name.clone()),
                                    ],
                                );
                            }
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
                if queue_first_propagation {
                    enqueue_workspace_first_propagation(&next);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionMtimeDecision {
    BaselineNew,
    TriggerNew,
    TriggerModified,
    Unchanged,
}

pub(crate) fn classify_session_mtime(
    seen: &HashMap<String, SystemTime>,
    key: &str,
    mtime: SystemTime,
    scan_initialized: bool,
) -> SessionMtimeDecision {
    match seen.get(key) {
        Some(previous) if mtime > *previous => SessionMtimeDecision::TriggerModified,
        Some(_) => SessionMtimeDecision::Unchanged,
        None if scan_initialized => SessionMtimeDecision::TriggerNew,
        None => SessionMtimeDecision::BaselineNew,
    }
}

pub(crate) fn start_session_mtime_scanner(config_path: PathBuf, fallback_config: SyncConfig) {
    std::thread::spawn(move || {
        let mut seen = HashMap::<String, SystemTime>::new();
        let mut content_seen = HashMap::<String, String>::new();
        let mut sync_seen = HashMap::<String, String>::new();
        let mut scan_initialized = false;
        loop {
            let mut config = load_config(&config_path).unwrap_or_else(|_| fallback_config.clone());
            if let Some(refreshed_config) = refresh_and_save_workspaces(&config_path) {
                config = refreshed_config;
            }
            run_pending_workspace_first_propagations(&config_path, &config);
            let interval_secs = refresh_interval_secs(&config);
            let targets = session_mtime_targets(&config);
            app_log(
                "session_mtime_scan_started",
                &[
                    ("target_count", targets.len().to_string()),
                    ("interval_secs", interval_secs.to_string()),
                ],
            );

            let mut triggered = HashSet::new();
            let mut path_mtimes = HashMap::<PathBuf, Option<SystemTime>>::new();
            for target in targets {
                let scan_limit = if target.tool == "codex" { 32 } else { 256 };
                let mtime = if let Some(cached) = path_mtimes.get(&target.path) {
                    *cached
                } else {
                    let found = latest_mtime_limited(&target.path, scan_limit);
                    path_mtimes.insert(target.path.clone(), found);
                    found
                };
                let Some(mtime) = mtime else {
                    continue;
                };
                let key = session_target_key(&target);
                let sync_key = session_sync_key(&target);
                let decision = classify_session_mtime(&seen, &key, mtime, scan_initialized);
                let is_new_target = decision == SessionMtimeDecision::TriggerNew;
                match decision {
                    SessionMtimeDecision::BaselineNew => {
                        baseline_session_target(
                            &config_path,
                            &config,
                            &target,
                            mtime,
                            &mut seen,
                            &mut content_seen,
                            &mut sync_seen,
                            "new_target_baselined",
                        );
                        continue;
                    }
                    SessionMtimeDecision::TriggerNew => app_log(
                        "new_session_target_detected",
                        &[
                            ("scope", target.scope.to_string()),
                            ("name", target.name.clone()),
                            ("peer", target.peer.clone()),
                            ("tool", target.tool.to_string()),
                            ("path", target.path.display().to_string()),
                        ],
                    ),
                    SessionMtimeDecision::TriggerModified => {}
                    SessionMtimeDecision::Unchanged => {
                        if let Some(fingerprint) = target_content_fingerprint(&target) {
                            content_seen.insert(key.clone(), fingerprint);
                        }
                        continue;
                    }
                }
                seen.insert(key.clone(), mtime);
                let content_key = key.clone();
                let fingerprint = target_content_fingerprint(&target);
                if fingerprint.is_some() && content_seen.get(&content_key) == fingerprint.as_ref() {
                    app_log(
                        "auto_sync_skipped_no_change",
                        &[
                            ("scope", target.scope.to_string()),
                            ("name", target.name.clone()),
                            ("peer", target.peer.clone()),
                            ("tool", target.tool.to_string()),
                            ("trigger", "mtime".to_string()),
                        ],
                    );
                    continue;
                }
                if let Some(fingerprint) = fingerprint {
                    content_seen.insert(content_key, fingerprint);
                }
                if incoming_sync_recent(&target.path) {
                    app_log(
                        "auto_sync_suppressed",
                        &[
                            ("scope", target.scope.to_string()),
                            ("name", target.name.clone()),
                            ("peer", target.peer.clone()),
                            ("tool", target.tool.to_string()),
                            ("reason", "incoming_receive".to_string()),
                            ("trigger", "mtime".to_string()),
                        ],
                    );
                    continue;
                }

                let sync_fingerprint = sync_fingerprint_for_target(&config, &target);
                if sync_fingerprint.is_some()
                    && sync_seen.get(&sync_key) == sync_fingerprint.as_ref()
                {
                    let hash = sync_fingerprint
                        .as_ref()
                        .map(|fingerprint| hash_prefix(fingerprint))
                        .unwrap_or_default();
                    app_log(
                        "sync_fingerprint_gate_hit",
                        &[
                            ("scope", target.scope.to_string()),
                            ("name", target.name.clone()),
                            ("peer", target.peer.clone()),
                            ("tool", target.tool.to_string()),
                            ("trigger", "mtime".to_string()),
                            ("target_key", key.clone()),
                            ("hash", hash),
                        ],
                    );
                    continue;
                }
                if let Some(fingerprint) = &sync_fingerprint {
                    app_log(
                        "sync_fingerprint_gate_miss",
                        &[
                            ("scope", target.scope.to_string()),
                            ("name", target.name.clone()),
                            ("peer", target.peer.clone()),
                            ("tool", target.tool.to_string()),
                            ("trigger", "mtime".to_string()),
                            ("target_key", key.clone()),
                            ("hash", hash_prefix(fingerprint)),
                            (
                                "previous",
                                sync_seen
                                    .get(&sync_key)
                                    .map(|previous| hash_prefix(previous))
                                    .unwrap_or_default(),
                            ),
                        ],
                    );
                }

                app_log(
                    "session_mtime_changed",
                    &[
                        ("scope", target.scope.to_string()),
                        ("name", target.name.clone()),
                        ("peer", target.peer.clone()),
                        ("tool", target.tool.to_string()),
                        ("path", target.path.display().to_string()),
                    ],
                );

                let trigger_key = auto_sync_gate_key(target.scope, &target.name, &target.peer);
                if !triggered.insert(trigger_key) {
                    continue;
                }

                let gate_key = if is_new_target {
                    begin_auto_sync_bypass_cooldown(
                        target.scope,
                        &target.name,
                        &target.peer,
                        "mtime_new_target",
                    )
                } else {
                    try_begin_auto_sync(target.scope, &target.name, &target.peer, "mtime")
                };
                let Some(gate_key) = gate_key else {
                    continue;
                };
                app_log(
                    "session_incremental_sync_started",
                    &[
                        ("scope", target.scope.to_string()),
                        ("name", target.name.clone()),
                        ("peer", target.peer.clone()),
                        ("tool", target.tool.to_string()),
                    ],
                );
                let workspace_for_history = if target.scope == "workspace" {
                    config
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.name == target.name)
                        .cloned()
                } else {
                    None
                };
                let result = if target.scope == "workspace" {
                    config
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.name == target.name)
                        .cloned()
                        .ok_or_else(|| {
                            AisyncError::Config(format!(
                                "workspace '{}' not found for mtime sync",
                                target.name
                            ))
                        })
                        .and_then(|workspace| {
                            run_workspace_auto_sync_outcome(&config_path, &config, &workspace, None)
                                .map(|outcome| (outcome.report, Some(outcome.child_file_counts)))
                        })
                } else {
                    run_project_auto_sync(&config_path, &config, &target.name, &target.peer, None)
                        .map(|report| (report, None))
                };

                match result {
                    Ok((report, child_file_counts)) => {
                        let scan_limit = if target.tool == "codex" { 32 } else { 256 };
                        if let Some(post_mtime) = latest_mtime_limited(&target.path, scan_limit) {
                            let post_config =
                                load_config(&config_path).unwrap_or_else(|_| config.clone());
                            baseline_session_target(
                                &config_path,
                                &post_config,
                                &target,
                                post_mtime,
                                &mut seen,
                                &mut content_seen,
                                &mut sync_seen,
                                "baseline_updated",
                            );
                        }
                        let files = (report.code_files_transferred
                            + report.session_files_transferred)
                            as u32;
                        let workspace =
                            (target.scope == "workspace").then_some(target.name.as_str());
                        record_auto_sync_history(
                            &config_path,
                            &target.name,
                            true,
                            files,
                            None,
                            workspace,
                            None,
                            "session",
                        );
                        if let Some(workspace) = &workspace_for_history {
                            record_auto_workspace_child_history(
                                &config_path,
                                workspace,
                                true,
                                None,
                                "session",
                                child_file_counts.as_ref(),
                            );
                        }
                        app_log(
                            "session_incremental_sync_complete",
                            &[
                                ("scope", target.scope.to_string()),
                                ("name", target.name.clone()),
                                ("peer", target.peer.clone()),
                                ("file_count", files.to_string()),
                            ],
                        );
                    }
                    Err(error) => {
                        let detail = error.to_string();
                        let workspace =
                            (target.scope == "workspace").then_some(target.name.as_str());
                        record_auto_sync_history(
                            &config_path,
                            &target.name,
                            false,
                            0,
                            Some(detail.clone()),
                            workspace,
                            None,
                            "session",
                        );
                        if let Some(workspace) = &workspace_for_history {
                            record_auto_workspace_child_history(
                                &config_path,
                                workspace,
                                false,
                                Some(&detail),
                                "session",
                                None,
                            );
                        }
                        app_log(
                            "session_incremental_sync_failed",
                            &[
                                ("scope", target.scope.to_string()),
                                ("name", target.name.clone()),
                                ("peer", target.peer.clone()),
                                ("error", error.to_string()),
                            ],
                        );
                    }
                }
                finish_auto_sync(&gate_key);
            }

            scan_initialized = true;
            std::thread::sleep(Duration::from_secs(interval_secs));
        }
    });
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
