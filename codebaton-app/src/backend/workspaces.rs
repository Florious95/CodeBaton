use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use codebaton_core::{AisyncError, Result};
use codebaton_sync::{load_config, save_config, DiscoveredProject, SyncModeConfig};
use codebaton_transport::{
    generate_tls_identity, TlsConfig, WorkspaceMappingAckPayload, WorkspaceMappingRequestPayload,
};

use super::auto_sync_gate::{finish_auto_sync, try_begin_auto_sync};
use super::auto_sync_orchestration::run_workspace_auto_sync_outcome;
use super::claude_paths::first_level_dir_names;
use super::history::{record_auto_sync_history, record_auto_workspace_child_history};
use super::peers::persist_peer_connection;
use super::transport::{
    advertised_local_endpoint, send_workspace_mapping_ack, send_workspace_mapping_request,
    workspace_mapping_ack_connection,
};
use super::{
    app_log, control_connection_for_peer, live_connection_for_config_peer,
    seed_session_baselines_for_workspace, start_workspace_watcher, sync_mode_from_label,
    sync_mode_label, with_endpoint_first, workspace_children, workspace_config,
    workspace_config_with_child_names, replace_workspace, Backend,
};

impl Backend {
    /// Scan a workspace root for first-level child projects (D2/D11).
    /// Scan an actual directory path for first-level child projects, WITHOUT
    /// requiring a configured workspace. Used by the "添加工作区" dialog where
    /// the workspace doesn't exist in config yet (the previous name-lookup
    /// version always returned empty there — BUG 248-250). `remote_root` is the
    /// peer's root used only to compute matched_remote display hints; an empty
    /// or nonexistent remote root just yields no matches.
    pub fn scan_workspace_path(
        &self,
        local_root: &Path,
        remote_root: &Path,
    ) -> Result<Vec<DiscoveredProject>> {
        if !local_root.is_dir() {
            return Err(AisyncError::Config(format!(
                "local root is not a directory: {}",
                local_root.display()
            )));
        }
        let remote_names = first_level_dir_names(remote_root).unwrap_or_default();
        let mut projects = Vec::new();
        for entry in fs::read_dir(local_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip dotfolders (.git etc) — they're not project subdirs.
            if name.starts_with('.') {
                continue;
            }
            projects.push(DiscoveredProject {
                name: name.clone(),
                local_code_dir: entry.path(),
                remote_code_dir: remote_root.join(&name),
                enabled: remote_names.contains(&name),
                matched_remote: remote_names.contains(&name),
            });
        }
        projects.sort_by(|left, right| left.name.cmp(&right.name));
        let names: Vec<String> = projects.iter().map(|p| p.name.clone()).collect();
        app_log(
            "workspace_scan_done",
            &[
                ("root", local_root.display().to_string()),
                ("count", projects.len().to_string()),
                ("children", format!("[{}]", names.join(","))),
            ],
        );
        Ok(projects)
    }

