use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aisync_core::{AisyncError, DeviceInfo, FileEntry, Result, SyncManifest, Transporter};
use fast_rsync::{apply, diff, Signature, SignatureOptions};
use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use rcgen::{CertificateParams, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as RustlsError, ServerConfig, SignatureScheme,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use walkdir::WalkDir;

pub const PROTOCOL_VERSION: u32 = 2;
pub const SMALL_FILE_THRESHOLD: u64 = 64 * 1024;
const FILE_CHUNK_SIZE: usize = 1024 * 1024;
const SMALL_FILE_BATCH_LIMIT: u64 = 8 * 1024 * 1024;
const FRAME_TYPE_SIZE: usize = 1;
const MAX_FRAME_SIZE: usize = 512 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_BODY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct TlsIdentity {
    pub cert_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub identity: TlsIdentity,
    pub pinned_peer_cert_der: Option<Vec<u8>>,
    pub server_name: String,
}

impl TlsConfig {
    pub fn new(identity: TlsIdentity, server_name: impl Into<String>) -> Self {
        Self {
            identity,
            pinned_peer_cert_der: None,
            server_name: server_name.into(),
        }
    }

    pub fn with_pinned_peer_cert(mut self, cert_der: Vec<u8>) -> Self {
        self.pinned_peer_cert_der = Some(cert_der);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Hello = 1,
    FileManifest = 2,
    FileSignatures = 3,
    FileDelta = 4,
    FileBatch = 5,
    FileDelete = 6,
    SessionData = 7,
    SyncComplete = 8,
    PairingRequest = 9,
    PairingAck = 10,
    ProjectMappingRequest = 11,
    ProjectMappingAck = 12,
    WorkspaceMappingRequest = 13,
    WorkspaceMappingAck = 14,
    FileChunk = 15,
    TextMessage = 16,
    FileTransferRequest = 17,
    FileTransferData = 18,
    FileTransferAck = 19,
    Error = 255,
}

impl TryFrom<u8> for MessageType {
    type Error = AisyncError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::FileManifest),
            3 => Ok(Self::FileSignatures),
            4 => Ok(Self::FileDelta),
            5 => Ok(Self::FileBatch),
            6 => Ok(Self::FileDelete),
            7 => Ok(Self::SessionData),
            8 => Ok(Self::SyncComplete),
            9 => Ok(Self::PairingRequest),
            10 => Ok(Self::PairingAck),
            11 => Ok(Self::ProjectMappingRequest),
            12 => Ok(Self::ProjectMappingAck),
            13 => Ok(Self::WorkspaceMappingRequest),
            14 => Ok(Self::WorkspaceMappingAck),
            15 => Ok(Self::FileChunk),
            16 => Ok(Self::TextMessage),
            17 => Ok(Self::FileTransferRequest),
            18 => Ok(Self::FileTransferData),
            19 => Ok(Self::FileTransferAck),
            255 => Ok(Self::Error),
            other => Err(transport_err(format!("unknown message type {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequestPayload {
    pub request_id: String,
    pub code: String,
    pub expires_at_unix_secs: u64,
    pub device: DeviceInfo,
    pub endpoint: Option<SocketAddr>,
    pub receiver_cert_der: Option<Vec<u8>>,
    pub server_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMappingRequestPayload {
    pub request_id: String,
    pub project_name: String,
    pub source_dir: PathBuf,
    pub mode: String,
    pub device: DeviceInfo,
    pub endpoint: Option<SocketAddr>,
    pub receiver_cert_der: Option<Vec<u8>>,
    pub server_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMappingAckPayload {
    pub request_id: String,
    pub accepted: bool,
    pub project_name: String,
    pub remote_dir: Option<PathBuf>,
    pub message: Option<String>,
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMappingRequestPayload {
    pub request_id: String,
    pub workspace_name: String,
    pub source_root: PathBuf,
    pub suggested_remote_root: PathBuf,
    pub mode: String,
    pub auto_enable_new: bool,
    pub children: Vec<String>,
    pub device: DeviceInfo,
    pub endpoint: Option<SocketAddr>,
    pub receiver_cert_der: Option<Vec<u8>>,
    pub server_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMappingAckPayload {
    pub request_id: String,
    pub accepted: bool,
    pub workspace_name: String,
    pub remote_root: Option<PathBuf>,
    pub message: Option<String>,
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMessagePayload {
    pub sender_name: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransferRequestPayload {
    pub transfer_id: String,
    pub filename: String,
    pub size: u64,
    pub sender_name: String,
    pub device: DeviceInfo,
    pub endpoint: Option<SocketAddr>,
    pub receiver_cert_der: Option<Vec<u8>>,
    pub server_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransferDataPayload {
    pub transfer_id: String,
    pub offset: u64,
    pub chunk: Vec<u8>,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransferAckPayload {
    pub transfer_id: String,
    pub accepted: bool,
    pub ready: bool,
    pub filename: String,
    pub message: Option<String>,
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello {
        protocol_version: u32,
        device_name: String,
    },
    FileManifest {
        manifest: SyncManifest,
        #[serde(default)]
        remote_dir: Option<PathBuf>,
    },
    FileSignatures {
        signatures: Vec<SignatureEntry>,
    },
    FileDelta {
        path: String,
        base_hash: Option<String>,
        target_hash: String,
        delta: Vec<u8>,
        size: u64,
    },
    FileChunk {
        path: String,
        target_hash: String,
        offset: u64,
        data: Vec<u8>,
        size: u64,
        done: bool,
    },
    FileBatch {
        tar_stream: Vec<u8>,
    },
    FileDelete {
        path: String,
    },
    SessionData {
        project_id: String,
        data: Vec<u8>,
    },
    SyncComplete,
    PairingRequest {
        request: PairingRequestPayload,
    },
    PairingAck {
        request_id: String,
    },
    ProjectMappingRequest {
        request: ProjectMappingRequestPayload,
    },
    ProjectMappingAck {
        ack: ProjectMappingAckPayload,
    },
    WorkspaceMappingRequest {
        request: WorkspaceMappingRequestPayload,
    },
    WorkspaceMappingAck {
        ack: WorkspaceMappingAckPayload,
    },
    TextMessage {
        message: TextMessagePayload,
    },
    FileTransferRequest {
        request: FileTransferRequestPayload,
    },
    FileTransferData {
        data: FileTransferDataPayload,
    },
    FileTransferAck {
        ack: FileTransferAckPayload,
    },
    Error {
        message: String,
    },
}

impl Message {
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Hello { .. } => MessageType::Hello,
            Self::FileManifest { .. } => MessageType::FileManifest,
            Self::FileSignatures { .. } => MessageType::FileSignatures,
            Self::FileDelta { .. } => MessageType::FileDelta,
            Self::FileChunk { .. } => MessageType::FileChunk,
            Self::FileBatch { .. } => MessageType::FileBatch,
            Self::FileDelete { .. } => MessageType::FileDelete,
            Self::SessionData { .. } => MessageType::SessionData,
            Self::SyncComplete => MessageType::SyncComplete,
            Self::PairingRequest { .. } => MessageType::PairingRequest,
            Self::PairingAck { .. } => MessageType::PairingAck,
            Self::ProjectMappingRequest { .. } => MessageType::ProjectMappingRequest,
            Self::ProjectMappingAck { .. } => MessageType::ProjectMappingAck,
            Self::WorkspaceMappingRequest { .. } => MessageType::WorkspaceMappingRequest,
            Self::WorkspaceMappingAck { .. } => MessageType::WorkspaceMappingAck,
            Self::TextMessage { .. } => MessageType::TextMessage,
            Self::FileTransferRequest { .. } => MessageType::FileTransferRequest,
            Self::FileTransferData { .. } => MessageType::FileTransferData,
            Self::FileTransferAck { .. } => MessageType::FileTransferAck,
            Self::Error { .. } => MessageType::Error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureEntry {
    pub relative_path: String,
    pub base_hash: String,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub added: Vec<FileEntry>,
    pub modified: Vec<FileEntry>,
    pub deleted: Vec<FileEntry>,
    pub unchanged: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub current_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveFile {
    pub relative_path: String,
    pub matched_pattern: String,
}

pub type ProgressCallback<'a> = dyn Fn(Progress) + Send + Sync + 'a;
pub type PairingRequestCallback<'a> = dyn Fn(PairingRequestPayload) + Send + Sync + 'a;
pub type ProjectMappingRequestCallback<'a> =
    dyn Fn(ProjectMappingRequestPayload) + Send + Sync + 'a;
pub type ProjectMappingAckCallback<'a> =
    dyn Fn(ProjectMappingAckPayload) -> Result<()> + Send + Sync + 'a;
pub type WorkspaceMappingRequestCallback<'a> =
    dyn Fn(WorkspaceMappingRequestPayload) + Send + Sync + 'a;
pub type WorkspaceMappingAckCallback<'a> =
    dyn Fn(WorkspaceMappingAckPayload) -> Result<()> + Send + Sync + 'a;
pub type TextMessageCallback<'a> = dyn Fn(TextMessagePayload) -> Result<()> + Send + Sync + 'a;
pub type FileTransferRequestCallback<'a> = dyn Fn(FileTransferRequestPayload) + Send + Sync + 'a;
pub type FileTransferAckCallback<'a> =
    dyn Fn(FileTransferAckPayload) -> Result<()> + Send + Sync + 'a;
pub type FileTransferDataCallback<'a> =
    dyn Fn(FileTransferDataPayload) -> Result<()> + Send + Sync + 'a;

pub struct TcpTransporter {
    stream: tokio_rustls::client::TlsStream<TcpStream>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncExchange {
    pub source_manifest: SyncManifest,
    pub remote_manifest: SyncManifest,
}

impl TcpTransporter {
    pub async fn connect_addr(addr: SocketAddr, tls: &TlsConfig) -> Result<Self> {
        let started = Instant::now();
        trace_stage("tcp_connect_start", format!("peer={addr}"));
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                transport_err(format!(
                    "TCP connect timed out after {}ms to {addr}",
                    CONNECT_TIMEOUT.as_millis()
                ))
            })??;
        tcp.set_nodelay(true)?;
        trace_stage(
            "tcp_connect_done",
            format!("peer={addr} elapsed_ms={}", started.elapsed().as_millis()),
        );
        let connector = TlsConnector::from(Arc::new(client_config(tls)?));
        let server_name = ServerName::try_from(tls.server_name.clone())
            .map_err(|error| transport_err(format!("invalid TLS server name: {error}")))?;
        let started = Instant::now();
        trace_stage(
            "tls_connect_start",
            format!("peer={addr} server_name={}", tls.server_name),
        );
        let stream =
            tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp))
                .await
                .map_err(|_| {
                    transport_err(format!(
                        "TLS connect timed out after {}ms to {addr}",
                        TLS_HANDSHAKE_TIMEOUT.as_millis()
                    ))
                })?
                .map_err(|error| transport_err(format!("TLS connect: {error}")))?;
        trace_stage(
            "tls_connect_done",
            format!("peer={addr} elapsed_ms={}", started.elapsed().as_millis()),
        );

        Ok(Self { stream })
    }

    pub async fn connect_to_peer(peer: &DeviceInfo, port: u16, tls: &TlsConfig) -> Result<Self> {
        let mut last_error = None;
        for ip in &peer.addresses {
            let addr = SocketAddr::new(*ip, port);
            trace_stage(
                "peer_connect_attempt",
                format!("peer_name={} peer={addr}", peer.name),
            );
            match Self::connect_addr(addr, tls).await {
                Ok(transporter) => return Ok(transporter),
                Err(error) => last_error = Some(error),
            }
        }

        match last_error {
            Some(error) => Err(error),
            None => Err(transport_err(format!(
                "peer '{}' has no addresses",
                peer.name
            ))),
        }
    }

    pub async fn sync_directory(
        &mut self,
        source_dir: &Path,
        progress: Option<&ProgressCallback<'_>>,
    ) -> Result<SyncManifest> {
        self.sync_directory_to(source_dir, None, progress).await
    }

    pub async fn send_pairing_request(&mut self, request: PairingRequestPayload) -> Result<()> {
        let request_id = request.request_id.clone();
        let started = Instant::now();
        trace_stage("hello_write_start", "role=client purpose=pairing");
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: request.device.name.clone(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=pairing elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage("hello_read_start", "role=client purpose=pairing");
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=pairing elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        trace_stage(
            "pairing_request_write_start",
            format!("request_id={request_id}"),
        );
        write_message(&mut self.stream, &Message::PairingRequest { request }).await?;
        match read_message(&mut self.stream).await? {
            Message::PairingAck { request_id: ack } if ack == request_id => {
                trace_stage("pairing_request_ack", format!("request_id={request_id}"));
                Ok(())
            }
            Message::PairingAck { request_id: ack } => Err(transport_err(format!(
                "pairing ack request mismatch: expected {request_id}, got {ack}"
            ))),
            Message::Error { message } => Err(transport_err(message)),
            other => Err(transport_err(format!(
                "expected PairingAck, got {:?}",
                other.message_type()
            ))),
        }
    }

    pub async fn send_project_mapping_request(
        &mut self,
        request: ProjectMappingRequestPayload,
    ) -> Result<()> {
        let request_id = request.request_id.clone();
        let started = Instant::now();
        trace_stage("hello_write_start", "role=client purpose=project_mapping");
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: request.device.name.clone(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=project_mapping elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage("hello_read_start", "role=client purpose=project_mapping");
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=project_mapping elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        trace_stage(
            "project_mapping_request_write_start",
            format!("request_id={request_id}"),
        );
        write_message(
            &mut self.stream,
            &Message::ProjectMappingRequest { request },
        )
        .await?;
        match read_message(&mut self.stream).await? {
            Message::ProjectMappingAck { ack } if ack.request_id == request_id => {
                trace_stage(
                    "project_mapping_request_received_ack",
                    format!("request_id={request_id} accepted={}", ack.accepted),
                );
                if ack.accepted {
                    Ok(())
                } else {
                    Err(transport_err(ack.message.unwrap_or_else(|| {
                        "project mapping request rejected".to_string()
                    })))
                }
            }
            Message::ProjectMappingAck { ack } => Err(transport_err(format!(
                "project mapping ack request mismatch: expected {request_id}, got {}",
                ack.request_id
            ))),
            Message::Error { message } => Err(transport_err(message)),
            other => Err(transport_err(format!(
                "expected ProjectMappingAck, got {:?}",
                other.message_type()
            ))),
        }
    }

    pub async fn send_project_mapping_ack(&mut self, ack: ProjectMappingAckPayload) -> Result<()> {
        let request_id = ack.request_id.clone();
        let started = Instant::now();
        trace_stage(
            "hello_write_start",
            "role=client purpose=project_mapping_ack",
        );
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: ack.device.name.clone(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=project_mapping_ack elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage(
            "hello_read_start",
            "role=client purpose=project_mapping_ack",
        );
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=project_mapping_ack elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        trace_stage(
            "project_mapping_ack_write_start",
            format!("request_id={request_id} accepted={}", ack.accepted),
        );
        write_message(&mut self.stream, &Message::ProjectMappingAck { ack }).await?;
        match read_message(&mut self.stream).await? {
            Message::ProjectMappingAck { ack } if ack.request_id == request_id => {
                trace_stage(
                    "project_mapping_ack_confirmed",
                    format!("request_id={request_id} accepted={}", ack.accepted),
                );
                if ack.accepted {
                    Ok(())
                } else {
                    Err(transport_err(ack.message.unwrap_or_else(|| {
                        "project mapping ack rejected".to_string()
                    })))
                }
            }
            Message::ProjectMappingAck { ack } => Err(transport_err(format!(
                "project mapping ack confirm mismatch: expected {request_id}, got {}",
                ack.request_id
            ))),
            Message::Error { message } => Err(transport_err(message)),
            other => Err(transport_err(format!(
                "expected ProjectMappingAck confirmation, got {:?}",
                other.message_type()
            ))),
        }
    }

    pub async fn send_workspace_mapping_request(
        &mut self,
        request: WorkspaceMappingRequestPayload,
    ) -> Result<()> {
        let request_id = request.request_id.clone();
        let started = Instant::now();
        trace_stage("hello_write_start", "role=client purpose=workspace_mapping");
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: request.device.name.clone(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=workspace_mapping elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage("hello_read_start", "role=client purpose=workspace_mapping");
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=workspace_mapping elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        trace_stage(
            "workspace_mapping_request_write_start",
            format!("request_id={request_id}"),
        );
        write_message(
            &mut self.stream,
            &Message::WorkspaceMappingRequest { request },
        )
        .await?;
        match read_message(&mut self.stream).await? {
            Message::WorkspaceMappingAck { ack } if ack.request_id == request_id => {
                trace_stage(
                    "workspace_mapping_request_received_ack",
                    format!("request_id={request_id} accepted={}", ack.accepted),
                );
                if ack.accepted {
                    Ok(())
                } else {
                    Err(transport_err(ack.message.unwrap_or_else(|| {
                        "workspace mapping request rejected".to_string()
                    })))
                }
            }
            Message::WorkspaceMappingAck { ack } => Err(transport_err(format!(
                "workspace mapping ack request mismatch: expected {request_id}, got {}",
                ack.request_id
            ))),
            Message::Error { message } => Err(transport_err(message)),
            other => Err(transport_err(format!(
                "expected WorkspaceMappingAck, got {:?}",
                other.message_type()
            ))),
        }
    }

    pub async fn send_workspace_mapping_ack(
        &mut self,
        ack: WorkspaceMappingAckPayload,
    ) -> Result<()> {
        let request_id = ack.request_id.clone();
        let started = Instant::now();
        trace_stage(
            "hello_write_start",
            "role=client purpose=workspace_mapping_ack",
        );
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: ack.device.name.clone(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=workspace_mapping_ack elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage(
            "hello_read_start",
            "role=client purpose=workspace_mapping_ack",
        );
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=workspace_mapping_ack elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        trace_stage(
            "workspace_mapping_ack_write_start",
            format!("request_id={request_id} accepted={}", ack.accepted),
        );
        write_message(&mut self.stream, &Message::WorkspaceMappingAck { ack }).await?;
        match read_message(&mut self.stream).await? {
            Message::WorkspaceMappingAck { ack } if ack.request_id == request_id => {
                trace_stage(
                    "workspace_mapping_ack_confirmed",
                    format!("request_id={request_id} accepted={}", ack.accepted),
                );
                if ack.accepted {
                    Ok(())
                } else {
                    Err(transport_err(ack.message.unwrap_or_else(|| {
                        "workspace mapping ack rejected".to_string()
                    })))
                }
            }
            Message::WorkspaceMappingAck { ack } => Err(transport_err(format!(
                "workspace mapping ack confirm mismatch: expected {request_id}, got {}",
                ack.request_id
            ))),
            Message::Error { message } => Err(transport_err(message)),
            other => Err(transport_err(format!(
                "expected WorkspaceMappingAck confirmation, got {:?}",
                other.message_type()
            ))),
        }
    }

    pub async fn send_text_message(&mut self, message: TextMessagePayload) -> Result<()> {
        let started = Instant::now();
        trace_stage("hello_write_start", "role=client purpose=text_message");
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: message.sender_name.clone(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=text_message elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage("hello_read_start", "role=client purpose=text_message");
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=text_message elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        trace_stage(
            "text_message_write_start",
            format!("sender={}", message.sender_name),
        );
        write_message(&mut self.stream, &Message::TextMessage { message }).await
    }

    pub async fn send_file_transfer_request(
        &mut self,
        request: FileTransferRequestPayload,
    ) -> Result<()> {
        let transfer_id = request.transfer_id.clone();
        let started = Instant::now();
        trace_stage(
            "hello_write_start",
            "role=client purpose=file_transfer_request",
        );
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: request.sender_name.clone(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=file_transfer_request elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage(
            "hello_read_start",
            "role=client purpose=file_transfer_request",
        );
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=file_transfer_request elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        trace_stage(
            "file_transfer_request_write_start",
            format!("transfer_id={transfer_id} filename={}", request.filename),
        );
        write_message(&mut self.stream, &Message::FileTransferRequest { request }).await?;
        match read_message(&mut self.stream).await? {
            Message::FileTransferAck { ack } if ack.transfer_id == transfer_id => {
                trace_stage(
                    "file_transfer_request_ack",
                    format!(
                        "transfer_id={transfer_id} accepted={} ready={}",
                        ack.accepted, ack.ready
                    ),
                );
                if ack.accepted {
                    Ok(())
                } else {
                    Err(transport_err(ack.message.unwrap_or_else(|| {
                        "file transfer request rejected".to_string()
                    })))
                }
            }
            Message::FileTransferAck { ack } => Err(transport_err(format!(
                "file transfer ack mismatch: expected {transfer_id}, got {}",
                ack.transfer_id
            ))),
            Message::Error { message } => Err(transport_err(message)),
            other => Err(transport_err(format!(
                "expected FileTransferAck, got {:?}",
                other.message_type()
            ))),
        }
    }

    pub async fn send_file_transfer_ack(&mut self, ack: FileTransferAckPayload) -> Result<()> {
        let transfer_id = ack.transfer_id.clone();
        let started = Instant::now();
        trace_stage("hello_write_start", "role=client purpose=file_transfer_ack");
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: ack.device.name.clone(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=file_transfer_ack elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage("hello_read_start", "role=client purpose=file_transfer_ack");
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=file_transfer_ack elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        trace_stage(
            "file_transfer_ack_write_start",
            format!(
                "transfer_id={transfer_id} accepted={} ready={}",
                ack.accepted, ack.ready
            ),
        );
        write_message(&mut self.stream, &Message::FileTransferAck { ack }).await?;
        match read_message(&mut self.stream).await? {
            Message::FileTransferAck { ack } if ack.transfer_id == transfer_id => {
                if ack.accepted {
                    Ok(())
                } else {
                    Err(transport_err(ack.message.unwrap_or_else(|| {
                        "file transfer ack rejected".to_string()
                    })))
                }
            }
            Message::FileTransferAck { ack } => Err(transport_err(format!(
                "file transfer ack confirm mismatch: expected {transfer_id}, got {}",
                ack.transfer_id
            ))),
            Message::Error { message } => Err(transport_err(message)),
            other => Err(transport_err(format!(
                "expected FileTransferAck confirmation, got {:?}",
                other.message_type()
            ))),
        }
    }

    pub async fn send_file_transfer_data(
        &mut self,
        transfer_id: String,
        source_path: &Path,
    ) -> Result<()> {
        let started = Instant::now();
        trace_stage(
            "hello_write_start",
            "role=client purpose=file_transfer_data",
        );
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "aisync-client".to_string(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=client purpose=file_transfer_data elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage("hello_read_start", "role=client purpose=file_transfer_data");
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=client purpose=file_transfer_data elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let mut file = tokio::fs::File::open(source_path).await?;
        let mut offset = 0u64;
        let mut buffer = vec![0u8; FILE_CHUNK_SIZE];
        loop {
            let n = file.read(&mut buffer).await?;
            let done = n == 0;
            let data = FileTransferDataPayload {
                transfer_id: transfer_id.clone(),
                offset,
                chunk: buffer[..n].to_vec(),
                done,
            };
            write_message(&mut self.stream, &Message::FileTransferData { data }).await?;
            match read_message(&mut self.stream).await? {
                Message::FileTransferAck { ack } if ack.transfer_id == transfer_id => {
                    if !ack.accepted {
                        return Err(transport_err(
                            ack.message
                                .unwrap_or_else(|| "file transfer data rejected".to_string()),
                        ));
                    }
                }
                Message::FileTransferAck { ack } => {
                    return Err(transport_err(format!(
                        "file transfer data ack mismatch: expected {transfer_id}, got {}",
                        ack.transfer_id
                    )));
                }
                Message::Error { message } => return Err(transport_err(message)),
                other => {
                    return Err(transport_err(format!(
                        "expected FileTransferAck, got {:?}",
                        other.message_type()
                    )));
                }
            }
            if done {
                return Ok(());
            }
            offset += n as u64;
        }
    }

    pub async fn sync_directory_to(
        &mut self,
        source_dir: &Path,
        remote_dir: Option<&Path>,
        progress: Option<&ProgressCallback<'_>>,
    ) -> Result<SyncManifest> {
        Ok(self
            .sync_directory_to_checked(source_dir, remote_dir, progress, |_, _| Ok(()))
            .await?
            .source_manifest)
    }

    pub async fn sync_directory_to_checked<F>(
        &mut self,
        source_dir: &Path,
        remote_dir: Option<&Path>,
        progress: Option<&ProgressCallback<'_>>,
        preflight: F,
    ) -> Result<SyncExchange>
    where
        F: FnOnce(&SyncManifest, &SyncManifest) -> Result<()>,
    {
        let started = Instant::now();
        trace_stage("hello_write_start", "role=client");
        write_message(
            &mut self.stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "aisync-client".to_string(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!("role=client elapsed_ms={}", started.elapsed().as_millis()),
        );

        let started = Instant::now();
        trace_stage("hello_read_start", "role=client");
        expect_hello(read_message(&mut self.stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!("role=client elapsed_ms={}", started.elapsed().as_millis()),
        );

        let started = Instant::now();
        trace_stage(
            "manifest_scan_start",
            format!("dir={}", source_dir.display()),
        );
        let source_manifest = scan_manifest(source_dir)?;
        trace_stage(
            "manifest_scan_done",
            format!(
                "dir={} files={} elapsed_ms={}",
                source_dir.display(),
                source_manifest.files.len(),
                started.elapsed().as_millis()
            ),
        );
        let started = Instant::now();
        trace_stage(
            "manifest_write_start",
            format!(
                "role=client files={} remote_dir={}",
                source_manifest.files.len(),
                remote_dir
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "-".to_string())
            ),
        );
        write_message(
            &mut self.stream,
            &Message::FileManifest {
                manifest: source_manifest.clone(),
                remote_dir: remote_dir.map(Path::to_path_buf),
            },
        )
        .await?;
        trace_stage(
            "manifest_write_done",
            format!("role=client elapsed_ms={}", started.elapsed().as_millis()),
        );

        let started = Instant::now();
        trace_stage("manifest_read_start", "role=client");
        let remote_manifest = match read_message(&mut self.stream).await? {
            Message::FileManifest { manifest, .. } => manifest,
            Message::Error { message } => return Err(transport_err(message)),
            other => {
                return Err(transport_err(format!(
                    "expected FileManifest, got {:?}",
                    other.message_type()
                )));
            }
        };
        trace_stage(
            "manifest_read_done",
            format!(
                "role=client files={} elapsed_ms={}",
                remote_manifest.files.len(),
                started.elapsed().as_millis()
            ),
        );
        let started = Instant::now();
        trace_stage("signatures_read_start", "role=client");
        let signatures = match read_message(&mut self.stream).await? {
            Message::FileSignatures { signatures } => signatures,
            Message::Error { message } => return Err(transport_err(message)),
            other => {
                return Err(transport_err(format!(
                    "expected FileSignatures, got {:?}",
                    other.message_type()
                )));
            }
        };
        trace_stage(
            "signatures_read_done",
            format!(
                "role=client signatures={} elapsed_ms={}",
                signatures.len(),
                started.elapsed().as_millis()
            ),
        );

        if let Err(error) = preflight(&source_manifest, &remote_manifest) {
            let _ = write_message(
                &mut self.stream,
                &Message::Error {
                    message: error.to_string(),
                },
            )
            .await;
            return Err(error);
        }

        let started = Instant::now();
        trace_stage("diff_send_start", "role=client");
        send_diff(
            &mut self.stream,
            source_dir,
            &source_manifest,
            &remote_manifest,
            &signatures,
            progress,
        )
        .await?;
        trace_stage(
            "diff_send_done",
            format!("role=client elapsed_ms={}", started.elapsed().as_millis()),
        );
        let started = Instant::now();
        trace_stage("sync_complete_write_start", "role=client");
        write_message(&mut self.stream, &Message::SyncComplete).await?;
        trace_stage(
            "sync_complete_write_done",
            format!("role=client elapsed_ms={}", started.elapsed().as_millis()),
        );

        let started = Instant::now();
        trace_stage("sync_complete_read_start", "role=client");
        match read_message(&mut self.stream).await? {
            Message::SyncComplete => {
                trace_stage(
                    "sync_complete_read_done",
                    format!("role=client elapsed_ms={}", started.elapsed().as_millis()),
                );
                Ok(SyncExchange {
                    source_manifest,
                    remote_manifest,
                })
            }
            Message::Error { message } => Err(transport_err(message)),
            other => Err(transport_err(format!(
                "expected SyncComplete, got {:?}",
                other.message_type()
            ))),
        }
    }
}

pub struct TransportServer {
    listener: StdTcpListener,
    acceptor: TlsAcceptor,
}

impl TransportServer {
    pub async fn bind(addr: SocketAddr, tls: &TlsConfig) -> Result<Self> {
        let listener = StdTcpListener::bind(addr)?;
        let acceptor = TlsAcceptor::from(Arc::new(server_config(tls)?));
        Ok(Self { listener, acceptor })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener.local_addr().map_err(Into::into)
    }

    pub async fn receive_once(
        &self,
        target_dir: &Path,
        progress: Option<&ProgressCallback<'_>>,
    ) -> Result<SyncManifest> {
        self.receive_once_with_control_handlers(
            target_dir, progress, None, None, None, None, None, None, None, None, None,
        )
        .await
    }

    pub async fn receive_once_with_pairing_handler(
        &self,
        target_dir: &Path,
        progress: Option<&ProgressCallback<'_>>,
        pairing_handler: Option<&PairingRequestCallback<'_>>,
    ) -> Result<SyncManifest> {
        self.receive_once_with_control_handlers(
            target_dir,
            progress,
            pairing_handler,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
    }

    pub async fn receive_once_with_control_handlers(
        &self,
        target_dir: &Path,
        progress: Option<&ProgressCallback<'_>>,
        pairing_handler: Option<&PairingRequestCallback<'_>>,
        project_mapping_request_handler: Option<&ProjectMappingRequestCallback<'_>>,
        project_mapping_ack_handler: Option<&ProjectMappingAckCallback<'_>>,
        workspace_mapping_request_handler: Option<&WorkspaceMappingRequestCallback<'_>>,
        workspace_mapping_ack_handler: Option<&WorkspaceMappingAckCallback<'_>>,
        text_message_handler: Option<&TextMessageCallback<'_>>,
        file_transfer_request_handler: Option<&FileTransferRequestCallback<'_>>,
        file_transfer_ack_handler: Option<&FileTransferAckCallback<'_>>,
        file_transfer_data_handler: Option<&FileTransferDataCallback<'_>>,
    ) -> Result<SyncManifest> {
        trace_stage(
            "accept_wait",
            format!("listen={}", self.local_addr()?.to_string()),
        );
        let started = Instant::now();
        let (tcp, peer_addr) = self.listener.accept()?;
        tcp.set_nodelay(true)?;
        tcp.set_nonblocking(true)?;
        let tcp = TcpStream::from_std(tcp)?;
        trace_stage(
            "accept_done",
            format!(
                "peer={peer_addr} elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );
        let started = Instant::now();
        trace_stage("tls_accept_start", format!("peer={peer_addr}"));
        let mut stream = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, self.acceptor.accept(tcp))
            .await
            .map_err(|_| {
                transport_err(format!(
                    "TLS accept timed out after {}ms from {peer_addr}",
                    TLS_HANDSHAKE_TIMEOUT.as_millis()
                ))
            })?
            .map_err(|error| transport_err(format!("TLS accept: {error}")))?;
        trace_stage(
            "tls_accept_done",
            format!(
                "peer={peer_addr} elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage("hello_read_start", format!("role=server peer={peer_addr}"));
        expect_hello(read_message(&mut stream).await?)?;
        trace_stage(
            "hello_read_done",
            format!(
                "role=server peer={peer_addr} elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );
        let started = Instant::now();
        trace_stage("hello_write_start", format!("role=server peer={peer_addr}"));
        write_message(
            &mut stream,
            &Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "aisync-server".to_string(),
            },
        )
        .await?;
        trace_stage(
            "hello_write_done",
            format!(
                "role=server peer={peer_addr} elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage(
            "message_read_start",
            format!("role=server peer={peer_addr}"),
        );
        let message = read_message(&mut stream).await?;
        if let Message::PairingRequest { request } = message {
            let request_id = request.request_id.clone();
            trace_stage(
                "pairing_request_received",
                format!(
                    "role=server peer={peer_addr} request_id={} device_id={} device_name={}",
                    request_id, request.device.id.0, request.device.name
                ),
            );
            if let Some(handler) = pairing_handler {
                handler(request);
            }
            write_message(&mut stream, &Message::PairingAck { request_id }).await?;
            return Ok(SyncManifest { files: Vec::new() });
        }
        if let Message::ProjectMappingRequest { request } = message {
            let request_id = request.request_id.clone();
            let project_name = request.project_name.clone();
            trace_stage(
                "project_mapping_request_received",
                format!(
                    "role=server peer={peer_addr} request_id={} project_name={} source_dir={}",
                    request_id,
                    project_name,
                    request.source_dir.display()
                ),
            );
            if let Some(handler) = project_mapping_request_handler {
                handler(request);
            }
            write_message(
                &mut stream,
                &Message::ProjectMappingAck {
                    ack: ProjectMappingAckPayload {
                        request_id,
                        accepted: true,
                        project_name,
                        remote_dir: None,
                        message: None,
                        device: server_device_info(),
                    },
                },
            )
            .await?;
            return Ok(SyncManifest { files: Vec::new() });
        }
        if let Message::ProjectMappingAck { ack } = message {
            let request_id = ack.request_id.clone();
            trace_stage(
                "project_mapping_ack_received",
                format!(
                    "role=server peer={peer_addr} request_id={} accepted={} project_name={}",
                    request_id, ack.accepted, ack.project_name
                ),
            );
            let result = match project_mapping_ack_handler {
                Some(handler) => handler(ack.clone()),
                None => Ok(()),
            };
            let confirm = match result {
                Ok(()) => ProjectMappingAckPayload {
                    accepted: true,
                    message: None,
                    ..ack
                },
                Err(error) => ProjectMappingAckPayload {
                    accepted: false,
                    message: Some(error.to_string()),
                    ..ack
                },
            };
            write_message(&mut stream, &Message::ProjectMappingAck { ack: confirm }).await?;
            return Ok(SyncManifest { files: Vec::new() });
        }
        if let Message::WorkspaceMappingRequest { request } = message {
            let request_id = request.request_id.clone();
            let workspace_name = request.workspace_name.clone();
            trace_stage(
                "workspace_mapping_request_received",
                format!(
                    "role=server peer={peer_addr} request_id={} workspace={} source_root={} suggested_remote_root={}",
                    request_id,
                    workspace_name,
                    request.source_root.display(),
                    request.suggested_remote_root.display()
                ),
            );
            if let Some(handler) = workspace_mapping_request_handler {
                handler(request);
            }
            write_message(
                &mut stream,
                &Message::WorkspaceMappingAck {
                    ack: WorkspaceMappingAckPayload {
                        request_id,
                        accepted: true,
                        workspace_name,
                        remote_root: None,
                        message: None,
                        device: server_device_info(),
                    },
                },
            )
            .await?;
            return Ok(SyncManifest { files: Vec::new() });
        }
        if let Message::WorkspaceMappingAck { ack } = message {
            let request_id = ack.request_id.clone();
            trace_stage(
                "workspace_mapping_ack_received",
                format!(
                    "role=server peer={peer_addr} request_id={} accepted={} workspace={}",
                    request_id, ack.accepted, ack.workspace_name
                ),
            );
            let result = match workspace_mapping_ack_handler {
                Some(handler) => handler(ack.clone()),
                None => Ok(()),
            };
            let confirm = match result {
                Ok(()) => WorkspaceMappingAckPayload {
                    accepted: true,
                    message: None,
                    ..ack
                },
                Err(error) => WorkspaceMappingAckPayload {
                    accepted: false,
                    message: Some(error.to_string()),
                    ..ack
                },
            };
            write_message(&mut stream, &Message::WorkspaceMappingAck { ack: confirm }).await?;
            return Ok(SyncManifest { files: Vec::new() });
        }
        if let Message::TextMessage { message } = message {
            trace_stage(
                "text_message_received",
                format!(
                    "role=server peer={peer_addr} sender={} bytes={}",
                    message.sender_name,
                    message.content.len()
                ),
            );
            if let Some(handler) = text_message_handler {
                if let Err(error) = handler(message) {
                    let _ = write_message(
                        &mut stream,
                        &Message::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    return Err(error);
                }
            }
            return Ok(SyncManifest { files: Vec::new() });
        }
        if let Message::FileTransferRequest { request } = message {
            let transfer_id = request.transfer_id.clone();
            let filename = request.filename.clone();
            trace_stage(
                "file_transfer_request_received",
                format!(
                    "role=server peer={peer_addr} transfer_id={} filename={} size={}",
                    transfer_id, filename, request.size
                ),
            );
            if let Some(handler) = file_transfer_request_handler {
                handler(request);
            }
            write_message(
                &mut stream,
                &Message::FileTransferAck {
                    ack: FileTransferAckPayload {
                        transfer_id,
                        accepted: true,
                        ready: false,
                        filename,
                        message: None,
                        device: server_device_info(),
                    },
                },
            )
            .await?;
            return Ok(SyncManifest { files: Vec::new() });
        }
        if let Message::FileTransferAck { ack } = message {
            let transfer_id = ack.transfer_id.clone();
            trace_stage(
                "file_transfer_ack_received",
                format!(
                    "role=server peer={peer_addr} transfer_id={} accepted={} ready={}",
                    transfer_id, ack.accepted, ack.ready
                ),
            );
            let result = match file_transfer_ack_handler {
                Some(handler) => handler(ack.clone()),
                None => Ok(()),
            };
            let confirm = match result {
                Ok(()) => FileTransferAckPayload {
                    accepted: true,
                    message: None,
                    ..ack
                },
                Err(error) => FileTransferAckPayload {
                    accepted: false,
                    message: Some(error.to_string()),
                    ..ack
                },
            };
            write_message(&mut stream, &Message::FileTransferAck { ack: confirm }).await?;
            return Ok(SyncManifest { files: Vec::new() });
        }
        if let Message::FileTransferData { data } = message {
            let mut next = Some(data);
            while let Some(data) = next {
                let transfer_id = data.transfer_id.clone();
                let done = data.done;
                let result = match file_transfer_data_handler {
                    Some(handler) => handler(data),
                    None => Ok(()),
                };
                let ack = match result {
                    Ok(()) => FileTransferAckPayload {
                        transfer_id: transfer_id.clone(),
                        accepted: true,
                        ready: true,
                        filename: String::new(),
                        message: None,
                        device: server_device_info(),
                    },
                    Err(error) => FileTransferAckPayload {
                        transfer_id: transfer_id.clone(),
                        accepted: false,
                        ready: true,
                        filename: String::new(),
                        message: Some(error.to_string()),
                        device: server_device_info(),
                    },
                };
                write_message(&mut stream, &Message::FileTransferAck { ack }).await?;
                if done {
                    return Ok(SyncManifest { files: Vec::new() });
                }
                next = match read_message(&mut stream).await? {
                    Message::FileTransferData { data } => Some(data),
                    Message::Error { message } => return Err(transport_err(message)),
                    other => {
                        return Err(transport_err(format!(
                            "expected FileTransferData, got {:?}",
                            other.message_type()
                        )));
                    }
                };
            }
            return Ok(SyncManifest { files: Vec::new() });
        }

        let (source_manifest, remote_dir) = match message {
            Message::FileManifest {
                manifest,
                remote_dir,
            } => (manifest, remote_dir),
            Message::Error { message } => return Err(transport_err(message)),
            other => {
                let error = format!("expected FileManifest, got {:?}", other.message_type());
                let _ = write_message(
                    &mut stream,
                    &Message::Error {
                        message: error.clone(),
                    },
                )
                .await;
                return Err(transport_err(error));
            }
        };
        let target_dir = remote_dir
            .map(expand_remote_dir)
            .unwrap_or_else(|| target_dir.to_path_buf());
        if !target_dir.exists() {
            match fs::create_dir_all(&target_dir) {
                Ok(()) => trace_stage(
                    "workspace_remote_dir_created",
                    format!("role=server peer={peer_addr} path={}", target_dir.display()),
                ),
                Err(error) => {
                    trace_stage(
                        "workspace_remote_dir_create_failed",
                        format!(
                            "role=server peer={peer_addr} path={} error={}",
                            target_dir.display(),
                            error
                        ),
                    );
                    return Err(error.into());
                }
            }
        }
        trace_stage(
            "manifest_read_done",
            format!(
                "role=server peer={peer_addr} files={} remote_dir={} elapsed_ms={}",
                source_manifest.files.len(),
                target_dir.display(),
                started.elapsed().as_millis()
            ),
        );

        let started = Instant::now();
        trace_stage(
            "manifest_scan_start",
            format!("role=server target={}", target_dir.display()),
        );
        let target_manifest = scan_manifest(&target_dir)?;
        let signatures = build_signature_entries(&target_dir, &source_manifest, &target_manifest)?;
        trace_stage(
            "manifest_scan_done",
            format!(
                "role=server files={} signatures={} elapsed_ms={}",
                target_manifest.files.len(),
                signatures.len(),
                started.elapsed().as_millis()
            ),
        );
        let started = Instant::now();
        trace_stage(
            "manifest_write_start",
            format!(
                "role=server peer={peer_addr} files={}",
                target_manifest.files.len()
            ),
        );
        write_message(
            &mut stream,
            &Message::FileManifest {
                manifest: target_manifest.clone(),
                remote_dir: None,
            },
        )
        .await?;
        trace_stage(
            "manifest_write_done",
            format!(
                "role=server peer={peer_addr} elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );
        let started = Instant::now();
        trace_stage(
            "signatures_write_start",
            format!(
                "role=server peer={peer_addr} signatures={}",
                signatures.len()
            ),
        );
        write_message(&mut stream, &Message::FileSignatures { signatures }).await?;
        trace_stage(
            "signatures_write_done",
            format!(
                "role=server peer={peer_addr} elapsed_ms={}",
                started.elapsed().as_millis()
            ),
        );

        let staging = prepare_staging(&target_dir)?;
        let started = Instant::now();
        trace_stage(
            "receive_changes_start",
            format!("role=server peer={peer_addr} staging={}", staging.display()),
        );
        let receive_result = receive_changes(&mut stream, &staging, progress)
            .await
            .and_then(|_| verify_manifest_checksums(&staging, &source_manifest));
        trace_stage(
            "receive_changes_done",
            format!(
                "role=server peer={peer_addr} ok={} elapsed_ms={}",
                receive_result.is_ok(),
                started.elapsed().as_millis()
            ),
        );

        match receive_result {
            Ok(()) => {
                let started = Instant::now();
                trace_stage(
                    "commit_start",
                    format!("role=server target={}", target_dir.display()),
                );
                commit_staging(&staging, &target_dir)?;
                let committed = scan_manifest(&target_dir)?;
                trace_stage(
                    "commit_done",
                    format!(
                        "role=server files={} elapsed_ms={}",
                        committed.files.len(),
                        started.elapsed().as_millis()
                    ),
                );
                let started = Instant::now();
                trace_stage(
                    "sync_complete_write_start",
                    format!("role=server peer={peer_addr}"),
                );
                write_message(&mut stream, &Message::SyncComplete).await?;
                trace_stage(
                    "sync_complete_write_done",
                    format!(
                        "role=server peer={peer_addr} elapsed_ms={}",
                        started.elapsed().as_millis()
                    ),
                );
                Ok(committed)
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                let _ = write_message(
                    &mut stream,
                    &Message::Error {
                        message: error.to_string(),
                    },
                )
                .await;
                Err(error)
            }
        }
    }
}

pub struct ReceiveService {
    server: TransportServer,
    target_dir: PathBuf,
}

impl ReceiveService {
    pub async fn bind(
        addr: SocketAddr,
        target_dir: impl Into<PathBuf>,
        tls: &TlsConfig,
    ) -> Result<Self> {
        Ok(Self {
            server: TransportServer::bind(addr, tls).await?,
            target_dir: target_dir.into(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.server.local_addr()
    }

    pub async fn receive_once(
        &self,
        progress: Option<&ProgressCallback<'_>>,
    ) -> Result<SyncManifest> {
        self.server.receive_once(&self.target_dir, progress).await
    }

    pub async fn receive_once_with_pairing_handler(
        &self,
        progress: Option<&ProgressCallback<'_>>,
        pairing_handler: Option<&PairingRequestCallback<'_>>,
    ) -> Result<SyncManifest> {
        self.server
            .receive_once_with_pairing_handler(&self.target_dir, progress, pairing_handler)
            .await
    }

    pub async fn receive_once_with_control_handlers(
        &self,
        progress: Option<&ProgressCallback<'_>>,
        pairing_handler: Option<&PairingRequestCallback<'_>>,
        project_mapping_request_handler: Option<&ProjectMappingRequestCallback<'_>>,
        project_mapping_ack_handler: Option<&ProjectMappingAckCallback<'_>>,
        workspace_mapping_request_handler: Option<&WorkspaceMappingRequestCallback<'_>>,
        workspace_mapping_ack_handler: Option<&WorkspaceMappingAckCallback<'_>>,
        text_message_handler: Option<&TextMessageCallback<'_>>,
        file_transfer_request_handler: Option<&FileTransferRequestCallback<'_>>,
        file_transfer_ack_handler: Option<&FileTransferAckCallback<'_>>,
        file_transfer_data_handler: Option<&FileTransferDataCallback<'_>>,
    ) -> Result<SyncManifest> {
        self.server
            .receive_once_with_control_handlers(
                &self.target_dir,
                progress,
                pairing_handler,
                project_mapping_request_handler,
                project_mapping_ack_handler,
                workspace_mapping_request_handler,
                workspace_mapping_ack_handler,
                text_message_handler,
                file_transfer_request_handler,
                file_transfer_ack_handler,
                file_transfer_data_handler,
            )
            .await
    }

    pub async fn serve_forever(&self, progress: Option<&ProgressCallback<'_>>) -> Result<()> {
        loop {
            if let Err(error) = self.receive_once(progress).await {
                trace_stage("receive_error", error.to_string());
            }
        }
    }

    pub async fn serve_forever_with_pairing_handler(
        &self,
        progress: Option<&ProgressCallback<'_>>,
        pairing_handler: Option<&PairingRequestCallback<'_>>,
    ) -> Result<()> {
        loop {
            if let Err(error) = self
                .receive_once_with_pairing_handler(progress, pairing_handler)
                .await
            {
                trace_stage("receive_error", error.to_string());
            }
        }
    }

    pub async fn serve_forever_with_control_handlers(
        &self,
        progress: Option<&ProgressCallback<'_>>,
        pairing_handler: Option<&PairingRequestCallback<'_>>,
        project_mapping_request_handler: Option<&ProjectMappingRequestCallback<'_>>,
        project_mapping_ack_handler: Option<&ProjectMappingAckCallback<'_>>,
        workspace_mapping_request_handler: Option<&WorkspaceMappingRequestCallback<'_>>,
        workspace_mapping_ack_handler: Option<&WorkspaceMappingAckCallback<'_>>,
        text_message_handler: Option<&TextMessageCallback<'_>>,
        file_transfer_request_handler: Option<&FileTransferRequestCallback<'_>>,
        file_transfer_ack_handler: Option<&FileTransferAckCallback<'_>>,
        file_transfer_data_handler: Option<&FileTransferDataCallback<'_>>,
    ) -> Result<()> {
        loop {
            if let Err(error) = self
                .receive_once_with_control_handlers(
                    progress,
                    pairing_handler,
                    project_mapping_request_handler,
                    project_mapping_ack_handler,
                    workspace_mapping_request_handler,
                    workspace_mapping_ack_handler,
                    text_message_handler,
                    file_transfer_request_handler,
                    file_transfer_ack_handler,
                    file_transfer_data_handler,
                )
                .await
            {
                trace_stage("receive_error", error.to_string());
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct NoopTransporter;

impl NoopTransporter {
    pub fn new() -> Self {
        Self
    }
}

impl Transporter for NoopTransporter {
    fn connect(&mut self, _peer: &DeviceInfo) -> Result<()> {
        Ok(())
    }

    fn send_manifest(&mut self, _manifest: &SyncManifest) -> Result<()> {
        Ok(())
    }

    fn send_files(&mut self, _root: &Path, _files: &[FileEntry]) -> Result<()> {
        Ok(())
    }

    fn receive_files(&mut self, _target_dir: &Path) -> Result<SyncManifest> {
        Ok(SyncManifest { files: Vec::new() })
    }
}

#[derive(Debug, Clone)]
pub struct FailingTransporter {
    message: String,
}

impl FailingTransporter {
    pub fn remote_transport_unavailable() -> Self {
        Self {
            message: "remote transport is not configured; refusing to treat remote paths as local filesystem paths".to_string(),
        }
    }

    fn error(&self) -> AisyncError {
        AisyncError::Transport(self.message.clone())
    }
}

impl Transporter for FailingTransporter {
    fn connect(&mut self, _peer: &DeviceInfo) -> Result<()> {
        Err(self.error())
    }

    fn send_manifest(&mut self, _manifest: &SyncManifest) -> Result<()> {
        Err(self.error())
    }

    fn send_files(&mut self, _root: &Path, _files: &[FileEntry]) -> Result<()> {
        Err(self.error())
    }

    fn receive_files(&mut self, _target_dir: &Path) -> Result<SyncManifest> {
        Err(self.error())
    }
}

pub fn generate_tls_identity(subject_name: &str) -> Result<TlsIdentity> {
    let key_pair =
        KeyPair::generate().map_err(|error| transport_err(format!("TLS key: {error}")))?;
    let params = CertificateParams::new(vec![subject_name.to_string()])
        .map_err(|error| transport_err(format!("TLS cert params: {error}")))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|error| transport_err(format!("TLS cert: {error}")))?;

    Ok(TlsIdentity {
        cert_der: cert.der().to_vec(),
        private_key_der: key_pair.serialize_der(),
    })
}

pub fn scan_manifest(root: &Path) -> Result<SyncManifest> {
    scan_manifest_with_patterns(root, &default_exclude_patterns())
}

pub fn scan_sensitive_files(root: &Path) -> Result<Vec<SensitiveFile>> {
    let patterns = sensitive_file_patterns();
    let matcher = build_globset(&patterns)?;
    let mut files = Vec::new();

    if !root.exists() {
        return Ok(files);
    }

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| transport_err(error.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| transport_err(error.to_string()))?;
        let relative_path = normalize_relative_path(relative)?;
        let relative_lower = relative_path.to_ascii_lowercase();
        let matches = matcher.matches(&relative_lower);
        if let Some(index) = matches.first() {
            files.push(SensitiveFile {
                relative_path,
                matched_pattern: patterns[*index].to_string(),
            });
        }
    }

    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

pub fn match_sensitive_file_path(path: &Path) -> Result<Option<SensitiveFile>> {
    let patterns = sensitive_file_patterns();
    let matcher = build_globset(&patterns)?;
    for candidate in sensitive_path_candidates(path) {
        let matches = matcher.matches(&candidate.to_ascii_lowercase());
        if let Some(index) = matches.first() {
            return Ok(Some(SensitiveFile {
                relative_path: candidate,
                matched_pattern: patterns[*index].to_string(),
            }));
        }
    }
    Ok(None)
}

pub fn scan_manifest_with_patterns(root: &Path, patterns: &[&str]) -> Result<SyncManifest> {
    let excludes = build_globset(patterns)?;
    let mut paths = Vec::new();

    if !root.exists() {
        return Ok(SyncManifest { files: Vec::new() });
    }

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| transport_err(error.to_string()))?;
        let path = entry.path();
        if path == root {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|error| transport_err(error.to_string()))?;
        let relative_str = normalize_relative_path(relative)?;

        if excludes.is_match(&relative_str) {
            if entry.file_type().is_dir() {
                continue;
            }
            continue;
        }

        if entry.file_type().is_file() {
            paths.push((path.to_path_buf(), relative_str));
        }
    }

    let results: Vec<Result<FileEntry>> = paths
        .par_iter()
        .map(|(path, relative_path)| file_entry(path, relative_path))
        .collect();

    let mut files = Vec::with_capacity(results.len());
    for result in results {
        files.push(result?);
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    Ok(SyncManifest { files })
}

pub fn diff_manifests(source: &SyncManifest, target: &SyncManifest) -> FileDiff {
    let target_by_path: HashMap<_, _> = target
        .files
        .iter()
        .map(|entry| (entry.relative_path.as_str(), entry))
        .collect();
    let source_paths: HashSet<_> = source
        .files
        .iter()
        .map(|entry| entry.relative_path.as_str())
        .collect();

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut unchanged = Vec::new();
    let mut deleted = Vec::new();

    for entry in &source.files {
        match target_by_path.get(entry.relative_path.as_str()) {
            None => added.push(entry.clone()),
            Some(target_entry) if target_entry.blake3_hash != entry.blake3_hash => {
                modified.push(entry.clone());
            }
            Some(_) => unchanged.push(entry.clone()),
        }
    }

    for entry in &target.files {
        if !source_paths.contains(entry.relative_path.as_str()) {
            deleted.push(entry.clone());
        }
    }

    FileDiff {
        added,
        modified,
        deleted,
        unchanged,
    }
}

pub fn make_delta(base: &[u8], target: &[u8]) -> Result<Vec<u8>> {
    let signature = Signature::calculate(base, signature_options());
    let indexed = signature.index();
    let mut delta = Vec::new();
    diff(&indexed, target, &mut delta).map_err(|error| transport_err(error.to_string()))?;
    Ok(delta)
}

pub fn apply_delta(base: &[u8], delta: &[u8], expected_hash: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    apply(base, delta, &mut out).map_err(|error| transport_err(error.to_string()))?;
    let actual_hash = blake3_hex(&out);
    if actual_hash != expected_hash {
        return Err(transport_err(format!(
            "delta integrity check failed: expected {expected_hash}, got {actual_hash}"
        )));
    }
    Ok(out)
}

pub fn verify_manifest_checksums(root: &Path, manifest: &SyncManifest) -> Result<()> {
    for entry in &manifest.files {
        let path = root.join(checked_relative_path(&entry.relative_path)?);
        let data = fs::read(&path).map_err(|error| {
            transport_err(format!(
                "integrity check failed for {}: {}",
                entry.relative_path, error
            ))
        })?;
        let actual_hash = blake3_hex(&data);
        if actual_hash != entry.blake3_hash {
            return Err(transport_err(format!(
                "integrity check failed for {}: expected {}, got {}",
                entry.relative_path, entry.blake3_hash, actual_hash
            )));
        }
    }

    Ok(())
}

pub fn pack_small_files(root: &Path, files: &[FileEntry]) -> Result<Vec<u8>> {
    let mut tar_stream = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_stream);
        for entry in files {
            let relative = checked_relative_path(&entry.relative_path)?;
            let path = root.join(relative);
            let data = fs::read(&path)?;
            if blake3_hex(&data) != entry.blake3_hash {
                return Err(transport_err(format!(
                    "source file changed while packing: {}",
                    entry.relative_path
                )));
            }

            let mut header = tar::Header::new_gnu();
            header
                .set_path(&entry.relative_path)
                .map_err(|error| transport_err(error.to_string()))?;
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append(&header, Cursor::new(data))
                .map_err(|error| transport_err(error.to_string()))?;
        }
        builder
            .finish()
            .map_err(|error| transport_err(error.to_string()))?;
    }
    Ok(tar_stream)
}

pub fn unpack_small_files(staging_dir: &Path, tar_stream: &[u8]) -> Result<u64> {
    let mut archive = tar::Archive::new(Cursor::new(tar_stream));
    let mut bytes_written = 0;

    for entry in archive
        .entries()
        .map_err(|error| transport_err(error.to_string()))?
    {
        let mut entry = entry.map_err(|error| transport_err(error.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let path = entry
            .path()
            .map_err(|error| transport_err(error.to_string()))?;
        let path_str = normalize_relative_path(&path)?;
        let destination = staging_dir.join(checked_relative_path(&path_str)?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut output = File::create(&destination)?;
        let copied = std::io::copy(&mut entry, &mut output)?;
        bytes_written += copied;
    }

    Ok(bytes_written)
}

pub async fn write_message<S>(stream: &mut S, message: &Message) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(message).map_err(|error| transport_err(error.to_string()))?;
    let frame_len = payload.len() + FRAME_TYPE_SIZE;
    if frame_len > MAX_FRAME_SIZE {
        return Err(transport_err(format!(
            "frame too large: {frame_len} bytes type={:?} serialized_bytes={}",
            message.message_type(),
            payload.len()
        )));
    }

    tokio::time::timeout(
        FRAME_HEADER_TIMEOUT,
        stream.write_all(&(frame_len as u32).to_be_bytes()),
    )
    .await
    .map_err(|_| {
        transport_err(format!(
            "timed out writing frame length after {}ms",
            FRAME_HEADER_TIMEOUT.as_millis()
        ))
    })??;
    tokio::time::timeout(
        FRAME_HEADER_TIMEOUT,
        stream.write_all(&[message.message_type() as u8]),
    )
    .await
    .map_err(|_| {
        transport_err(format!(
            "timed out writing frame type after {}ms",
            FRAME_HEADER_TIMEOUT.as_millis()
        ))
    })??;
    tokio::time::timeout(FRAME_BODY_TIMEOUT, stream.write_all(&payload))
        .await
        .map_err(|_| {
            transport_err(format!(
                "timed out writing frame payload after {}ms",
                FRAME_BODY_TIMEOUT.as_millis()
            ))
        })??;
    tokio::time::timeout(FRAME_HEADER_TIMEOUT, stream.flush())
        .await
        .map_err(|_| {
            transport_err(format!(
                "timed out flushing frame after {}ms",
                FRAME_HEADER_TIMEOUT.as_millis()
            ))
        })??;
    Ok(())
}

pub async fn read_message<S>(stream: &mut S) -> Result<Message>
where
    S: AsyncRead + Unpin,
{
    let mut len_bytes = [0_u8; 4];
    tokio::time::timeout(FRAME_HEADER_TIMEOUT, stream.read_exact(&mut len_bytes))
        .await
        .map_err(|_| {
            transport_err(format!(
                "timed out reading frame length after {}ms",
                FRAME_HEADER_TIMEOUT.as_millis()
            ))
        })??;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len < FRAME_TYPE_SIZE || len > MAX_FRAME_SIZE {
        return Err(transport_err(format!("invalid frame length: {len}")));
    }

    let mut frame = vec![0_u8; len];
    tokio::time::timeout(FRAME_BODY_TIMEOUT, stream.read_exact(&mut frame))
        .await
        .map_err(|_| {
            transport_err(format!(
                "timed out reading frame payload after {}ms",
                FRAME_BODY_TIMEOUT.as_millis()
            ))
        })??;
    let frame_type = MessageType::try_from(frame[0])?;
    let message: Message =
        serde_json::from_slice(&frame[1..]).map_err(|error| transport_err(error.to_string()))?;

    if message.message_type() != frame_type {
        return Err(transport_err(
            "frame type does not match payload".to_string(),
        ));
    }

    Ok(message)
}

async fn send_diff<S>(
    stream: &mut S,
    source_dir: &Path,
    source_manifest: &SyncManifest,
    remote_manifest: &SyncManifest,
    signatures: &[SignatureEntry],
    progress: Option<&ProgressCallback<'_>>,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let manifest_diff = diff_manifests(source_manifest, remote_manifest);
    let total_bytes = manifest_diff
        .added
        .iter()
        .chain(manifest_diff.modified.iter())
        .map(|entry| entry.size)
        .sum();
    let mut bytes_done = 0;

    for entry in &manifest_diff.deleted {
        write_message(
            stream,
            &Message::FileDelete {
                path: entry.relative_path.clone(),
            },
        )
        .await?;
    }

    let mut small_files = Vec::new();
    let mut small_file_bytes = 0u64;
    let _ = signatures;

    for entry in manifest_diff
        .added
        .iter()
        .chain(manifest_diff.modified.iter())
    {
        if entry.size < SMALL_FILE_THRESHOLD {
            if small_file_bytes > 0
                && small_file_bytes.saturating_add(entry.size) > SMALL_FILE_BATCH_LIMIT
            {
                let tar_stream = pack_small_files(source_dir, &small_files)?;
                trace_stage(
                    "file_batch_write_start",
                    format!(
                        "files={} tar_bytes={} estimated_json_bytes={}",
                        small_files.len(),
                        tar_stream.len(),
                        tar_stream.len().saturating_mul(4)
                    ),
                );
                write_message(stream, &Message::FileBatch { tar_stream }).await?;

                for file in small_files.drain(..) {
                    bytes_done += file.size;
                    emit_progress(progress, bytes_done, total_bytes, Some(file.relative_path));
                }
                small_file_bytes = 0;
            }
            small_file_bytes = small_file_bytes.saturating_add(entry.size);
            small_files.push(entry.clone());
            continue;
        }

        send_file_chunks(stream, source_dir, entry).await?;
        bytes_done += entry.size;
        emit_progress(
            progress,
            bytes_done,
            total_bytes,
            Some(entry.relative_path.clone()),
        );
    }

    if !small_files.is_empty() {
        let tar_stream = pack_small_files(source_dir, &small_files)?;
        trace_stage(
            "file_batch_write_start",
            format!(
                "files={} tar_bytes={} estimated_json_bytes={}",
                small_files.len(),
                tar_stream.len(),
                tar_stream.len().saturating_mul(4)
            ),
        );
        write_message(stream, &Message::FileBatch { tar_stream }).await?;

        for entry in small_files {
            bytes_done += entry.size;
            emit_progress(progress, bytes_done, total_bytes, Some(entry.relative_path));
        }
    }

    Ok(())
}

async fn send_file_chunks<S>(stream: &mut S, source_dir: &Path, entry: &FileEntry) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let path = source_dir.join(checked_relative_path(&entry.relative_path)?);
    let mut file = File::open(&path)?;
    let mut offset = 0u64;
    let mut buf = vec![0u8; FILE_CHUNK_SIZE];
    trace_stage(
        "file_chunk_write_start",
        format!("path={} size={}", entry.relative_path, entry.size),
    );
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            if entry.size == 0 {
                write_message(
                    stream,
                    &Message::FileChunk {
                        path: entry.relative_path.clone(),
                        target_hash: entry.blake3_hash.clone(),
                        offset: 0,
                        data: Vec::new(),
                        size: 0,
                        done: true,
                    },
                )
                .await?;
            }
            break;
        }
        let next_offset = offset + n as u64;
        write_message(
            stream,
            &Message::FileChunk {
                path: entry.relative_path.clone(),
                target_hash: entry.blake3_hash.clone(),
                offset,
                data: buf[..n].to_vec(),
                size: entry.size,
                done: next_offset == entry.size,
            },
        )
        .await?;
        offset = next_offset;
    }
    trace_stage(
        "file_chunk_write_done",
        format!("path={} size={}", entry.relative_path, entry.size),
    );
    Ok(())
}

async fn receive_changes<S>(
    stream: &mut S,
    staging_dir: &Path,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let mut bytes_done = 0;

    loop {
        match read_message(stream).await? {
            Message::FileBatch { tar_stream } => {
                let written = unpack_small_files(staging_dir, &tar_stream)?;
                bytes_done += written;
                emit_progress(progress, bytes_done, 0, None);
            }
            Message::FileDelta {
                path,
                base_hash,
                target_hash,
                delta,
                size,
            } => {
                let relative = checked_relative_path(&path)?;
                let destination = staging_dir.join(relative);
                let base = match fs::read(&destination) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(error) => return Err(error.into()),
                };
                let actual_base_hash = if base.is_empty() {
                    None
                } else {
                    Some(blake3_hex(&base))
                };
                if base_hash.is_some() && actual_base_hash != base_hash {
                    return Err(transport_err(format!("base hash mismatch for {path}")));
                }

                let reconstructed = apply_delta(&base, &delta, &target_hash)?;
                write_file_atomic(&destination, &reconstructed)?;
                bytes_done += size;
                emit_progress(progress, bytes_done, 0, Some(path));
            }
            Message::FileChunk {
                path,
                target_hash,
                offset,
                data,
                size,
                done,
            } => {
                let relative = checked_relative_path(&path)?;
                let destination = staging_dir.join(relative);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                if offset == 0 {
                    let mut output = File::create(&destination)?;
                    output.write_all(&data)?;
                } else {
                    let current = fs::metadata(&destination)
                        .map_err(|error| {
                            transport_err(format!(
                                "chunk target missing for {path} at offset {offset}: {error}"
                            ))
                        })?
                        .len();
                    if current != offset {
                        return Err(transport_err(format!(
                            "chunk offset mismatch for {path}: expected {current}, got {offset}"
                        )));
                    }
                    let mut output = fs::OpenOptions::new().append(true).open(&destination)?;
                    output.write_all(&data)?;
                }
                if done {
                    let bytes = fs::read(&destination)?;
                    if bytes.len() as u64 != size {
                        return Err(transport_err(format!(
                            "chunk size mismatch for {path}: expected {size}, got {}",
                            bytes.len()
                        )));
                    }
                    let actual = blake3_hex(&bytes);
                    if actual != target_hash {
                        return Err(transport_err(format!(
                            "chunk integrity check failed for {path}: expected {target_hash}, got {actual}"
                        )));
                    }
                }
                bytes_done += data.len() as u64;
                emit_progress(progress, bytes_done, 0, Some(path));
            }
            Message::FileDelete { path } => {
                let destination = staging_dir.join(checked_relative_path(&path)?);
                match fs::remove_file(&destination) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                remove_empty_parents(staging_dir, destination.parent());
            }
            Message::SessionData { project_id, data } => {
                let destination = staging_dir.join(session_data_path(&project_id)?);
                write_file_atomic(&destination, &data)?;
                bytes_done += data.len() as u64;
                emit_progress(progress, bytes_done, 0, Some(project_id));
            }
            Message::SyncComplete => return Ok(()),
            Message::Error { message } => return Err(transport_err(message)),
            other => {
                return Err(transport_err(format!(
                    "unexpected transfer message {:?}",
                    other.message_type()
                )));
            }
        }
    }
}

fn build_signature_entries(
    target_dir: &Path,
    source_manifest: &SyncManifest,
    target_manifest: &SyncManifest,
) -> Result<Vec<SignatureEntry>> {
    let source_by_path: HashMap<_, _> = source_manifest
        .files
        .iter()
        .map(|entry| (entry.relative_path.as_str(), entry))
        .collect();

    let mut signatures = Vec::new();
    for target_entry in &target_manifest.files {
        let Some(source_entry) = source_by_path.get(target_entry.relative_path.as_str()) else {
            continue;
        };
        if source_entry.blake3_hash == target_entry.blake3_hash
            || source_entry.size < SMALL_FILE_THRESHOLD
        {
            continue;
        }

        let bytes = fs::read(target_dir.join(checked_relative_path(&target_entry.relative_path)?))?;
        let signature = Signature::calculate(&bytes, signature_options()).into_serialized();
        signatures.push(SignatureEntry {
            relative_path: target_entry.relative_path.clone(),
            base_hash: target_entry.blake3_hash.clone(),
            signature,
        });
    }

    Ok(signatures)
}

fn expect_hello(message: Message) -> Result<()> {
    match message {
        Message::Hello {
            protocol_version, ..
        } if protocol_version == PROTOCOL_VERSION => Ok(()),
        Message::Hello {
            protocol_version, ..
        } => Err(transport_err(format!(
            "protocol version mismatch: local {PROTOCOL_VERSION}, remote {protocol_version}"
        ))),
        Message::Error { message } => Err(transport_err(message)),
        other => Err(transport_err(format!(
            "expected Hello, got {:?}",
            other.message_type()
        ))),
    }
}

fn server_device_info() -> DeviceInfo {
    DeviceInfo {
        id: aisync_core::DeviceId::new(),
        name: "aisync-server".to_string(),
        os: aisync_core::OsType::Other(std::env::consts::OS.to_string()),
        addresses: Vec::new(),
        protocol_version: PROTOCOL_VERSION,
    }
}

fn server_config(tls: &TlsConfig) -> Result<ServerConfig> {
    ensure_crypto_provider();
    let cert = CertificateDer::from(tls.identity.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        tls.identity.private_key_der.clone(),
    ));

    ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|error| transport_err(format!("TLS server config: {error}")))
}

fn client_config(tls: &TlsConfig) -> Result<ClientConfig> {
    let peer_cert = tls
        .pinned_peer_cert_der
        .as_ref()
        .ok_or_else(|| transport_err("TLS peer certificate is not pinned".to_string()))?;
    let provider = ensure_crypto_provider();
    let verifier = Arc::new(PinnedPeerCertVerifier {
        cert_der: peer_cert.clone(),
        provider,
    });

    Ok(
        ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    )
}

fn ensure_crypto_provider() -> Arc<CryptoProvider> {
    match CryptoProvider::get_default() {
        Some(provider) => Arc::clone(provider),
        None => {
            let provider = rustls::crypto::aws_lc_rs::default_provider();
            let _ = provider.clone().install_default();
            CryptoProvider::get_default()
                .cloned()
                .unwrap_or_else(|| Arc::new(provider))
        }
    }
}

#[derive(Debug)]
struct PinnedPeerCertVerifier {
    cert_der: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedPeerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        if end_entity.as_ref() == self.cert_der.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(RustlsError::General(
                "server certificate does not match pinned peer certificate".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn default_exclude_patterns() -> Vec<&'static str> {
    vec![
        ".env*",
        "**/.env*",
        "*.key",
        "**/*.key",
        "*.pem",
        "**/*.pem",
        "credentials.*",
        "**/credentials.*",
        ".git/**",
        "**/.git/**",
        ".git/objects/**",
        "**/.git/objects/**",
        "node_modules/**",
        "**/node_modules/**",
        "target/**",
        "**/target/**",
        "__pycache__/**",
        "**/__pycache__/**",
    ]
}

fn sensitive_file_patterns() -> Vec<&'static str> {
    vec![
        ".env*",
        "**/.env*",
        "*credential*",
        "**/*credential*",
        "*.key",
        "**/*.key",
        "*.pem",
        "**/*.pem",
        "*secret*",
        "**/*secret*",
    ]
}

fn sensitive_path_candidates(path: &Path) -> Vec<String> {
    let mut candidates = Vec::new();
    let parts = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let component_path = parts.join("/");
    if !component_path.is_empty() {
        candidates.push(component_path);
    }
    for part in parts {
        if !part.is_empty() && !candidates.contains(&part) {
            candidates.push(part);
        }
    }
    if let Some(name) = path.file_name() {
        let filename = name.to_string_lossy().into_owned();
        if !filename.is_empty() && !candidates.contains(&filename) {
            candidates.push(filename);
        }
    }
    candidates
}

fn build_globset(patterns: &[&str]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(|error| transport_err(error.to_string()))?);
    }
    builder
        .build()
        .map_err(|error| transport_err(error.to_string()))
}

fn file_entry(path: &Path, relative_path: &str) -> Result<FileEntry> {
    let metadata = fs::metadata(path)?;
    let data = fs::read(path)?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);

    Ok(FileEntry {
        relative_path: relative_path.to_string(),
        size: metadata.len(),
        blake3_hash: blake3_hex(&data),
        mtime,
    })
}

fn prepare_staging(target_dir: &Path) -> Result<PathBuf> {
    let parent = target_dir.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".aisync-staging-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_nanos()
    ));

    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;

    if target_dir.exists() {
        copy_dir_contents(target_dir, &staging)?;
    }

    Ok(staging)
}

fn expand_remote_dir(path: PathBuf) -> PathBuf {
    if path == Path::new("~") {
        return home_dir().unwrap_or(path);
    }
    if let Ok(stripped) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(stripped);
        }
    }
    path
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn commit_staging(staging: &Path, target_dir: &Path) -> Result<()> {
    let parent = target_dir.parent().unwrap_or_else(|| Path::new("."));
    let backup = parent.join(format!(
        ".aisync-backup-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_nanos()
    ));

    if target_dir.exists() {
        fs::rename(target_dir, &backup)?;
    }

    match fs::rename(staging, target_dir) {
        Ok(()) => {
            if backup.exists() {
                fs::remove_dir_all(backup)?;
            }
            Ok(())
        }
        Err(error) => {
            if backup.exists() {
                if let Err(restore_error) = fs::rename(&backup, target_dir) {
                    eprintln!(
                        "[aisync-transport] failed to restore backup after commit error: backup={} target={} commit_error={} restore_error={}",
                        backup.display(),
                        target_dir.display(),
                        error,
                        restore_error
                    );
                }
            }
            Err(error.into())
        }
    }
}

fn copy_dir_contents(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| transport_err(error.to_string()))?;
        let path = entry.path();
        if path == source {
            continue;
        }

        let relative = path
            .strip_prefix(source)
            .map_err(|error| transport_err(error.to_string()))?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, target)?;
        }
    }
    Ok(())
}

fn write_file_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("aisync-tmp");
    fs::write(&tmp, data)?;
    if cfg!(windows) && path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}

fn remove_empty_parents(root: &Path, start: Option<&Path>) {
    let Some(mut current) = start else {
        return;
    };
    while current != root {
        if fs::remove_dir(current).is_err() {
            return;
        }
        let Some(parent) = current.parent() else {
            return;
        };
        current = parent;
    }
}

fn checked_relative_path(relative_path: &str) -> Result<PathBuf> {
    let path = Path::new(relative_path);
    if path.is_absolute() {
        return Err(transport_err(format!(
            "absolute path rejected: {relative_path}"
        )));
    }

    let mut checked = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => checked.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(transport_err(format!(
                    "unsafe relative path rejected: {relative_path}"
                )));
            }
        }
    }

    Ok(checked)
}

fn session_data_path(project_id: &str) -> Result<PathBuf> {
    if project_id.is_empty()
        || project_id.contains('/')
        || project_id.contains('\\')
        || project_id == "."
        || project_id == ".."
    {
        return Err(transport_err(format!(
            "unsafe session project id rejected: {project_id}"
        )));
    }

    checked_relative_path(&format!(".aisync-sessions/{project_id}.bin"))
}

fn normalize_relative_path(path: &Path) -> Result<String> {
    let checked = checked_relative_path(&path.to_string_lossy())?;
    Ok(checked
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

fn signature_options() -> SignatureOptions {
    SignatureOptions {
        block_size: 8192,
        crypto_hash_size: 16,
    }
}

fn blake3_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

fn emit_progress(
    progress: Option<&ProgressCallback<'_>>,
    bytes_done: u64,
    total_bytes: u64,
    current_file: Option<String>,
) {
    if let Some(callback) = progress {
        callback(Progress {
            bytes_done,
            total_bytes,
            current_file,
        });
    }
}

fn trace_stage(stage: &str, detail: impl std::fmt::Display) {
    eprintln!("[aisync-transport] stage={stage} {detail}");
}

fn transport_err(message: impl Into<String>) -> AisyncError {
    AisyncError::Transport(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, data: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, data).unwrap();
    }

    #[test]
    fn protocol_version_is_bumped_for_control_frames() {
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn frame_round_trips_with_type_prefix() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (mut client, mut server) = tokio::io::duplex(1024);
            let message = Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "left".to_string(),
            };

            write_message(&mut client, &message).await.unwrap();
            let decoded = read_message(&mut server).await.unwrap();

            assert_eq!(decoded, message);
        });
    }

    #[test]
    fn control_frames_round_trip_for_messages_and_file_transfer() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let device = DeviceInfo {
                id: aisync_core::DeviceId::new(),
                name: "peer".to_string(),
                os: aisync_core::OsType::Darwin,
                addresses: Vec::new(),
                protocol_version: PROTOCOL_VERSION,
            };
            let messages = vec![
                Message::TextMessage {
                    message: TextMessagePayload {
                        sender_name: "alice".to_string(),
                        content: "hello".to_string(),
                        timestamp: 42,
                    },
                },
                Message::FileTransferRequest {
                    request: FileTransferRequestPayload {
                        transfer_id: "tx1".to_string(),
                        filename: "note.txt".to_string(),
                        size: 5,
                        sender_name: "alice".to_string(),
                        device: device.clone(),
                        endpoint: None,
                        receiver_cert_der: None,
                        server_name: None,
                    },
                },
                Message::FileTransferData {
                    data: FileTransferDataPayload {
                        transfer_id: "tx1".to_string(),
                        offset: 0,
                        chunk: b"hello".to_vec(),
                        done: true,
                    },
                },
                Message::FileTransferAck {
                    ack: FileTransferAckPayload {
                        transfer_id: "tx1".to_string(),
                        accepted: true,
                        ready: true,
                        filename: "note.txt".to_string(),
                        message: None,
                        device,
                    },
                },
            ];

            for message in messages {
                let (mut client, mut server) = tokio::io::duplex(2048);
                write_message(&mut client, &message).await.unwrap();
                let decoded = read_message(&mut server).await.unwrap();
                assert_eq!(decoded, message);
            }
        });
    }

    #[test]
    fn manifest_scan_hashes_and_applies_default_excludes() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/main.rs"), b"fn main() {}\n");
        write(&dir.path().join(".env"), b"SECRET=1\n");
        write(&dir.path().join(".env.local"), b"SECRET=2\n");
        write(&dir.path().join("node_modules/pkg/index.js"), b"ignored");
        write(&dir.path().join(".git/objects/aa/object"), b"ignored");
        write(&dir.path().join("target/debug/app"), b"ignored");

        let manifest = scan_manifest(dir.path()).unwrap();

        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].relative_path, "src/main.rs");
        assert_eq!(manifest.files[0].blake3_hash, blake3_hex(b"fn main() {}\n"));
    }

    #[test]
    fn sensitive_file_scan_marks_files_for_ui_review() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".env.local"), b"secret");
        write(&dir.path().join("config/Credentials.json"), b"secret");
        write(&dir.path().join("certs/server.pem"), b"secret");
        write(&dir.path().join("keys/app.key"), b"secret");
        write(&dir.path().join("notes/secret-plan.txt"), b"secret");
        write(&dir.path().join("src/main.rs"), b"fn main() {}\n");

        let sensitive = scan_sensitive_files(dir.path()).unwrap();
        let paths: Vec<_> = sensitive
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect();

        assert_eq!(
            paths,
            vec![
                ".env.local",
                "certs/server.pem",
                "config/Credentials.json",
                "keys/app.key",
                "notes/secret-plan.txt",
            ]
        );
    }

    #[test]
    fn sensitive_path_match_catches_single_file_transfers() {
        let secret = match_sensitive_file_path(Path::new("/tmp/project/secret-dir/note.txt"))
            .unwrap()
            .expect("secret path should match");
        assert_eq!(secret.matched_pattern, "*secret*");

        let env = match_sensitive_file_path(Path::new(".env.local"))
            .unwrap()
            .expect("env file should match");
        assert_eq!(env.matched_pattern, ".env*");

        assert!(match_sensitive_file_path(Path::new("src/main.rs"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn manifest_checksum_verification_rejects_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("ok.txt"), b"ok");
        write(&dir.path().join("bad.txt"), b"corrupt");
        let manifest = SyncManifest {
            files: vec![
                FileEntry {
                    relative_path: "ok.txt".to_string(),
                    size: 2,
                    blake3_hash: blake3_hex(b"ok"),
                    mtime: 0,
                },
                FileEntry {
                    relative_path: "bad.txt".to_string(),
                    size: 3,
                    blake3_hash: blake3_hex(b"expected"),
                    mtime: 0,
                },
            ],
        };

        let error = verify_manifest_checksums(dir.path(), &manifest).unwrap_err();

        assert!(error
            .to_string()
            .contains("integrity check failed for bad.txt"));
    }

    #[test]
    fn rsync_delta_reconstructs_large_file() {
        let base = vec![b'a'; SMALL_FILE_THRESHOLD as usize + 128];
        let mut target = base.clone();
        target[1024..1030].copy_from_slice(b"change");

        let delta = make_delta(&base, &target).unwrap();
        let rebuilt = apply_delta(&base, &delta, &blake3_hex(&target)).unwrap();

        assert_eq!(rebuilt, target);
        assert!(delta.len() < target.len());
    }

    #[test]
    fn tar_batch_round_trips_small_files() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        write(&source.path().join("a.txt"), b"alpha");
        write(&source.path().join("nested/b.txt"), b"beta");
        let manifest = scan_manifest(source.path()).unwrap();

        let tar_stream = pack_small_files(source.path(), &manifest.files).unwrap();
        unpack_small_files(target.path(), &tar_stream).unwrap();

        assert_eq!(fs::read(target.path().join("a.txt")).unwrap(), b"alpha");
        assert_eq!(
            fs::read(target.path().join("nested/b.txt")).unwrap(),
            b"beta"
        );
    }

    #[test]
    fn interrupted_receive_leaves_target_unchanged() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target");
            write(&target.join("keep.txt"), b"original");
            let staging = prepare_staging(&target).unwrap();

            let (mut client, mut server) = tokio::io::duplex(1024 * 1024);
            write_message(
                &mut client,
                &Message::FileBatch {
                    tar_stream: {
                        let source = tempfile::tempdir().unwrap();
                        write(&source.path().join("keep.txt"), b"changed");
                        let manifest = scan_manifest(source.path()).unwrap();
                        pack_small_files(source.path(), &manifest.files).unwrap()
                    },
                },
            )
            .await
            .unwrap();
            drop(client);

            let result = receive_changes(&mut server, &staging, None).await;
            assert!(result.is_err());
            let _ = fs::remove_dir_all(&staging);

            assert_eq!(fs::read(target.join("keep.txt")).unwrap(), b"original");
        });
    }

    #[test]
    fn commit_replaces_target_only_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let staging = dir.path().join(".aisync-staging-test");
        write(&target.join("old.txt"), b"old");
        write(&staging.join("new.txt"), b"new");

        commit_staging(&staging, &target).unwrap();

        assert!(!target.join("old.txt").exists());
        assert_eq!(fs::read(target.join("new.txt")).unwrap(), b"new");
    }

    #[test]
    fn diff_classifies_added_modified_deleted_and_unchanged() {
        let source = SyncManifest {
            files: vec![
                FileEntry {
                    relative_path: "added.txt".to_string(),
                    size: 1,
                    blake3_hash: "a".to_string(),
                    mtime: 0,
                },
                FileEntry {
                    relative_path: "modified.txt".to_string(),
                    size: 1,
                    blake3_hash: "new".to_string(),
                    mtime: 0,
                },
                FileEntry {
                    relative_path: "same.txt".to_string(),
                    size: 1,
                    blake3_hash: "same".to_string(),
                    mtime: 0,
                },
            ],
        };
        let target = SyncManifest {
            files: vec![
                FileEntry {
                    relative_path: "deleted.txt".to_string(),
                    size: 1,
                    blake3_hash: "d".to_string(),
                    mtime: 0,
                },
                FileEntry {
                    relative_path: "modified.txt".to_string(),
                    size: 1,
                    blake3_hash: "old".to_string(),
                    mtime: 0,
                },
                FileEntry {
                    relative_path: "same.txt".to_string(),
                    size: 1,
                    blake3_hash: "same".to_string(),
                    mtime: 0,
                },
            ],
        };

        let diff = diff_manifests(&source, &target);

        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.modified.len(), 1);
        assert_eq!(diff.deleted.len(), 1);
        assert_eq!(diff.unchanged.len(), 1);
    }

    #[test]
    fn tls_transport_syncs_directory_byte_identical() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let source = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            write(&source.path().join("small.txt"), b"small");
            write(
                &source.path().join("large.bin"),
                &vec![b'x'; SMALL_FILE_THRESHOLD as usize + 2048],
            );
            write(&target.path().join("stale.txt"), b"remove me");
            write(&target.path().join(".env"), b"KEEP=1");

            let server_identity = generate_tls_identity("localhost").unwrap();
            let server_tls = TlsConfig::new(server_identity.clone(), "localhost");
            let client_identity = generate_tls_identity("localhost").unwrap();
            let client_tls = TlsConfig::new(client_identity, "localhost")
                .with_pinned_peer_cert(server_identity.cert_der.clone());

            let service =
                ReceiveService::bind("127.0.0.1:0".parse().unwrap(), target.path(), &server_tls)
                    .await
                    .unwrap();
            let addr = service.local_addr().unwrap();
            let server_task = tokio::spawn(async move { service.receive_once(None).await });
            let peer = DeviceInfo {
                id: aisync_core::DeviceId::new(),
                name: "receiver".to_string(),
                os: aisync_core::OsType::Darwin,
                addresses: vec![addr.ip()],
                protocol_version: PROTOCOL_VERSION,
            };

            let mut client = TcpTransporter::connect_to_peer(&peer, addr.port(), &client_tls)
                .await
                .unwrap();
            client.sync_directory(source.path(), None).await.unwrap();
            server_task.await.unwrap().unwrap();

            assert_eq!(fs::read(target.path().join("small.txt")).unwrap(), b"small");
            assert_eq!(
                fs::read(target.path().join("large.bin")).unwrap(),
                vec![b'x'; SMALL_FILE_THRESHOLD as usize + 2048]
            );
            assert!(!target.path().join("stale.txt").exists());
            assert_eq!(fs::read(target.path().join(".env")).unwrap(), b"KEEP=1");
        });
    }

    #[test]
    fn tls_transport_commits_to_manifest_remote_dir() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let source = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let default_target = root.path().join("received");
            let mapped_target = root.path().join("mapped");
            write(&source.path().join("main.rs"), b"fn main() {}\n");
            fs::create_dir_all(&default_target).unwrap();

            let server_identity = generate_tls_identity("localhost").unwrap();
            let server_tls = TlsConfig::new(server_identity.clone(), "localhost");
            let client_identity = generate_tls_identity("localhost").unwrap();
            let client_tls = TlsConfig::new(client_identity, "localhost")
                .with_pinned_peer_cert(server_identity.cert_der.clone());

            let service =
                ReceiveService::bind("127.0.0.1:0".parse().unwrap(), &default_target, &server_tls)
                    .await
                    .unwrap();
            let addr = service.local_addr().unwrap();
            let server_task = tokio::spawn(async move { service.receive_once(None).await });
            let peer = DeviceInfo {
                id: aisync_core::DeviceId::new(),
                name: "receiver".to_string(),
                os: aisync_core::OsType::Darwin,
                addresses: vec![addr.ip()],
                protocol_version: PROTOCOL_VERSION,
            };

            let mut client = TcpTransporter::connect_to_peer(&peer, addr.port(), &client_tls)
                .await
                .unwrap();
            client
                .sync_directory_to(source.path(), Some(&mapped_target), None)
                .await
                .unwrap();
            server_task.await.unwrap().unwrap();

            assert_eq!(
                fs::read(mapped_target.join("main.rs")).unwrap(),
                b"fn main() {}\n"
            );
            assert!(!default_target.join("main.rs").exists());
        });
    }

    #[test]
    fn receive_changes_writes_session_data_frame() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().to_path_buf();
            let (mut client, mut server) = tokio::io::duplex(4096);
            let server_task =
                tokio::spawn(async move { receive_changes(&mut server, &target, None).await });

            write_message(
                &mut client,
                &Message::SessionData {
                    project_id: "app".to_string(),
                    data: b"session".to_vec(),
                },
            )
            .await
            .unwrap();
            write_message(&mut client, &Message::SyncComplete)
                .await
                .unwrap();
            server_task.await.unwrap().unwrap();

            assert_eq!(
                fs::read(dir.path().join(".aisync-sessions/app.bin")).unwrap(),
                b"session"
            );
        });
    }
}
