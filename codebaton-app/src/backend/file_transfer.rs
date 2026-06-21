use std::collections::HashMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use codebaton_core::{AisyncError, DeviceInfo, Result};
use codebaton_discovery::local_device_addresses;
use codebaton_sync::load_config;
use codebaton_transport::{
    generate_tls_identity, match_sensitive_file_path, FileTransferAckPayload,
    FileTransferDataPayload, FileTransferRequestPayload, TlsConfig,
};

use super::time_util::epoch_millis_now;
use super::{
    advertised_local_endpoint, app_log, control_connection_for_peer, default_file_receive_dir,
    file_transfer_ack_connection, local_os_type, send_file_transfer_ack, send_file_transfer_data,
    send_file_transfer_request, with_endpoint_first, Backend,
};
use super::history::{append_json_line, read_jsonl};

#[derive(Clone)]
pub(crate) struct OutboundFileTransfer {
    pub(crate) path: PathBuf,
    pub(crate) peer_name: String,
}

pub(crate) struct FileReceiveState {
    pub(crate) target_path: PathBuf,
    pub(crate) tmp_path: PathBuf,
    pub(crate) expected_size: u64,
    pub(crate) bytes_written: u64,
    pub(crate) filename: String,
    pub(crate) sender_name: String,
    pub(crate) history_config_path: PathBuf,
}