    pub fn add_workspace(
        &self,
        name: String,
        local_root: PathBuf,
        peer_name: String,
        remote_root: PathBuf,
        mode: SyncModeConfig,
        auto_enable_new: bool,
    ) -> Result<String> {
        if !local_root.is_dir() {
            return Err(AisyncError::Config(format!(
                "workspace local root is not a directory: {}",
                local_root.display()
            )));
        }
        let name = if name.trim().is_empty() {
            local_root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".to_string())
        } else {
            name
        };

        let (request_id, endpoint, tls, request, target_peer_name) = {
            let mut g = self.inner.lock().unwrap();
            if g.config
                .workspaces
                .iter()
                .any(|workspace| workspace.name == name)
            {
                return Err(AisyncError::Config(format!(
                    "workspace '{name}' already exists"
                )));
            }
            if !g.config.peers.contains_key(&peer_name) {
                return Err(AisyncError::Config(format!("peer '{peer_name}' not found")));
            }
            let (endpoint, tls) = control_connection_for_peer(&g, &peer_name)?;
            let local_device = g.discoverer.local_device().clone();
            let serve = g
                .serve
                .clone()
                .ok_or_else(|| AisyncError::Config("local receiver is not running".to_string()))?;
            let local_endpoint = advertised_local_endpoint(&local_device, &serve, endpoint)?;
            let receiver_cert_der = fs::read(&serve.cert_path).map_err(|error| {
                AisyncError::Transport(format!(
                    "local receiver certificate not found at {}: {}",
                    serve.cert_path.display(),
                    error
                ))
            })?;
            let children: Vec<String> =
                workspace_children(&local_root, &remote_root, auto_enable_new)?
                    .into_iter()
                    .map(|child| child.name)
                    .collect();
            let request_id = codebaton_discovery::new_pairing_request_id();
            let request = WorkspaceMappingRequestPayload {
                request_id: request_id.clone(),
                workspace_name: name.clone(),
                source_root: local_root.clone(),
                suggested_remote_root: remote_root,
                mode: sync_mode_label(mode).to_string(),
                auto_enable_new,
                children,
                device: with_endpoint_first(local_device, Some(local_endpoint)),
                endpoint: Some(local_endpoint),
                receiver_cert_der: Some(receiver_cert_der),
                server_name: Some("aisync-receiver".to_string()),
            };
            let target_peer_name = peer_name.clone();
            g.outbound_workspace_mappings.insert(
                request_id.clone(),
                super::OutboundWorkspaceMapping {
                    workspace_name: name,
                    local_root,
                    peer_name,
                    mode,
                    auto_enable_new,
                },
            );
            (request_id, endpoint, tls, request, target_peer_name)
        };

        if let Err(error) = send_workspace_mapping_request(endpoint, tls, request.clone()) {
            let mut g = self.inner.lock().unwrap();
            g.outbound_workspace_mappings.remove(&request_id);
            return Err(error);
        }
        app_log(
            "workspace_request_sent",
            &[
                ("request_id", request_id.clone()),
                ("workspace", request.workspace_name.clone()),
                ("peer", target_peer_name),
                ("local_root", request.source_root.display().to_string()),
                (
                    "remote_root",
                    request.suggested_remote_root.display().to_string(),
                ),
            ],
        );
        Ok(request_id)
    }

    pub fn take_pending_workspace_mapping_request(&self) -> Option<WorkspaceMappingRequestPayload> {
        let request = self
            .pending_workspace_mapping_requests
            .lock()
            .unwrap()
            .pop_front()?;
        let mut g = self.inner.lock().unwrap();
        g.workspace_mapping_requests
            .insert(request.request_id.clone(), request.clone());
        app_log(
            "workspace_request_ready",
            &[
                ("request_id", request.request_id.clone()),
                ("workspace", request.workspace_name.clone()),
                ("peer", request.device.name.clone()),
                ("source_root", request.source_root.display().to_string()),
                (
                    "suggested_remote_root",
                    request.suggested_remote_root.display().to_string(),
                ),
            ],
        );
        Some(request)
    }

