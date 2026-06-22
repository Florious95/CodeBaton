use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use codebaton_core::{AisyncError, Result};
use codebaton_sync::{save_config, SyncModeConfig};
use codebaton_transport::{
    generate_tls_identity, ProjectMappingAckPayload, ProjectMappingRequestPayload, TlsConfig,
};

use super::transport::{project_mapping_ack_connection, send_project_mapping_ack};
use super::{app_log, project_config, sync_mode_from_label, with_endpoint_first, Backend};

impl Backend {
    pub fn take_pending_project_mapping_request(&self) -> Option<ProjectMappingRequestPayload> {
        let request = self
            .pending_project_mapping_requests
            .lock()
            .unwrap()
            .pop_front()?;
        let mut g = self.inner.lock().unwrap();
        g.project_mapping_requests
            .insert(request.request_id.clone(), request.clone());
        app_log(
            "project_mapping_request_ready",
            &[
                ("request_id", request.request_id.clone()),
                ("project", request.project_name.clone()),
                ("peer", request.device.name.clone()),
                ("source_dir", request.source_dir.display().to_string()),
            ],
        );
        Some(request)
    }

    pub fn confirm_project_mapping_request(
        &self,
        request_id: &str,
        local_dir: PathBuf,
    ) -> Result<()> {
        let (endpoint, tls, ack, candidate, config_path, peer_name, log_remote_dir) = {
            let g = self.inner.lock().unwrap();
            let request = g
                .project_mapping_requests
                .get(request_id)
                .cloned()
                .ok_or_else(|| {
                    AisyncError::Config(format!("project mapping request '{request_id}' not found"))
                })?;
            // Auto-create the local destination if it doesn't exist — the peer
            // confirm flow should never fail just because the folder is new
            // (BUG 252). Recursive mkdir -p.
            if !local_dir.exists() {
                fs::create_dir_all(&local_dir).map_err(|e| {
                    AisyncError::Config(format!(
                        "failed to create local dir {}: {e}",
                        local_dir.display()
                    ))
                })?;
                app_log(
                    "project_mapping_local_dir_created",
                    &[("path", local_dir.display().to_string())],
                );
            }
            if g.config
                .projects
                .iter()
                .any(|project| project.name == request.project_name)
            {
                return Err(AisyncError::Config(format!(
                    "project '{}' already exists",
                    request.project_name
                )));
            }
            let live_connection = g
                .discoverer
                .peer_connection_info(&request.device.id)
                .ok()
                .flatten();
            let ack_connection = project_mapping_ack_connection(live_connection, &request)?;
            let identity = generate_tls_identity("aisync-client")?;
            let tls = TlsConfig::new(identity, ack_connection.server_name.clone())
                .with_pinned_peer_cert(ack_connection.receiver_cert_der);
            let peer_name = request.device.name.clone();
            let mut candidate = g.config.clone();
            candidate.projects.push(project_config(
                request.project_name.clone(),
                local_dir.clone(),
                peer_name.clone(),
                request.source_dir.clone(),
                sync_mode_from_label(&request.mode),
            ));
            let local_device = g.discoverer.local_device().clone();
            let local_endpoint = g.serve.as_ref().and_then(|serve| {
                local_device
                    .addresses
                    .first()
                    .map(|ip| SocketAddr::new(*ip, serve.port))
            });
            let ack = ProjectMappingAckPayload {
                request_id: request.request_id,
                accepted: true,
                project_name: request.project_name,
                remote_dir: Some(local_dir.clone()),
                message: None,
                device: with_endpoint_first(local_device, local_endpoint),
            };
            app_log(
                "project_mapping_ack_connect_prepared",
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
                peer_name,
                local_dir.display().to_string(),
            )
        };
        send_project_mapping_ack(endpoint, tls, ack.clone())?;
        {
            let mut g = self.inner.lock().unwrap();
            save_config(&config_path, &candidate)?;
            g.config = candidate.clone();
            g.project_mapping_requests.remove(request_id);
        }
        app_log(
            "project_mapping_confirmed",
            &[
                ("request_id", request_id.to_string()),
                ("project", ack.project_name),
                ("peer", peer_name),
                ("remote_dir", log_remote_dir),
            ],
        );
        Ok(())
    }