impl Backend {
    pub fn request_file_transfer(
        &self,
        peer_name: &str,
        path: PathBuf,
        confirmed_sensitive: &[String],
    ) -> Result<String> {
        let (transfer_id, endpoint, tls, request) = {
            let mut g = self.inner.lock().unwrap();
            let metadata = fs::metadata(&path).map_err(|error| {
                app_log(
                    "ft_metadata_failed",
                    &[
                        ("peer", peer_name.to_string()),
                        ("path", path.display().to_string()),
                        ("error", error.to_string()),
                    ],
                );
                AisyncError::from(error)
            })?;
            if !metadata.is_file() {
                return Err(AisyncError::Config(format!(
                    "file transfer source is not a file: {}",
                    path.display()
                )));
            }
            let filename = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    AisyncError::Config(format!(
                        "file transfer path has no filename: {}",
                        path.display()
                    ))
                })?;
            app_log(
                "ft_metadata_ok",
                &[
                    ("peer", peer_name.to_string()),
                    ("path", path.display().to_string()),
                    ("filename", filename.clone()),
                    ("size", metadata.len().to_string()),
                ],
            );
            ensure_file_transfer_source_allowed(&path, confirmed_sensitive)?;
            app_log(
                "ft_sensitive_ok",
                &[
                    ("peer", peer_name.to_string()),
                    ("path", path.display().to_string()),
                    ("confirmed_count", confirmed_sensitive.len().to_string()),
                ],
            );
            let (endpoint, tls) = control_connection_for_peer(&g, peer_name)?;
            app_log(
                "ft_peer_endpoint_resolved",
                &[
                    ("peer", peer_name.to_string()),
                    ("endpoint", endpoint.to_string()),
                    ("server_name", tls.server_name.clone()),
                ],
            );
            let peer_endpoint = endpoint;
            let local_device = g.discoverer.local_device().clone();
            let serve = g
                .serve
                .clone()
                .ok_or_else(|| AisyncError::Config("local receiver is not running".to_string()))?;
            let local_endpoint = advertised_local_endpoint(&local_device, &serve, peer_endpoint)?;
            let receiver_cert_der = fs::read(&serve.cert_path).map_err(|error| {
                AisyncError::Transport(format!(
                    "local receiver certificate not found at {}: {}",
                    serve.cert_path.display(),
                    error
                ))
            })?;
            app_log(
                "ft_cert_loaded",
                &[
                    ("peer", peer_name.to_string()),
                    ("cert_path", serve.cert_path.display().to_string()),
                    ("cert_bytes", receiver_cert_der.len().to_string()),
                ],
            );
            let transfer_id = codebaton_discovery::new_pairing_request_id();
            let request = FileTransferRequestPayload {
                transfer_id: transfer_id.clone(),
                filename: filename.clone(),
                size: metadata.len(),
                sender_name: g.config.device.name.clone(),
                device: with_endpoint_first(local_device, Some(local_endpoint)),
                endpoint: Some(local_endpoint),
                receiver_cert_der: Some(receiver_cert_der),
                server_name: Some("aisync-receiver".to_string()),
            };
            g.outbound_file_transfers.insert(
                transfer_id.clone(),
                OutboundFileTransfer {
                    path,
                    peer_name: peer_name.to_string(),
                },
            );
            (transfer_id, endpoint, tls, request)
        };

        app_log(
            "ft_control_send_start",
            &[
                ("transfer_id", transfer_id.clone()),
                ("peer", peer_name.to_string()),
                ("endpoint", endpoint.to_string()),
                ("filename", request.filename.clone()),
            ],
        );
        if let Err(error) = send_file_transfer_request(endpoint, tls, request.clone()) {
            let mut g = self.inner.lock().unwrap();
            g.outbound_file_transfers.remove(&transfer_id);
            app_log(
                "ft_control_send_failed",
                &[
                    ("transfer_id", transfer_id.clone()),
                    ("peer", peer_name.to_string()),
                    ("filename", request.filename),
                    ("error", error.to_string()),
                ],
            );
            return Err(error);
        }
        app_log(
            "ft_control_send_ok",
            &[
                ("transfer_id", transfer_id.clone()),
                ("peer", peer_name.to_string()),
                ("filename", request.filename.clone()),
            ],
        );
        app_log(
            "file_transfer_request_sent",
            &[
                ("transfer_id", transfer_id.clone()),
                ("filename", request.filename),
                ("size", request.size.to_string()),
                ("peer", peer_name.to_string()),
            ],
        );
        Ok(transfer_id)
    }

    pub fn take_pending_file_transfer_request(&self) -> Option<FileTransferRequestPayload> {
        let request = self
            .pending_file_transfer_requests
            .lock()
            .unwrap()
            .pop_front()?;
        let mut g = self.inner.lock().unwrap();
        g.file_transfer_requests
            .insert(request.transfer_id.clone(), request.clone());
        Some(request)
    }

    pub fn pending_file_transfers(&self) -> Vec<FileTransferRequestPayload> {
        let mut queued = Vec::new();
        {
            let mut queue = self.pending_file_transfer_requests.lock().unwrap();
            while let Some(request) = queue.pop_front() {
                queued.push(request);
            }
        }
        let mut g = self.inner.lock().unwrap();
        for request in queued {
            g.file_transfer_requests
                .insert(request.transfer_id.clone(), request);
        }
        let mut pending: Vec<_> = g.file_transfer_requests.values().cloned().collect();
        pending.sort_by(|left, right| left.transfer_id.cmp(&right.transfer_id));
        pending
    }

    pub fn accept_file_transfer(&self, transfer_id: &str, save_dir: PathBuf) -> Result<()> {
        let filename = {
            let g = self.inner.lock().unwrap();
            g.file_transfer_requests
                .get(transfer_id)
                .map(|request| safe_filename(&request.filename))
                .ok_or_else(|| {
                    AisyncError::Config(format!("file transfer request '{transfer_id}' not found"))
                })?
        };
        self.confirm_file_transfer_request(transfer_id, save_dir.join(filename))
    }

    pub fn confirm_file_transfer_request(
        &self,
        transfer_id: &str,
        target_path: PathBuf,
    ) -> Result<()> {
        let (endpoint, tls, ack, state) = {
            let g = self.inner.lock().unwrap();
            let request = g
                .file_transfer_requests
                .get(transfer_id)
                .cloned()
                .ok_or_else(|| {
                    AisyncError::Config(format!("file transfer request '{transfer_id}' not found"))
                })?;
            let receive_dir = target_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| default_file_receive_dir(&g.config_path, &g.config));
            let target_path = ensure_file_receive_target(&receive_dir, &target_path)?;
            let live_connection = g
                .discoverer
                .peer_connection_info(&request.device.id)
                .ok()
                .flatten();
            let ack_connection = file_transfer_ack_connection(live_connection, &request)?;
            let identity = generate_tls_identity("aisync-client")?;
            let tls = TlsConfig::new(identity, ack_connection.server_name)
                .with_pinned_peer_cert(ack_connection.receiver_cert_der);
            let local_device = g.discoverer.local_device().clone();
            let local_endpoint = g.serve.as_ref().and_then(|serve| {
                local_device
                    .addresses
                    .first()
                    .map(|ip| SocketAddr::new(*ip, serve.port))
            });
            let ack = FileTransferAckPayload {
                transfer_id: request.transfer_id.clone(),
                accepted: true,
                ready: true,
                filename: request.filename.clone(),
                message: None,
                device: with_endpoint_first(local_device, local_endpoint),
            };
            let tmp_path = file_transfer_tmp_path(&target_path, transfer_id);
            let _ = fs::remove_file(&tmp_path);
            let state = FileReceiveState {
                target_path,
                tmp_path,
                expected_size: request.size,
                bytes_written: 0,
                filename: request.filename,
                sender_name: request.sender_name,
                history_config_path: g.config_path.clone(),
            };
            (ack_connection.endpoint, tls, ack, state)
        };
        self.file_receive_states
            .lock()
            .unwrap()
            .insert(transfer_id.to_string(), state);
        if let Err(error) = send_file_transfer_ack(endpoint, tls, ack.clone()) {
            self.file_receive_states.lock().unwrap().remove(transfer_id);
            return Err(error);
        }
        self.inner
            .lock()
            .unwrap()
            .file_transfer_requests
            .remove(transfer_id);
        app_log(
            "file_transfer_confirmed",
            &[
                ("transfer_id", transfer_id.to_string()),
                ("filename", ack.filename),
                ("target_path", self.file_receive_target_path(transfer_id)),
            ],
        );
        Ok(())
    }

    pub fn file_transfer_history(&self, peer_name: Option<&str>) -> Vec<serde_json::Value> {
        let path = self
            .config_path()
            .with_file_name("file_transfer_history.jsonl");
        read_jsonl(&path)
            .into_iter()
            .filter(|row| {
                peer_name
                    .map(|peer| row.get("peer").and_then(|v| v.as_str()) == Some(peer))
                    .unwrap_or(true)
            })
            .collect()
    }

    pub fn process_file_transfer_acks(&self) -> Result<usize> {
        let mut processed = 0;
        loop {
            let ack = self.pending_file_transfer_acks.lock().unwrap().pop_front();
            let Some(ack) = ack else {
                return Ok(processed);
            };
            if !ack.accepted || !ack.ready {
                if !ack.accepted {
                    return Err(AisyncError::Transport(
                        ack.message
                            .unwrap_or_else(|| "file transfer rejected".to_string()),
                    ));
                }
                continue;
            }
            let (endpoint, tls, source_path, peer_name, config_path) = {
                let mut g = self.inner.lock().unwrap();
                let Some(outbound) = g.outbound_file_transfers.remove(&ack.transfer_id) else {
                    continue;
                };
                let (endpoint, tls) = control_connection_for_peer(&g, &outbound.peer_name)?;
                (
                    endpoint,
                    tls,
                    outbound.path,
                    outbound.peer_name,
                    g.config_path.clone(),
                )
            };
            let size = fs::metadata(&source_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            send_file_transfer_data(endpoint, tls, ack.transfer_id.clone(), source_path.clone())?;
            processed += 1;
            record_file_transfer_history(
                &config_path,
                "out",
                &peer_name,
                &ack.filename,
                &source_path,
                size,
                &ack.transfer_id,
                "sent",
                None,
            );
            app_log(
                "file_transfer_data_sent",
                &[("transfer_id", ack.transfer_id), ("filename", ack.filename)],
            );
        }
    }
}

