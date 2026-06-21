use std::fs;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use codebaton_core::{AisyncError, DeviceInfo, Result};
use codebaton_discovery::PeerConnectionInfo;
use codebaton_sync::SyncConfig;
use codebaton_transport::{
    generate_tls_identity, FileTransferAckPayload, FileTransferRequestPayload, PairingRequestPayload,
    ProjectMappingAckPayload, ProjectMappingRequestPayload, TcpTransporter, TextMessagePayload,
    TlsConfig, WorkspaceMappingAckPayload, WorkspaceMappingRequestPayload,
};

use super::{
    app_log, log_line, peer_device_info, with_endpoint_first, PairingConnection, ServeInfo,
};

pub(crate) const FILE_TRANSFER_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn advertised_local_endpoint(
    local_device: &DeviceInfo,
    serve: &ServeInfo,
    peer_endpoint: SocketAddr,
) -> Result<SocketAddr> {
    let ip =
        if peer_endpoint.ip().is_loopback() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            local_device.addresses.first().copied().ok_or_else(|| {
                AisyncError::Config("local receiver has no advertised IP".to_string())
            })?
        };
    Ok(SocketAddr::new(ip, serve.port))
}

pub(crate) fn pairing_tls_config(connection: Option<&PairingConnection>) -> Option<TlsConfig> {
    let connection = connection?;
    let cert = connection.receiver_cert_der.clone()?;
    let identity = generate_tls_identity("aisync-client").ok()?;
    Some(
        TlsConfig::new(
            identity,
            connection
                .server_name
                .clone()
                .unwrap_or_else(|| "aisync-receiver".to_string()),
        )
        .with_pinned_peer_cert(cert),
    )
}

pub(crate) fn send_pairing_request_async(
    endpoint: SocketAddr,
    tls: TlsConfig,
    request: PairingRequestPayload,
) {
    std::thread::spawn(move || {
        let request_id = request.request_id.clone();
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                log_line(&format!(
                    "[pair] pairing_request_send_failed request_id={request_id} endpoint={endpoint} reason=runtime:{error}"
                ));
                return;
            }
        };
        let result = runtime.block_on(async move {
            let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
            client.send_pairing_request(request).await
        });
        match result {
            Ok(()) => log_line(&format!(
                "[pair] pairing_request_sent request_id={request_id} endpoint={endpoint}"
            )),
            Err(error) => log_line(&format!(
                "[pair] pairing_request_send_failed request_id={request_id} endpoint={endpoint} reason={error}"
            )),
        }
    });
}

pub(crate) fn run_control_future<F, Fut>(name: &'static str, build: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| AisyncError::Transport(format!("tokio runtime: {error}")))?;
        runtime.block_on(build())
    })
    .join()
    .map_err(|_| AisyncError::Transport(format!("{name} sender thread panicked")))?
}

pub(crate) fn send_project_mapping_request(
    endpoint: SocketAddr,
    tls: TlsConfig,
    request: ProjectMappingRequestPayload,
) -> Result<()> {
    run_control_future("send_project_mapping_request", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_project_mapping_request(request).await
    })
}

pub(crate) fn send_project_mapping_ack(
    endpoint: SocketAddr,
    tls: TlsConfig,
    ack: ProjectMappingAckPayload,
) -> Result<()> {
    run_control_future("send_project_mapping_ack", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_project_mapping_ack(ack).await
    })
}

pub(crate) fn send_workspace_mapping_request(
    endpoint: SocketAddr,
    tls: TlsConfig,
    request: WorkspaceMappingRequestPayload,
) -> Result<()> {
    run_control_future("send_workspace_mapping_request", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_workspace_mapping_request(request).await
    })
}

pub(crate) fn send_workspace_mapping_ack(
    endpoint: SocketAddr,
    tls: TlsConfig,
    ack: WorkspaceMappingAckPayload,
) -> Result<()> {
    run_control_future("send_workspace_mapping_ack", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_workspace_mapping_ack(ack).await
    })
}

pub(crate) fn send_text_message(
    endpoint: SocketAddr,
    tls: TlsConfig,
    message: TextMessagePayload,
) -> Result<()> {
    run_control_future("send_text_message", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_text_message(message).await
    })
}

