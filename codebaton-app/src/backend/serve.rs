//! Local serve (receive) daemon: types + daemon spawner.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use codebaton_sync::SyncConfig;
use codebaton_transport::{
    FileTransferAckPayload, FileTransferDataPayload, FileTransferRequestPayload,
    PairingRequestPayload, ProjectMappingAckPayload, ProjectMappingRequestPayload, ReceiveService,
    TextMessagePayload, TlsConfig, WorkspaceMappingAckPayload, WorkspaceMappingRequestPayload,
};

use super::time_util::normalize_epoch_millis;
use super::identity::{load_or_create_receiver_identity, receiver_cert_path};
use super::file_transfer::{
    prepare_default_file_transfer_accept, receive_file_transfer_data, FileReceiveState,
};
use super::history::record_receiver_sync_history;
use super::messaging::record_text_message_history;
use super::transport::send_file_transfer_ack;
use super::{app_log, log_line, receive_root};

/// Local serve-daemon coordinates the GUI exposes for pairing / manual setup.
#[derive(Clone)]
pub struct ServeInfo {
    pub port: u16,
    pub cert_path: PathBuf,
    pub receive_dir: PathBuf,
}

/// 停止 serve 守护：置 stop 标志，再 poke 端口一次唤醒阻塞的 `accept()`，
/// 让守护循环检查标志后退出，释放 socket 与 runtime 线程。
/// 用于测试结束防 orphan 线程，也用于 receiver 重启场景。
pub struct ServeShutdownHandle {
    stop: Arc<AtomicBool>,
    port: u16,
}

impl ServeShutdownHandle {
    /// 触发关闭：尽力而为，幂等。
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // poke 一次本地端口，唤醒守护里阻塞的 accept()，使其下一轮看到 stop。
        let _ = std::net::TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], self.port)),
            Duration::from_millis(200),
        );
    }
}