pub(crate) fn prepare_default_file_transfer_accept(
    config_path: &Path,
    receive_port: u16,
    request: &FileTransferRequestPayload,
) -> Result<(
    SocketAddr,
    TlsConfig,
    FileTransferAckPayload,
    FileReceiveState,
)> {
    let config = load_config(config_path)?;
    let receive_dir = default_file_receive_dir(config_path, &config);
    let target_path = ensure_file_receive_target(
        &receive_dir,
        &receive_dir.join(safe_filename(&request.filename)),
    )?;
    let ack_connection = file_transfer_ack_connection(None, request)?;
    let identity = generate_tls_identity("aisync-client")?;
    let tls = TlsConfig::new(identity, ack_connection.server_name)
        .with_pinned_peer_cert(ack_connection.receiver_cert_der);
    let local_device = DeviceInfo {
        id: config.device.id,
        name: config.device.name,
        os: local_os_type(),
        addresses: local_device_addresses(),
        protocol_version: 1,
    };
    let local_ip = if ack_connection.endpoint.ip().is_loopback() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        local_device
            .addresses
            .first()
            .copied()
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
    };
    let ack = FileTransferAckPayload {
        transfer_id: request.transfer_id.clone(),
        accepted: true,
        ready: true,
        filename: request.filename.clone(),
        message: None,
        device: with_endpoint_first(local_device, Some(SocketAddr::new(local_ip, receive_port))),
    };
    let tmp_path = file_transfer_tmp_path(&target_path, &request.transfer_id);
    let _ = fs::remove_file(&tmp_path);
    let state = FileReceiveState {
        target_path,
        tmp_path,
        expected_size: request.size,
        bytes_written: 0,
        filename: request.filename.clone(),
        sender_name: request.sender_name.clone(),
        history_config_path: config_path.to_path_buf(),
    };
    Ok((ack_connection.endpoint, tls, ack, state))
}

pub(crate) fn ensure_file_transfer_source_allowed(
    path: &Path,
    confirmed_sensitive: &[String],
) -> Result<()> {
    let Some(sensitive) = match_sensitive_file_path(path)? else {
        return Ok(());
    };
    let path_text = path.to_string_lossy().into_owned();
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let confirmed = confirmed_sensitive.iter().any(|confirmed| {
        confirmed == &path_text
            || confirmed == &sensitive.relative_path
            || filename.as_ref().is_some_and(|name| confirmed == name)
    });
    if confirmed {
        return Ok(());
    }
    app_log(
        "file_transfer_sensitive_blocked",
        &[
            ("path", path_text.clone()),
            ("pattern", sensitive.matched_pattern.clone()),
        ],
    );
    Err(AisyncError::Config(format!(
        "sensitive-file:{path_text}:{}",
        sensitive.matched_pattern
    )))
}

