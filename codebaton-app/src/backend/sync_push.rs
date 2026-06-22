use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use codebaton_core::{AisyncError, Direction, Result};
use codebaton_discovery::PeerConnectionInfo;
use codebaton_sync::{load_config, save_config, SyncConfig, SyncReport, WorkspaceConfig};
use codebaton_transport::{generate_tls_identity, TcpTransporter, TlsConfig};

use super::{app_log, peer_transport_connection, AiToolKind};
use super::{child_manifest, manifest_fingerprint};
use super::{count_files_recursive, increment_child_file_count, WorkspaceSyncOutcome};
use super::{refresh_workspace_children, workspace_project_mapping};

pub(crate) fn run_tcp_push(
    config_path: &Path,
    config: &SyncConfig,
    peer_name: &str,
    project: &codebaton_core::ProjectMapping,
    live_connection: Option<PeerConnectionInfo>,
    confirm_overwrite: bool,
) -> Result<SyncReport> {
    let connection = peer_transport_connection(config_path, config, peer_name, live_connection)?;
    app_log(
        "transport_peer_connection_selected",
        &[
            ("project", project.project_id.clone()),
            ("peer", peer_name.to_string()),
            ("endpoint", connection.endpoint.to_string()),
            ("cert_source", connection.cert_source.clone()),
        ],
    );
    let source = project.local_code_dir.clone();
    let remote_code_dir = project.remote_code_dir.clone();
    let mut session_plans = Vec::new();
    for tool in AiToolKind::all() {
        if let Some(plan) = tool.prepare_project(config_path, config, peer_name, project)? {
            session_plans.push((tool.name(), plan));
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| AisyncError::Transport(format!("tokio runtime: {error}")))?;
    let (code_manifest, session_file_counts) = runtime.block_on(async {
        let identity = generate_tls_identity("aisync-client")?;
        let tls = TlsConfig::new(identity, connection.server_name.clone())
            .with_pinned_peer_cert(connection.receiver_cert_der.clone());
        let mut transporter =
            TcpTransporter::connect_to_peer(&connection.peer, connection.endpoint.port(), &tls)
                .await?
                .with_confirm_overwrite(confirm_overwrite);
        let code_manifest = transporter
            .sync_directory_to(&source, Some(&remote_code_dir), None)
            .await?;
        // 发 close_notify 再断，避免对端下次读到「without close_notify」错误。
        transporter.shutdown().await;

        let mut session_file_counts = Vec::new();
        for (_, plan) in &session_plans {
            let identity = generate_tls_identity("aisync-client")?;
            let tls = TlsConfig::new(identity, connection.server_name.clone())
                .with_pinned_peer_cert(connection.receiver_cert_der.clone());
            let mut transporter =
                TcpTransporter::connect_to_peer(&connection.peer, connection.endpoint.port(), &tls)
                    .await?;
            let manifest = transporter
                .sync_directory_to(
                    &plan.staged_project_dir,
                    Some(&plan.remote_project_dir),
                    None,
                )
                .await?;
            transporter.shutdown().await;
            session_file_counts.push(manifest.files.len());
        }

        Ok::<_, AisyncError>((code_manifest, session_file_counts))
    })?;

    let session_files: usize = session_file_counts.iter().sum();
    let rewritten_sessions: usize = session_plans
        .iter()
        .map(|(_, plan)| plan.rewritten_sessions)
        .sum();
    for ((tool, plan), file_count) in session_plans.iter().zip(session_file_counts.iter()) {
        app_log(
            "session_files_transferred",
            &[
                ("tool", (*tool).to_string()),
                ("project", project.project_id.clone()),
                ("peer", peer_name.to_string()),
                ("remote_dir", plan.remote_project_dir.display().to_string()),
                ("file_count", file_count.to_string()),
                ("bytes", plan.bytes.to_string()),
            ],
        );
    }
    for (_, plan) in session_plans {
        let _ = fs::remove_dir_all(plan.staging_root);
    }

    // 快照：一次成功推送后，对端 code 目录内容 == 本端源内容，故两端指纹相同。
    // 持久化供下次推送做脑裂检测（对端当前指纹 vs 此处存的 peer_last_known_hash）。
    let synced_hash = codebaton_transport::manifest_hash(&code_manifest);
    if let Ok(mut persisted) = load_config(config_path) {
        persisted.set_sync_snapshot(
            &project.project_id,
            peer_name,
            codebaton_sync::SyncSnapshot {
                peer_last_known_hash: synced_hash.clone(),
                self_last_synced_hash: synced_hash.clone(),
            },
        );
        if let Err(error) = save_config(config_path, &persisted) {
            app_log(
                "sync_snapshot_persist_failed",
                &[
                    ("project", project.project_id.clone()),
                    ("peer", peer_name.to_string()),
                    ("error", error.to_string()),
                ],
            );
        } else {
            app_log(
                "sync_snapshot_persisted",
                &[
                    ("project", project.project_id.clone()),
                    ("peer", peer_name.to_string()),
                    ("hash", synced_hash),
                ],
            );
        }
    }

    Ok(SyncReport {
        project_id: project.project_id.clone(),
        peer_id: connection.peer.id,
        direction: Direction::LocalToRemote,
        code_files_transferred: code_manifest.files.len(),
        session_files_transferred: session_files,
        deleted_files: 0,
        rewritten_sessions,
        local_version: 0,
        remote_version: 0,
        stages: vec![
            codebaton_sync::SyncStage {
                name: "connect",
                percent: 5,
                current_file: None,
            },
            codebaton_sync::SyncStage {
                name: "transfer_session",
                percent: 90,
                current_file: None,
            },
            codebaton_sync::SyncStage {
                name: "sync_complete",
                percent: 100,
                current_file: None,
            },
        ],
    })
}

pub(crate) fn run_workspace_tcp_push(
    config_path: &Path,
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
    live_connection: Option<PeerConnectionInfo>,
) -> Result<WorkspaceSyncOutcome> {
    let peer_name = workspace.effective_peer().ok_or_else(|| {
        AisyncError::Config(format!("workspace '{}' has no peer", workspace.name))
    })?;
    let remote_root = workspace.effective_remote_root(peer_name).ok_or_else(|| {
        AisyncError::Config(format!(
            "workspace '{}' has no remote root for peer '{}'",
            workspace.name, peer_name
        ))
    })?;
    let connection = peer_transport_connection(config_path, config, peer_name, live_connection)?;
    let peer_id = config
        .peers
        .get(peer_name)
        .map(|peer| peer.id)
        .ok_or_else(|| AisyncError::Config(format!("peer '{peer_name}' not found")))?;
    app_log(
        "transport_peer_connection_selected",
        &[
            ("peer", peer_name.to_string()),
            ("endpoint", connection.endpoint.to_string()),
            ("cert_source", connection.cert_source.clone()),
        ],
    );
    let previous_children: HashSet<String> = workspace
        .children
        .iter()
        .map(|child| child.name.clone())
        .collect();
    let previous_child_fingerprints: HashMap<String, String> = workspace
        .children
        .iter()
        .filter_map(|child| {
            child
                .last_fingerprint
                .as_ref()
                .map(|fingerprint| (child.name.clone(), fingerprint.clone()))
        })
        .collect();
    let workspace = refresh_workspace_children(workspace, &remote_root)?;
    for child in &workspace.children {
        if !previous_children.contains(&child.name) {
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
    let source = workspace.effective_local_root().to_path_buf();

    app_log(
        "workspace_sync_started",
        &[
            ("workspace", workspace.name.clone()),
            ("peer", peer_name.to_string()),
            ("local_root", source.display().to_string()),
            ("remote_root", remote_root.display().to_string()),
            ("child_count", workspace.children.len().to_string()),
        ],
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| AisyncError::Transport(format!("tokio runtime: {error}")))?;
    // 一次性交接：直推所有 child，不做脑裂/冲突分析；传输层的「覆盖前备份」是安全网。
    let source_manifest = runtime.block_on(async {
        let identity = generate_tls_identity("aisync-client")?;
        let tls = TlsConfig::new(identity, connection.server_name.clone())
            .with_pinned_peer_cert(connection.receiver_cert_der.clone());
        let mut transporter =
            TcpTransporter::connect_to_peer(&connection.peer, connection.endpoint.port(), &tls)
                .await?;
        let manifest = transporter
            .sync_directory_to(&source, Some(&remote_root), None)
            .await;
        transporter.shutdown().await;
        manifest
    })?;

    let code_files = source_manifest.files.len();
    let mut workspace = workspace;
    let mut child_file_counts = HashMap::new();
    for child in &mut workspace.children {
        if !child.enabled {
            continue;
        }
        let local = child_manifest(&source_manifest, &child.name);
        let local_fingerprint = manifest_fingerprint(&local);
        if previous_child_fingerprints.get(&child.name) != Some(&local_fingerprint) {
            increment_child_file_count(&mut child_file_counts, &child.name, local.files.len());
        }
        child.last_fingerprint = Some(local_fingerprint);
    }

    let empty_children: Vec<_> = workspace
        .children
        .iter()
        .filter(|child| child.enabled)
        .filter(|child| count_files_recursive(&child.local_dir) == 0)
        .cloned()
        .collect();
    if !empty_children.is_empty() {
        runtime.block_on(async {
            for child in &empty_children {
                let identity = generate_tls_identity("aisync-client")?;
                let tls = TlsConfig::new(identity, connection.server_name.clone())
                    .with_pinned_peer_cert(connection.receiver_cert_der.clone());
                let mut transporter = TcpTransporter::connect_to_peer(
                    &connection.peer,
                    connection.endpoint.port(),
                    &tls,
                )
                .await?;
                let manifest = transporter
                    .sync_directory_to(&child.local_dir, Some(&child.remote_dir), None)
                    .await?;
                transporter.shutdown().await;
                app_log(
                    "workspace_empty_child_dir_transferred",
                    &[
                        ("workspace", workspace.name.clone()),
                        ("child", child.name.clone()),
                        ("remote_dir", child.remote_dir.display().to_string()),
                        ("file_count", manifest.files.len().to_string()),
                    ],
                );
            }
            Ok::<_, AisyncError>(())
        })?;
    }

    let project = workspace_project_mapping(config, &workspace, peer_name, &remote_root)?;
    let no_conflicts: HashSet<String> = HashSet::new();
    let mut session_plans = Vec::new();
    for tool in AiToolKind::all() {
        if let Some(plan) =
            tool.prepare_workspace(config_path, config, peer_name, &project, &no_conflicts)?
        {
            session_plans.push(plan);
        }
    }

    let session_file_counts = runtime.block_on(async {
        let mut counts = Vec::new();
        for plan in &session_plans {
            let mut plan_files = 0usize;
            for transfer in &plan.transfers {
                let identity = generate_tls_identity("aisync-client")?;
                let tls = TlsConfig::new(identity, connection.server_name.clone())
                    .with_pinned_peer_cert(connection.receiver_cert_der.clone());
                let mut transporter = TcpTransporter::connect_to_peer(
                    &connection.peer,
                    connection.endpoint.port(),
                    &tls,
                )
                .await?;
                let manifest = transporter
                    .sync_directory_to(&transfer.staged_dir, Some(&transfer.remote_dir), None)
                    .await?;
                transporter.shutdown().await;
                plan_files += manifest.files.len();
            }
            for (child_name, files) in &plan.child_file_counts {
                increment_child_file_count(&mut child_file_counts, child_name, *files as usize);
            }
            counts.push(plan_files);
        }

        Ok::<_, AisyncError>(counts)
    })?;

    let session_files: usize = session_file_counts.iter().sum();
    let rewritten_sessions: usize = session_plans
        .iter()
        .map(|plan| plan.rewritten_sessions)
        .sum();
    for (plan, file_count) in session_plans.iter().zip(session_file_counts.iter()) {
        app_log(
            "session_files_transferred",
            &[
                ("tool", plan.tool.to_string()),
                ("project", workspace.name.clone()),
                ("peer", peer_name.to_string()),
                ("remote_dir", plan.remote_projects_dir.display().to_string()),
                ("file_count", file_count.to_string()),
                ("bytes", plan.bytes.to_string()),
            ],
        );
    }
    for plan in session_plans {
        let _ = fs::remove_dir_all(plan.staging_root);
    }

    app_log(
        "workspace_sync_complete",
        &[
            ("workspace", workspace.name.clone()),
            ("peer", peer_name.to_string()),
            ("remote_root", remote_root.display().to_string()),
            ("file_count", code_files.to_string()),
            ("session_files", session_files.to_string()),
        ],
    );

    Ok(WorkspaceSyncOutcome {
        report: SyncReport {
            project_id: workspace.name.clone(),
            peer_id,
            direction: Direction::LocalToRemote,
            code_files_transferred: code_files,
            session_files_transferred: session_files,
            deleted_files: 0,
            rewritten_sessions,
            local_version: 0,
            remote_version: 0,
            stages: vec![
                codebaton_sync::SyncStage {
                    name: "connect",
                    percent: 5,
                    current_file: None,
                },
                codebaton_sync::SyncStage {
                    name: "transfer_workspace",
                    percent: 70,
                    current_file: None,
                },
                codebaton_sync::SyncStage {
                    name: "transfer_session",
                    percent: 90,
                    current_file: None,
                },
                codebaton_sync::SyncStage {
                    name: "sync_complete",
                    percent: 100,
                    current_file: None,
                },
            ],
        },
        workspace,
        child_file_counts,
    })
}