    pub fn confirm_workspace_mapping_request(
        &self,
        request_id: &str,
        local_root: PathBuf,
    ) -> Result<()> {
        let (endpoint, tls, ack, candidate, config_path, workspace, peer_name) = {
            let g = self.inner.lock().unwrap();
            let request = g
                .workspace_mapping_requests
                .get(request_id)
                .cloned()
                .ok_or_else(|| {
                    AisyncError::Config(format!(
                        "workspace mapping request '{request_id}' not found"
                    ))
                })?;
            if !local_root.exists() {
                fs::create_dir_all(&local_root).map_err(|error| {
                    AisyncError::Config(format!(
                        "failed to create workspace root {}: {error}",
                        local_root.display()
                    ))
                })?;
                app_log(
                    "workspace_remote_dir_created",
                    &[("path", local_root.display().to_string())],
                );
            }
            let live_connection = g
                .discoverer
                .peer_connection_info(&request.device.id)
                .ok()
                .flatten();
            let ack_connection = workspace_mapping_ack_connection(live_connection, &request)?;
            let identity = generate_tls_identity("aisync-client")?;
            let tls = TlsConfig::new(identity, ack_connection.server_name.clone())
                .with_pinned_peer_cert(ack_connection.receiver_cert_der);
            let peer_name = request.device.name.clone();
            let mut candidate = g.config.clone();
            persist_peer_connection(
                &mut candidate,
                &g.config_path,
                request.device.clone(),
                request.endpoint,
                request.receiver_cert_der.as_deref(),
                request.server_name.clone(),
            )?;
            let workspace = workspace_config_with_child_names(
                request.workspace_name.clone(),
                local_root.clone(),
                peer_name.clone(),
                request.source_root.clone(),
                sync_mode_from_label(&request.mode),
                request.auto_enable_new,
                &request.children,
            );
            replace_workspace(&mut candidate, workspace.clone());
            let local_device = g.discoverer.local_device().clone();
            let local_endpoint = g.serve.as_ref().and_then(|serve| {
                local_device
                    .addresses
                    .first()
                    .map(|ip| SocketAddr::new(*ip, serve.port))
            });
            let ack = WorkspaceMappingAckPayload {
                request_id: request.request_id,
                accepted: true,
                workspace_name: request.workspace_name,
                remote_root: Some(local_root.clone()),
                message: None,
                device: with_endpoint_first(local_device, local_endpoint),
            };
            app_log(
                "workspace_confirm_prepared",
                &[
                    ("request_id", request_id.to_string()),
                    ("endpoint", ack_connection.endpoint.to_string()),
                    ("cert_source", ack_connection.cert_source.clone()),
                    ("server_name", ack_connection.server_name.clone()),
                ],
            );
            (
                ack_connection.endpoint,
                tls,
                ack,
                candidate,
                g.config_path.clone(),
                workspace,
                peer_name,
            )
        };
        send_workspace_mapping_ack(endpoint, tls, ack.clone())?;
        {
            let mut g = self.inner.lock().unwrap();
            save_config(&config_path, &candidate)?;
            g.config = candidate.clone();
            g.workspace_mapping_requests.remove(request_id);
            g.workspace_watchers.remove(&workspace.name);
            if let Some(watcher) = start_workspace_watcher(&config_path, &candidate, &workspace) {
                g.workspace_watchers.insert(workspace.name.clone(), watcher);
            }
        }
        app_log(
            "workspace_entity_created",
            &[
                ("workspace", workspace.name.clone()),
                ("peer", peer_name.clone()),
                ("local_root", workspace.local_root.display().to_string()),
                ("remote_root", workspace.remote_root.display().to_string()),
                ("children", workspace.children.len().to_string()),
                ("side", "receiver".to_string()),
            ],
        );
        app_log(
            "workspace_confirmed",
            &[
                ("request_id", request_id.to_string()),
                ("workspace", ack.workspace_name.clone()),
                ("peer", peer_name),
                (
                    "remote_root",
                    ack.remote_root
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                ),
            ],
        );
        app_log(
            "workspace_saved",
            &[
                ("workspace", ack.workspace_name),
                ("request_id", request_id.to_string()),
                ("side", "receiver".to_string()),
            ],
        );
        Ok(())
    }