pub(crate) fn send_file_transfer_request(
    endpoint: SocketAddr,
    tls: TlsConfig,
    request: FileTransferRequestPayload,
) -> Result<()> {
    run_control_future("send_file_transfer_request", move || async move {
        let transfer_id = request.transfer_id.clone();
        let filename = request.filename.clone();
        app_log(
            "ft_control_connect_start",
            &[
                ("endpoint", endpoint.to_string()),
                ("transfer_id", transfer_id.clone()),
                ("filename", filename.clone()),
            ],
        );
        let mut client = match tokio::time::timeout(
            FILE_TRANSFER_CONTROL_TIMEOUT,
            TcpTransporter::connect_addr(endpoint, &tls),
        )
        .await
        {
            Ok(Ok(client)) => {
                app_log(
                    "ft_control_connect_ok",
                    &[
                        ("endpoint", endpoint.to_string()),
                        ("transfer_id", transfer_id.clone()),
                        ("filename", filename.clone()),
                    ],
                );
                client
            }
            Ok(Err(error)) => {
                app_log(
                    "ft_control_connect_failed",
                    &[
                        ("endpoint", endpoint.to_string()),
                        ("transfer_id", transfer_id.clone()),
                        ("filename", filename.clone()),
                        ("error", error.to_string()),
                    ],
                );
                return Err(error);
            }
            Err(_) => {
                let error = AisyncError::Transport(format!(
                    "file transfer control connect timed out after {}ms to {}",
                    FILE_TRANSFER_CONTROL_TIMEOUT.as_millis(),
                    endpoint
                ));
                app_log(
                    "ft_control_connect_failed",
                    &[
                        ("endpoint", endpoint.to_string()),
                        ("transfer_id", transfer_id.clone()),
                        ("filename", filename.clone()),
                        ("error", error.to_string()),
                    ],
                );
                return Err(error);
            }
        };

        tokio::time::timeout(FILE_TRANSFER_CONTROL_TIMEOUT, async move {
            client
                .send_file_transfer_request_with_stage_log(request, |event, fields| {
                    app_log(event, &fields);
                })
                .await
        })
        .await
        .map_err(|_| {
            let error = AisyncError::Transport(format!(
                "file transfer control request timed out after {}ms to {}",
                FILE_TRANSFER_CONTROL_TIMEOUT.as_millis(),
                endpoint
            ));
            app_log(
                "ft_control_ack_timeout",
                &[
                    ("endpoint", endpoint.to_string()),
                    ("transfer_id", transfer_id),
                    ("filename", filename),
                    ("error", error.to_string()),
                ],
            );
            error
        })?
    })
}

pub(crate) fn send_file_transfer_ack(
    endpoint: SocketAddr,
    tls: TlsConfig,
    ack: FileTransferAckPayload,
) -> Result<()> {
    run_control_future("send_file_transfer_ack", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_file_transfer_ack(ack).await
    })
}

pub(crate) fn send_file_transfer_data(
    endpoint: SocketAddr,
    tls: TlsConfig,
    transfer_id: String,
    source_path: PathBuf,
) -> Result<()> {
    run_control_future("send_file_transfer_data", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client
            .send_file_transfer_data(transfer_id, &source_path)
            .await
    })
}

pub(crate) struct PeerTransportConnection {
    pub(crate) peer: DeviceInfo,
    pub(crate) endpoint: SocketAddr,
    pub(crate) receiver_cert_der: Vec<u8>,
    pub(crate) server_name: String,
    pub(crate) cert_source: String,
}

pub(crate) fn peer_transport_connection(
    config_path: &Path,
    config: &SyncConfig,
    peer_name: &str,
    live: Option<PeerConnectionInfo>,
) -> Result<PeerTransportConnection> {
    let peer = config
        .peers
        .get(peer_name)
        .ok_or_else(|| AisyncError::Config(format!("peer '{peer_name}' not found")))?;
    let endpoint = live
        .as_ref()
        .and_then(|connection| connection.endpoint)
        .or(peer.endpoint)
        .ok_or_else(|| {
            AisyncError::Config(format!(
                "peer '{peer_name}' has no endpoint; configure peer.endpoint before syncing"
            ))
        })?;
    let (receiver_cert_der, cert_source) = if let Some(cert) = live
        .as_ref()
        .and_then(|connection| connection.receiver_cert_der.clone())
    {
        (cert, "discovery".to_string())
    } else {
        let server_cert_path = peer
            .server_cert
            .clone()
            .unwrap_or_else(|| config_path.with_file_name("receiver.der"));
        (
            fs::read(&server_cert_path).map_err(|error| {
                AisyncError::Transport(format!(
                    "server certificate not found at {}: {}",
                    server_cert_path.display(),
                    error
                ))
            })?,
            "config".to_string(),
        )
    };
    let server_name = live
        .as_ref()
        .and_then(|connection| connection.server_name.clone())
        .or_else(|| peer.server_name.clone())
        .unwrap_or_else(|| "aisync-receiver".to_string());
    let peer_info = with_endpoint_first(peer_device_info(config, peer_name)?, Some(endpoint));
    Ok(PeerTransportConnection {
        peer: peer_info,
        endpoint,
        receiver_cert_der,
        server_name,
        cert_source,
    })
}