/// Start the TLS receive daemon on a dedicated thread so other CodeBaton instances
/// can push to this one. Writes the receiver's self-signed cert (.der) next to
/// the config so a peer can pin it. Returns connection coordinates, or `None`
/// if binding fails (e.g. port in use) — the UI still works, just can't receive.
pub(crate) fn start_serve_daemon(
    config: &SyncConfig,
    config_path: &Path,
    port: u16,
    pending_pairing_requests: Arc<Mutex<VecDeque<PairingRequestPayload>>>,
    pending_project_mapping_requests: Arc<Mutex<VecDeque<ProjectMappingRequestPayload>>>,
    pending_project_mapping_acks: Arc<Mutex<VecDeque<ProjectMappingAckPayload>>>,
    pending_workspace_mapping_requests: Arc<Mutex<VecDeque<WorkspaceMappingRequestPayload>>>,
    pending_workspace_mapping_acks: Arc<Mutex<VecDeque<WorkspaceMappingAckPayload>>>,
    pending_text_messages: Arc<Mutex<VecDeque<TextMessagePayload>>>,
    pending_file_transfer_requests: Arc<Mutex<VecDeque<FileTransferRequestPayload>>>,
    pending_file_transfer_acks: Arc<Mutex<VecDeque<FileTransferAckPayload>>>,
    file_receive_states: Arc<Mutex<HashMap<String, FileReceiveState>>>,
    receive_limit: Option<usize>,
) -> Option<(ServeInfo, ServeShutdownHandle)> {
    let receive_dir = receive_root(config, config_path);
    let cert_path = receiver_cert_path(config_path);
    if let Err(e) = fs::create_dir_all(&receive_dir) {
        eprintln!("[codebaton-app] receive dir create failed: {e}");
        return None;
    }

    let listen = SocketAddr::from(([0, 0, 0, 0], port));
    let target = receive_dir.clone();
    let cert_out = cert_path.clone();

    // Bind synchronously first so we know the daemon is live before returning.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .ok()?;

    let identity = match load_or_create_receiver_identity(config_path) {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!("[codebaton-app] receiver identity failed: {e}");
            return None;
        }
    };
    if let Err(e) = fs::write(&cert_out, &identity.cert_der) {
        eprintln!("[codebaton-app] write receiver cert failed: {e}");
        return None;
    }

    let service = match runtime.block_on(async {
        let tls = TlsConfig::new(identity, "aisync-receiver");
        ReceiveService::bind(listen, target, &tls).await
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[codebaton-app] receive daemon bind failed on {listen}: {e}");
            return None;
        }
    };
    let bound = service.local_addr().ok().map(|a| a.port()).unwrap_or(port);
    log_line(&format!(
        "[codebaton-app] receive daemon listening on :{bound} → {} (cert {})",
        receive_dir.display(),
        cert_path.display()
    ));
    let history_config_path = config_path.to_path_buf();
    let history_receive_dir = receive_dir.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_loop = Arc::clone(&stop);

    // Production runs for the process lifetime; tests pass a finite limit.
    std::thread::spawn(move || {
        runtime.block_on(async move {
            let pairing_handler = move |request: PairingRequestPayload| {
                log_line(&format!(
                    "[pair] pairing_request_received request_id={} peer_id={} peer_name={} code={} expires_at={}",
                    request.request_id,
                    request.device.id.0,
                    request.device.name,
                    request.code,
                    request.expires_at_unix_secs
                ));
                pending_pairing_requests.lock().unwrap().push_back(request);
            };
            let project_mapping_request_handler = move |request: ProjectMappingRequestPayload| {
                log_line(&format!(
                    "[project] project_mapping_request_received request_id={} project={} peer_id={} peer_name={} source_dir={}",
                    request.request_id,
                    request.project_name,
                    request.device.id.0,
                    request.device.name,
                    request.source_dir.display()
                ));
                pending_project_mapping_requests
                    .lock()
                    .unwrap()
                    .push_back(request);
            };
            let project_mapping_ack_handler = move |ack: ProjectMappingAckPayload| {
                log_line(&format!(
                    "[project] project_mapping_ack_received request_id={} accepted={} project={} peer_id={} peer_name={}",
                    ack.request_id,
                    ack.accepted,
                    ack.project_name,
                    ack.device.id.0,
                    ack.device.name
                ));
                pending_project_mapping_acks.lock().unwrap().push_back(ack);
                Ok(())
            };
            let workspace_mapping_request_handler =
                move |request: WorkspaceMappingRequestPayload| {
                    log_line(&format!(
                        "[workspace] workspace_request_received request_id={} workspace={} peer_id={} peer_name={} source_root={} suggested_remote_root={}",
                        request.request_id,
                        request.workspace_name,
                        request.device.id.0,
                        request.device.name,
                        request.source_root.display(),
                        request.suggested_remote_root.display()
                    ));
                    pending_workspace_mapping_requests
                        .lock()
                        .unwrap()
                        .push_back(request);
                };
            let workspace_mapping_ack_handler = move |ack: WorkspaceMappingAckPayload| {
                log_line(&format!(
                    "[workspace] workspace_ack_received request_id={} accepted={} workspace={} peer_id={} peer_name={}",
                    ack.request_id,
                    ack.accepted,
                    ack.workspace_name,
                    ack.device.id.0,
                    ack.device.name
                ));
                pending_workspace_mapping_acks.lock().unwrap().push_back(ack);
                Ok(())
            };
            let text_history_config_path = history_config_path.clone();
            let text_message_handler = move |mut message: TextMessagePayload| {
                message.timestamp = normalize_epoch_millis(message.timestamp);
                log_line(&format!(
                    "[message] text_message_received sender={} bytes={} timestamp={}",
                    message.sender_name,
                    message.content.len(),
                    message.timestamp
                ));
                record_text_message_history(&text_history_config_path, None, &message, false);
                app_log(
                    "text_message_enqueued",
                    &[
                        ("sender", message.sender_name.clone()),
                        ("bytes", message.content.len().to_string()),
                    ],
                );
                pending_text_messages.lock().unwrap().push_back(message);
                Ok(())
            };
            let file_request_config_path = history_config_path.clone();
            let file_receive_states_for_requests = Arc::clone(&file_receive_states);
            let pending_requests_for_auto_accept = Arc::clone(&pending_file_transfer_requests);
            let file_transfer_request_handler = move |request: FileTransferRequestPayload| {
                log_line(&format!(
                    "[file] file_transfer_request_received transfer_id={} filename={} size={} sender={}",
                    request.transfer_id,
                    request.filename,
                    request.size,
                    request.sender_name
                ));
                match prepare_default_file_transfer_accept(&file_request_config_path, bound, &request) {
                    Ok((endpoint, tls, ack, state)) => {
                        let transfer_id = request.transfer_id.clone();
                        let filename = request.filename.clone();
                        let target_path = state.target_path.clone();
                        file_receive_states_for_requests
                            .lock()
                            .unwrap()
                            .insert(transfer_id.clone(), state);
                        let states = Arc::clone(&file_receive_states_for_requests);
                        let pending = Arc::clone(&pending_requests_for_auto_accept);
                        std::thread::spawn(move || {
                            if let Err(error) = send_file_transfer_ack(endpoint, tls, ack) {
                                states.lock().unwrap().remove(&transfer_id);
                                pending.lock().unwrap().push_back(request);
                                app_log(
                                    "file_transfer_auto_accept_failed",
                                    &[
                                        ("transfer_id", transfer_id),
                                        ("filename", filename),
                                        ("error", error.to_string()),
                                    ],
                                );
                            } else {
                                app_log(
                                    "file_transfer_auto_accepted",
                                    &[
                                        ("transfer_id", transfer_id),
                                        ("filename", filename),
                                        ("target_path", target_path.display().to_string()),
                                    ],
                                );
                            }
                        });
                    }
                    Err(error) => {
                        app_log(
                            "file_transfer_pending_confirmation",
                            &[
                                ("transfer_id", request.transfer_id.clone()),
                                ("filename", request.filename.clone()),
                                ("reason", error.to_string()),
                            ],
                        );
                        pending_file_transfer_requests
                            .lock()
                            .unwrap()
                            .push_back(request);
                    }
                }
            };
            let file_transfer_ack_handler = move |ack: FileTransferAckPayload| {
                log_line(&format!(
                    "[file] file_transfer_ack_received transfer_id={} accepted={} ready={} filename={}",
                    ack.transfer_id,
                    ack.accepted,
                    ack.ready,
                    ack.filename
                ));
                pending_file_transfer_acks.lock().unwrap().push_back(ack);
                Ok(())
            };
            let file_transfer_data_handler = move |data: FileTransferDataPayload| {
                receive_file_transfer_data(&file_receive_states, data)
            };
            let mut handled = 0usize;
            loop {
                if stop_loop.load(Ordering::SeqCst) {
                    log_line("[codebaton-app] receive daemon stop requested");
                    break;
                }
                if receive_limit
                    .map(|limit| handled >= limit)
                    .unwrap_or(false)
                {
                    log_line(&format!(
                        "[codebaton-app] receive daemon test limit reached: {handled}"
                    ));
                    break;
                }
                handled += 1;
                if receive_limit.is_some() {
                    match tokio::time::timeout(
                        Duration::from_secs(30),
                        service.receive_once_with_control_handlers(
                            None,
                            Some(&pairing_handler),
                            Some(&project_mapping_request_handler),
                            Some(&project_mapping_ack_handler),
                            Some(&workspace_mapping_request_handler),
                            Some(&workspace_mapping_ack_handler),
                            Some(&text_message_handler),
                            Some(&file_transfer_request_handler),
                            Some(&file_transfer_ack_handler),
                            Some(&file_transfer_data_handler),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(manifest)) => {
                            record_receiver_sync_history(
                                &history_config_path,
                                &manifest,
                                &history_receive_dir,
                            );
                        }
                        Ok(Err(e)) => eprintln!("[codebaton-app] receive daemon error: {e}"),
                        Err(_) => {
                            log_line("[codebaton-app] receive daemon test idle timeout");
                            break;
                        }
                    }
                } else {
                    match service
                        .receive_once_with_control_handlers(
                            None,
                            Some(&pairing_handler),
                            Some(&project_mapping_request_handler),
                            Some(&project_mapping_ack_handler),
                            Some(&workspace_mapping_request_handler),
                            Some(&workspace_mapping_ack_handler),
                            Some(&text_message_handler),
                            Some(&file_transfer_request_handler),
                            Some(&file_transfer_ack_handler),
                            Some(&file_transfer_data_handler),
                        )
                        .await
                    {
                        Ok(manifest) => {
                            record_receiver_sync_history(
                                &history_config_path,
                                &manifest,
                                &history_receive_dir,
                            );
                        }
                        Err(e) => eprintln!("[codebaton-app] receive daemon error: {e}"),
                    }
                }
            }
        });
    });

    Some((
        ServeInfo {
            port: bound,
            cert_path,
            receive_dir,
        },
        ServeShutdownHandle { stop, port: bound },
    ))
}