    pub fn process_workspace_mapping_acks(&self) -> Result<usize> {
        let mut processed = 0;
        loop {
            let ack = self
                .pending_workspace_mapping_acks
                .lock()
                .unwrap()
                .pop_front();
            let Some(ack) = ack else {
                return Ok(processed);
            };
            let (candidate, config_path, workspace, peer_name) = {
                let mut g = self.inner.lock().unwrap();
                let Some(outbound) = g.outbound_workspace_mappings.remove(&ack.request_id) else {
                    continue;
                };
                if !ack.accepted {
                    return Err(AisyncError::Config(ack.message.unwrap_or_else(|| {
                        "workspace mapping request rejected".to_string()
                    })));
                }
                let remote_root = ack.remote_root.clone().ok_or_else(|| {
                    AisyncError::Config(
                        "workspace mapping ack did not include remote_root".to_string(),
                    )
                })?;
                let workspace = workspace_config(
                    outbound.workspace_name.clone(),
                    outbound.local_root.clone(),
                    outbound.peer_name.clone(),
                    remote_root.clone(),
                    outbound.mode,
                    outbound.auto_enable_new,
                )?;
                let mut candidate = g.config.clone();
                replace_workspace(&mut candidate, workspace.clone());
                let config_path = g.config_path.clone();
                save_config(&config_path, &candidate)?;
                g.config = candidate.clone();
                g.workspace_watchers.remove(&workspace.name);
                if let Some(watcher) = start_workspace_watcher(&config_path, &candidate, &workspace)
                {
                    g.workspace_watchers.insert(workspace.name.clone(), watcher);
                }
                processed += 1;
                app_log(
                    "workspace_ack_applied",
                    &[
                        ("request_id", ack.request_id.clone()),
                        ("workspace", outbound.workspace_name.clone()),
                        ("peer", outbound.peer_name.clone()),
                        ("remote_root", remote_root.display().to_string()),
                    ],
                );
                (candidate, config_path, workspace, outbound.peer_name)
            };
            app_log(
                "workspace_saved",
                &[
                    ("workspace", workspace.name.clone()),
                    ("peer", peer_name.clone()),
                    ("local_root", workspace.local_root.display().to_string()),
                    ("remote_root", workspace.remote_root.display().to_string()),
                    ("children", workspace.children.len().to_string()),
                    ("side", "requester".to_string()),
                ],
            );
            app_log(
                "workspace_initial_sync_started",
                &[
                    ("workspace", workspace.name.clone()),
                    ("peer", peer_name.clone()),
                    ("local_root", workspace.local_root.display().to_string()),
                    ("remote_root", workspace.remote_root.display().to_string()),
                ],
            );
            let live_connection = {
                let g = self.inner.lock().unwrap();
                live_connection_for_config_peer(&g, &peer_name)
            };
            let initial_gate =
                try_begin_auto_sync("workspace", &workspace.name, &peer_name, "initial_sync");
            if initial_gate.is_none() {
                app_log(
                    "workspace_initial_sync_suppressed",
                    &[
                        ("workspace", workspace.name.clone()),
                        ("peer", peer_name.clone()),
                        ("reason", "coalesced".to_string()),
                    ],
                );
                continue;
            }
            let initial_gate = initial_gate.unwrap();
            match run_workspace_auto_sync_outcome(
                &config_path,
                &candidate,
                &workspace,
                live_connection,
            ) {
                Ok(outcome) => {
                    let post_config =
                        load_config(&config_path).unwrap_or_else(|_| candidate.clone());
                    seed_session_baselines_for_workspace(
                        &config_path,
                        &post_config,
                        &workspace.name,
                        &peer_name,
                    );
                    let files = (outcome.report.code_files_transferred
                        + outcome.report.session_files_transferred)
                        as u32;
                    record_auto_sync_history(
                        &config_path,
                        &workspace.name,
                        true,
                        files,
                        None,
                        Some(&workspace.name),
                        None,
                        "mixed",
                    );
                    record_auto_workspace_child_history(
                        &config_path,
                        &outcome.workspace,
                        true,
                        None,
                        "mixed",
                        Some(&outcome.child_file_counts),
                    );
                    app_log(
                        "workspace_initial_sync_complete",
                        &[
                            ("workspace", workspace.name.clone()),
                            ("peer", peer_name.clone()),
                            ("file_count", files.to_string()),
                        ],
                    );
                }
                Err(error) => {
                    let detail = error.to_string();
                    record_auto_sync_history(
                        &config_path,
                        &workspace.name,
                        false,
                        0,
                        Some(detail.clone()),
                        Some(&workspace.name),
                        None,
                        "mixed",
                    );
                    record_auto_workspace_child_history(
                        &config_path,
                        &workspace,
                        false,
                        Some(&detail),
                        "mixed",
                        None,
                    );
                    app_log(
                        "workspace_initial_sync_failed",
                        &[
                            ("workspace", workspace.name.clone()),
                            ("peer", peer_name.clone()),
                            ("error", detail),
                        ],
                    );
                    finish_auto_sync(&initial_gate);
                    return Err(error);
                }
            }
            finish_auto_sync(&initial_gate);
            app_log(
                "workspace_saved",
                &[
                    ("workspace", workspace.name),
                    ("peer", peer_name),
                    ("request_id", ack.request_id),
                    ("side", "requester".to_string()),
                ],
            );
        }
    }
}