pub(crate) fn ensure_file_receive_target(receive_dir: &Path, target_path: &Path) -> Result<PathBuf> {
    fs::create_dir_all(receive_dir)?;
    let root = receive_dir.canonicalize()?;
    let requested = if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        receive_dir.join(target_path)
    };
    let filename = requested
        .file_name()
        .filter(|name| !name.to_string_lossy().is_empty())
        .ok_or_else(|| {
            AisyncError::Config(format!(
                "file receive path has no filename: {}",
                target_path.display()
            ))
        })?;
    let parent = requested.parent().ok_or_else(|| {
        AisyncError::Config(format!(
            "file receive path has no parent: {}",
            target_path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    if !parent.starts_with(&root) {
        return Err(AisyncError::Config(format!(
            "file receive path escapes receive dir: {}",
            target_path.display()
        )));
    }
    Ok(parent.join(filename))
}

pub(crate) fn safe_filename(filename: &str) -> String {
    Path::new(filename)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "received-file".to_string())
}

pub(crate) fn file_transfer_tmp_path(target_path: &Path, transfer_id: &str) -> PathBuf {
    let filename = target_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "received-file".to_string());
    target_path.with_file_name(format!("{filename}.{transfer_id}.part"))
}

pub(crate) fn receive_file_transfer_data(
    states: &Arc<Mutex<HashMap<String, FileReceiveState>>>,
    data: FileTransferDataPayload,
) -> Result<()> {
    let mut states = states.lock().unwrap();
    let state = states.get_mut(&data.transfer_id).ok_or_else(|| {
        AisyncError::Transport(format!(
            "file transfer '{}' has not been confirmed",
            data.transfer_id
        ))
    })?;
    if data.offset != state.bytes_written {
        return Err(AisyncError::Transport(format!(
            "file transfer offset mismatch for {}: expected {}, got {}",
            data.transfer_id, state.bytes_written, data.offset
        )));
    }
    if !data.chunk.is_empty() {
        if let Some(parent) = state.tmp_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&state.tmp_path)?;
        use std::io::Write as _;
        file.write_all(&data.chunk)?;
        state.bytes_written += data.chunk.len() as u64;
    }
    let completed = if data.done {
        if state.bytes_written != state.expected_size {
            return Err(AisyncError::Transport(format!(
                "file transfer size mismatch for {}: expected {}, got {}",
                data.transfer_id, state.expected_size, state.bytes_written
            )));
        }
        if let Some(parent) = state.target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&state.tmp_path, &state.target_path)?;
        Some((
            state.target_path.clone(),
            state.expected_size,
            state.filename.clone(),
            state.sender_name.clone(),
            state.history_config_path.clone(),
        ))
    } else {
        None
    };
    if let Some((completed_path, bytes, filename, sender_name, history_config_path)) = completed {
        states.remove(&data.transfer_id);
        record_file_transfer_history(
            &history_config_path,
            "in",
            &sender_name,
            &filename,
            &completed_path,
            bytes,
            &data.transfer_id,
            "received",
            None,
        );
        app_log(
            "file_transfer_received",
            &[
                ("transfer_id", data.transfer_id.clone()),
                ("filename", filename),
                ("sender", sender_name),
                ("path", completed_path.display().to_string()),
            ],
        );
    }
    Ok(())
}

pub(crate) fn record_file_transfer_history(
    config_path: &Path,
    direction: &str,
    peer: &str,
    filename: &str,
    path: &Path,
    bytes: u64,
    transfer_id: &str,
    status: &str,
    detail: Option<String>,
) {
    let history_path = config_path.with_file_name("file_transfer_history.jsonl");
    let entry = serde_json::json!({
        "timestamp": epoch_millis_now(),
        "transferId": transfer_id,
        "direction": direction,
        "peer": peer,
        "filename": filename,
        "path": path.display().to_string(),
        "bytes": bytes,
        "status": status,
        "detail": detail,
    });
    match append_json_line(&history_path, &entry) {
        Ok(()) => app_log(
            "file_transfer_history_recorded",
            &[
                ("transfer_id", transfer_id.to_string()),
                ("direction", direction.to_string()),
                ("peer", peer.to_string()),
                ("filename", filename.to_string()),
                ("path", history_path.display().to_string()),
            ],
        ),
        Err(error) => app_log(
            "file_transfer_history_failed",
            &[
                ("transfer_id", transfer_id.to_string()),
                ("direction", direction.to_string()),
                ("path", history_path.display().to_string()),
                ("error", error.to_string()),
            ],
        ),
    }
}