    pub fn process_project_mapping_acks(&self) -> Result<usize> {
        let mut processed = 0;
        loop {
            let ack = self
                .pending_project_mapping_acks
                .lock()
                .unwrap()
                .pop_front();
            let Some(ack) = ack else {
                return Ok(processed);
            };
            let mut g = self.inner.lock().unwrap();
            let Some(outbound) = g.outbound_project_mappings.remove(&ack.request_id) else {
                continue;
            };
            if !ack.accepted {
                return Err(AisyncError::Config(
                    ack.message
                        .unwrap_or_else(|| "project mapping request rejected".to_string()),
                ));
            }
            let remote_dir = ack.remote_dir.clone().ok_or_else(|| {
                AisyncError::Config("project mapping ack did not include remote_dir".to_string())
            })?;
            let mut candidate = g.config.clone();
            candidate.projects.push(project_config(
                outbound.project_name.clone(),
                outbound.local_dir.clone(),
                outbound.peer_name.clone(),
                remote_dir.clone(),
                outbound.mode,
            ));
            let path = g.config_path.clone();
            save_config(&path, &candidate)?;
            g.config = candidate.clone();
            processed += 1;
            app_log(
                "project_mapping_ack_applied",
                &[
                    ("request_id", ack.request_id),
                    ("project", outbound.project_name),
                    ("peer", outbound.peer_name),
                    ("remote_dir", remote_dir.display().to_string()),
                ],
            );
        }
    }

    /// Add a project mapping to config and persist (D1).
    ///
    /// `create_local_dir`: when true, the local dir is created (mkdir -p) if it
    /// doesn't exist — the GUI sets this only after the user confirmed the
    /// "目录不存在，是否新建" prompt. When false and the dir is missing, returns
    /// a structured `local-dir-missing:<path>` error the GUI turns into that
    /// prompt (instead of silently creating or failing opaquely).
    ///
    /// On any failure the in-memory config is left UNCHANGED — a failed add must
    /// not leave a phantom project that then blocks retry with "already exists".
    pub fn add_project(
        &self,
        name: String,
        local: PathBuf,
        peer_name: String,
        remote: PathBuf,
        mode: SyncModeConfig,
        create_local_dir: bool,
    ) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        if g.config.projects.iter().any(|p| p.name == name) {
            return Err(AisyncError::Config(format!(
                "project '{name}' already exists"
            )));
        }

        // Local dir handling: prompt-then-create, never silent.
        if !local.exists() {
            if create_local_dir {
                fs::create_dir_all(&local).map_err(|e| {
                    AisyncError::Config(format!(
                        "failed to create local dir {}: {e}",
                        local.display()
                    ))
                })?;
                app_log(
                    "project_local_dir_created",
                    &[("path", local.display().to_string())],
                );
            } else {
                // Signal the GUI to show the "目录不存在，是否新建" confirm.
                return Err(AisyncError::Config(format!(
                    "local-dir-missing:{}",
                    local.display()
                )));
            }
        }

        let log_project = name.clone();
        let log_peer = peer_name.clone();
        let log_remote = remote.display().to_string();
        // Save against a CLONE first; only commit to the live config if the
        // validated write succeeds — this is the rollback that fixes the
        // "failed add still leaves a phantom project" bug.
        let mut candidate = g.config.clone();
        candidate
            .projects
            .push(project_config(name, local, peer_name, remote, mode));
        let path = g.config_path.clone();
        save_config(&path, &candidate)?;
        g.config = candidate.clone();
        app_log(
            "project_mapping_created",
            &[
                ("project", log_project),
                ("peer", log_peer),
                ("remote_dir", log_remote),
                ("file_count", "0".to_string()),
                ("bytes", "0".to_string()),
            ],
        );
        Ok(())
    }

    pub fn delete_project(&self, project_name: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let mut candidate = g.config.clone();
        let original_len = candidate.projects.len();
        candidate
            .projects
            .retain(|project| project.name != project_name);
        if candidate.projects.len() == original_len {
            return Err(AisyncError::Config(format!(
                "project '{project_name}' not found"
            )));
        }

        let path = g.config_path.clone();
        save_config(&path, &candidate)?;
        g.config = candidate;
        app_log(
            "project_mapping_deleted",
            &[("project", project_name.to_string())],
        );
        Ok(())
    }
}