pub(crate) struct ProjectMappingAckConnection {
    pub(crate) endpoint: SocketAddr,
    pub(crate) receiver_cert_der: Vec<u8>,
    pub(crate) server_name: String,
    pub(crate) cert_source: String,
}

pub(crate) fn project_mapping_ack_connection(
    live: Option<codebaton_discovery::PeerConnectionInfo>,
    request: &ProjectMappingRequestPayload,
) -> Result<ProjectMappingAckConnection> {
    let endpoint = live
        .as_ref()
        .and_then(|connection| connection.endpoint)
        .or(request.endpoint)
        .ok_or_else(|| {
            AisyncError::Config("project mapping requester has no endpoint".to_string())
        })?;
    let (receiver_cert_der, cert_source) = if let Some(cert) = live
        .as_ref()
        .and_then(|connection| connection.receiver_cert_der.clone())
    {
        (cert, "discovery".to_string())
    } else {
        (
            request.receiver_cert_der.clone().ok_or_else(|| {
                AisyncError::Config("project mapping requester has no pinned receiver cert".into())
            })?,
            "request".to_string(),
        )
    };
    let server_name = live
        .as_ref()
        .and_then(|connection| connection.server_name.clone())
        .or_else(|| request.server_name.clone())
        .unwrap_or_else(|| "aisync-receiver".to_string());
    Ok(ProjectMappingAckConnection {
        endpoint,
        receiver_cert_der,
        server_name,
        cert_source,
    })
}

pub(crate) fn workspace_mapping_ack_connection(
    live: Option<codebaton_discovery::PeerConnectionInfo>,
    request: &WorkspaceMappingRequestPayload,
) -> Result<ProjectMappingAckConnection> {
    let endpoint = live
        .as_ref()
        .and_then(|connection| connection.endpoint)
        .or(request.endpoint)
        .ok_or_else(|| {
            AisyncError::Config("workspace mapping requester has no endpoint".to_string())
        })?;
    let (receiver_cert_der, cert_source) = if let Some(cert) = live
        .as_ref()
        .and_then(|connection| connection.receiver_cert_der.clone())
    {
        (cert, "discovery".to_string())
    } else {
        (
            request.receiver_cert_der.clone().ok_or_else(|| {
                AisyncError::Config(
                    "workspace mapping requester has no pinned receiver cert".into(),
                )
            })?,
            "request".to_string(),
        )
    };
    let server_name = live
        .as_ref()
        .and_then(|connection| connection.server_name.clone())
        .or_else(|| request.server_name.clone())
        .unwrap_or_else(|| "aisync-receiver".to_string());
    Ok(ProjectMappingAckConnection {
        endpoint,
        receiver_cert_der,
        server_name,
        cert_source,
    })
}

pub(crate) fn file_transfer_ack_connection(
    live: Option<codebaton_discovery::PeerConnectionInfo>,
    request: &FileTransferRequestPayload,
) -> Result<ProjectMappingAckConnection> {
    let endpoint = live
        .as_ref()
        .and_then(|connection| connection.endpoint)
        .or(request.endpoint)
        .ok_or_else(|| {
            AisyncError::Config("file transfer requester has no endpoint".to_string())
        })?;
    let (receiver_cert_der, cert_source) = if let Some(cert) = live
        .as_ref()
        .and_then(|connection| connection.receiver_cert_der.clone())
    {
        (cert, "discovery".to_string())
    } else {
        (
            request.receiver_cert_der.clone().ok_or_else(|| {
                AisyncError::Config("file transfer requester has no pinned receiver cert".into())
            })?,
            "request".to_string(),
        )
    };
    let server_name = live
        .as_ref()
        .and_then(|connection| connection.server_name.clone())
        .or_else(|| request.server_name.clone())
        .unwrap_or_else(|| "aisync-receiver".to_string());
    Ok(ProjectMappingAckConnection {
        endpoint,
        receiver_cert_der,
        server_name,
        cert_source,
    })
}
