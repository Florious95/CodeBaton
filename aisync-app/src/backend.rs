//! Real backend wiring.
//!
//! Bridges the Tauri IPC layer to the actual aisync crates:
//! - [`aisync_discovery::MdnsDiscoverer`] for LAN discovery + pairing
//! - [`aisync_transport::TcpTransporter`] for push transport
//! - [`aisync_transport`] for manifest / sensitive-file scanning
//!
//! The GUI starts a local receive daemon on launch. Pairing persists the peer's
//! advertised endpoint and pinned receiver certificate so push can connect
//! directly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use aisync_core::{
    AisyncError, DeviceId, DeviceInfo, Direction, Discoverer, OsType, Result, RewriteDirection,
    SyncManifest,
};
use aisync_discovery::{
    local_device_addresses, DiscoveryConfig, MdnsDiscoverer, PeerConnectionInfo,
};
use aisync_session::{ClaudeCodeParser, PathRule, RuleBasedRewriter};
use aisync_sync::{
    default_state_path, load_config, save_config, DiscoveredProject, FsWatcher, PeerConfig,
    ProjectConfig, SyncConfig, SyncModeConfig, SyncReport, WatchConfig, WorkspaceChildConfig,
    WorkspaceConfig,
};
use aisync_transport::{
    generate_tls_identity, match_sensitive_file_path, scan_sensitive_files, FileTransferAckPayload,
    FileTransferDataPayload, FileTransferRequestPayload, PairingRequestPayload,
    ProjectMappingAckPayload, ProjectMappingRequestPayload, ReceiveService, SensitiveFile,
    TargetStatusRequestPayload, TcpTransporter, TextMessagePayload, TlsConfig, TlsIdentity,
    WorkspaceMappingAckPayload, WorkspaceMappingRequestPayload,
};

const AUTO_SYNC_COOLDOWN: Duration = Duration::from_secs(90);
const FILE_TRANSFER_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const HISTORY_FILE_LIMIT: usize = 5;
static INCOMING_SYNC_SUPPRESSIONS: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
static AUTO_SYNC_GATES: OnceLock<Mutex<HashMap<String, AutoSyncGate>>> = OnceLock::new();
static SESSION_BASELINE_SEEDS: OnceLock<Mutex<HashMap<String, SessionBaseline>>> = OnceLock::new();
static WORKSPACE_PROPAGATION_BYPASS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Clone, Copy)]
struct AutoSyncGate {
    in_flight: bool,
    cooldown_until: Instant,
}

#[derive(Clone)]
struct SessionBaseline {
    mtime: SystemTime,
    content_fingerprint: Option<String>,
    sync_fingerprint: Option<String>,
}

fn incoming_sync_suppressions() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    INCOMING_SYNC_SUPPRESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn auto_sync_gates() -> &'static Mutex<HashMap<String, AutoSyncGate>> {
    AUTO_SYNC_GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn session_baseline_seeds() -> &'static Mutex<HashMap<String, SessionBaseline>> {
    SESSION_BASELINE_SEEDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn workspace_propagation_bypass() -> &'static Mutex<HashSet<String>> {
    WORKSPACE_PROPAGATION_BYPASS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn mark_incoming_sync_root(root: &Path) {
    incoming_sync_suppressions()
        .lock()
        .unwrap()
        .insert(root.to_path_buf(), Instant::now() + AUTO_SYNC_COOLDOWN);
}

fn incoming_sync_recent(root: &Path) -> bool {
    let now = Instant::now();
    let mut guard = incoming_sync_suppressions().lock().unwrap();
    guard.retain(|_, until| *until > now);
    guard
        .keys()
        .any(|incoming| incoming.starts_with(root) || root.starts_with(incoming))
}

fn auto_sync_gate_key(scope: &str, name: &str, peer: &str) -> String {
    format!("{scope}:{name}:{peer}")
}

fn try_begin_auto_sync(scope: &str, name: &str, peer: &str, trigger: &str) -> Option<String> {
    let key = auto_sync_gate_key(scope, name, peer);
    let now = Instant::now();
    let mut gates = auto_sync_gates().lock().unwrap();
    gates.retain(|_, gate| gate.in_flight || gate.cooldown_until > now);
    if let Some(gate) = gates.get(&key) {
        let reason = if gate.in_flight {
            "in_flight"
        } else {
            "cooldown"
        };
        app_log(
            "auto_sync_suppressed",
            &[
                ("scope", scope.to_string()),
                ("name", name.to_string()),
                ("peer", peer.to_string()),
                ("trigger", trigger.to_string()),
                ("reason", reason.to_string()),
            ],
        );
        return None;
    }
    gates.insert(
        key.clone(),
        AutoSyncGate {
            in_flight: true,
            cooldown_until: now,
        },
    );
    Some(key)
}

fn begin_auto_sync_bypass_cooldown(
    scope: &str,
    name: &str,
    peer: &str,
    trigger: &str,
) -> Option<String> {
    let key = auto_sync_gate_key(scope, name, peer);
    let now = Instant::now();
    let mut gates = auto_sync_gates().lock().unwrap();
    gates.retain(|_, gate| gate.in_flight || gate.cooldown_until > now);
    if gates.get(&key).map(|gate| gate.in_flight).unwrap_or(false) {
        app_log(
            "auto_sync_suppressed",
            &[
                ("scope", scope.to_string()),
                ("name", name.to_string()),
                ("peer", peer.to_string()),
                ("trigger", trigger.to_string()),
                ("reason", "in_flight".to_string()),
            ],
        );
        return None;
    }
    gates.insert(
        key.clone(),
        AutoSyncGate {
            in_flight: true,
            cooldown_until: now,
        },
    );
    Some(key)
}

fn finish_auto_sync(gate_key: &str) {
    auto_sync_gates().lock().unwrap().insert(
        gate_key.to_string(),
        AutoSyncGate {
            in_flight: false,
            cooldown_until: Instant::now() + AUTO_SYNC_COOLDOWN,
        },
    );
}

fn enqueue_workspace_first_propagation(workspace: &WorkspaceConfig) {
    let Some(peer) = workspace.effective_peer() else {
        return;
    };
    workspace_propagation_bypass()
        .lock()
        .unwrap()
        .insert(auto_sync_gate_key("workspace", &workspace.name, peer));
    app_log(
        "workspace_first_propagation_queued",
        &[
            ("workspace", workspace.name.clone()),
            ("peer", peer.to_string()),
        ],
    );
}

fn workspace_first_propagation_pending(workspace_name: &str, peer_name: &str) -> bool {
    workspace_propagation_bypass()
        .lock()
        .unwrap()
        .contains(&auto_sync_gate_key("workspace", workspace_name, peer_name))
}

fn clear_workspace_first_propagation(workspace_name: &str, peer_name: &str) {
    workspace_propagation_bypass()
        .lock()
        .unwrap()
        .remove(&auto_sync_gate_key("workspace", workspace_name, peer_name));
}

/// Where an incoming push lands. Per-instance receive root next to the config.
fn receive_root(config_path: &Path) -> PathBuf {
    std::env::var("AISYNC_RECEIVE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| config_path.with_file_name("received"))
}

/// Local serve-daemon coordinates the GUI exposes for pairing / manual setup.
#[derive(Clone)]
pub struct ServeInfo {
    pub port: u16,
    pub cert_path: PathBuf,
    pub receive_dir: PathBuf,
}

/// Owns the long-lived backend state behind a single mutex.
pub struct Backend {
    inner: Mutex<Inner>,
    pending_pairing_requests: Arc<Mutex<VecDeque<PairingRequestPayload>>>,
    pending_project_mapping_requests: Arc<Mutex<VecDeque<ProjectMappingRequestPayload>>>,
    pending_project_mapping_acks: Arc<Mutex<VecDeque<ProjectMappingAckPayload>>>,
    pending_workspace_mapping_requests: Arc<Mutex<VecDeque<WorkspaceMappingRequestPayload>>>,
    pending_workspace_mapping_acks: Arc<Mutex<VecDeque<WorkspaceMappingAckPayload>>>,
    pending_text_messages: Arc<Mutex<VecDeque<TextMessagePayload>>>,
    pending_file_transfer_requests: Arc<Mutex<VecDeque<FileTransferRequestPayload>>>,
    pending_file_transfer_acks: Arc<Mutex<VecDeque<FileTransferAckPayload>>>,
    file_receive_states: Arc<Mutex<HashMap<String, FileReceiveState>>>,
}

struct Inner {
    config: SyncConfig,
    config_path: PathBuf,
    discoverer: MdnsDiscoverer,
    auto_sync_paused: bool,
    serve: Option<ServeInfo>,
    pairing_sessions: HashMap<DeviceId, PairingSession>,
    project_mapping_requests: HashMap<String, ProjectMappingRequestPayload>,
    outbound_project_mappings: HashMap<String, OutboundProjectMapping>,
    workspace_mapping_requests: HashMap<String, WorkspaceMappingRequestPayload>,
    outbound_workspace_mappings: HashMap<String, OutboundWorkspaceMapping>,
    file_transfer_requests: HashMap<String, FileTransferRequestPayload>,
    outbound_file_transfers: HashMap<String, OutboundFileTransfer>,
    project_watchers: HashMap<String, FsWatcher>,
    workspace_watchers: HashMap<String, FsWatcher>,
}

pub struct PairingInfo {
    pub peer: DeviceInfo,
    pub code: String,
    pub request_id: String,
    pub expires_at_unix_secs: u64,
}

#[derive(Clone)]
struct PairingSession {
    peer: DeviceInfo,
    request_id: String,
    code: String,
    expires_at_unix_secs: u64,
    connection: Option<PairingConnection>,
    inbound: bool,
}

#[derive(Clone)]
struct PairingConnection {
    endpoint: Option<SocketAddr>,
    receiver_cert_der: Option<Vec<u8>>,
    server_name: Option<String>,
}

#[derive(Clone)]
struct OutboundProjectMapping {
    project_name: String,
    local_dir: PathBuf,
    peer_name: String,
    mode: SyncModeConfig,
}

#[derive(Clone)]
struct OutboundWorkspaceMapping {
    workspace_name: String,
    local_root: PathBuf,
    peer_name: String,
    mode: SyncModeConfig,
    auto_enable_new: bool,
}

#[derive(Clone)]
struct OutboundFileTransfer {
    path: PathBuf,
    peer_name: String,
}

struct FileReceiveState {
    target_path: PathBuf,
    tmp_path: PathBuf,
    expected_size: u64,
    bytes_written: u64,
    filename: String,
    sender_name: String,
    history_config_path: PathBuf,
}

impl Backend {
    /// Build the backend, loading config from `~/.aisync/config.toml` (or a
    /// default in-memory config when absent) and starting live mDNS discovery.
    pub fn new() -> Result<Self> {
        let config_path = aisync_sync::default_config_path()
            .unwrap_or_else(|| PathBuf::from(".aisync/config.toml"));
        let existed = config_path.exists();
        let mut config = if existed {
            load_config(&config_path)?
        } else {
            SyncConfig::new(default_device_name())
        };
        let mut changed = !existed;
        // Heal a stale/placeholder device name left by an older build (BUG-007).
        // Earlier versions persisted the literal "CodeBaton Device" because the
        // subprocess hostname lookup failed in the release sandbox; now that we
        // read the hostname in-process, re-derive a real name on next launch.
        if is_placeholder_device_name(&config.device.name) {
            let real = default_device_name();
            if real != config.device.name {
                config.device.name = real;
                changed = true;
            }
        }
        if config.state_path.is_none() {
            config.state_path = Some(
                default_state_path().unwrap_or_else(|| config_path.with_file_name("state.toml")),
            );
            changed = true;
        }
        if changed {
            save_config(&config_path, &config)?;
        }

        // Start the receive daemon before advertising mDNS so peers discover a
        // live endpoint and the certificate they must pin.
        let pending_pairing_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_project_mapping_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_project_mapping_acks = Arc::new(Mutex::new(VecDeque::new()));
        let pending_workspace_mapping_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_workspace_mapping_acks = Arc::new(Mutex::new(VecDeque::new()));
        let pending_text_messages = Arc::new(Mutex::new(VecDeque::new()));
        let pending_file_transfer_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_file_transfer_acks = Arc::new(Mutex::new(VecDeque::new()));
        let file_receive_states = Arc::new(Mutex::new(HashMap::new()));
        let serve = start_serve_daemon(
            &config_path,
            config.receive_port,
            Arc::clone(&pending_pairing_requests),
            Arc::clone(&pending_project_mapping_requests),
            Arc::clone(&pending_project_mapping_acks),
            Arc::clone(&pending_workspace_mapping_requests),
            Arc::clone(&pending_workspace_mapping_acks),
            Arc::clone(&pending_text_messages),
            Arc::clone(&pending_file_transfer_requests),
            Arc::clone(&pending_file_transfer_acks),
            Arc::clone(&file_receive_states),
            None,
        );
        if let Some(serve) = &serve {
            config.receive_port = serve.port;
        }

        let mut disco_cfg = DiscoveryConfig::new(config.device.name.clone(), config.receive_port);
        disco_cfg.local_device.id = config.device.id;
        disco_cfg.local_device.addresses = local_device_addresses();
        if let Some(serve) = &serve {
            disco_cfg.receiver_cert_der = fs::read(&serve.cert_path).ok();
        }
        let mut discoverer = MdnsDiscoverer::new(disco_cfg)?;
        // Best-effort: failing to start mDNS (no network) must not break the UI.
        let _ = discoverer.start();

        let project_watchers = start_project_watchers(&config_path, &config);
        let workspace_watchers = start_workspace_watchers(&config_path, &config);
        start_session_mtime_scanner(config_path.clone(), config.clone());

        Ok(Self {
            inner: Mutex::new(Inner {
                config,
                config_path,
                discoverer,
                auto_sync_paused: false,
                serve,
                pairing_sessions: HashMap::new(),
                project_mapping_requests: HashMap::new(),
                outbound_project_mappings: HashMap::new(),
                workspace_mapping_requests: HashMap::new(),
                outbound_workspace_mappings: HashMap::new(),
                file_transfer_requests: HashMap::new(),
                outbound_file_transfers: HashMap::new(),
                project_watchers,
                workspace_watchers,
            }),
            pending_pairing_requests,
            pending_project_mapping_requests,
            pending_project_mapping_acks,
            pending_workspace_mapping_requests,
            pending_workspace_mapping_acks,
            pending_text_messages,
            pending_file_transfer_requests,
            pending_file_transfer_acks,
            file_receive_states,
        })
    }

    /// Construct from an explicit config (used by integration tests).
    #[allow(dead_code)]
    pub fn with_config(config: SyncConfig, config_path: PathBuf) -> Result<Self> {
        let pending_pairing_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_project_mapping_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_project_mapping_acks = Arc::new(Mutex::new(VecDeque::new()));
        let pending_workspace_mapping_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_workspace_mapping_acks = Arc::new(Mutex::new(VecDeque::new()));
        let pending_text_messages = Arc::new(Mutex::new(VecDeque::new()));
        let pending_file_transfer_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_file_transfer_acks = Arc::new(Mutex::new(VecDeque::new()));
        let file_receive_states = Arc::new(Mutex::new(HashMap::new()));
        let mut disco_cfg = DiscoveryConfig::new(config.device.name.clone(), config.receive_port);
        disco_cfg.local_device.id = config.device.id;
        disco_cfg.local_device.addresses = local_device_addresses();
        let discoverer = MdnsDiscoverer::new(disco_cfg)?;
        let project_watchers = start_project_watchers(&config_path, &config);
        let workspace_watchers = start_workspace_watchers(&config_path, &config);

        Ok(Self {
            inner: Mutex::new(Inner {
                config,
                config_path,
                discoverer,
                auto_sync_paused: false,
                serve: None,
                pairing_sessions: HashMap::new(),
                project_mapping_requests: HashMap::new(),
                outbound_project_mappings: HashMap::new(),
                workspace_mapping_requests: HashMap::new(),
                outbound_workspace_mappings: HashMap::new(),
                file_transfer_requests: HashMap::new(),
                outbound_file_transfers: HashMap::new(),
                project_watchers,
                workspace_watchers,
            }),
            pending_pairing_requests,
            pending_project_mapping_requests,
            pending_project_mapping_acks,
            pending_workspace_mapping_requests,
            pending_workspace_mapping_acks,
            pending_text_messages,
            pending_file_transfer_requests,
            pending_file_transfer_acks,
            file_receive_states,
        })
    }

    /// Like [`Backend::with_config`] but also starts the receive daemon (used
    /// by the GUI-to-GUI integration test). The daemon binds
    /// `config.receive_port`; pass a free port in tests to avoid conflicts.
    #[allow(dead_code)]
    pub fn with_config_serving(mut config: SyncConfig, config_path: PathBuf) -> Result<Self> {
        let pending_pairing_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_project_mapping_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_project_mapping_acks = Arc::new(Mutex::new(VecDeque::new()));
        let pending_workspace_mapping_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_workspace_mapping_acks = Arc::new(Mutex::new(VecDeque::new()));
        let pending_text_messages = Arc::new(Mutex::new(VecDeque::new()));
        let pending_file_transfer_requests = Arc::new(Mutex::new(VecDeque::new()));
        let pending_file_transfer_acks = Arc::new(Mutex::new(VecDeque::new()));
        let file_receive_states = Arc::new(Mutex::new(HashMap::new()));
        let serve = start_serve_daemon(
            &config_path,
            config.receive_port,
            Arc::clone(&pending_pairing_requests),
            Arc::clone(&pending_project_mapping_requests),
            Arc::clone(&pending_project_mapping_acks),
            Arc::clone(&pending_workspace_mapping_requests),
            Arc::clone(&pending_workspace_mapping_acks),
            Arc::clone(&pending_text_messages),
            Arc::clone(&pending_file_transfer_requests),
            Arc::clone(&pending_file_transfer_acks),
            Arc::clone(&file_receive_states),
            Some(64),
        );
        if let Some(serve) = &serve {
            config.receive_port = serve.port;
        }
        let mut disco_cfg = DiscoveryConfig::new(config.device.name.clone(), config.receive_port);
        disco_cfg.local_device.id = config.device.id;
        disco_cfg.local_device.addresses = local_device_addresses();
        if let Some(serve) = &serve {
            disco_cfg.receiver_cert_der = fs::read(&serve.cert_path).ok();
        }
        let discoverer = MdnsDiscoverer::new(disco_cfg)?;
        let project_watchers = start_project_watchers(&config_path, &config);
        let workspace_watchers = start_workspace_watchers(&config_path, &config);

        Ok(Self {
            inner: Mutex::new(Inner {
                config,
                config_path,
                discoverer,
                auto_sync_paused: false,
                serve,
                pairing_sessions: HashMap::new(),
                project_mapping_requests: HashMap::new(),
                outbound_project_mappings: HashMap::new(),
                workspace_mapping_requests: HashMap::new(),
                outbound_workspace_mappings: HashMap::new(),
                file_transfer_requests: HashMap::new(),
                outbound_file_transfers: HashMap::new(),
                project_watchers,
                workspace_watchers,
            }),
            pending_pairing_requests,
            pending_project_mapping_requests,
            pending_project_mapping_acks,
            pending_workspace_mapping_requests,
            pending_workspace_mapping_acks,
            pending_text_messages,
            pending_file_transfer_requests,
            pending_file_transfer_acks,
            file_receive_states,
        })
    }

    /// Local serve-daemon info (listening port, pinned cert, receive dir).
    pub fn serve_info(&self) -> Option<ServeInfo> {
        self.inner.lock().unwrap().serve.clone()
    }

    pub fn send_text_message(&self, peer_name: &str, content: String) -> Result<()> {
        let (endpoint, tls, message) = {
            let g = self.inner.lock().unwrap();
            let (endpoint, tls) = control_connection_for_peer(&g, peer_name)?;
            let message = TextMessagePayload {
                sender_name: g.config.device.name.clone(),
                content,
                timestamp: epoch_millis_now_u64(),
            };
            (endpoint, tls, message)
        };
        send_text_message(endpoint, tls, message.clone())?;
        let config_path = self.config_path();
        record_text_message_history(&config_path, Some(peer_name), &message, true);
        Ok(())
    }

    pub fn take_pending_text_message(&self) -> Option<TextMessagePayload> {
        self.pending_text_messages.lock().unwrap().pop_front()
    }

    pub fn text_messages(&self, peer_name: Option<&str>) -> Vec<serde_json::Value> {
        let path = self.config_path().with_file_name("chat_history.jsonl");
        read_jsonl(&path)
            .into_iter()
            .filter(|row| {
                peer_name
                    .map(|peer| row.get("peerName").and_then(|v| v.as_str()) == Some(peer))
                    .unwrap_or(true)
            })
            .collect()
    }

    pub fn default_file_receive_dir(&self) -> PathBuf {
        let g = self.inner.lock().unwrap();
        default_file_receive_dir(&g.config_path, &g.config)
    }

    pub fn set_default_file_receive_dir(&self, path: PathBuf) -> Result<()> {
        fs::create_dir_all(&path)?;
        let mut g = self.inner.lock().unwrap();
        g.config.default_file_receive_dir = Some(path);
        let config_path = g.config_path.clone();
        let config = g.config.clone();
        save_config(&config_path, &config)
    }

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
            let transfer_id = aisync_discovery::new_pairing_request_id();
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

    fn file_receive_target_path(&self, transfer_id: &str) -> String {
        self.file_receive_states
            .lock()
            .unwrap()
            .get(transfer_id)
            .map(|state| state.target_path.display().to_string())
            .unwrap_or_default()
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

    pub fn suggested_file_receive_path(&self, filename: &str) -> PathBuf {
        let g = self.inner.lock().unwrap();
        default_file_receive_dir(&g.config_path, &g.config).join(safe_filename(filename))
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

    pub fn take_pending_pairing_request(&self) -> Option<(DeviceInfo, String, String, u64)> {
        let request = self.pending_pairing_requests.lock().unwrap().pop_front()?;
        let mut g = self.inner.lock().unwrap();
        let connection = PairingConnection {
            endpoint: request.endpoint,
            receiver_cert_der: request.receiver_cert_der.clone(),
            server_name: request.server_name.clone(),
        };
        let peer = with_endpoint_first(request.device.clone(), connection.endpoint);
        g.pairing_sessions.insert(
            peer.id,
            PairingSession {
                peer: peer.clone(),
                request_id: request.request_id.clone(),
                code: request.code.clone(),
                expires_at_unix_secs: request.expires_at_unix_secs,
                connection: Some(connection),
                inbound: true,
            },
        );
        log_line(&format!(
            "[pair] pairing_request_ready peer_id={} peer_name={} request_id={} expires_at={}",
            peer.id.0, peer.name, request.request_id, request.expires_at_unix_secs
        ));
        Some((
            peer,
            request.code,
            request.request_id,
            request.expires_at_unix_secs,
        ))
    }

    /// Register (or update) a peer's push endpoint + pinned certificate so the
    /// GUI can push to it. Persists to config.
    pub fn add_peer_endpoint(
        &self,
        name: String,
        id: DeviceId,
        endpoint: SocketAddr,
        server_cert: Option<PathBuf>,
        server_name: Option<String>,
    ) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.config.peers.insert(
            name.clone(),
            PeerConfig {
                id,
                name,
                endpoint: Some(endpoint),
                server_cert,
                server_name,
                last_seen: None,
            },
        );
        let path = g.config_path.clone();
        let cfg = g.config.clone();
        save_config(&path, &cfg)
    }

    pub fn config(&self) -> SyncConfig {
        self.inner.lock().unwrap().config.clone()
    }

    pub fn config_with_refreshed_workspaces(&self) -> SyncConfig {
        let mut g = self.inner.lock().unwrap();
        let (refreshed, changed) = refresh_workspaces_in_config(&g.config);
        if changed {
            g.config = refreshed;
            let _ = save_config(&g.config_path, &g.config);
        }
        g.config.clone()
    }

    /// Append one sync record to the persisted history (`~/.aisync/history.jsonl`,
    /// next to the config). One JSON object per line, newest appended last.
    pub fn record_sync(
        &self,
        project_id: &str,
        direction: &str,
        success: bool,
        files: u32,
        bytes: u64,
        detail: Option<String>,
        timestamp: String,
    ) {
        self.record_sync_scoped(
            project_id, direction, success, files, bytes, detail, timestamp, None, None,
        );
    }

    pub fn record_sync_scoped(
        &self,
        project_id: &str,
        direction: &str,
        success: bool,
        files: u32,
        bytes: u64,
        detail: Option<String>,
        timestamp: String,
        workspace_name: Option<&str>,
        child_name: Option<&str>,
    ) {
        let (path, summary) = {
            let g = self.inner.lock().unwrap();
            let summary = if success {
                history_summary_from_config(
                    &g.config,
                    project_id,
                    workspace_name,
                    child_name,
                    "mixed",
                )
            } else {
                HistoryFileSummary::default()
            };
            (g.config_path.with_file_name("history.jsonl"), summary)
        };
        let bytes = if success && bytes == 0 {
            summary.bytes
        } else {
            bytes
        };
        let file_path = summary.file_paths.first().cloned();
        let file_name = file_path.as_deref().and_then(|path| {
            Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        let file_names: Vec<String> = summary
            .file_paths
            .iter()
            .filter_map(|path| {
                Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect();
        let event_id = aisync_discovery::new_pairing_request_id();
        let entry = serde_json::json!({
            "eventId": event_id,
            "timestamp": timestamp,
            "projectId": project_id,
            "direction": direction,
            "success": success,
            "files": files,
            "bytes": bytes,
            "detail": detail,
            "workspaceName": workspace_name,
            "childName": child_name,
            "trigger": "manual",
            "role": "sender",
            "fileType": "mixed",
            "file_path": file_path,
            "file_paths": summary.file_paths,
            "file_name": file_name,
            "file_names": file_names,
        });
        match append_json_line(&path, &entry) {
            Ok(()) => app_log(
                "sender_sync_history_recorded",
                &[
                    ("project", project_id.to_string()),
                    ("event_id", event_id.clone()),
                    ("role", "sender".to_string()),
                    ("path", path.display().to_string()),
                ],
            ),
            Err(error) => app_log(
                "history_write_failed",
                &[
                    ("project", project_id.to_string()),
                    ("event_id", event_id),
                    ("path", path.display().to_string()),
                    ("error", error.to_string()),
                ],
            ),
        }
    }

    /// Read persisted sync history (newest first). When `project_id` is given,
    /// only that project's records are returned.
    pub fn sync_history(&self, project_id: Option<&str>) -> Vec<serde_json::Value> {
        let path = self
            .inner
            .lock()
            .unwrap()
            .config_path
            .with_file_name("history.jsonl");
        let Ok(text) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut rows: Vec<serde_json::Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| {
                project_id
                    .map(|pid| {
                        v.get("projectId").and_then(|p| p.as_str()) == Some(pid)
                            || v.get("workspaceName").and_then(|p| p.as_str()) == Some(pid)
                    })
                    .unwrap_or(true)
            })
            .collect();
        rows.reverse(); // newest first
        rows
    }

    /// True once the first-run wizard completed. Older configs without the flag
    /// are also considered onboarded if they already contain real mappings.
    pub fn is_onboarded(&self) -> bool {
        let g = self.inner.lock().unwrap();
        g.config.onboarded
            || !g.config.peers.is_empty()
            || !g.config.projects.is_empty()
            || !g.config.workspaces.is_empty()
            || !g.discoverer.paired_peers().is_empty()
    }

    /// The config file path backing this backend (for diagnostics / onboarding).
    pub fn config_path(&self) -> PathBuf {
        self.inner.lock().unwrap().config_path.clone()
    }

    /// Persist a new device name to config (used by the onboarding wizard).
    pub fn set_device_name(&self, name: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        update_device_name_locked(&mut g, name, false)
    }

    pub fn set_refresh_interval_secs(&self, secs: u64) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.config.refresh_interval_secs = if secs == 0 {
            aisync_sync::default_refresh_interval_secs()
        } else {
            secs
        };
        save_config(&g.config_path, &g.config)
    }

    /// Complete first-run onboarding and keep config/discovery/UI identity in sync.
    pub fn complete_onboarding(&self, name: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        update_device_name_locked(&mut g, name, true)?;
        app_log(
            "onboarding_completed",
            &[
                ("device_id", g.config.device.id.0.to_string()),
                ("device_name", g.config.device.name.clone()),
                ("config", g.config_path.display().to_string()),
            ],
        );
        Ok(())
    }

    /// Scan the local AI-tool config dirs and report real installed-state +
    /// session counts. Currently covers Claude Code (`~/.claude`) and Codex
    /// (`~/.codex`); Gemini CLI is listed as not-installed until it ships a
    /// stable session layout. Counts are real `.jsonl` session files on disk.
    pub fn ai_tools(&self) -> Vec<AiTool> {
        let home = home_dir();
        let claude_dir = home.as_ref().map(|h| h.join(".claude"));
        let codex_dir = home.as_ref().map(|h| h.join(".codex"));
        // Count = number of project dirs under <tool>/projects/ (one per project
        // the tool touched), matching the "N 个项目会话" label.
        let claude_count = claude_dir.as_deref().map(count_project_dirs).unwrap_or(0);
        let codex_count = codex_dir.as_deref().map(count_project_dirs).unwrap_or(0);
        // Log the count oracle so qa can reconcile against
        // `ls ~/.claude/projects | wc -l`.
        app_log(
            "ai_tool_scan_done",
            &[
                ("claude_project_dirs", claude_count.to_string()),
                ("codex_project_dirs", codex_count.to_string()),
                (
                    "claude_installed",
                    claude_dir
                        .as_deref()
                        .map(|d| d.exists())
                        .unwrap_or(false)
                        .to_string(),
                ),
                (
                    "codex_installed",
                    codex_dir
                        .as_deref()
                        .map(|d| d.exists())
                        .unwrap_or(false)
                        .to_string(),
                ),
            ],
        );
        vec![
            AiTool {
                name: "Claude Code".to_string(),
                config_dir: "~/.claude/".to_string(),
                session_count: claude_count,
                installed: claude_dir.as_deref().map(|d| d.exists()).unwrap_or(false),
            },
            AiTool {
                name: "Codex".to_string(),
                config_dir: "~/.codex/".to_string(),
                session_count: codex_count,
                installed: codex_dir.as_deref().map(|d| d.exists()).unwrap_or(false),
            },
            AiTool {
                name: "Gemini CLI".to_string(),
                config_dir: String::new(),
                session_count: 0,
                installed: false,
            },
        ]
    }

    pub fn auto_sync_paused(&self) -> bool {
        self.inner.lock().unwrap().auto_sync_paused
    }

    pub fn set_auto_sync_paused(&self, paused: bool) {
        self.inner.lock().unwrap().auto_sync_paused = paused;
    }

    pub fn local_device(&self) -> DeviceInfo {
        self.inner.lock().unwrap().discoverer.local_device().clone()
    }

    /// Paired peers (from the discoverer's persisted pairing store) merged with
    /// peers declared in config. Returns `(DeviceInfo, online)` pairs.
    pub fn paired_peers(&self) -> Vec<(DeviceInfo, bool)> {
        let g = self.inner.lock().unwrap();
        let live = live_peers_with_endpoints(&g.discoverer);
        let now = unix_secs_now();
        let mut out = Vec::new();
        for p in g.discoverer.paired_peers() {
            if p.device.id == g.config.device.id {
                continue;
            }
            let live_match = live.iter().find(|(device, _)| device.id == p.device.id);
            let session = active_pairing_session(&g, &p.device.id, now);
            let configured = g
                .config
                .peers
                .values()
                .find(|peer| peer.id == p.device.id || peer.name == p.device.name);
            let endpoint = live_match
                .and_then(|(_, endpoint)| *endpoint)
                .or_else(|| session.and_then(|session| session.connection.as_ref()?.endpoint))
                .or_else(|| configured.and_then(|peer| peer.endpoint));
            let online =
                live_match.is_some() || session.is_some() || endpoint.is_some_and(endpoint_online);
            out.push((with_endpoint_first(p.device, endpoint), online));
        }
        // Also surface config-declared peers not present in the pairing store.
        for (name, peer) in &g.config.peers {
            if peer.id == g.config.device.id {
                continue;
            }
            if out.iter().any(|(d, _)| d.id == peer.id) {
                continue;
            }
            let live_match = live.iter().find(|(device, endpoint)| {
                device.id == peer.id || (peer.endpoint.is_some() && *endpoint == peer.endpoint)
            });
            let session = active_pairing_session(&g, &peer.id, now);
            let endpoint = live_match
                .and_then(|(_, endpoint)| *endpoint)
                .or(peer.endpoint)
                .or_else(|| session.and_then(|session| session.connection.as_ref()?.endpoint));
            let online =
                live_match.is_some() || session.is_some() || endpoint.is_some_and(endpoint_online);
            out.push((
                with_endpoint_first(
                    DeviceInfo {
                        id: peer.id,
                        name: name.clone(),
                        os: live_match
                            .map(|(device, _)| device.os.clone())
                            .unwrap_or_else(|| OsType::Other("configured".to_string())),
                        addresses: live_match
                            .map(|(device, _)| device.addresses.clone())
                            .unwrap_or_default(),
                        protocol_version: live_match
                            .map(|(device, _)| device.protocol_version)
                            .unwrap_or(1),
                    },
                    endpoint,
                ),
                online,
            ));
        }
        out
    }

    /// Discovered-but-not-paired peers seen on the LAN.
    pub fn discovered_peers(&self) -> Vec<DeviceInfo> {
        let g = self.inner.lock().unwrap();
        let paired: HashSet<DeviceId> = g
            .discoverer
            .paired_peers()
            .iter()
            .map(|p| p.device.id)
            .collect();
        let configured_ids: HashSet<DeviceId> =
            g.config.peers.values().map(|peer| peer.id).collect();
        let configured_endpoints: HashSet<SocketAddr> = g
            .config
            .peers
            .values()
            .filter_map(|peer| peer.endpoint)
            .collect();
        let mut seen = HashSet::new();
        live_peers_with_endpoints(&g.discoverer)
            .into_iter()
            .filter_map(|(device, endpoint)| {
                if device.id == g.config.device.id
                    || paired.contains(&device.id)
                    || configured_ids.contains(&device.id)
                    || endpoint.is_some_and(|endpoint| configured_endpoints.contains(&endpoint))
                    || !seen.insert((device.id, endpoint))
                {
                    return None;
                }
                app_log(
                    "discovery_peer_seen",
                    &[
                        ("device_id", device.id.0.to_string()),
                        ("device_name", device.name.clone()),
                        (
                            "endpoint",
                            endpoint
                                .map(|endpoint| endpoint.to_string())
                                .unwrap_or_default(),
                        ),
                    ],
                );
                Some(with_endpoint_first(device, endpoint))
            })
            .collect()
    }

    /// Pairing code for a peer (D4). Derives from both `DeviceInfo`s.
    ///
    /// Primary path: the discoverer's live `begin_pairing` (requires the peer to
    /// be currently online). When the peer flickered out of the live mDNS list
    /// between the user seeing it and clicking 配对 — which the field report
    /// observed on Tailscale — fall back to a config-declared peer so the dialog
    /// still gets a stable, order-independent code instead of failing silently.
    pub fn pairing_code(&self, peer_id: &DeviceId) -> Result<PairingInfo> {
        let (peer, request_id, code, expires_at_unix_secs, connection, local_payload, tls_config) = {
            let mut g = self.inner.lock().unwrap();
            let now = unix_secs_now();
            if let Some(session) = g.pairing_sessions.get(peer_id).cloned() {
                if session.expires_at_unix_secs > now {
                    log_line(&format!(
                        "[pair] pairing_session_reused peer_id={} request_id={} inbound={}",
                        peer_id.0, session.request_id, session.inbound
                    ));
                    return Ok(PairingInfo {
                        peer: with_endpoint_first(
                            session.peer,
                            session.connection.as_ref().and_then(|c| c.endpoint),
                        ),
                        code: session.code,
                        request_id: session.request_id,
                        expires_at_unix_secs: session.expires_at_unix_secs,
                    });
                }
                g.pairing_sessions.remove(peer_id);
            }
            let live_connection = g.discoverer.peer_connection_info(peer_id).ok().flatten();
            let fallback_peer = peer_from_config(&g.config, peer_id);
            let (peer, request_id, code, expires_at_unix_secs) =
                match g.discoverer.begin_pairing(peer_id, "gui-local-key") {
                    Ok(req) => (
                        with_endpoint_first(
                            req.peer,
                            live_connection.as_ref().and_then(|c| c.endpoint),
                        ),
                        req.request_id,
                        req.pairing_code,
                        req.expires_at_unix_secs,
                    ),
                    Err(e) => {
                        log_line(&format!(
                            "[pair] live begin_pairing failed ({e}); trying config peer fallback"
                        ));
                        let peer = fallback_peer.clone().ok_or(e)?;
                        let request_id = aisync_discovery::new_pairing_request_id();
                        let code = aisync_discovery::derive_pairing_code_with_nonce(
                            g.discoverer.local_device(),
                            &peer,
                            &request_id,
                        );
                        log_line(&format!(
                            "[pair] pair_fallback_config peer_id={} peer_name={}",
                            peer_id.0, peer.name
                        ));
                        (peer, request_id, code, unix_secs_now() + 120)
                    }
                };
            let connection = live_connection
                .as_ref()
                .map(pairing_connection_from_discovery)
                .or_else(|| connection_from_config(&g.config, peer_id));
            let peer = with_endpoint_first(peer, connection.as_ref().and_then(|c| c.endpoint));
            g.pairing_sessions.insert(
                *peer_id,
                PairingSession {
                    peer: peer.clone(),
                    request_id: request_id.clone(),
                    code: code.clone(),
                    expires_at_unix_secs,
                    connection: connection.clone(),
                    inbound: false,
                },
            );
            let local = g.discoverer.local_device().clone();
            let local_endpoint = g.serve.as_ref().and_then(|serve| {
                local
                    .addresses
                    .first()
                    .map(|ip| SocketAddr::new(*ip, serve.port))
            });
            let local_payload = PairingRequestPayload {
                request_id: request_id.clone(),
                code: code.clone(),
                expires_at_unix_secs,
                device: with_endpoint_first(local, local_endpoint),
                endpoint: local_endpoint,
                receiver_cert_der: g
                    .serve
                    .as_ref()
                    .and_then(|serve| fs::read(&serve.cert_path).ok()),
                server_name: Some("aisync-receiver".to_string()),
            };
            let tls_config = pairing_tls_config(connection.as_ref());
            (
                peer,
                request_id,
                code,
                expires_at_unix_secs,
                connection,
                local_payload,
                tls_config,
            )
        };

        let endpoint = connection.as_ref().and_then(|c| c.endpoint);
        if let (Some(tls_config), Some(endpoint)) = (tls_config, endpoint) {
            send_pairing_request_async(endpoint, tls_config, local_payload);
        } else {
            log_line(&format!(
                "[pair] pairing_request_not_sent peer_id={} reason=missing_endpoint_or_cert",
                peer_id.0
            ));
        }

        Ok(PairingInfo {
            peer,
            code,
            request_id,
            expires_at_unix_secs,
        })
    }

    /// Confirm pairing and persist the peer to `config.peers`.
    ///
    /// The on-disk `config.peers` entry is what moves a device from 发现 to
    /// 已配对 and survives restarts, so we ALWAYS write it here — even when the
    /// live discoverer confirm fails because the peer flickered off the mDNS
    /// list (the Tailscale case from the field). The discoverer confirm and the
    /// live connection info (endpoint + pinned cert) are best-effort enrichments
    /// layered on top; their absence must not block local pairing.
    pub fn confirm_pairing(&self, peer_id: &DeviceId) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let session = g.pairing_sessions.get(peer_id).cloned();

        // Best-effort: record the pairing in the discoverer's store + grab the
        // peer DeviceInfo it returns. If the peer isn't live, fall back to the
        // discovered/config peer so we still have a name+id to persist.
        let confirmed_peer =
            match g
                .discoverer
                .confirm_pairing(peer_id, "gui-local-key", "gui-peer-key")
            {
                Ok(result) => Some(result.peer),
                Err(e) => {
                    log_line(&format!(
                        "[pair] discoverer confirm best-effort failed ({e}); persisting anyway"
                    ));
                    None
                }
            };
        let peer = session
            .as_ref()
            .map(|session| session.peer.clone())
            .or(confirmed_peer)
            .or_else(|| {
                g.discoverer
                    .peers()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|d| d.id == *peer_id)
            })
            .or_else(|| {
                g.config
                    .peers
                    .iter()
                    .find(|(_, p)| p.id == *peer_id)
                    .map(|(name, p)| DeviceInfo {
                        id: p.id,
                        name: name.clone(),
                        os: OsType::Other("configured".to_string()),
                        addresses: p.endpoint.map(|ep| vec![ep.ip()]).unwrap_or_default(),
                        protocol_version: 1,
                    })
            })
            .ok_or_else(|| {
                AisyncError::Discovery(format!("peer {} not found to confirm", peer_id.0))
            })?;

        // Live connection info (endpoint + pinned cert) if the peer is reachable;
        // None is fine — we still persist the peer entry.
        let connection = g.discoverer.peer_connection_info(peer_id).ok().flatten();
        let session_connection = session.and_then(|session| session.connection);
        let config_path = g.config_path.clone();
        persist_peer_connection(
            &mut g.config,
            &config_path,
            peer,
            connection
                .as_ref()
                .and_then(|c| c.endpoint)
                .or_else(|| session_connection.as_ref().and_then(|c| c.endpoint)),
            connection
                .as_ref()
                .and_then(|c| c.receiver_cert_der.as_deref())
                .or_else(|| {
                    session_connection
                        .as_ref()
                        .and_then(|c| c.receiver_cert_der.as_deref())
                }),
            connection
                .as_ref()
                .and_then(|c| c.server_name.clone())
                .or_else(|| {
                    session_connection
                        .as_ref()
                        .and_then(|c| c.server_name.clone())
                }),
        )?;
        let cfg = g.config.clone();
        save_config(&config_path, &cfg)?;
        log_line(&format!(
            "[pair] config.peers now has {} entr{}",
            g.config.peers.len(),
            if g.config.peers.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ));
        Ok(())
    }

    pub fn unpair(&self, peer_id: &DeviceId) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.discoverer.unpair(peer_id)?;
        // Drop config peer + its project mappings for this peer.
        let name = g
            .config
            .peers
            .iter()
            .find(|(_, p)| p.id == *peer_id)
            .map(|(n, _)| n.clone());
        if let Some(name) = name {
            g.config.peers.remove(&name);
            g.config.claude_config.peers.remove(&name);
            for project in &mut g.config.projects {
                project.peers.remove(&name);
            }
            for workspace in &mut g.config.workspaces {
                workspace.peers.remove(&name);
            }
        }
        let path = g.config_path.clone();
        let cfg = g.config.clone();
        let _ = save_config(&path, &cfg);
        Ok(())
    }

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

    pub fn scan_workspace(
        &self,
        workspace_name: &str,
        peer_name: &str,
    ) -> Result<Vec<DiscoveredProject>> {
        let g = self.inner.lock().unwrap();
        let workspace = g
            .config
            .workspaces
            .iter()
            .find(|w| w.name == workspace_name)
            .ok_or_else(|| AisyncError::Config(format!("workspace '{workspace_name}' not found")))?
            .clone();
        scan_workspace_direct(&workspace, peer_name)
    }

    /// List sensitive files (G6) under a project's local code dir.
    pub fn sensitive_files(&self, project_name: &str) -> Result<Vec<SensitiveFile>> {
        let g = self.inner.lock().unwrap();
        let project = g
            .config
            .projects
            .iter()
            .find(|p| p.name == project_name)
            .ok_or_else(|| AisyncError::Config(format!("project '{project_name}' not found")))?;
        scan_sensitive_files(&project.local)
    }

    pub fn request_project_mapping(
        &self,
        name: String,
        local: PathBuf,
        peer_name: String,
        mode: SyncModeConfig,
        create_local_dir: bool,
    ) -> Result<String> {
        let (request_id, endpoint, tls, request) = {
            let mut g = self.inner.lock().unwrap();
            if g.config.projects.iter().any(|p| p.name == name) {
                return Err(AisyncError::Config(format!(
                    "project '{name}' already exists"
                )));
            }
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
                    return Err(AisyncError::Config(format!(
                        "local-dir-missing:{}",
                        local.display()
                    )));
                }
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
            let request_id = aisync_discovery::new_pairing_request_id();
            let request = ProjectMappingRequestPayload {
                request_id: request_id.clone(),
                project_name: name.clone(),
                source_dir: local.clone(),
                mode: sync_mode_label(mode).to_string(),
                device: with_endpoint_first(local_device, Some(local_endpoint)),
                endpoint: Some(local_endpoint),
                receiver_cert_der: Some(receiver_cert_der),
                server_name: Some("aisync-receiver".to_string()),
            };
            g.outbound_project_mappings.insert(
                request_id.clone(),
                OutboundProjectMapping {
                    project_name: name,
                    local_dir: local,
                    peer_name,
                    mode,
                },
            );
            (request_id, endpoint, tls, request)
        };

        if let Err(error) = send_project_mapping_request(endpoint, tls, request.clone()) {
            let mut g = self.inner.lock().unwrap();
            g.outbound_project_mappings.remove(&request_id);
            return Err(error);
        }
        app_log(
            "project_mapping_request_sent",
            &[
                ("request_id", request_id.clone()),
                ("project", request.project_name),
                ("peer", request.device.name),
            ],
        );
        Ok(request_id)
    }

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
            if let Some(project) = candidate
                .projects
                .iter()
                .find(|project| project.name == ack.project_name.as_str())
                .cloned()
            {
                g.project_watchers.remove(&project.name);
                if let Some(watcher) = start_project_watcher(&config_path, &candidate, &project) {
                    g.project_watchers.insert(project.name.clone(), watcher);
                }
            }
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
            let project = project_config(
                outbound.project_name.clone(),
                outbound.local_dir.clone(),
                outbound.peer_name.clone(),
                remote_dir.clone(),
                outbound.mode,
            );
            candidate.projects.push(project.clone());
            let path = g.config_path.clone();
            save_config(&path, &candidate)?;
            g.config = candidate.clone();
            g.project_watchers.remove(&project.name);
            if let Some(watcher) = start_project_watcher(&path, &candidate, &project) {
                g.project_watchers.insert(project.name.clone(), watcher);
            }
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
        let project = project_config(name, local, peer_name, remote, mode);
        candidate.projects.push(project.clone());
        let path = g.config_path.clone();
        save_config(&path, &candidate)?;
        g.config = candidate.clone();
        g.project_watchers.remove(&project.name);
        if let Some(watcher) = start_project_watcher(&path, &candidate, &project) {
            g.project_watchers.insert(project.name.clone(), watcher);
        }
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
        g.project_watchers.remove(project_name);
        app_log(
            "project_mapping_deleted",
            &[("project", project_name.to_string())],
        );
        Ok(())
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
            let request_id = aisync_discovery::new_pairing_request_id();
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
                OutboundWorkspaceMapping {
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

    pub fn run_workspace_sync(
        &self,
        workspace_name: &str,
        direction: Direction,
    ) -> Result<SyncReport> {
        if direction != Direction::LocalToRemote {
            return Err(AisyncError::Transport(
                "workspace pull over TCP is not implemented".to_string(),
            ));
        }
        let (config_path, config, workspace, live_connection) = {
            let g = self.inner.lock().unwrap();
            let workspace = g
                .config
                .workspaces
                .iter()
                .find(|workspace| workspace.name == workspace_name)
                .cloned()
                .ok_or_else(|| {
                    AisyncError::Config(format!("workspace '{workspace_name}' not found"))
                })?;
            let peer_name = workspace.effective_peer().map(str::to_string);
            let live_connection = peer_name
                .as_deref()
                .and_then(|peer_name| live_connection_for_config_peer(&g, peer_name));
            (
                g.config_path.clone(),
                g.config.clone(),
                workspace,
                live_connection,
            )
        };
        let outcome = run_workspace_tcp_push(&config_path, &config, &workspace, live_connection)?;
        self.persist_workspace_update(outcome.workspace)?;
        Ok(outcome.report)
    }

    fn persist_workspace_update(&self, workspace: WorkspaceConfig) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        replace_workspace(&mut g.config, workspace);
        save_config(&g.config_path, &g.config)
    }

    /// Run a real push/pull through TCP transport.
    ///
    /// G6: sensitive files are excluded by default. Any path in
    /// `confirmed_sensitive` is explicitly re-included; the rest stay excluded
    /// for this run by adding their exact relative paths to the exclude set.
    pub fn run_sync(
        &self,
        project_name: &str,
        peer_name: &str,
        direction: Direction,
        confirmed_sensitive: &[String],
        confirm_overwrite: bool,
    ) -> Result<SyncReport> {
        let mut g = self.inner.lock().unwrap();
        let project = g.config.project_mapping(project_name, peer_name)?;

        // G6 — compute the unconfirmed sensitive files and exclude them by their
        // exact relative path so confirmed ones flow through normally.
        let sensitive = scan_sensitive_files(&project.local_code_dir)?;
        let confirmed: Vec<&str> = confirmed_sensitive.iter().map(|s| s.as_str()).collect();
        let unconfirmed: Vec<String> = sensitive
            .iter()
            .filter(|s| !confirmed.contains(&s.relative_path.as_str()))
            .map(|s| s.relative_path.clone())
            .collect();

        // Inject the per-run excludes onto the project config entry. Restore
        // afterwards so confirmation is scoped to this single sync.
        let saved = inject_excludes(&mut g.config, project_name, &unconfirmed);

        let live_connection = live_connection_for_config_peer(&g, peer_name);
        let coordinator_cfg = g.config.clone();
        let config_path = g.config_path.clone();
        let log_project = project.project_id.clone();
        let log_remote = project.remote_code_dir.display().to_string();
        let log_bytes = directory_bytes(&project.local_code_dir).unwrap_or(0)
            + directory_bytes(&project.local_session_dir).unwrap_or(0);
        app_log(
            "sync_started",
            &[
                ("project", log_project.clone()),
                ("peer", peer_name.to_string()),
                ("remote_dir", log_remote.clone()),
                ("file_count", "0".to_string()),
                ("bytes", log_bytes.to_string()),
            ],
        );
        let result = match direction {
            Direction::LocalToRemote => run_tcp_push(
                &config_path,
                &coordinator_cfg,
                peer_name,
                &project,
                live_connection,
                confirm_overwrite,
            ),
            Direction::RemoteToLocal => Err(AisyncError::Transport(
                "pull over TCP requires a remote control channel; start a local receiver and run send on the peer".to_string(),
            )),
        };

        restore_excludes(&mut g.config, project_name, saved);
        match &result {
            Ok(report) => app_log(
                "sync_complete",
                &[
                    ("project", log_project),
                    ("peer", peer_name.to_string()),
                    ("remote_dir", log_remote),
                    (
                        "file_count",
                        (report.code_files_transferred + report.session_files_transferred)
                            .to_string(),
                    ),
                    ("bytes", log_bytes.to_string()),
                ],
            ),
            Err(error) => app_log(
                "sync_failed",
                &[
                    ("project", log_project),
                    ("peer", peer_name.to_string()),
                    ("remote_dir", log_remote),
                    ("file_count", "0".to_string()),
                    ("bytes", log_bytes.to_string()),
                    ("error", error.to_string()),
                ],
            ),
        }
        result
    }

    /// 连到对端、查询本项目 remote_code_dir 的状态（是否非空 + 当前 manifest 指纹）。
    /// 一次往返同时服务覆盖检测与脑裂检测。对端离线/连接失败时返回 Err。
    fn probe_target_status(
        &self,
        project_name: &str,
        peer_name: &str,
    ) -> Result<aisync_transport::TargetStatusResponsePayload> {
        let g = self.inner.lock().unwrap();
        let project = g.config.project_mapping(project_name, peer_name)?;
        let live_connection = live_connection_for_config_peer(&g, peer_name);
        let config = g.config.clone();
        let config_path = g.config_path.clone();
        let local_device = g.discoverer.local_device().clone();
        drop(g);

        let connection =
            peer_transport_connection(&config_path, &config, peer_name, live_connection)?;
        let target_dir = project.remote_code_dir.clone();
        let request_id = aisync_discovery::new_pairing_request_id();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| AisyncError::Transport(format!("tokio runtime: {error}")))?;
        runtime.block_on(async {
            let identity = generate_tls_identity("aisync-client")?;
            let tls = TlsConfig::new(identity, connection.server_name.clone())
                .with_pinned_peer_cert(connection.receiver_cert_der.clone());
            let mut transporter = TcpTransporter::connect_to_peer(
                &connection.peer,
                connection.endpoint.port(),
                &tls,
            )
            .await?;
            let response = transporter
                .send_target_status_request(TargetStatusRequestPayload {
                    request_id,
                    target_dir,
                    device: local_device,
                })
                .await;
            transporter.shutdown().await;
            response
        })
    }

    /// 推送前覆盖检测（初始场景：从未同步过）：对端目标目录是否已有文件。
    /// 出错时（对端离线/连接失败）返回 false（视为空，不阻断推送），但记日志。
    pub fn check_target_not_empty(&self, project_name: &str, peer_name: &str) -> Result<bool> {
        match self.probe_target_status(project_name, peer_name) {
            Ok(resp) => {
                app_log(
                    "check_target_not_empty",
                    &[
                        ("project", project_name.to_string()),
                        ("peer", peer_name.to_string()),
                        ("not_empty", resp.not_empty.to_string()),
                        ("file_count", resp.file_count.to_string()),
                    ],
                );
                Ok(resp.not_empty)
            }
            Err(error) => {
                app_log(
                    "check_target_not_empty_failed",
                    &[
                        ("project", project_name.to_string()),
                        ("peer", peer_name.to_string()),
                        ("error", error.to_string()),
                    ],
                );
                Ok(false)
            }
        }
    }

    /// 推送前脑裂检测：比对对端当前 manifest 指纹 vs 本端存的 peer_last_known_hash。
    /// 返回前端弹窗所需的最小状态，不含文件级 diff。
    pub fn check_split_brain(&self, project_name: &str, peer_name: &str) -> SplitBrainStatus {
        let snapshot = {
            let g = self.inner.lock().unwrap();
            g.config.sync_snapshot(project_name, peer_name)
        };

        let resp = match self.probe_target_status(project_name, peer_name) {
            Ok(resp) => resp,
            Err(error) => {
                // 对端不可达：无法判定，交由调用方/前端处理（视作未知，不阻断也不误报脑裂）。
                app_log(
                    "check_split_brain_unreachable",
                    &[
                        ("project", project_name.to_string()),
                        ("peer", peer_name.to_string()),
                        ("error", error.to_string()),
                    ],
                );
                return SplitBrainStatus {
                    reachable: false,
                    has_snapshot: snapshot.is_some(),
                    peer_not_empty: false,
                    split_brain: false,
                };
            }
        };

        let split_brain = match &snapshot {
            // 有快照：对端当前指纹 != 上次已知指纹 => 对端有独立变化 => 脑裂。
            Some(snap) => resp.manifest_hash != snap.peer_last_known_hash,
            // 无快照：从未同步过，不算脑裂（由 not_empty 覆盖检测处理）。
            None => false,
        };

        app_log(
            "check_split_brain",
            &[
                ("project", project_name.to_string()),
                ("peer", peer_name.to_string()),
                ("has_snapshot", snapshot.is_some().to_string()),
                ("peer_not_empty", resp.not_empty.to_string()),
                ("split_brain", split_brain.to_string()),
            ],
        );

        SplitBrainStatus {
            reachable: true,
            has_snapshot: snapshot.is_some(),
            peer_not_empty: resp.not_empty,
            split_brain,
        }
    }
}

/// 推送前脑裂/覆盖检测结果，供前端决定弹哪种确认框。
#[derive(Debug, Clone)]
pub struct SplitBrainStatus {
    /// 对端是否可达（不可达时其余字段无意义）。
    pub reachable: bool,
    /// 本端是否存有该 (项目, 对端) 的同步快照。
    pub has_snapshot: bool,
    /// 对端目标目录当前是否非空。
    pub peer_not_empty: bool,
    /// 是否检测到脑裂（有快照且对端当前指纹与上次已知不一致）。
    pub split_brain: bool,
}

/// Real AI-tool status surfaced to the overview/settings UI.
pub struct AiTool {
    pub name: String,
    pub config_dir: String,
    pub session_count: u32,
    pub installed: bool,
}

// ── helpers ──────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn live_peers_with_endpoints(discoverer: &MdnsDiscoverer) -> Vec<(DeviceInfo, Option<SocketAddr>)> {
    discoverer
        .peers()
        .unwrap_or_default()
        .into_iter()
        .map(|device| {
            let endpoint = discoverer
                .peer_connection_info(&device.id)
                .ok()
                .flatten()
                .and_then(|connection| connection.endpoint);
            (with_endpoint_first(device, endpoint), endpoint)
        })
        .collect()
}

fn active_pairing_session<'a>(
    g: &'a Inner,
    peer_id: &DeviceId,
    now: u64,
) -> Option<&'a PairingSession> {
    g.pairing_sessions
        .get(peer_id)
        .filter(|session| session.expires_at_unix_secs > now)
}

fn with_endpoint_first(mut device: DeviceInfo, endpoint: Option<SocketAddr>) -> DeviceInfo {
    let Some(endpoint) = endpoint else {
        return device;
    };
    let ip = endpoint.ip();
    device.addresses.retain(|address| *address != ip);
    device.addresses.insert(0, ip);
    device
}

fn advertised_local_endpoint(
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

fn peer_from_config(config: &SyncConfig, peer_id: &DeviceId) -> Option<DeviceInfo> {
    config
        .peers
        .iter()
        .find(|(_, peer)| peer.id == *peer_id)
        .map(|(name, peer)| {
            with_endpoint_first(
                DeviceInfo {
                    id: peer.id,
                    name: name.clone(),
                    os: OsType::Other("configured".to_string()),
                    addresses: peer
                        .endpoint
                        .map(|endpoint| vec![endpoint.ip()])
                        .unwrap_or_default(),
                    protocol_version: 1,
                },
                peer.endpoint,
            )
        })
}

fn connection_from_config(config: &SyncConfig, peer_id: &DeviceId) -> Option<PairingConnection> {
    let peer = config.peers.values().find(|peer| peer.id == *peer_id)?;
    Some(PairingConnection {
        endpoint: peer.endpoint,
        receiver_cert_der: peer
            .server_cert
            .as_ref()
            .and_then(|path| fs::read(path).ok()),
        server_name: peer.server_name.clone(),
    })
}

fn pairing_connection_from_discovery(
    connection: &aisync_discovery::PeerConnectionInfo,
) -> PairingConnection {
    PairingConnection {
        endpoint: connection.endpoint,
        receiver_cert_der: connection.receiver_cert_der.clone(),
        server_name: connection.server_name.clone(),
    }
}

fn pairing_tls_config(connection: Option<&PairingConnection>) -> Option<TlsConfig> {
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

fn send_pairing_request_async(
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

fn run_control_future<F, Fut>(name: &'static str, build: F) -> Result<()>
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

fn send_project_mapping_request(
    endpoint: SocketAddr,
    tls: TlsConfig,
    request: ProjectMappingRequestPayload,
) -> Result<()> {
    run_control_future("send_project_mapping_request", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_project_mapping_request(request).await
    })
}

fn send_project_mapping_ack(
    endpoint: SocketAddr,
    tls: TlsConfig,
    ack: ProjectMappingAckPayload,
) -> Result<()> {
    run_control_future("send_project_mapping_ack", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_project_mapping_ack(ack).await
    })
}

fn send_workspace_mapping_request(
    endpoint: SocketAddr,
    tls: TlsConfig,
    request: WorkspaceMappingRequestPayload,
) -> Result<()> {
    run_control_future("send_workspace_mapping_request", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_workspace_mapping_request(request).await
    })
}

fn send_workspace_mapping_ack(
    endpoint: SocketAddr,
    tls: TlsConfig,
    ack: WorkspaceMappingAckPayload,
) -> Result<()> {
    run_control_future("send_workspace_mapping_ack", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_workspace_mapping_ack(ack).await
    })
}

fn send_text_message(
    endpoint: SocketAddr,
    tls: TlsConfig,
    message: TextMessagePayload,
) -> Result<()> {
    run_control_future("send_text_message", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_text_message(message).await
    })
}

fn send_file_transfer_request(
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

fn send_file_transfer_ack(
    endpoint: SocketAddr,
    tls: TlsConfig,
    ack: FileTransferAckPayload,
) -> Result<()> {
    run_control_future("send_file_transfer_ack", move || async move {
        let mut client = TcpTransporter::connect_addr(endpoint, &tls).await?;
        client.send_file_transfer_ack(ack).await
    })
}

fn send_file_transfer_data(
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

fn live_connection_for_config_peer(g: &Inner, peer_name: &str) -> Option<PeerConnectionInfo> {
    let peer = g.config.peers.get(peer_name)?;
    g.discoverer.peer_connection_info(&peer.id).ok().flatten()
}

struct PeerTransportConnection {
    peer: DeviceInfo,
    endpoint: SocketAddr,
    receiver_cert_der: Vec<u8>,
    server_name: String,
    cert_source: String,
}

fn peer_transport_connection(
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

fn control_connection_for_peer(g: &Inner, peer_name: &str) -> Result<(SocketAddr, TlsConfig)> {
    let peer = g
        .config
        .peers
        .get(peer_name)
        .ok_or_else(|| AisyncError::Config(format!("peer '{peer_name}' not found")))?;
    let live = g.discoverer.peer_connection_info(&peer.id).ok().flatten();
    let endpoint = live
        .as_ref()
        .and_then(|connection| connection.endpoint)
        .or(peer.endpoint)
        .ok_or_else(|| {
            AisyncError::Config(format!("peer '{peer_name}' has no receiver endpoint"))
        })?;
    let cert = live
        .as_ref()
        .and_then(|connection| connection.receiver_cert_der.clone())
        .or_else(|| {
            peer.server_cert
                .as_ref()
                .and_then(|path| fs::read(path).ok())
        })
        .ok_or_else(|| {
            AisyncError::Config(format!("peer '{peer_name}' has no pinned receiver cert"))
        })?;
    let server_name = live
        .as_ref()
        .and_then(|connection| connection.server_name.clone())
        .or_else(|| peer.server_name.clone())
        .unwrap_or_else(|| "aisync-receiver".to_string());
    let identity = generate_tls_identity("aisync-client")?;
    Ok((
        endpoint,
        TlsConfig::new(identity, server_name).with_pinned_peer_cert(cert),
    ))
}

struct ProjectMappingAckConnection {
    endpoint: SocketAddr,
    receiver_cert_der: Vec<u8>,
    server_name: String,
    cert_source: String,
}

fn project_mapping_ack_connection(
    live: Option<aisync_discovery::PeerConnectionInfo>,
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

fn workspace_mapping_ack_connection(
    live: Option<aisync_discovery::PeerConnectionInfo>,
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

fn file_transfer_ack_connection(
    live: Option<aisync_discovery::PeerConnectionInfo>,
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

fn prepare_default_file_transfer_accept(
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

fn local_os_type() -> OsType {
    match std::env::consts::OS {
        "macos" => OsType::Darwin,
        "windows" => OsType::Windows,
        "linux" => OsType::Linux,
        other => OsType::Other(other.to_string()),
    }
}

fn default_file_receive_dir(config_path: &Path, config: &SyncConfig) -> PathBuf {
    config.default_file_receive_dir.clone().unwrap_or_else(|| {
        default_downloads_receive_dir().unwrap_or_else(|| config_path.with_file_name("files"))
    })
}

fn default_downloads_receive_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads").join("CodeBaton"))
}

fn ensure_file_transfer_source_allowed(path: &Path, confirmed_sensitive: &[String]) -> Result<()> {
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

fn ensure_file_receive_target(receive_dir: &Path, target_path: &Path) -> Result<PathBuf> {
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

fn safe_filename(filename: &str) -> String {
    Path::new(filename)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "received-file".to_string())
}

fn file_transfer_tmp_path(target_path: &Path, transfer_id: &str) -> PathBuf {
    let filename = target_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "received-file".to_string());
    target_path.with_file_name(format!("{filename}.{transfer_id}.part"))
}

fn receive_file_transfer_data(
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

fn unix_secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn epoch_millis_now_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn epoch_millis_now() -> String {
    epoch_millis_now_u64().to_string()
}

fn normalize_epoch_millis(timestamp: u64) -> u64 {
    if timestamp > 0 && timestamp < 1_000_000_000_000 {
        timestamp.saturating_mul(1000)
    } else {
        timestamp
    }
}

fn update_device_name_locked(g: &mut Inner, name: &str, onboarded: bool) -> Result<()> {
    let name = name.to_string();
    g.config.device.name = name.clone();
    if onboarded {
        g.config.onboarded = true;
    }
    if let Err(error) = g.discoverer.set_local_device_name(name.clone()) {
        app_log(
            "discovery_local_identity_update_failed",
            &[("error", error.to_string())],
        );
    }
    let path = g.config_path.clone();
    let cfg = g.config.clone();
    let result = save_config(&path, &cfg);
    if result.is_ok() {
        app_log(
            "device_name_persisted",
            &[("device_name", name), ("onboarded", onboarded.to_string())],
        );
    }
    result
}

fn directory_bytes(root: &Path) -> Result<u64> {
    let mut total = 0;
    if !root.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total += directory_bytes(&path)?;
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

fn app_log(event: &str, fields: &[(&str, String)]) {
    let mut line = format!("[aisync-app] event={event}");
    for (key, value) in fields {
        let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"<encode-error>\"".into());
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&encoded);
    }
    log_line(&line);
}

fn append_json_line(path: &Path, entry: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    use std::io::Write;
    writeln!(file, "{entry}")?;
    Ok(())
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect()
}

fn record_file_transfer_history(
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

fn record_text_message_history(
    config_path: &Path,
    peer_name: Option<&str>,
    message: &TextMessagePayload,
    mine: bool,
) {
    let path = config_path.with_file_name("chat_history.jsonl");
    let peer_name = peer_name.unwrap_or(&message.sender_name);
    let entry = serde_json::json!({
        "timestamp": normalize_epoch_millis(message.timestamp),
        "peerName": peer_name,
        "senderName": message.sender_name,
        "content": message.content,
        "mine": mine,
    });
    match append_json_line(&path, &entry) {
        Ok(()) => app_log(
            "chat_store_appended",
            &[
                ("peer", peer_name.to_string()),
                ("sender", message.sender_name.clone()),
                ("bytes", message.content.len().to_string()),
            ],
        ),
        Err(error) => app_log(
            "chat_store_append_failed",
            &[
                ("peer", peer_name.to_string()),
                ("sender", message.sender_name.clone()),
                ("error", error.to_string()),
            ],
        ),
    }
}

#[derive(Debug, Clone, Default)]
struct HistoryFileSummary {
    bytes: u64,
    file_paths: Vec<String>,
}

impl HistoryFileSummary {
    fn add_file(&mut self, path: &Path) {
        if let Ok(metadata) = fs::metadata(path) {
            if metadata.is_file() {
                self.bytes = self.bytes.saturating_add(metadata.len());
                if self.file_paths.len() < HISTORY_FILE_LIMIT {
                    self.file_paths.push(path.display().to_string());
                }
            }
        }
    }
}

fn history_summary_from_config(
    config: &SyncConfig,
    project_id: &str,
    workspace_name: Option<&str>,
    child_name: Option<&str>,
    file_type: &str,
) -> HistoryFileSummary {
    if let Some(workspace_name) = workspace_name {
        if let Some(workspace) = config
            .workspaces
            .iter()
            .find(|workspace| workspace.name == workspace_name)
        {
            return workspace_history_summary(config, workspace, child_name, file_type);
        }
    }
    if let Some(workspace) = config
        .workspaces
        .iter()
        .find(|workspace| workspace.name == project_id)
    {
        return workspace_history_summary(config, workspace, child_name, file_type);
    }
    if let Some(workspace) = config.workspaces.iter().find(|workspace| {
        workspace
            .children
            .iter()
            .any(|child| child.name == project_id)
    }) {
        return workspace_history_summary(config, workspace, Some(project_id), file_type);
    }
    if let Some(project) = config
        .projects
        .iter()
        .find(|project| project.name == project_id)
    {
        return project_history_summary(config, project, file_type);
    }
    HistoryFileSummary::default()
}

fn project_history_summary(
    config: &SyncConfig,
    project: &ProjectConfig,
    file_type: &str,
) -> HistoryFileSummary {
    let mut summary = HistoryFileSummary::default();
    if matches!(file_type, "code" | "mixed") {
        add_tree_history_summary(&mut summary, &project.local);
    }
    if matches!(file_type, "session" | "mixed") {
        for path in claude_mtime_paths(config, std::slice::from_ref(&project.local)) {
            add_tree_history_summary(&mut summary, &path);
        }
        add_codex_history_summary(&mut summary, |file| {
            codex_session_file_matches_project(file, &project.local)
        });
    }
    summary
}

fn workspace_history_summary(
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
    child_name: Option<&str>,
    file_type: &str,
) -> HistoryFileSummary {
    let mut summary = HistoryFileSummary::default();
    let roots: Vec<PathBuf> = if let Some(child_name) = child_name {
        workspace
            .children
            .iter()
            .find(|child| child.name == child_name)
            .map(|child| vec![child.local_dir.clone()])
            .unwrap_or_default()
    } else {
        vec![workspace.effective_local_root().to_path_buf()]
    };
    if matches!(file_type, "code" | "mixed") {
        for root in &roots {
            add_tree_history_summary(&mut summary, root);
        }
    }
    if matches!(file_type, "session" | "mixed") {
        for path in claude_mtime_paths(config, &roots) {
            add_tree_history_summary(&mut summary, &path);
        }
        let excluded = workspace
            .children
            .iter()
            .filter(|child| child.conflicted || !child.enabled)
            .map(|child| child.name.clone())
            .collect::<HashSet<_>>();
        add_codex_history_summary(&mut summary, |file| {
            if let Some(child_root) = roots.first().filter(|_| child_name.is_some()) {
                codex_session_file_matches_project(file, child_root)
            } else {
                codex_session_file_matches_workspace(
                    file,
                    workspace.effective_local_root(),
                    &excluded,
                )
            }
        });
    }
    summary
}

fn add_tree_history_summary(summary: &mut HistoryFileSummary, root: &Path) {
    if !root.exists() || should_skip_hash_path(root) {
        return;
    }
    let Ok(metadata) = fs::metadata(root) else {
        return;
    };
    if metadata.is_file() {
        summary.add_file(root);
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        add_tree_history_summary(summary, &path);
    }
}

fn add_codex_history_summary(
    summary: &mut HistoryFileSummary,
    mut matches: impl FnMut(&Path) -> bool,
) {
    let Some(root) = local_codex_sessions_dir() else {
        return;
    };
    let mut files = Vec::new();
    if collect_jsonl_files(&root, &mut files).is_err() {
        return;
    }
    files.retain(|file| matches(file));
    files.sort();
    for file in files {
        summary.add_file(&file);
    }
}

fn record_auto_sync_history(
    config_path: &Path,
    project_id: &str,
    success: bool,
    files: u32,
    detail: Option<String>,
    workspace_name: Option<&str>,
    child_name: Option<&str>,
    file_type: &str,
) {
    let path = config_path.with_file_name("history.jsonl");
    let summary = if success {
        load_config(config_path)
            .ok()
            .map(|config| {
                history_summary_from_config(
                    &config,
                    project_id,
                    workspace_name,
                    child_name,
                    file_type,
                )
            })
            .unwrap_or_default()
    } else {
        HistoryFileSummary::default()
    };
    let bytes = summary.bytes;
    let file_paths = summary.file_paths.clone();
    let file_path = summary.file_paths.first().cloned();
    let file_name = file_path.as_deref().and_then(|path| {
        Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    let file_names: Vec<String> = summary
        .file_paths
        .iter()
        .filter_map(|path| {
            Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    let event_id = aisync_discovery::new_pairing_request_id();
    let entry = serde_json::json!({
        "eventId": event_id,
        "timestamp": epoch_millis_now(),
        "projectId": project_id,
        "direction": "push",
        "success": success,
        "files": files,
        "bytes": bytes,
        "detail": detail,
        "workspaceName": workspace_name,
        "childName": child_name,
        "trigger": "auto",
        "role": "sender",
        "fileType": file_type,
        "file_path": file_path,
        "file_paths": file_paths,
        "file_name": file_name,
        "file_names": file_names,
    });
    app_log(
        "record_sync_started",
        &[
            ("project", project_id.to_string()),
            ("trigger", "auto".to_string()),
            ("success", success.to_string()),
            ("bytes", bytes.to_string()),
            (
                "file_path",
                entry
                    .get("file_path")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ),
        ],
    );
    let result = (|| -> std::io::Result<()> {
        append_json_line(&path, &entry)?;
        Ok(())
    })();
    match result {
        Ok(()) => app_log(
            "sender_sync_history_recorded",
            &[
                ("project", project_id.to_string()),
                ("event_id", event_id.clone()),
                ("role", "sender".to_string()),
                ("path", path.display().to_string()),
            ],
        ),
        Err(error) => app_log(
            "history_write_failed",
            &[
                ("project", project_id.to_string()),
                ("event_id", event_id),
                ("path", path.display().to_string()),
                ("error", error.to_string()),
            ],
        ),
    }
}

fn record_receiver_sync_history(config_path: &Path, manifest: &SyncManifest, receive_dir: &Path) {
    let _ = refresh_and_save_workspaces(config_path);
    if manifest.files.is_empty() {
        return;
    }
    let path = config_path.with_file_name("history.jsonl");
    let bytes: u64 = manifest.files.iter().map(|file| file.size).sum();
    let file_type = manifest_file_type(manifest);
    let (project_id, workspace_name, child_name, suppress_root) =
        receiver_history_scope(config_path, manifest);
    if let Some(root) = suppress_root.as_deref().or(Some(receive_dir)) {
        mark_incoming_sync_root(root);
    }
    if matches!(file_type, "session" | "mixed") {
        mark_incoming_session_roots(config_path);
    }
    let event_id = aisync_discovery::new_pairing_request_id();
    let entry = serde_json::json!({
        "eventId": event_id,
        "timestamp": epoch_millis_now(),
        "projectId": project_id,
        "direction": "receive",
        "success": true,
        "files": manifest.files.len() as u32,
        "bytes": bytes,
        "detail": format!("received into {}", receive_dir.display()),
        "workspaceName": workspace_name,
        "childName": child_name,
        "trigger": "auto",
        "role": "receiver",
        "fileType": file_type,
    });
    let result = (|| -> std::io::Result<()> {
        append_json_line(&path, &entry)?;
        Ok(())
    })();
    match result {
        Ok(()) => app_log(
            "receiver_sync_history_recorded",
            &[
                ("project", project_id),
                ("event_id", event_id.clone()),
                ("file_count", manifest.files.len().to_string()),
                ("file_type", file_type.to_string()),
                ("path", path.display().to_string()),
            ],
        ),
        Err(error) => app_log(
            "receiver_sync_history_failed",
            &[
                ("project", project_id),
                ("event_id", event_id),
                ("path", path.display().to_string()),
                ("error", error.to_string()),
            ],
        ),
    }
}

fn refresh_and_save_workspaces(config_path: &Path) -> Option<SyncConfig> {
    let Ok(config) = load_config(config_path) else {
        return None;
    };
    let (refreshed, changed) = refresh_workspaces_in_config(&config);
    if changed {
        if let Err(error) = save_config(config_path, &refreshed) {
            app_log(
                "workspace_children_persist_failed",
                &[
                    ("config", config_path.display().to_string()),
                    ("error", error.to_string()),
                ],
            );
            return Some(config);
        }
        app_log(
            "workspace_children_refresh_notified",
            &[("config", config_path.display().to_string())],
        );
    }
    Some(refreshed)
}

fn mark_incoming_session_roots(config_path: &Path) {
    if let Ok(config) = load_config(config_path) {
        if let Some(path) = local_claude_projects_root(&config) {
            mark_incoming_sync_root(&path);
        }
    }
    if let Some(path) = local_codex_sessions_dir() {
        mark_incoming_sync_root(&path);
    }
}

fn receiver_history_scope(
    config_path: &Path,
    manifest: &SyncManifest,
) -> (String, Option<String>, Option<String>, Option<PathBuf>) {
    let Ok(config) = load_config(config_path) else {
        return ("incoming".to_string(), None, None, None);
    };

    for workspace in &config.workspaces {
        let root = workspace.effective_local_root();
        let matched = manifest.files.iter().any(|file| {
            safe_relative_path(&file.relative_path)
                .map(|rel| root.join(rel).exists())
                .unwrap_or(false)
        });
        if !matched {
            continue;
        }
        let child_names: HashSet<String> = manifest
            .files
            .iter()
            .filter_map(|file| Path::new(&file.relative_path).components().next())
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .filter(|name| workspace.children.iter().any(|child| child.name == *name))
            .collect();
        let child_name = (child_names.len() == 1)
            .then(|| child_names.iter().next().cloned())
            .flatten();
        let suppress_root = child_name
            .as_ref()
            .and_then(|name| workspace.children.iter().find(|child| child.name == *name))
            .map(|child| child.local_dir.clone())
            .unwrap_or_else(|| root.to_path_buf());
        let project_id = child_name.clone().unwrap_or_else(|| workspace.name.clone());
        return (
            project_id,
            Some(workspace.name.clone()),
            child_name,
            Some(suppress_root),
        );
    }

    for project in &config.projects {
        let matched = manifest.files.iter().any(|file| {
            safe_relative_path(&file.relative_path)
                .map(|rel| project.local.join(rel).exists())
                .unwrap_or(false)
        });
        if matched {
            return (
                project.name.clone(),
                None,
                None,
                Some(project.local.clone()),
            );
        }
    }

    ("incoming".to_string(), None, None, None)
}

fn safe_relative_path(relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return None;
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(path.to_path_buf())
}

fn record_auto_workspace_child_history(
    config_path: &Path,
    workspace: &WorkspaceConfig,
    success: bool,
    detail: Option<&str>,
    file_type: &str,
    child_file_counts: Option<&HashMap<String, u32>>,
) {
    for child in &workspace.children {
        if !child.enabled || child.conflicted {
            continue;
        }
        let files = match (success, child_file_counts) {
            (true, Some(counts)) => {
                let files = counts.get(&child.name).copied().unwrap_or(0);
                if files == 0 {
                    continue;
                }
                files
            }
            (true, None) => {
                let Ok(config) = load_config(config_path) else {
                    continue;
                };
                let summary = history_summary_from_config(
                    &config,
                    &child.name,
                    Some(&workspace.name),
                    Some(&child.name),
                    file_type,
                );
                let files = summary.file_paths.len() as u32;
                if files == 0 {
                    continue;
                }
                files
            }
            (false, _) => 0,
        };
        record_auto_sync_history(
            config_path,
            &child.name,
            success,
            files,
            detail.map(str::to_string),
            Some(&workspace.name),
            Some(&child.name),
            file_type,
        );
    }
}

fn manifest_file_type(manifest: &SyncManifest) -> &'static str {
    let session_files = manifest
        .files
        .iter()
        .filter(|file| {
            file.relative_path.ends_with(".jsonl")
                || file.relative_path.contains(".claude")
                || file.relative_path.contains(".codex")
        })
        .count();
    if session_files == 0 {
        "code"
    } else if session_files == manifest.files.len() {
        "session"
    } else {
        "mixed"
    }
}

/// Tee a log line to stderr AND `~/.aisync/logs/aisync.log`.
///
/// When the DMG is launched via `open -a`, stderr is redirected to /dev/null,
/// so stderr-only logs are invisible to qa. The file sink makes
/// `cat ~/.aisync/logs/aisync.log` work for field diagnostics.
pub fn log_line(line: &str) {
    eprintln!("{line}");
    if let Some(path) = log_file_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            let _ = writeln!(f, "{} {}", now_stamp(), line);
        }
    }
}

fn log_file_path() -> Option<PathBuf> {
    std::env::var_os("AISYNC_LOG_FILE")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".aisync").join("logs").join("aisync.log")))
}

/// Coarse wall-clock stamp for log lines (no extra deps): seconds since epoch.
fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("[t={}]", d.as_secs()),
        Err(_) => "[t=?]".to_string(),
    }
}

/// Count first-level **project directories** under an AI-tool's `projects/`
/// dir — this is the "项目会话" number the UI shows (one entry per project the
/// tool has touched), NOT the raw `.jsonl` session-file count, which inflates
/// it ~3× (a project accumulates many session files). Falls back to counting
/// child dirs of the config dir itself when there is no `projects/` subdir.
/// Best-effort: a missing dir counts as zero.
fn count_project_dirs(config_dir: &Path) -> u32 {
    let projects = config_dir.join("projects");
    let root = if projects.is_dir() {
        projects
    } else {
        config_dir.to_path_buf()
    };
    let mut count = 0u32;
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                count += 1;
            }
        }
    }
    count
}

fn peer_device_info(config: &SyncConfig, peer_name: &str) -> Result<DeviceInfo> {
    let peer = config
        .peers
        .get(peer_name)
        .ok_or_else(|| AisyncError::Config(format!("peer '{peer_name}' not found")))?;
    Ok(DeviceInfo {
        id: peer.id,
        name: peer.name.clone(),
        os: OsType::Other("configured".to_string()),
        addresses: peer
            .endpoint
            .map(|endpoint| vec![endpoint.ip()])
            .unwrap_or_default(),
        protocol_version: 1,
    })
}

fn scan_workspace_direct(
    workspace: &aisync_sync::WorkspaceConfig,
    peer_name: &str,
) -> Result<Vec<DiscoveredProject>> {
    if workspace.scan_depth != 1 {
        return Err(AisyncError::Config(
            "workspace scan_depth other than 1 is not supported".to_string(),
        ));
    }
    let remote_base = workspace.effective_remote_root(peer_name).ok_or_else(|| {
        AisyncError::Config(format!(
            "workspace '{}' has no mapping for peer '{}'",
            workspace.name, peer_name
        ))
    })?;
    let remote_names = first_level_dir_names(&remote_base)?;
    let mut projects = Vec::new();
    for entry in fs::read_dir(workspace.effective_local_root())? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        projects.push(DiscoveredProject {
            name: name.clone(),
            local_code_dir: entry.path(),
            remote_code_dir: remote_base.join(&name),
            enabled: workspace.auto_enable_new && remote_names.contains(&name),
            matched_remote: remote_names.contains(&name),
        });
    }
    projects.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(projects)
}

fn workspace_config(
    name: String,
    local_root: PathBuf,
    peer_name: String,
    remote_root: PathBuf,
    mode: SyncModeConfig,
    auto_enable_new: bool,
) -> Result<WorkspaceConfig> {
    let mut peers = HashMap::new();
    peers.insert(peer_name.clone(), remote_root.clone());
    let children = workspace_children(&local_root, &remote_root, auto_enable_new)?;
    Ok(WorkspaceConfig {
        name,
        local_root: local_root.clone(),
        remote_root: remote_root.clone(),
        peer: peer_name,
        children,
        local: local_root,
        peers,
        scan_depth: 1,
        auto_enable_new,
        sync_mode: mode,
        enabled: true,
        exclude_rules: Vec::new(),
    })
}

fn workspace_config_with_child_names(
    name: String,
    local_root: PathBuf,
    peer_name: String,
    remote_root: PathBuf,
    mode: SyncModeConfig,
    auto_enable_new: bool,
    child_names: &[String],
) -> WorkspaceConfig {
    let mut peers = HashMap::new();
    peers.insert(peer_name.clone(), remote_root.clone());
    let mut children: Vec<WorkspaceChildConfig> = child_names
        .iter()
        .filter(|name| !name.starts_with('.'))
        .map(|name| WorkspaceChildConfig {
            name: name.clone(),
            local_dir: local_root.join(name),
            remote_dir: remote_root.join(name),
            enabled: auto_enable_new,
            conflicted: false,
            last_fingerprint: None,
        })
        .collect();
    children.sort_by(|left, right| left.name.cmp(&right.name));
    WorkspaceConfig {
        name,
        local_root: local_root.clone(),
        remote_root: remote_root.clone(),
        peer: peer_name,
        children,
        local: local_root,
        peers,
        scan_depth: 1,
        auto_enable_new,
        sync_mode: mode,
        enabled: true,
        exclude_rules: Vec::new(),
    }
}

fn workspace_children(
    local_root: &Path,
    remote_root: &Path,
    enabled: bool,
) -> Result<Vec<WorkspaceChildConfig>> {
    let mut children = Vec::new();
    for entry in fs::read_dir(local_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        children.push(WorkspaceChildConfig {
            name: name.clone(),
            local_dir: entry.path(),
            remote_dir: remote_root.join(&name),
            enabled,
            conflicted: false,
            last_fingerprint: None,
        });
    }
    children.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(children)
}

fn start_project_watchers(config_path: &Path, config: &SyncConfig) -> HashMap<String, FsWatcher> {
    let mut watchers = HashMap::new();
    for project in &config.projects {
        if let Some(watcher) = start_project_watcher(config_path, config, project) {
            watchers.insert(project.name.clone(), watcher);
        }
    }
    watchers
}

fn start_project_watcher(
    config_path: &Path,
    config: &SyncConfig,
    project: &ProjectConfig,
) -> Option<FsWatcher> {
    if !project.enabled {
        return None;
    }
    let peer_name = project.peers.keys().next()?.clone();
    let paths = project_watch_paths(config, project);
    if paths.is_empty() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    let watcher = FsWatcher::start(
        WatchConfig {
            paths: paths.clone(),
            debounce: aisync_sync::DEFAULT_DEBOUNCE,
            exclude_rules: project_exclude_rules(config, project),
        },
        tx,
    )
    .map_err(|error| {
        app_log(
            "project_watch_failed",
            &[
                ("project", project.name.clone()),
                ("local_root", project.local.display().to_string()),
                ("error", error.to_string()),
            ],
        );
        error
    })
    .ok()?;

    let config_path = config_path.to_path_buf();
    let fallback_config = config.clone();
    let project_name = project.name.clone();
    let project_root = project.local.clone();
    let initial_fingerprint = project_auto_sync_fingerprint(config, &project_name, &peer_name);
    std::thread::spawn(move || {
        let mut suppress_until: Option<Instant> = None;
        let mut last_fingerprint = initial_fingerprint;
        while let Ok(batch) = rx.recv() {
            let config = load_config(&config_path).unwrap_or_else(|_| fallback_config.clone());
            let fingerprint = project_auto_sync_fingerprint(&config, &project_name, &peer_name);
            if suppress_until
                .map(|until| Instant::now() < until)
                .unwrap_or(false)
            {
                if let Some(fingerprint) = fingerprint {
                    app_log(
                        "baseline_updated",
                        &[
                            ("scope", "project".to_string()),
                            ("name", project_name.clone()),
                            ("peer", peer_name.clone()),
                            ("trigger", "watcher_cooldown".to_string()),
                            ("hash", hash_prefix(&fingerprint)),
                        ],
                    );
                    last_fingerprint = Some(fingerprint);
                }
                app_log(
                    "project_auto_sync_suppressed",
                    &[
                        ("project", project_name.clone()),
                        ("peer", peer_name.clone()),
                        ("reason", "cooldown".to_string()),
                        ("change_count", batch.changes.len().to_string()),
                    ],
                );
                continue;
            }
            app_log(
                "project_change_detected",
                &[
                    ("project", project_name.clone()),
                    ("change_count", batch.changes.len().to_string()),
                ],
            );
            if fingerprint.is_some() && fingerprint == last_fingerprint {
                app_log(
                    "sync_fingerprint_gate_hit",
                    &[
                        ("scope", "project".to_string()),
                        ("name", project_name.clone()),
                        ("peer", peer_name.clone()),
                        ("trigger", "watcher".to_string()),
                        (
                            "hash",
                            fingerprint
                                .as_ref()
                                .map(|hash| hash_prefix(hash))
                                .unwrap_or_default(),
                        ),
                    ],
                );
                continue;
            }
            if let Some(fingerprint) = &fingerprint {
                app_log(
                    "sync_fingerprint_gate_miss",
                    &[
                        ("scope", "project".to_string()),
                        ("name", project_name.clone()),
                        ("peer", peer_name.clone()),
                        ("trigger", "watcher".to_string()),
                        ("hash", hash_prefix(fingerprint)),
                        (
                            "previous",
                            last_fingerprint
                                .as_ref()
                                .map(|previous| hash_prefix(previous))
                                .unwrap_or_default(),
                        ),
                    ],
                );
            }
            if incoming_sync_recent(&project_root) {
                app_log(
                    "auto_sync_suppressed",
                    &[
                        ("scope", "project".to_string()),
                        ("name", project_name.clone()),
                        ("peer", peer_name.clone()),
                        ("reason", "incoming_receive".to_string()),
                    ],
                );
                if fingerprint.is_some() {
                    last_fingerprint = fingerprint;
                }
                suppress_until = Some(Instant::now() + AUTO_SYNC_COOLDOWN);
                continue;
            }
            let Some(gate_key) =
                try_begin_auto_sync("project", &project_name, &peer_name, "watcher")
            else {
                if let Some(fingerprint) = fingerprint {
                    last_fingerprint = Some(fingerprint);
                }
                continue;
            };
            match run_project_auto_sync(&config_path, &config, &project_name, &peer_name, None) {
                Ok(report) => {
                    let post_config = load_config(&config_path).unwrap_or_else(|_| config.clone());
                    if let Some(post_fingerprint) =
                        project_auto_sync_fingerprint(&post_config, &project_name, &peer_name)
                    {
                        app_log(
                            "baseline_updated",
                            &[
                                ("scope", "project".to_string()),
                                ("name", project_name.clone()),
                                ("peer", peer_name.clone()),
                                ("trigger", "watcher".to_string()),
                                ("hash", hash_prefix(&post_fingerprint)),
                            ],
                        );
                        last_fingerprint = Some(post_fingerprint);
                    }
                    let files =
                        (report.code_files_transferred + report.session_files_transferred) as u32;
                    record_auto_sync_history(
                        &config_path,
                        &project_name,
                        true,
                        files,
                        None,
                        None,
                        None,
                        "mixed",
                    );
                    app_log(
                        "project_auto_sync_complete",
                        &[
                            ("project", project_name.clone()),
                            ("peer", peer_name.clone()),
                            ("file_count", files.to_string()),
                        ],
                    );
                }
                Err(error) => {
                    record_auto_sync_history(
                        &config_path,
                        &project_name,
                        false,
                        0,
                        Some(error.to_string()),
                        None,
                        None,
                        "mixed",
                    );
                    app_log(
                        "project_auto_sync_failed",
                        &[
                            ("project", project_name.clone()),
                            ("peer", peer_name.clone()),
                            ("error", error.to_string()),
                        ],
                    );
                }
            }
            finish_auto_sync(&gate_key);
            suppress_until = Some(Instant::now() + AUTO_SYNC_COOLDOWN);
        }
    });

    app_log(
        "project_watch_started",
        &[
            ("project", project.name.clone()),
            ("local_root", project.local.display().to_string()),
            ("path_count", paths.len().to_string()),
        ],
    );
    Some(watcher)
}

fn start_workspace_watchers(config_path: &Path, config: &SyncConfig) -> HashMap<String, FsWatcher> {
    let mut watchers = HashMap::new();
    for workspace in &config.workspaces {
        if let Some(watcher) = start_workspace_watcher(config_path, config, workspace) {
            watchers.insert(workspace.name.clone(), watcher);
        }
    }
    watchers
}

fn start_workspace_watcher(
    config_path: &Path,
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
) -> Option<FsWatcher> {
    if !workspace.enabled {
        return None;
    }
    let local_root = workspace.effective_local_root().to_path_buf();
    let paths = workspace_watch_paths(config, workspace);
    if paths.is_empty() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    let watcher = FsWatcher::start(
        WatchConfig {
            paths: paths.clone(),
            debounce: aisync_sync::DEFAULT_DEBOUNCE,
            exclude_rules: workspace_exclude_rules(config, workspace),
        },
        tx,
    )
    .map_err(|error| {
        app_log(
            "workspace_watch_failed",
            &[
                ("workspace", workspace.name.clone()),
                ("local_root", local_root.display().to_string()),
                ("error", error.to_string()),
            ],
        );
        error
    })
    .ok()?;

    let config_path = config_path.to_path_buf();
    let fallback_config = config.clone();
    let workspace_name = workspace.name.clone();
    let workspace_root = local_root.clone();
    let initial_fingerprint = workspace_auto_sync_fingerprint(config, workspace);
    std::thread::spawn(move || {
        let mut suppress_until: Option<Instant> = None;
        let mut last_fingerprint = initial_fingerprint;
        while let Ok(batch) = rx.recv() {
            let config = load_config(&config_path).unwrap_or_else(|_| fallback_config.clone());
            let Some(workspace) = config
                .workspaces
                .iter()
                .find(|workspace| workspace.name == workspace_name)
                .cloned()
            else {
                continue;
            };
            let peer_name = workspace.effective_peer().unwrap_or_default().to_string();
            let bypass_pending = workspace_first_propagation_pending(&workspace_name, &peer_name);
            let fingerprint = workspace_auto_sync_fingerprint(&config, &workspace);
            if !bypass_pending
                && suppress_until
                    .map(|until| Instant::now() < until)
                    .unwrap_or(false)
            {
                if let Some(fingerprint) = fingerprint {
                    app_log(
                        "baseline_updated",
                        &[
                            ("scope", "workspace".to_string()),
                            ("name", workspace_name.clone()),
                            ("peer", peer_name.clone()),
                            ("trigger", "watcher_cooldown".to_string()),
                            ("hash", hash_prefix(&fingerprint)),
                        ],
                    );
                    last_fingerprint = Some(fingerprint);
                }
                app_log(
                    "workspace_auto_sync_suppressed",
                    &[
                        ("workspace", workspace_name.clone()),
                        ("reason", "cooldown".to_string()),
                        ("change_count", batch.changes.len().to_string()),
                    ],
                );
                continue;
            }
            app_log(
                "workspace_change_detected",
                &[
                    ("workspace", workspace_name.clone()),
                    ("change_count", batch.changes.len().to_string()),
                ],
            );
            if !bypass_pending && fingerprint.is_some() && fingerprint == last_fingerprint {
                app_log(
                    "sync_fingerprint_gate_hit",
                    &[
                        ("scope", "workspace".to_string()),
                        ("name", workspace_name.clone()),
                        ("trigger", "watcher".to_string()),
                        (
                            "hash",
                            fingerprint
                                .as_ref()
                                .map(|hash| hash_prefix(hash))
                                .unwrap_or_default(),
                        ),
                    ],
                );
                continue;
            }
            if let Some(fingerprint) = &fingerprint {
                app_log(
                    "sync_fingerprint_gate_miss",
                    &[
                        ("scope", "workspace".to_string()),
                        ("name", workspace_name.clone()),
                        ("trigger", "watcher".to_string()),
                        ("hash", hash_prefix(fingerprint)),
                        (
                            "previous",
                            last_fingerprint
                                .as_ref()
                                .map(|previous| hash_prefix(previous))
                                .unwrap_or_default(),
                        ),
                    ],
                );
            }
            if incoming_sync_recent(&workspace_root) {
                app_log(
                    "auto_sync_suppressed",
                    &[
                        ("scope", "workspace".to_string()),
                        ("name", workspace_name.clone()),
                        ("reason", "incoming_receive".to_string()),
                    ],
                );
                if fingerprint.is_some() {
                    last_fingerprint = fingerprint;
                }
                suppress_until = Some(Instant::now() + AUTO_SYNC_COOLDOWN);
                continue;
            }
            let gate_key = if bypass_pending {
                begin_auto_sync_bypass_cooldown(
                    "workspace",
                    &workspace_name,
                    &peer_name,
                    "new_child",
                )
            } else {
                try_begin_auto_sync("workspace", &workspace_name, &peer_name, "watcher")
            };
            let Some(gate_key) = gate_key else {
                if let Some(fingerprint) = fingerprint {
                    last_fingerprint = Some(fingerprint);
                }
                continue;
            };
            if bypass_pending {
                clear_workspace_first_propagation(&workspace_name, &peer_name);
            }
            match run_workspace_auto_sync_outcome(&config_path, &config, &workspace, None) {
                Ok(outcome) => {
                    let post_config = load_config(&config_path).unwrap_or_else(|_| config.clone());
                    if let Some(updated_workspace) = post_config
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.name == workspace_name)
                    {
                        if let Some(post_fingerprint) =
                            workspace_auto_sync_fingerprint(&post_config, updated_workspace)
                        {
                            app_log(
                                "baseline_updated",
                                &[
                                    ("scope", "workspace".to_string()),
                                    ("name", workspace_name.clone()),
                                    ("peer", peer_name.clone()),
                                    ("trigger", "watcher".to_string()),
                                    ("hash", hash_prefix(&post_fingerprint)),
                                ],
                            );
                            last_fingerprint = Some(post_fingerprint);
                        }
                    }
                    let files = (outcome.report.code_files_transferred
                        + outcome.report.session_files_transferred)
                        as u32;
                    record_auto_sync_history(
                        &config_path,
                        &workspace_name,
                        true,
                        files,
                        None,
                        Some(&workspace_name),
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
                        "workspace_auto_sync_complete",
                        &[
                            ("workspace", workspace_name.clone()),
                            ("file_count", files.to_string()),
                        ],
                    );
                }
                Err(error) => {
                    let detail = error.to_string();
                    record_auto_sync_history(
                        &config_path,
                        &workspace_name,
                        false,
                        0,
                        Some(detail.clone()),
                        Some(&workspace_name),
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
                        "workspace_auto_sync_failed",
                        &[
                            ("workspace", workspace_name.clone()),
                            ("error", error.to_string()),
                        ],
                    );
                }
            }
            finish_auto_sync(&gate_key);
            suppress_until = Some(Instant::now() + AUTO_SYNC_COOLDOWN);
        }
    });

    app_log(
        "workspace_watch_started",
        &[
            ("workspace", workspace.name.clone()),
            ("local_root", local_root.display().to_string()),
            ("path_count", paths.len().to_string()),
        ],
    );
    Some(watcher)
}

fn replace_workspace(config: &mut SyncConfig, workspace: WorkspaceConfig) {
    config
        .workspaces
        .retain(|existing| existing.name != workspace.name);
    config.workspaces.push(workspace);
}

fn refresh_workspaces_in_config(config: &SyncConfig) -> (SyncConfig, bool) {
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

fn first_level_dir_names(root: &Path) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    if !root.exists() {
        return Ok(names);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(names)
}

fn workspace_exclude_rules(config: &SyncConfig, workspace: &WorkspaceConfig) -> Vec<String> {
    let mut rules = aisync_sync::default_exclude_rules();
    rules.extend(config.exclude_rules.clone());
    rules.extend(workspace.exclude_rules.clone());
    aisync_sync::expand_exclude_rules(&rules)
}

fn project_exclude_rules(config: &SyncConfig, project: &ProjectConfig) -> Vec<String> {
    let mut rules = aisync_sync::default_exclude_rules();
    rules.extend(config.exclude_rules.clone());
    rules.extend(project.exclude_rules.clone());
    aisync_sync::expand_exclude_rules(&rules)
}

fn project_watch_paths(config: &SyncConfig, project: &ProjectConfig) -> Vec<PathBuf> {
    let mut paths = vec![project.local.clone()];
    paths.extend(claude_watch_paths(
        config,
        std::slice::from_ref(&project.local),
    ));
    existing_unique_paths(paths)
}

fn workspace_watch_paths(config: &SyncConfig, workspace: &WorkspaceConfig) -> Vec<PathBuf> {
    let mut code_roots = vec![workspace.effective_local_root().to_path_buf()];
    code_roots.extend(
        workspace
            .children
            .iter()
            .map(|child| child.local_dir.clone()),
    );

    let mut paths = vec![workspace.effective_local_root().to_path_buf()];
    paths.extend(claude_watch_paths(config, &code_roots));
    existing_unique_paths(paths)
}

fn claude_watch_paths(config: &SyncConfig, code_roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(projects_root) = local_claude_projects_root(config) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for root in code_roots {
        let encoded = projects_root.join(claude_project_dir_name(root));
        if encoded.exists() {
            paths.push(encoded);
        }
    }
    paths
}

fn local_claude_projects_root(config: &SyncConfig) -> Option<PathBuf> {
    let configured = if config.claude_config.local.as_os_str().is_empty() {
        home_dir()?.join(".claude")
    } else {
        config.claude_config.local.clone()
    };
    local_claude_projects_dir(&configured)
}

fn existing_unique_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for path in paths {
        if path.exists() && seen.insert(path.clone()) {
            unique.push(path);
        }
    }
    unique
}

#[derive(Clone)]
struct SessionMtimeTarget {
    scope: &'static str,
    name: String,
    peer: String,
    tool: &'static str,
    path: PathBuf,
}

fn session_target_key(target: &SessionMtimeTarget) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        target.scope,
        target.name,
        target.peer,
        target.tool,
        target.path.display()
    )
}

fn session_sync_key(target: &SessionMtimeTarget) -> String {
    format!(
        "{}:{}:{}:{}",
        target.scope, target.name, target.peer, target.tool
    )
}

fn session_seed_key(config_path: &Path, target_key: &str) -> String {
    format!("{}:{target_key}", config_path.display())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionMtimeDecision {
    BaselineNew,
    TriggerNew,
    TriggerModified,
    Unchanged,
}

fn classify_session_mtime(
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

fn hash_prefix(fingerprint: &str) -> String {
    fingerprint.chars().take(8).collect()
}

fn baseline_session_target(
    config_path: &Path,
    config: &SyncConfig,
    target: &SessionMtimeTarget,
    mtime: SystemTime,
    seen: &mut HashMap<String, SystemTime>,
    content_seen: &mut HashMap<String, String>,
    sync_seen: &mut HashMap<String, String>,
    event: &str,
) {
    let key = session_target_key(target);
    let sync_key = session_sync_key(target);
    let seeded = session_baseline_seeds()
        .lock()
        .unwrap()
        .remove(&session_seed_key(config_path, &key));
    let baseline = seeded.unwrap_or_else(|| SessionBaseline {
        mtime,
        content_fingerprint: target_content_fingerprint(target),
        sync_fingerprint: sync_fingerprint_for_target(config, target),
    });
    seen.insert(key.clone(), baseline.mtime);
    if let Some(fingerprint) = baseline.content_fingerprint {
        content_seen.insert(key.clone(), fingerprint);
    }
    let sync_hash = baseline
        .sync_fingerprint
        .as_ref()
        .map(|hash| hash_prefix(hash));
    if let Some(fingerprint) = baseline.sync_fingerprint {
        sync_seen.insert(sync_key.clone(), fingerprint);
    }
    app_log(
        event,
        &[
            ("target_key", key),
            ("sync_key", sync_key),
            ("scope", target.scope.to_string()),
            ("name", target.name.clone()),
            ("peer", target.peer.clone()),
            ("tool", target.tool.to_string()),
            ("path", target.path.display().to_string()),
            ("hash", sync_hash.unwrap_or_default()),
        ],
    );
}

fn seed_session_baselines_for_workspace(
    config_path: &Path,
    config: &SyncConfig,
    workspace_name: &str,
    peer_name: &str,
) {
    for target in session_mtime_targets(config) {
        if target.scope != "workspace" || target.name != workspace_name || target.peer != peer_name
        {
            continue;
        }
        let scan_limit = if target.tool == "codex" { 32 } else { 256 };
        let Some(mtime) = latest_mtime_limited(&target.path, scan_limit) else {
            continue;
        };
        let key = session_target_key(&target);
        let sync_fingerprint = sync_fingerprint_for_target(config, &target);
        let sync_hash = sync_fingerprint.as_ref().map(|hash| hash_prefix(hash));
        session_baseline_seeds().lock().unwrap().insert(
            session_seed_key(config_path, &key),
            SessionBaseline {
                mtime,
                content_fingerprint: target_content_fingerprint(&target),
                sync_fingerprint,
            },
        );
        app_log(
            "baseline_updated",
            &[
                ("target_key", key),
                ("scope", target.scope.to_string()),
                ("name", target.name.clone()),
                ("peer", target.peer.clone()),
                ("tool", target.tool.to_string()),
                ("trigger", "initial_sync".to_string()),
                ("hash", sync_hash.unwrap_or_default()),
            ],
        );
    }
}

fn run_pending_workspace_first_propagations(config_path: &Path, config: &SyncConfig) {
    for workspace in &config.workspaces {
        if !workspace.enabled {
            continue;
        }
        let Some(peer_name) = workspace.effective_peer().map(str::to_string) else {
            continue;
        };
        if !workspace_first_propagation_pending(&workspace.name, &peer_name) {
            continue;
        }
        let Some(gate_key) =
            begin_auto_sync_bypass_cooldown("workspace", &workspace.name, &peer_name, "new_child")
        else {
            continue;
        };
        clear_workspace_first_propagation(&workspace.name, &peer_name);
        app_log(
            "workspace_first_propagation_started",
            &[
                ("workspace", workspace.name.clone()),
                ("peer", peer_name.clone()),
                ("trigger", "new_child".to_string()),
            ],
        );
        match run_workspace_auto_sync_outcome(config_path, config, workspace, None) {
            Ok(outcome) => {
                let files = (outcome.report.code_files_transferred
                    + outcome.report.session_files_transferred) as u32;
                record_auto_sync_history(
                    config_path,
                    &workspace.name,
                    true,
                    files,
                    None,
                    Some(&workspace.name),
                    None,
                    "mixed",
                );
                record_auto_workspace_child_history(
                    config_path,
                    &outcome.workspace,
                    true,
                    None,
                    "mixed",
                    Some(&outcome.child_file_counts),
                );
                let post_config = load_config(config_path).unwrap_or_else(|_| config.clone());
                seed_session_baselines_for_workspace(
                    config_path,
                    &post_config,
                    &workspace.name,
                    &peer_name,
                );
                app_log(
                    "workspace_first_propagation_complete",
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
                    config_path,
                    &workspace.name,
                    false,
                    0,
                    Some(detail.clone()),
                    Some(&workspace.name),
                    None,
                    "mixed",
                );
                record_auto_workspace_child_history(
                    config_path,
                    workspace,
                    false,
                    Some(&detail),
                    "mixed",
                    None,
                );
                app_log(
                    "workspace_first_propagation_failed",
                    &[
                        ("workspace", workspace.name.clone()),
                        ("peer", peer_name.clone()),
                        ("error", detail),
                    ],
                );
            }
        }
        finish_auto_sync(&gate_key);
    }
}

fn start_session_mtime_scanner(config_path: PathBuf, fallback_config: SyncConfig) {
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

fn refresh_interval_secs(config: &SyncConfig) -> u64 {
    match config.refresh_interval_secs {
        0 => aisync_sync::default_refresh_interval_secs(),
        secs => secs,
    }
}

fn session_mtime_targets(config: &SyncConfig) -> Vec<SessionMtimeTarget> {
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

fn claude_mtime_paths(config: &SyncConfig, code_roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(projects_root) = local_claude_projects_root(config) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for root in code_roots {
        let encoded = projects_root.join(claude_project_dir_name(root));
        if encoded.exists() {
            paths.push(encoded);
        }
    }
    existing_unique_paths(paths)
}

fn dedupe_mtime_targets(targets: Vec<SessionMtimeTarget>) -> Vec<SessionMtimeTarget> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for target in targets {
        let key = format!(
            "{}:{}:{}:{}:{}",
            target.scope,
            target.name,
            target.peer,
            target.tool,
            target.path.display()
        );
        if target.path.exists() && seen.insert(key) {
            unique.push(target);
        }
    }
    unique
}

fn latest_mtime_limited(root: &Path, limit: usize) -> Option<SystemTime> {
    let mut latest = fs::metadata(root).and_then(|meta| meta.modified()).ok()?;
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(path) = stack.pop() {
        if visited >= limit {
            break;
        }
        visited += 1;
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if let Ok(modified) = meta.modified() {
                if modified > latest {
                    latest = modified;
                }
            }
            if meta.is_dir() {
                stack.push(entry_path);
            }
        }
    }
    Some(latest)
}

fn refresh_workspace_children(
    workspace: &WorkspaceConfig,
    remote_root: &Path,
) -> Result<WorkspaceConfig> {
    let mut refreshed = workspace.clone();
    let mut existing: HashMap<String, WorkspaceChildConfig> = refreshed
        .children
        .drain(..)
        .map(|child| (child.name.clone(), child))
        .collect();
    let mut children = Vec::new();

    for entry in fs::read_dir(refreshed.effective_local_root())? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let mut child = existing
            .remove(&name)
            .unwrap_or_else(|| WorkspaceChildConfig {
                name: name.clone(),
                local_dir: entry.path(),
                remote_dir: remote_root.join(&name),
                enabled: refreshed.auto_enable_new,
                conflicted: false,
                last_fingerprint: None,
            });
        child.local_dir = entry.path();
        child.remote_dir = remote_root.join(&name);
        children.push(child);
    }

    children.sort_by(|left, right| left.name.cmp(&right.name));
    refreshed.children = children;
    Ok(refreshed)
}

#[derive(Debug, Clone)]
struct WorkspaceConflictAnalysis {
    workspace: WorkspaceConfig,
    safe_children: Vec<WorkspaceChildConfig>,
    conflicted_children: Vec<String>,
}

fn analyze_workspace_conflicts(
    workspace: &WorkspaceConfig,
    source_manifest: &SyncManifest,
    remote_manifest: &SyncManifest,
) -> WorkspaceConflictAnalysis {
    let mut analyzed = workspace.clone();
    let mut safe_children = Vec::new();
    let mut conflicted_children = Vec::new();

    for child in &mut analyzed.children {
        if !child.enabled {
            continue;
        }
        let local = child_manifest(source_manifest, &child.name);
        let remote = child_manifest(remote_manifest, &child.name);
        let local_fingerprint = manifest_fingerprint(&local);
        let remote_fingerprint = manifest_fingerprint(&remote);
        let split_brain = child
            .last_fingerprint
            .as_ref()
            .map(|last| {
                local_fingerprint != *last
                    && remote_fingerprint != *last
                    && local_fingerprint != remote_fingerprint
            })
            .unwrap_or(false);

        if split_brain || (child.conflicted && local_fingerprint != remote_fingerprint) {
            child.conflicted = true;
            conflicted_children.push(child.name.clone());
            continue;
        }

        child.conflicted = false;
        child.last_fingerprint = Some(local_fingerprint);
        safe_children.push(child.clone());
    }

    WorkspaceConflictAnalysis {
        workspace: analyzed,
        safe_children,
        conflicted_children,
    }
}

fn child_manifest(manifest: &SyncManifest, child_name: &str) -> SyncManifest {
    let prefix = format!("{child_name}/");
    let mut files = Vec::new();
    for entry in &manifest.files {
        let Some(relative_path) = entry.relative_path.strip_prefix(&prefix) else {
            continue;
        };
        let mut child_entry = entry.clone();
        child_entry.relative_path = relative_path.to_string();
        files.push(child_entry);
    }
    SyncManifest { files }
}

fn manifest_fingerprint(manifest: &SyncManifest) -> String {
    let mut hasher = blake3::Hasher::new();
    for file in &manifest.files {
        hasher.update(file.relative_path.as_bytes());
        hasher.update(file.blake3_hash.as_bytes());
        hasher.update(&file.size.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn project_auto_sync_fingerprint(
    config: &SyncConfig,
    project_name: &str,
    peer_name: &str,
) -> Option<String> {
    let project = config.projects.iter().find(|project| {
        project.name == project_name && project.peers.contains_key(peer_name) && project.enabled
    })?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"project");
    hasher.update(project.name.as_bytes());
    hash_tree_contents(&mut hasher, "code", &project.local, 8192);
    for path in claude_mtime_paths(config, std::slice::from_ref(&project.local)) {
        hash_tree_contents(&mut hasher, "claude", &path, 2048);
    }
    Some(hasher.finalize().to_hex().to_string())
}

fn workspace_local_session_roots(workspace: &WorkspaceConfig) -> Vec<PathBuf> {
    let root = workspace.effective_local_root();
    let mut roots = vec![root.to_path_buf()];
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                roots.push(entry.path());
            }
        }
    }
    roots.extend(
        workspace
            .children
            .iter()
            .map(|child| child.local_dir.clone()),
    );
    roots.sort();
    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn workspace_auto_sync_fingerprint(
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
) -> Option<String> {
    if !workspace.enabled {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"workspace");
    hasher.update(workspace.name.as_bytes());
    hash_tree_contents(&mut hasher, "code", workspace.effective_local_root(), 16384);
    let roots = workspace_local_session_roots(workspace);
    for path in claude_mtime_paths(config, &roots) {
        hash_tree_contents(&mut hasher, "claude", &path, 4096);
    }
    Some(hasher.finalize().to_hex().to_string())
}

fn sync_fingerprint_for_target(config: &SyncConfig, target: &SessionMtimeTarget) -> Option<String> {
    match target.scope {
        "project" => project_sync_fingerprint_for_target(config, target),
        "workspace" => workspace_sync_fingerprint_for_target(config, target),
        _ => None,
    }
}

fn project_sync_fingerprint_for_target(
    config: &SyncConfig,
    target: &SessionMtimeTarget,
) -> Option<String> {
    let project = config.projects.iter().find(|project| {
        project.name == target.name && project.peers.contains_key(&target.peer) && project.enabled
    })?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"project-sync");
    hasher.update(project.name.as_bytes());
    hasher.update(target.peer.as_bytes());
    hash_tree_contents(&mut hasher, "code", &project.local, 8192);
    for path in claude_mtime_paths(config, std::slice::from_ref(&project.local)) {
        hash_tree_contents(&mut hasher, "claude", &path, 2048);
    }
    if target.tool == "codex" {
        hash_codex_sessions_matching(&mut hasher, "codex", |file| {
            codex_session_file_matches_project(file, &project.local)
        });
    }
    Some(hasher.finalize().to_hex().to_string())
}

fn workspace_sync_fingerprint_for_target(
    config: &SyncConfig,
    target: &SessionMtimeTarget,
) -> Option<String> {
    let workspace = config
        .workspaces
        .iter()
        .find(|workspace| workspace.name == target.name && workspace.enabled)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"workspace-sync");
    hasher.update(workspace.name.as_bytes());
    hasher.update(target.peer.as_bytes());
    hash_tree_contents(&mut hasher, "code", workspace.effective_local_root(), 16384);
    let roots = workspace_local_session_roots(workspace);
    for path in claude_mtime_paths(config, &roots) {
        hash_tree_contents(&mut hasher, "claude", &path, 4096);
    }
    if target.tool == "codex" {
        let excluded = HashSet::<String>::new();
        hash_codex_sessions_matching(&mut hasher, "codex", |file| {
            codex_session_file_matches_workspace(file, workspace.effective_local_root(), &excluded)
        });
    }
    Some(hasher.finalize().to_hex().to_string())
}

fn target_content_fingerprint(target: &SessionMtimeTarget) -> Option<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(target.scope.as_bytes());
    hasher.update(target.name.as_bytes());
    hasher.update(target.peer.as_bytes());
    hasher.update(target.tool.as_bytes());
    hash_tree_contents(&mut hasher, target.tool, &target.path, 4096);
    Some(hasher.finalize().to_hex().to_string())
}

fn hash_codex_sessions_matching(
    hasher: &mut blake3::Hasher,
    label: &str,
    mut matches: impl FnMut(&Path) -> bool,
) {
    let Some(root) = local_codex_sessions_dir() else {
        hasher.update(label.as_bytes());
        hasher.update(b":missing");
        return;
    };
    let mut files = Vec::new();
    if collect_jsonl_files(&root, &mut files).is_err() {
        hasher.update(label.as_bytes());
        hasher.update(b":scan-error");
        return;
    }
    files.retain(|file| matches(file));
    files.sort();
    for file in files {
        let relative = file.strip_prefix(&root).unwrap_or(&file);
        hasher.update(label.as_bytes());
        hasher.update(relative.to_string_lossy().as_bytes());
        hash_file_contents(hasher, &file);
    }
}

fn hash_tree_contents(hasher: &mut blake3::Hasher, label: &str, root: &Path, max_entries: usize) {
    if !root.exists() {
        hasher.update(label.as_bytes());
        hasher.update(b":missing");
        return;
    }
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(path) = stack.pop() {
        if visited >= max_entries {
            hasher.update(b":truncated");
            break;
        }
        visited += 1;
        if should_skip_hash_path(&path) {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        hasher.update(label.as_bytes());
        hasher.update(relative.to_string_lossy().as_bytes());
        let Ok(metadata) = fs::metadata(&path) else {
            hasher.update(b":metadata-error");
            continue;
        };
        if metadata.is_dir() {
            hasher.update(b":dir");
            let Ok(entries) = fs::read_dir(&path) else {
                continue;
            };
            let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
            children.sort();
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        } else if metadata.is_file() {
            hasher.update(b":file");
            hasher.update(&metadata.len().to_le_bytes());
            hash_file_contents(hasher, &path);
        }
    }
}

fn hash_file_contents(hasher: &mut blake3::Hasher, path: &Path) {
    if let Ok(mut file) = fs::File::open(path) {
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buffer[..n]),
                Err(_) => {
                    hasher.update(b":read-error");
                    break;
                }
            };
        }
    } else {
        hasher.update(b":open-error");
    }
}

fn should_skip_hash_path(path: &Path) -> bool {
    let mut previous_was_team = false;
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();
        if previous_was_team && name == "runtime" {
            return true;
        }
        previous_was_team = name == ".team";
    }
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            ".git" | "node_modules" | "target" | ".aisync" | ".DS_Store"
        )
    })
}

fn run_project_auto_sync(
    config_path: &Path,
    config: &SyncConfig,
    project_name: &str,
    peer_name: &str,
    live_connection: Option<PeerConnectionInfo>,
) -> Result<SyncReport> {
    let project = config.project_mapping(project_name, peer_name)?;
    // 自动同步绝不静默覆盖：confirm_overwrite=false，由 50% 安全阀 + 回收站兜底。
    run_tcp_push(config_path, config, peer_name, &project, live_connection, false)
}

fn run_workspace_auto_sync_outcome(
    config_path: &Path,
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
    live_connection: Option<PeerConnectionInfo>,
) -> Result<WorkspaceSyncOutcome> {
    let outcome = run_workspace_tcp_push(config_path, config, workspace, live_connection)?;
    let mut updated = config.clone();
    replace_workspace(&mut updated, outcome.workspace.clone());
    save_config(config_path, &updated)?;
    app_log(
        "workspace_children_persisted",
        &[
            ("workspace", outcome.report.project_id.clone()),
            ("config", config_path.display().to_string()),
        ],
    );
    Ok(outcome)
}

fn run_tcp_push(
    config_path: &Path,
    config: &SyncConfig,
    peer_name: &str,
    project: &aisync_core::ProjectMapping,
    live_connection: Option<PeerConnectionInfo>,
    confirm_overwrite: bool,
) -> Result<SyncReport> {
    let connection = peer_transport_connection(config_path, config, peer_name, live_connection)?;
    app_log(
        "transport_peer_connection_selected",
        &[
            ("peer", peer_name.to_string()),
            ("endpoint", connection.endpoint.to_string()),
            ("cert_source", connection.cert_source.clone()),
        ],
    );
    let source = project.local_code_dir.clone();
    let remote_code_dir = project.remote_code_dir.clone();
    let mut session_plans = Vec::new();
    if let Some(plan) = prepare_claude_session_sync(config_path, config, peer_name, project)? {
        session_plans.push(("claude", plan));
    }
    if let Some(plan) = prepare_codex_session_sync(config_path, peer_name, project)? {
        session_plans.push(("codex", plan));
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
    let synced_hash = aisync_transport::manifest_hash(&code_manifest);
    if let Ok(mut persisted) = load_config(config_path) {
        persisted.set_sync_snapshot(
            &project.project_id,
            peer_name,
            aisync_sync::SyncSnapshot {
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
            aisync_sync::SyncStage {
                name: "connect",
                percent: 5,
                current_file: None,
            },
            aisync_sync::SyncStage {
                name: "transfer_session",
                percent: 90,
                current_file: None,
            },
            aisync_sync::SyncStage {
                name: "sync_complete",
                percent: 100,
                current_file: None,
            },
        ],
    })
}

struct WorkspaceSyncOutcome {
    report: SyncReport,
    workspace: WorkspaceConfig,
    child_file_counts: HashMap<String, u32>,
}

fn increment_child_file_count(counts: &mut HashMap<String, u32>, child_name: &str, files: usize) {
    if files == 0 {
        return;
    }
    let entry = counts.entry(child_name.to_string()).or_insert(0);
    *entry = entry.saturating_add(files as u32);
}

fn run_workspace_tcp_push(
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
    let analysis_slot: Arc<Mutex<Option<WorkspaceConflictAnalysis>>> = Arc::new(Mutex::new(None));
    let preflight_workspace = workspace.clone();
    let preflight_slot = Arc::clone(&analysis_slot);
    let code_result = runtime.block_on(async {
        let identity = generate_tls_identity("aisync-client")?;
        let tls = TlsConfig::new(identity, connection.server_name.clone())
            .with_pinned_peer_cert(connection.receiver_cert_der.clone());
        let mut transporter =
            TcpTransporter::connect_to_peer(&connection.peer, connection.endpoint.port(), &tls)
                .await?;
        let result = transporter
            .sync_directory_to_checked(&source, Some(&remote_root), None, |source, remote| {
                let analysis = analyze_workspace_conflicts(&preflight_workspace, source, remote);
                let has_conflicts = !analysis.conflicted_children.is_empty();
                *preflight_slot.lock().unwrap() = Some(analysis.clone());
                if has_conflicts {
                    return Err(AisyncError::ConflictDetected(
                        aisync_core::ConflictDetails {
                            project_id: preflight_workspace.name.clone(),
                            local_version: 0,
                            remote_version: 0,
                            summary: "workspace child changed on both devices".to_string(),
                        },
                    ));
                }
                Ok(())
            })
            .await;
        transporter.shutdown().await;
        result
    });

    let (code_files, workspace, conflicted_children, mut child_file_counts) = match code_result {
        Ok(exchange) => {
            let analysis = analysis_slot.lock().unwrap().take().unwrap_or_else(|| {
                analyze_workspace_conflicts(
                    &workspace,
                    &exchange.source_manifest,
                    &exchange.remote_manifest,
                )
            });
            let mut child_file_counts = HashMap::new();
            for child in &analysis.workspace.children {
                if !child.enabled || child.conflicted {
                    continue;
                }
                let local = child_manifest(&exchange.source_manifest, &child.name);
                let local_fingerprint = manifest_fingerprint(&local);
                if previous_child_fingerprints.get(&child.name) != Some(&local_fingerprint) {
                    increment_child_file_count(
                        &mut child_file_counts,
                        &child.name,
                        local.files.len(),
                    );
                }
            }
            (
                exchange.source_manifest.files.len(),
                analysis.workspace,
                Vec::new(),
                child_file_counts,
            )
        }
        Err(error) => {
            let Some(analysis) = analysis_slot.lock().unwrap().take() else {
                return Err(error);
            };
            if analysis.conflicted_children.is_empty() {
                return Err(error);
            }
            for child in &analysis.conflicted_children {
                app_log(
                    "workspace_child_conflict_detected",
                    &[
                        ("workspace", workspace.name.clone()),
                        ("child", child.clone()),
                        ("peer", peer_name.to_string()),
                    ],
                );
            }
            let (code_files, child_file_counts) = runtime.block_on(async {
                let mut transferred = 0usize;
                let mut child_file_counts = HashMap::new();
                for child in &analysis.safe_children {
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
                    transferred += manifest.files.len();
                    increment_child_file_count(
                        &mut child_file_counts,
                        &child.name,
                        manifest.files.len(),
                    );
                }
                Ok::<_, AisyncError>((transferred, child_file_counts))
            })?;
            (
                code_files,
                analysis.workspace,
                analysis.conflicted_children,
                child_file_counts,
            )
        }
    };

    let conflicted: HashSet<String> = conflicted_children.iter().cloned().collect();
    let empty_children: Vec<_> = workspace
        .children
        .iter()
        .filter(|child| child.enabled && !conflicted.contains(&child.name))
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
    let mut session_plans = Vec::new();
    if let Some(plan) = prepare_claude_workspace_session_sync(
        config_path,
        config,
        peer_name,
        &project,
        &conflicted,
    )? {
        session_plans.push(plan);
    }
    if let Some(plan) =
        prepare_codex_workspace_session_sync(config_path, peer_name, &project, &conflicted)?
    {
        session_plans.push(plan);
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
            ("conflicted_children", conflicted_children.len().to_string()),
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
                aisync_sync::SyncStage {
                    name: "connect",
                    percent: 5,
                    current_file: None,
                },
                aisync_sync::SyncStage {
                    name: "transfer_workspace",
                    percent: 70,
                    current_file: None,
                },
                aisync_sync::SyncStage {
                    name: "transfer_session",
                    percent: 90,
                    current_file: None,
                },
                aisync_sync::SyncStage {
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

struct SessionSyncPlan {
    staging_root: PathBuf,
    staged_project_dir: PathBuf,
    remote_project_dir: PathBuf,
    bytes: u64,
    rewritten_sessions: usize,
}

struct WorkspaceSessionSyncPlan {
    tool: &'static str,
    staging_root: PathBuf,
    remote_projects_dir: PathBuf,
    transfers: Vec<WorkspaceSessionTransfer>,
    child_file_counts: HashMap<String, u32>,
    bytes: u64,
    rewritten_sessions: usize,
}

struct WorkspaceSessionTransfer {
    staged_dir: PathBuf,
    remote_dir: PathBuf,
}

fn workspace_project_mapping(
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
    peer_name: &str,
    remote_root: &Path,
) -> Result<aisync_core::ProjectMapping> {
    Ok(aisync_core::ProjectMapping {
        project_id: workspace.name.clone(),
        local_code_dir: workspace.effective_local_root().to_path_buf(),
        local_session_dir: if config.claude_config.local.as_os_str().is_empty() {
            home_dir()
                .map(|home| home.join(".claude"))
                .unwrap_or_else(|| workspace.effective_local_root().join(".claude"))
        } else {
            config.claude_config.local.clone()
        },
        remote_code_dir: remote_root.to_path_buf(),
        remote_session_dir: config
            .claude_config
            .peers
            .get(peer_name)
            .cloned()
            .unwrap_or_else(|| PathBuf::from("~/.claude")),
        original_source_path: workspace
            .effective_local_root()
            .to_string_lossy()
            .into_owned(),
        enabled: workspace.enabled,
    })
}

fn prepare_claude_session_sync(
    config_path: &Path,
    config: &SyncConfig,
    peer_name: &str,
    project: &aisync_core::ProjectMapping,
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

fn prepare_codex_session_sync(
    config_path: &Path,
    peer_name: &str,
    project: &aisync_core::ProjectMapping,
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

fn prepare_claude_workspace_session_sync(
    config_path: &Path,
    config: &SyncConfig,
    peer_name: &str,
    project: &aisync_core::ProjectMapping,
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

fn prepare_codex_workspace_session_sync(
    config_path: &Path,
    peer_name: &str,
    project: &aisync_core::ProjectMapping,
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

fn local_claude_projects_dir(configured: &Path) -> Option<PathBuf> {
    if configured.file_name().and_then(|name| name.to_str()) == Some("projects")
        && configured.is_dir()
    {
        return Some(configured.to_path_buf());
    }
    let configured_projects = configured.join("projects");
    if configured_projects.is_dir() {
        return Some(configured_projects);
    }
    let home_projects = home_dir()?.join(".claude").join("projects");
    home_projects.is_dir().then_some(home_projects)
}

fn local_codex_sessions_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AISYNC_CODEX_SESSIONS_DIR").map(PathBuf::from) {
        return path.is_dir().then_some(path);
    }
    let sessions = home_dir()?.join(".codex").join("sessions");
    sessions.is_dir().then_some(sessions)
}

fn collect_jsonl_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_jsonl_files(&path, out)?;
        } else if file_type.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn codex_session_file_matches_project(file: &Path, local_code_dir: &Path) -> bool {
    let project_path = local_code_dir.to_string_lossy().into_owned();
    file_contains(file, |line| line.contains(&project_path))
}

fn codex_session_file_matches_workspace(
    file: &Path,
    local_root: &Path,
    excluded_children: &HashSet<String>,
) -> bool {
    let root = local_root.to_string_lossy();
    let excluded_paths: Vec<String> = excluded_children
        .iter()
        .map(|child| local_root.join(child).to_string_lossy().into_owned())
        .collect();
    file_contains(file, |line| {
        if !line.contains(root.as_ref()) {
            return false;
        }
        !excluded_paths
            .iter()
            .any(|child_path| line.contains(child_path))
    })
}

fn codex_session_child_name(
    file: &Path,
    local_root: &Path,
    excluded_children: &HashSet<String>,
) -> Option<String> {
    let child_names = first_level_dir_names(local_root).ok()?;
    let mut candidates: Vec<String> = child_names
        .into_iter()
        .filter(|name| !excluded_children.contains(name))
        .collect();
    candidates.sort();
    for child_name in candidates {
        let child_path = local_root.join(&child_name).to_string_lossy().into_owned();
        if file_contains(file, |line| line.contains(&child_path)) {
            return Some(child_name);
        }
    }
    None
}

fn file_contains(file: &Path, mut predicate: impl FnMut(&str) -> bool) -> bool {
    let Ok(file) = fs::File::open(file) else {
        return false;
    };
    for line in BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        if predicate(&line) {
            return true;
        }
    }
    false
}

fn count_files_recursive(root: &Path) -> usize {
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

fn remote_claude_projects_dir(
    config: &SyncConfig,
    peer_name: &str,
    project: &aisync_core::ProjectMapping,
) -> PathBuf {
    let root = config
        .claude_config
        .peers
        .get(peer_name)
        .cloned()
        .unwrap_or_else(|| project.remote_session_dir.clone());
    if config.claude_config.peers.contains_key(peer_name) {
        return claude_projects_dir(root);
    }
    PathBuf::from("~/.claude/projects")
}

fn claude_projects_dir(root: PathBuf) -> PathBuf {
    if root.file_name().and_then(|name| name.to_str()) == Some("projects") {
        root
    } else {
        root.join("projects")
    }
}

fn same_project_path(
    session_path: &str,
    local_code_dir: &Path,
    original_source_path: &str,
) -> bool {
    if session_path == local_code_dir.to_string_lossy() || session_path == original_source_path {
        return true;
    }
    let session = Path::new(session_path);
    match (fs::canonicalize(session), fs::canonicalize(local_code_dir)) {
        (Ok(session), Ok(local)) => session == local,
        _ => false,
    }
}

fn session_path_under(session_path: &str, local_root: &Path) -> bool {
    let session = Path::new(session_path);
    if session.starts_with(local_root) {
        return true;
    }
    match (fs::canonicalize(session), fs::canonicalize(local_root)) {
        (Ok(session), Ok(root)) => session.starts_with(root),
        _ => false,
    }
}

fn session_child_name(session_path: &str, local_root: &Path) -> Option<String> {
    let session = Path::new(session_path);
    if let Ok(relative) = session.strip_prefix(local_root) {
        return relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .map(str::to_string);
    }
    let session = fs::canonicalize(session).ok()?;
    let root = fs::canonicalize(local_root).ok()?;
    let relative = session.strip_prefix(root).ok()?;
    relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .map(str::to_string)
}

fn path_rule_for_project(project: &aisync_core::ProjectMapping) -> PathRule {
    let source = project.local_code_dir.to_string_lossy().into_owned();
    let target = project.remote_code_dir.to_string_lossy().into_owned();
    if target.contains('\\') {
        PathRule::unix_to_windows(source, target)
    } else {
        PathRule::unix_to_unix(source, target)
    }
}

fn project_rewriter(
    config: &SyncConfig,
    peer_name: &str,
    project: &aisync_core::ProjectMapping,
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

fn same_mapping_path(left: &Path, right: &Path) -> bool {
    if left == right || left.to_string_lossy() == right.to_string_lossy() {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn claude_project_dir_name(path: &Path) -> String {
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

fn unix_nanos_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Temporarily add exact-path excludes to a project. Returns the previous
/// exclude_rules so they can be restored.
fn inject_excludes(
    config: &mut SyncConfig,
    project_name: &str,
    extra: &[String],
) -> Option<Vec<String>> {
    let project = config
        .projects
        .iter_mut()
        .find(|p| p.name == project_name)?;
    let saved = project.exclude_rules.clone();
    project.exclude_rules.extend(extra.iter().cloned());
    Some(saved)
}

fn restore_excludes(config: &mut SyncConfig, project_name: &str, saved: Option<Vec<String>>) {
    if let (Some(saved), Some(project)) = (
        saved,
        config.projects.iter_mut().find(|p| p.name == project_name),
    ) {
        project.exclude_rules = saved;
    }
}

/// Resolve a human-friendly device name from the host on first run.
///
/// Order: explicit override env → platform hostname → generic fallback. On
/// macOS we prefer the friendly `ComputerName` ("Alice's MacBook Pro") over the
/// DNS-style `hostname` ("alices-macbook-pro.local"). Never fabricates a name
/// like "CodeBaton Device" unless every real source fails.
fn default_device_name() -> String {
    if let Ok(name) = std::env::var("AISYNC_DEVICE_NAME") {
        if !name.trim().is_empty() {
            return name;
        }
    }
    if let Some(name) = system_hostname() {
        return name;
    }
    PLACEHOLDER_DEVICE_NAME.to_string()
}

const PLACEHOLDER_DEVICE_NAME: &str = "CodeBaton Device";

/// A device name that should be re-derived from the host: empty, whitespace, or
/// the legacy placeholder a sandboxed older build wrote.
fn is_placeholder_device_name(name: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n == PLACEHOLDER_DEVICE_NAME || n == "aisync-device"
}

/// Best-effort real hostname.
///
/// Uses the `gethostname(2)` syscall directly on Unix (no subprocess — a
/// sandboxed/hardened-runtime app cannot spawn `scutil`/`hostname`, which is why
/// the earlier subprocess approach silently fell back to the placeholder in the
/// release build). On Windows it reads `%COMPUTERNAME%`.
fn system_hostname() -> Option<String> {
    #[cfg(windows)]
    {
        let name = std::env::var("COMPUTERNAME").ok()?;
        let name = name.trim().to_string();
        return if name.is_empty() { None } else { Some(name) };
    }

    #[cfg(unix)]
    {
        // gethostname into a fixed buffer; truncate at the NUL terminator.
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc != 0 {
            return None;
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let raw = String::from_utf8_lossy(&buf[..len]).to_string();
        // Strip the trailing `.local` / DNS domain so the UI shows a short name
        // (e.g. "MacBook-Air" instead of "macbook-air.local").
        let short = raw.split('.').next().unwrap_or(&raw).trim().to_string();
        if short.is_empty() {
            None
        } else {
            Some(short)
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

fn persist_peer_connection(
    config: &mut SyncConfig,
    config_path: &Path,
    peer: DeviceInfo,
    endpoint: Option<SocketAddr>,
    receiver_cert_der: Option<&[u8]>,
    server_name: Option<String>,
) -> Result<()> {
    let server_cert = if let Some(cert) = receiver_cert_der {
        let path = peer_receiver_cert_path(config_path, &peer.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, cert)?;
        Some(path)
    } else {
        None
    };

    let entry = config
        .peers
        .entry(peer.name.clone())
        .or_insert_with(|| PeerConfig {
            id: peer.id,
            name: peer.name.clone(),
            endpoint: None,
            server_cert: None,
            server_name: None,
            last_seen: None,
        });
    entry.id = peer.id;
    entry.name = peer.name;
    if let Some(endpoint) = endpoint {
        entry.endpoint = Some(endpoint);
    }
    if let Some(server_cert) = server_cert {
        entry.server_cert = Some(server_cert);
    }
    entry.server_name = Some(server_name.unwrap_or_else(|| "aisync-receiver".to_string()));
    Ok(())
}

fn peer_receiver_cert_path(config_path: &Path, peer_id: &DeviceId) -> PathBuf {
    config_path
        .with_file_name("peers")
        .join(format!("{}-receiver.der", peer_id.0))
}

fn endpoint_online(endpoint: SocketAddr) -> bool {
    TcpStream::connect_timeout(&endpoint, Duration::from_millis(250)).is_ok()
}

fn receiver_cert_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("receiver.der")
}

fn receiver_key_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("receiver.key.der")
}

fn load_or_create_receiver_identity(config_path: &Path) -> Result<TlsIdentity> {
    let cert_path = receiver_cert_path(config_path);
    let key_path = receiver_key_path(config_path);
    if let (Ok(cert_der), Ok(private_key_der)) = (fs::read(&cert_path), fs::read(&key_path)) {
        return Ok(TlsIdentity {
            cert_der,
            private_key_der,
        });
    }

    let identity = generate_tls_identity("aisync-receiver")?;
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&cert_path, &identity.cert_der)?;
    fs::write(&key_path, &identity.private_key_der)?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener};
    use uuid::Uuid;

    #[test]
    fn persist_peer_connection_writes_endpoint_and_cert() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = SyncConfig::new("local");
        let peer_id = DeviceId(Uuid::new_v4());
        let peer = DeviceInfo {
            id: peer_id,
            name: "peer".to_string(),
            os: OsType::Darwin,
            addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            protocol_version: 1,
        };
        let endpoint = SocketAddr::from(([127, 0, 0, 1], 52000));
        let cert = b"receiver-cert";

        persist_peer_connection(
            &mut config,
            &config_path,
            peer,
            Some(endpoint),
            Some(cert),
            Some("aisync-receiver".to_string()),
        )
        .unwrap();

        let stored = config.peers.get("peer").unwrap();
        assert_eq!(stored.id, peer_id);
        assert_eq!(stored.endpoint, Some(endpoint));
        assert_eq!(stored.server_name.as_deref(), Some("aisync-receiver"));
        let cert_path = stored.server_cert.as_ref().unwrap();
        assert_eq!(fs::read(cert_path).unwrap(), cert);
        assert!(cert_path.starts_with(tmp.path().join("peers")));
    }

    #[test]
    fn system_hostname_resolves_in_process() {
        // BUG-007: must work without spawning a subprocess and must not be the
        // placeholder. On any real CI/dev/release host gethostname succeeds.
        let name = system_hostname().expect("gethostname should succeed");
        assert!(!name.is_empty());
        assert_ne!(name, PLACEHOLDER_DEVICE_NAME);
        // default_device_name() must then surface that real name.
        std::env::remove_var("AISYNC_DEVICE_NAME");
        assert_eq!(default_device_name(), name);
    }

    #[test]
    fn placeholder_names_are_detected_for_healing() {
        assert!(is_placeholder_device_name("CodeBaton Device"));
        assert!(is_placeholder_device_name("aisync-device"));
        assert!(is_placeholder_device_name(""));
        assert!(is_placeholder_device_name("   "));
        assert!(!is_placeholder_device_name("MacBook-Air"));
        assert!(!is_placeholder_device_name("Mac"));
    }

    #[test]
    fn receiver_identity_is_reused_between_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let first = load_or_create_receiver_identity(&config_path).unwrap();
        let second = load_or_create_receiver_identity(&config_path).unwrap();

        assert_eq!(first.cert_der, second.cert_der);
        assert_eq!(first.private_key_der, second.private_key_der);
    }

    #[test]
    fn default_file_receive_dir_is_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        let backend = Backend::with_config(config, config_path.clone()).unwrap();
        let receive_dir = tmp.path().join("incoming");

        backend
            .set_default_file_receive_dir(receive_dir.clone())
            .unwrap();

        assert_eq!(backend.default_file_receive_dir(), receive_dir);
        assert_eq!(
            load_config(&config_path).unwrap().default_file_receive_dir,
            Some(receive_dir)
        );
    }

    #[test]
    fn default_file_receive_dir_uses_downloads_codebaton_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let config = SyncConfig::new("local");
        let receive_dir = default_file_receive_dir(&config_path, &config);

        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                receive_dir,
                PathBuf::from(home).join("Downloads").join("CodeBaton")
            );
        } else {
            assert_eq!(receive_dir, config_path.with_file_name("files"));
        }
    }

    #[test]
    fn text_message_history_normalizes_seconds_to_epoch_millis() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let message = TextMessagePayload {
            sender_name: "peer".to_string(),
            content: "hello".to_string(),
            timestamp: 1_781_900_000,
        };

        record_text_message_history(&config_path, Some("peer"), &message, false);

        let rows = read_jsonl(&tmp.path().join("chat_history.jsonl"));
        assert_eq!(
            rows[0].get("timestamp").and_then(|v| v.as_u64()),
            Some(1_781_900_000_000)
        );
    }

    #[test]
    fn default_file_transfer_accept_targets_default_receive_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let receive_dir = tmp.path().join("downloads");
        let mut config = SyncConfig::new("receiver");
        config.default_file_receive_dir = Some(receive_dir.clone());
        config.state_path = Some(tmp.path().join("state.toml"));
        save_config(&config_path, &config).unwrap();
        let request = FileTransferRequestPayload {
            transfer_id: "transfer-1".to_string(),
            filename: "../note.txt".to_string(),
            size: 42,
            sender_name: "sender".to_string(),
            device: DeviceInfo {
                id: DeviceId(Uuid::new_v4()),
                name: "sender".to_string(),
                os: OsType::Darwin,
                addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
                protocol_version: 1,
            },
            endpoint: Some(SocketAddr::from(([127, 0, 0, 1], 62000))),
            receiver_cert_der: Some(vec![1, 2, 3]),
            server_name: Some("aisync-receiver".to_string()),
        };

        let (_endpoint, _tls, ack, state) =
            prepare_default_file_transfer_accept(&config_path, 52000, &request).unwrap();

        assert!(ack.accepted);
        assert!(ack.ready);
        assert_eq!(ack.filename, "../note.txt");
        assert_eq!(state.filename, "../note.txt");
        assert_eq!(state.sender_name, "sender");
        assert_eq!(state.expected_size, 42);
        assert_eq!(
            state.target_path,
            receive_dir.canonicalize().unwrap().join("note.txt")
        );
        assert_eq!(ack.device.name, "receiver");
        assert_eq!(
            ack.device.addresses.first(),
            Some(&IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    #[test]
    fn file_receive_target_must_stay_under_receive_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let receive_dir = tmp.path().join("incoming");
        let accepted = ensure_file_receive_target(&receive_dir, Path::new("nested/note.txt"))
            .expect("relative target should resolve under receive dir");

        assert!(accepted.starts_with(receive_dir.canonicalize().unwrap()));
        assert_eq!(accepted.file_name().unwrap(), "note.txt");

        let outside = tmp.path().join("outside/note.txt");
        let error = ensure_file_receive_target(&receive_dir, &outside).unwrap_err();
        assert!(error.to_string().contains("escapes receive dir"));
    }

    #[test]
    fn sensitive_file_transfer_requires_explicit_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join(".env.local");
        fs::write(&env_path, b"SECRET=1").unwrap();

        let error = ensure_file_transfer_source_allowed(&env_path, &[]).unwrap_err();
        assert!(error.to_string().contains("sensitive-file:"));

        ensure_file_transfer_source_allowed(&env_path, &[env_path.to_string_lossy().into_owned()])
            .unwrap();
    }

    #[test]
    fn file_transfer_data_writes_tmp_then_renames_on_done() {
        let tmp = tempfile::tempdir().unwrap();
        let target_path = tmp.path().join("incoming/note.txt");
        let tmp_path = file_transfer_tmp_path(&target_path, "transfer-1");
        let states = Arc::new(Mutex::new(HashMap::from([(
            "transfer-1".to_string(),
            FileReceiveState {
                target_path: target_path.clone(),
                tmp_path: tmp_path.clone(),
                expected_size: 5,
                bytes_written: 0,
                filename: "note.txt".to_string(),
                sender_name: "peer".to_string(),
                history_config_path: tmp.path().join("config.toml"),
            },
        )])));

        receive_file_transfer_data(
            &states,
            FileTransferDataPayload {
                transfer_id: "transfer-1".to_string(),
                offset: 0,
                chunk: b"hello".to_vec(),
                done: true,
            },
        )
        .unwrap();

        assert_eq!(fs::read(&target_path).unwrap(), b"hello");
        assert!(!tmp_path.exists());
        assert!(states.lock().unwrap().is_empty());
    }

    #[test]
    fn cross_device_same_path_session_mapping_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let project_dir = tmp.path().join("same-path-project");
        fs::create_dir_all(&project_dir).unwrap();
        let claude_dir = tmp.path().join(".claude");
        let session_dir = claude_dir
            .join("projects")
            .join(claude_project_dir_name(&project_dir));
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("s.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user",
                    "cwd": project_dir.to_string_lossy(),
                    "sessionId": "s"
                })
            ),
        )
        .unwrap();

        let mut config = SyncConfig::new("local");
        config.device.id = DeviceId(Uuid::new_v4());
        config.claude_config.local = claude_dir;
        config.peers.insert(
            "peer".to_string(),
            PeerConfig {
                id: DeviceId(Uuid::new_v4()),
                name: "peer".to_string(),
                endpoint: None,
                server_cert: None,
                server_name: None,
                last_seen: None,
            },
        );
        let project = aisync_core::ProjectMapping {
            project_id: "same".to_string(),
            local_code_dir: project_dir.clone(),
            local_session_dir: config.claude_config.local.clone(),
            remote_code_dir: project_dir.clone(),
            remote_session_dir: PathBuf::from("~/.claude"),
            original_source_path: project_dir.to_string_lossy().into_owned(),
            enabled: true,
        };

        let plan = prepare_claude_session_sync(&config_path, &config, "peer", &project)
            .unwrap()
            .expect("same-path cross-device mapping should stage sessions");
        let staged = plan.staged_project_dir.join("s.jsonl");
        assert!(staged.exists());
        assert!(fs::read_to_string(&staged)
            .unwrap()
            .contains(&project_dir.to_string_lossy().to_string()));
        let _ = fs::remove_dir_all(plan.staging_root);
    }

    #[test]
    fn same_device_same_path_session_mapping_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("same-path-project");
        let mut config = SyncConfig::new("local");
        let local_id = config.device.id;
        config.peers.insert(
            "self".to_string(),
            PeerConfig {
                id: local_id,
                name: "self".to_string(),
                endpoint: None,
                server_cert: None,
                server_name: None,
                last_seen: None,
            },
        );
        let project = aisync_core::ProjectMapping {
            project_id: "same".to_string(),
            local_code_dir: project_dir.clone(),
            local_session_dir: tmp.path().join(".claude"),
            remote_code_dir: project_dir,
            remote_session_dir: PathBuf::from("~/.claude"),
            original_source_path: String::new(),
            enabled: true,
        };

        let error = project_rewriter(&config, "self", &project).unwrap_err();
        assert!(error.to_string().contains("circular mapping"));
    }

    #[test]
    fn complete_onboarding_persists_device_name_and_updates_local_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = SyncConfig::new("old-name");
        config.state_path = Some(tmp.path().join("state.toml"));
        let backend = Backend::with_config(config, config_path.clone()).unwrap();

        backend.complete_onboarding("MacBook Air QA").unwrap();

        let persisted = load_config(&config_path).unwrap();
        assert_eq!(persisted.device.name, "MacBook Air QA");
        assert!(persisted.onboarded);
        assert_eq!(backend.config().device.name, "MacBook Air QA");
        assert_eq!(backend.local_device().name, "MacBook Air QA");
        assert!(backend.is_onboarded());
    }

    #[test]
    fn inbound_pairing_request_reuses_code_and_keeps_peer_online_with_endpoint_ip() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        let backend = Backend::with_config(config, config_path.clone()).unwrap();
        let peer_id = DeviceId(Uuid::new_v4());
        let endpoint = SocketAddr::from(([192, 168, 31, 241], 52000));
        let request_id = aisync_discovery::new_pairing_request_id();

        backend
            .pending_pairing_requests
            .lock()
            .unwrap()
            .push_back(PairingRequestPayload {
                request_id: request_id.clone(),
                code: "123456".to_string(),
                expires_at_unix_secs: unix_secs_now() + 120,
                device: DeviceInfo {
                    id: peer_id,
                    name: "Mac-mini".to_string(),
                    os: OsType::Darwin,
                    addresses: vec![IpAddr::V6(Ipv6Addr::LOCALHOST)],
                    protocol_version: 1,
                },
                endpoint: Some(endpoint),
                receiver_cert_der: Some(vec![1, 2, 3]),
                server_name: Some("aisync-receiver".to_string()),
            });

        let (pending_peer, pending_code, pending_request_id, _) =
            backend.take_pending_pairing_request().unwrap();
        assert_eq!(pending_peer.addresses.first(), Some(&endpoint.ip()));
        assert_eq!(pending_code, "123456");
        assert_eq!(pending_request_id, request_id);

        let pairing = backend.pairing_code(&peer_id).unwrap();
        assert_eq!(pairing.code, "123456");
        assert_eq!(pairing.request_id, request_id);
        assert_eq!(pairing.peer.addresses.first(), Some(&endpoint.ip()));

        backend.confirm_pairing(&peer_id).unwrap();
        let persisted = load_config(&config_path).unwrap();
        let stored = persisted.peers.get("Mac-mini").unwrap();
        assert_eq!(stored.endpoint, Some(endpoint));

        let paired = backend.paired_peers();
        let (device, online) = paired
            .iter()
            .find(|(device, _)| device.id == peer_id)
            .expect("paired peer should be present");
        assert_eq!(device.addresses.first(), Some(&endpoint.ip()));
        assert!(*online);
    }

    #[test]
    fn project_mapping_request_is_queued_for_ui_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        let backend = Backend::with_config(config, config_path).unwrap();
        let request_id = "mapping-request-1".to_string();
        let peer_id = DeviceId(Uuid::new_v4());
        let source_dir = PathBuf::from("/peer/source");

        backend
            .pending_project_mapping_requests
            .lock()
            .unwrap()
            .push_back(ProjectMappingRequestPayload {
                request_id: request_id.clone(),
                project_name: "demo".to_string(),
                source_dir: source_dir.clone(),
                mode: "twoWayAuto".to_string(),
                device: DeviceInfo {
                    id: peer_id,
                    name: "MacBook".to_string(),
                    os: OsType::Darwin,
                    addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 31, 10))],
                    protocol_version: 1,
                },
                endpoint: Some(SocketAddr::from(([192, 168, 31, 10], 52000))),
                receiver_cert_der: Some(vec![1, 2, 3]),
                server_name: Some("aisync-receiver".to_string()),
            });

        let request = backend
            .take_pending_project_mapping_request()
            .expect("pending project mapping request");
        assert_eq!(request.request_id, request_id);
        assert_eq!(request.project_name, "demo");
        assert_eq!(request.source_dir, source_dir);
        assert!(backend
            .inner
            .lock()
            .unwrap()
            .project_mapping_requests
            .contains_key(&request_id));
    }

    #[test]
    fn project_mapping_ack_uses_discovered_cert_before_request_cert() {
        let request_endpoint = SocketAddr::from(([192, 168, 31, 10], 52000));
        let live_endpoint = SocketAddr::from(([192, 168, 31, 10], 52001));
        let request = ProjectMappingRequestPayload {
            request_id: "mapping-request-cert".to_string(),
            project_name: "demo".to_string(),
            source_dir: PathBuf::from("/peer/source"),
            mode: "twoWayAuto".to_string(),
            device: DeviceInfo {
                id: DeviceId(Uuid::new_v4()),
                name: "MacBook".to_string(),
                os: OsType::Darwin,
                addresses: vec![request_endpoint.ip()],
                protocol_version: 1,
            },
            endpoint: Some(request_endpoint),
            receiver_cert_der: Some(vec![1, 2, 3]),
            server_name: Some("old-name".to_string()),
        };
        let connection = aisync_discovery::PeerConnectionInfo {
            endpoint: Some(live_endpoint),
            receiver_cert_der: Some(vec![9, 8, 7]),
            server_name: Some("new-name".to_string()),
        };

        let selected = project_mapping_ack_connection(Some(connection), &request).unwrap();

        assert_eq!(selected.endpoint, live_endpoint);
        assert_eq!(selected.receiver_cert_der, vec![9, 8, 7]);
        assert_eq!(selected.server_name, "new-name");
        assert_eq!(selected.cert_source, "discovery");
    }

    #[test]
    fn push_transport_prefers_discovery_cert_over_stale_config_cert() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let old_cert_path = tmp.path().join("old-receiver.der");
        fs::write(&old_cert_path, b"old-cert").unwrap();
        let peer_id = DeviceId(Uuid::new_v4());
        let old_endpoint = SocketAddr::from(([192, 168, 31, 10], 52000));
        let live_endpoint = SocketAddr::from(([192, 168, 31, 10], 52001));
        let mut config = SyncConfig::new("local");
        config.peers.insert(
            "Mac-mini".to_string(),
            PeerConfig {
                id: peer_id,
                name: "Mac-mini".to_string(),
                endpoint: Some(old_endpoint),
                server_cert: Some(old_cert_path),
                server_name: Some("old-name".to_string()),
                last_seen: None,
            },
        );
        let live = PeerConnectionInfo {
            endpoint: Some(live_endpoint),
            receiver_cert_der: Some(b"new-cert".to_vec()),
            server_name: Some("new-name".to_string()),
        };

        let selected =
            peer_transport_connection(&config_path, &config, "Mac-mini", Some(live)).unwrap();

        assert_eq!(selected.endpoint, live_endpoint);
        assert_eq!(selected.receiver_cert_der, b"new-cert");
        assert_eq!(selected.server_name, "new-name");
        assert_eq!(selected.cert_source, "discovery");
        assert_eq!(selected.peer.addresses.first(), Some(&live_endpoint.ip()));
    }

    #[test]
    fn project_mapping_confirm_failure_does_not_persist_before_ack() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        let backend = Backend::with_config(config, config_path.clone()).unwrap();
        let local_dir = tmp.path().join("local-project");
        fs::create_dir_all(&local_dir).unwrap();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let endpoint = listener.local_addr().unwrap();
        drop(listener);
        let request_id = "mapping-request-no-persist".to_string();

        backend
            .inner
            .lock()
            .unwrap()
            .project_mapping_requests
            .insert(
                request_id.clone(),
                ProjectMappingRequestPayload {
                    request_id: request_id.clone(),
                    project_name: "demo".to_string(),
                    source_dir: PathBuf::from("/peer/source"),
                    mode: "twoWayAuto".to_string(),
                    device: DeviceInfo {
                        id: DeviceId(Uuid::new_v4()),
                        name: "MacBook".to_string(),
                        os: OsType::Darwin,
                        addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
                        protocol_version: 1,
                    },
                    endpoint: Some(endpoint),
                    receiver_cert_der: Some(vec![1, 2, 3]),
                    server_name: Some("aisync-receiver".to_string()),
                },
            );

        assert!(backend
            .confirm_project_mapping_request(&request_id, local_dir)
            .is_err());
        assert!(backend.config().projects.is_empty());
        if config_path.exists() {
            assert!(load_config(&config_path).unwrap().projects.is_empty());
        }
        assert!(backend
            .inner
            .lock()
            .unwrap()
            .project_mapping_requests
            .contains_key(&request_id));
    }

    #[test]
    fn project_mapping_ack_persists_outbound_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        let backend = Backend::with_config(config, config_path.clone()).unwrap();
        let request_id = "mapping-request-2".to_string();
        let local_dir = tmp.path().join("local-project");
        let remote_dir = PathBuf::from("/peer/chosen-project");
        fs::create_dir_all(&local_dir).unwrap();

        backend
            .inner
            .lock()
            .unwrap()
            .outbound_project_mappings
            .insert(
                request_id.clone(),
                OutboundProjectMapping {
                    project_name: "demo".to_string(),
                    local_dir: local_dir.clone(),
                    peer_name: "Mac-mini".to_string(),
                    mode: SyncModeConfig::TwoWayAuto,
                },
            );
        backend
            .pending_project_mapping_acks
            .lock()
            .unwrap()
            .push_back(ProjectMappingAckPayload {
                request_id: request_id.clone(),
                accepted: true,
                project_name: "demo".to_string(),
                remote_dir: Some(remote_dir.clone()),
                message: None,
                device: DeviceInfo {
                    id: DeviceId(Uuid::new_v4()),
                    name: "Mac-mini".to_string(),
                    os: OsType::Darwin,
                    addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 31, 11))],
                    protocol_version: 1,
                },
            });

        assert_eq!(backend.process_project_mapping_acks().unwrap(), 1);
        let persisted = load_config(&config_path).unwrap();
        let project = persisted
            .projects
            .iter()
            .find(|project| project.name == "demo")
            .expect("project persisted");
        assert_eq!(project.local, local_dir);
        assert_eq!(project.peers.get("Mac-mini"), Some(&remote_dir));
        assert_eq!(project.sync_mode, SyncModeConfig::TwoWayAuto);
        assert!(!backend
            .inner
            .lock()
            .unwrap()
            .outbound_project_mappings
            .contains_key(&request_id));
    }

    #[test]
    fn workspace_conflict_analysis_isolates_first_level_child() {
        let tmp = tempfile::tempdir().unwrap();
        let local_root = tmp.path().join("workspace");
        let remote_root = tmp.path().join("remote");
        fs::create_dir_all(&local_root).unwrap();
        let base_a = SyncManifest {
            files: vec![manifest_entry("main.rs", "base-a")],
        };
        let base_b = SyncManifest {
            files: vec![manifest_entry("main.rs", "base-b")],
        };
        let workspace = WorkspaceConfig {
            name: "workspace".to_string(),
            local_root: local_root.clone(),
            remote_root: remote_root.clone(),
            peer: "peer".to_string(),
            children: vec![
                WorkspaceChildConfig {
                    name: "app-a".to_string(),
                    local_dir: local_root.join("app-a"),
                    remote_dir: remote_root.join("app-a"),
                    enabled: true,
                    conflicted: false,
                    last_fingerprint: Some(manifest_fingerprint(&base_a)),
                },
                WorkspaceChildConfig {
                    name: "app-b".to_string(),
                    local_dir: local_root.join("app-b"),
                    remote_dir: remote_root.join("app-b"),
                    enabled: true,
                    conflicted: false,
                    last_fingerprint: Some(manifest_fingerprint(&base_b)),
                },
            ],
            local: local_root,
            peers: HashMap::new(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        };
        let source = SyncManifest {
            files: vec![
                manifest_entry("app-a/main.rs", "local-a"),
                manifest_entry("app-b/main.rs", "local-b"),
            ],
        };
        let remote = SyncManifest {
            files: vec![
                manifest_entry("app-a/main.rs", "remote-a"),
                manifest_entry("app-b/main.rs", "base-b"),
            ],
        };

        let analysis = analyze_workspace_conflicts(&workspace, &source, &remote);

        assert_eq!(analysis.conflicted_children, vec!["app-a"]);
        assert_eq!(analysis.safe_children.len(), 1);
        assert_eq!(analysis.safe_children[0].name, "app-b");
        let app_a = analysis
            .workspace
            .children
            .iter()
            .find(|child| child.name == "app-a")
            .unwrap();
        let app_b = analysis
            .workspace
            .children
            .iter()
            .find(|child| child.name == "app-b")
            .unwrap();
        assert!(app_a.conflicted);
        assert!(!app_b.conflicted);
        assert_eq!(
            app_b.last_fingerprint.as_deref(),
            Some(manifest_fingerprint(&child_manifest(&source, "app-b")).as_str())
        );
    }

    #[test]
    fn with_config_starts_watchers_for_existing_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        config.workspaces.push(WorkspaceConfig {
            name: "workspace".to_string(),
            local_root: workspace_root.clone(),
            remote_root: tmp.path().join("remote"),
            peer: "peer".to_string(),
            children: Vec::new(),
            local: workspace_root,
            peers: HashMap::new(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        });
        let backend = Backend::with_config(config, tmp.path().join("config.toml")).unwrap();

        assert!(backend
            .inner
            .lock()
            .unwrap()
            .workspace_watchers
            .contains_key("workspace"));
    }

    #[test]
    fn config_refresh_persists_new_workspace_child() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().join("workspace");
        let remote_root = tmp.path().join("remote");
        fs::create_dir_all(workspace_root.join("old")).unwrap();
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        config.workspaces.push(WorkspaceConfig {
            name: "workspace".to_string(),
            local_root: workspace_root.clone(),
            remote_root: remote_root.clone(),
            peer: "peer".to_string(),
            children: workspace_children(&workspace_root, &remote_root, true).unwrap(),
            local: workspace_root.clone(),
            peers: HashMap::new(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        });
        let config_path = tmp.path().join("config.toml");
        let backend = Backend::with_config(config, config_path.clone()).unwrap();

        fs::create_dir_all(workspace_root.join("new-child")).unwrap();
        let refreshed = backend.config_with_refreshed_workspaces();

        assert!(refreshed.workspaces[0]
            .children
            .iter()
            .any(|child| child.name == "new-child" && child.enabled));
        let persisted = load_config(&config_path).unwrap();
        assert!(persisted.workspaces[0]
            .children
            .iter()
            .any(|child| child.name == "new-child" && child.enabled));
    }

    #[test]
    fn receiver_history_refreshes_workspace_children_before_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().join("workspace");
        let remote_root = tmp.path().join("remote");
        fs::create_dir_all(workspace_root.join("old")).unwrap();
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        config.workspaces.push(WorkspaceConfig {
            name: "workspace".to_string(),
            local_root: workspace_root.clone(),
            remote_root: remote_root.clone(),
            peer: "peer".to_string(),
            children: workspace_children(&workspace_root, &remote_root, true).unwrap(),
            local: workspace_root.clone(),
            peers: HashMap::new(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        });
        let config_path = tmp.path().join("config.toml");
        save_config(&config_path, &config).unwrap();
        fs::create_dir_all(workspace_root.join("new-child")).unwrap();
        fs::write(workspace_root.join("new-child/main.rs"), b"fn main() {}\n").unwrap();

        record_receiver_sync_history(
            &config_path,
            &SyncManifest {
                files: vec![manifest_entry("new-child/main.rs", "hash")],
            },
            &workspace_root,
        );

        let persisted = load_config(&config_path).unwrap();
        assert!(persisted.workspaces[0]
            .children
            .iter()
            .any(|child| child.name == "new-child" && child.enabled));
        let history = fs::read_to_string(tmp.path().join("history.jsonl")).unwrap();
        assert!(history.contains("\"workspaceName\":\"workspace\""));
        assert!(history.contains("\"childName\":\"new-child\""));
    }

    #[test]
    fn session_mtime_decision_triggers_new_target_after_initial_scan() {
        let key = "project:empty:peer:claude:/session".to_string();
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let mut seen = HashMap::new();

        assert_eq!(
            classify_session_mtime(&seen, &key, mtime, false),
            SessionMtimeDecision::BaselineNew
        );
        assert_eq!(
            classify_session_mtime(&seen, &key, mtime, true),
            SessionMtimeDecision::TriggerNew
        );

        seen.insert(key.clone(), mtime);
        assert_eq!(
            classify_session_mtime(&seen, &key, mtime, true),
            SessionMtimeDecision::Unchanged
        );
        assert_eq!(
            classify_session_mtime(&seen, &key, mtime + std::time::Duration::from_secs(1), true,),
            SessionMtimeDecision::TriggerModified
        );
    }

    #[test]
    fn empty_project_session_target_uses_claude_encoded_dir_when_it_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("empty-project");
        let remote_dir = tmp.path().join("remote-project");
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&project_dir).unwrap();
        fs::create_dir_all(claude_dir.join("projects")).unwrap();

        let mut config = SyncConfig::new("local");
        config.claude_config.local = claude_dir.clone();
        config.projects.push(ProjectConfig {
            name: "empty".to_string(),
            local: project_dir.clone(),
            peers: HashMap::from([("peer".to_string(), remote_dir)]),
            sync_mode: SyncModeConfig::OneWayPush,
            enabled: true,
            exclude_rules: Vec::new(),
            sync_snapshots: HashMap::new(),
        });
        assert!(!session_mtime_targets(&config)
            .iter()
            .any(|target| target.tool == "claude"));

        let session_dir = claude_dir
            .join("projects")
            .join(claude_project_dir_name(&project_dir));
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("s.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user",
                    "cwd": project_dir.to_string_lossy(),
                    "sessionId": "s"
                })
            ),
        )
        .unwrap();

        let targets = session_mtime_targets(&config);
        let target = targets
            .iter()
            .find(|target| {
                target.scope == "project" && target.name == "empty" && target.tool == "claude"
            })
            .expect("new Claude session dir should become a project target");
        assert_eq!(target.path, session_dir);
    }

    #[test]
    fn empty_workspace_child_session_target_uses_child_encoded_dir_when_it_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().join("workspace");
        let remote_root = tmp.path().join("remote");
        let child_dir = workspace_root.join("empty-child");
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&child_dir).unwrap();
        fs::create_dir_all(claude_dir.join("projects")).unwrap();

        let mut config = SyncConfig::new("local");
        config.claude_config.local = claude_dir.clone();
        config.workspaces.push(WorkspaceConfig {
            name: "workspace".to_string(),
            local_root: workspace_root.clone(),
            remote_root: remote_root.clone(),
            peer: "peer".to_string(),
            children: workspace_children(&workspace_root, &remote_root, true).unwrap(),
            local: workspace_root,
            peers: HashMap::new(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        });

        let session_dir = claude_dir
            .join("projects")
            .join(claude_project_dir_name(&child_dir));
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("s.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user",
                    "cwd": child_dir.to_string_lossy(),
                    "sessionId": "s"
                })
            ),
        )
        .unwrap();

        let targets = session_mtime_targets(&config);
        let target = targets
            .iter()
            .find(|target| {
                target.scope == "workspace"
                    && target.name == "workspace"
                    && target.tool == "claude"
                    && target.path == session_dir
            })
            .expect("new child Claude session dir should become a workspace target");
        assert_eq!(target.peer, "peer");
    }

    #[test]
    fn workspace_sync_fingerprint_ignores_unrelated_codex_sessions() {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let previous = std::env::var_os("AISYNC_CODEX_SESSIONS_DIR");
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().join("workspace");
        let remote_root = tmp.path().join("remote");
        let codex_sessions = tmp.path().join("codex/sessions");
        fs::create_dir_all(workspace_root.join("app")).unwrap();
        fs::write(workspace_root.join("app/main.rs"), b"fn main() {}\n").unwrap();
        fs::create_dir_all(&codex_sessions).unwrap();
        std::env::set_var("AISYNC_CODEX_SESSIONS_DIR", &codex_sessions);

        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        config.workspaces.push(WorkspaceConfig {
            name: "workspace".to_string(),
            local_root: workspace_root.clone(),
            remote_root: remote_root.clone(),
            peer: "peer".to_string(),
            children: workspace_children(&workspace_root, &remote_root, true).unwrap(),
            local: workspace_root.clone(),
            peers: HashMap::new(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        });
        let target = SessionMtimeTarget {
            scope: "workspace",
            name: "workspace".to_string(),
            peer: "peer".to_string(),
            tool: "codex",
            path: codex_sessions.clone(),
        };

        let initial = sync_fingerprint_for_target(&config, &target).unwrap();
        fs::write(
            codex_sessions.join("unrelated.jsonl"),
            "{\"cwd\":\"/tmp/other-project\"}\n",
        )
        .unwrap();
        let unrelated = sync_fingerprint_for_target(&config, &target).unwrap();
        fs::write(
            codex_sessions.join("related.jsonl"),
            format!("{{\"cwd\":\"{}\"}}\n", workspace_root.display()),
        )
        .unwrap();
        let related = sync_fingerprint_for_target(&config, &target).unwrap();

        match previous {
            Some(value) => std::env::set_var("AISYNC_CODEX_SESSIONS_DIR", value),
            None => std::env::remove_var("AISYNC_CODEX_SESSIONS_DIR"),
        }
        assert_eq!(initial, unrelated);
        assert_ne!(unrelated, related);
    }

    #[test]
    fn session_sync_key_includes_tool_dimension() {
        let target = SessionMtimeTarget {
            scope: "workspace",
            name: "workspace".to_string(),
            peer: "peer".to_string(),
            tool: "claude",
            path: PathBuf::from("/tmp/claude"),
        };
        let mut codex = target.clone();
        codex.tool = "codex";

        assert_ne!(session_sync_key(&target), session_sync_key(&codex));
    }

    #[test]
    fn workspace_auto_fingerprint_ignores_child_status_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_root = tmp.path().join("workspace");
        let remote_root = tmp.path().join("remote");
        let child_dir = workspace_root.join("app");
        fs::create_dir_all(&child_dir).unwrap();
        fs::write(child_dir.join("main.rs"), b"fn main() {}\n").unwrap();
        let child = WorkspaceChildConfig {
            name: "app".to_string(),
            local_dir: child_dir,
            remote_dir: remote_root.join("app"),
            enabled: true,
            conflicted: false,
            last_fingerprint: Some("old".to_string()),
        };
        let mut workspace = WorkspaceConfig {
            name: "workspace".to_string(),
            local_root: workspace_root.clone(),
            remote_root,
            peer: "peer".to_string(),
            children: vec![child],
            local: workspace_root,
            peers: HashMap::new(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        };
        let config = SyncConfig::new("local");
        let initial = workspace_auto_sync_fingerprint(&config, &workspace).unwrap();

        workspace.children[0].enabled = false;
        workspace.children[0].conflicted = true;
        workspace.children[0].last_fingerprint = Some("new".to_string());
        let metadata_changed = workspace_auto_sync_fingerprint(&config, &workspace).unwrap();

        assert_eq!(initial, metadata_changed);
    }

    #[test]
    fn record_auto_sync_history_creates_history_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        record_auto_sync_history(&config_path, "proj", true, 3, None, None, None, "mixed");

        let text = fs::read_to_string(tmp.path().join("history.jsonl")).unwrap();
        assert!(text.contains("\"projectId\":\"proj\""));
        assert!(text.contains("\"trigger\":\"auto\""));
        assert!(text.contains("\"files\":3"));
    }

    #[test]
    fn record_auto_sync_history_records_bytes_and_file_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let project_dir = tmp.path().join("project");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("main.rs"), b"fn main() {}\n").unwrap();
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        config.projects.push(ProjectConfig {
            name: "proj".to_string(),
            local: project_dir.clone(),
            peers: HashMap::from([("peer".to_string(), tmp.path().join("remote"))]),
            sync_mode: SyncModeConfig::OneWayPush,
            enabled: true,
            exclude_rules: Vec::new(),
            sync_snapshots: HashMap::new(),
        });
        save_config(&config_path, &config).unwrap();

        record_auto_sync_history(&config_path, "proj", true, 1, None, None, None, "mixed");

        let rows = read_jsonl(&tmp.path().join("history.jsonl"));
        assert_eq!(
            rows[0].get("bytes").and_then(|value| value.as_u64()),
            Some(13)
        );
        assert_eq!(
            rows[0].get("file_name").and_then(|value| value.as_str()),
            Some("main.rs")
        );
        assert!(rows[0]
            .get("file_paths")
            .and_then(|value| value.as_array())
            .is_some_and(|paths| paths
                .iter()
                .any(|path| path.as_str().unwrap_or_default().ends_with("main.rs"))));
    }

    #[test]
    fn workspace_child_history_uses_transferred_child_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let local_root = tmp.path().join("workspace");
        let remote_root = tmp.path().join("remote");
        let child_a = local_root.join("a");
        let child_b = local_root.join("b");
        fs::create_dir_all(&child_a).unwrap();
        fs::create_dir_all(&child_b).unwrap();
        fs::write(child_a.join("main.rs"), b"fn main() {}\n").unwrap();

        let workspace = WorkspaceConfig {
            name: "workspace".to_string(),
            local_root: local_root.clone(),
            remote_root: remote_root.clone(),
            peer: "peer".to_string(),
            children: vec![
                WorkspaceChildConfig {
                    name: "a".to_string(),
                    local_dir: child_a,
                    remote_dir: remote_root.join("a"),
                    enabled: true,
                    conflicted: false,
                    last_fingerprint: None,
                },
                WorkspaceChildConfig {
                    name: "b".to_string(),
                    local_dir: child_b,
                    remote_dir: remote_root.join("b"),
                    enabled: true,
                    conflicted: false,
                    last_fingerprint: None,
                },
            ],
            local: local_root,
            peers: HashMap::new(),
            scan_depth: 1,
            auto_enable_new: true,
            sync_mode: SyncModeConfig::TwoWayAuto,
            enabled: true,
            exclude_rules: Vec::new(),
        };
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        config.workspaces.push(workspace.clone());
        save_config(&config_path, &config).unwrap();
        let counts = HashMap::from([("a".to_string(), 2), ("b".to_string(), 0)]);

        record_auto_workspace_child_history(
            &config_path,
            &workspace,
            true,
            None,
            "mixed",
            Some(&counts),
        );

        let rows = read_jsonl(&tmp.path().join("history.jsonl"));
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("projectId").and_then(|value| value.as_str()),
            Some("a")
        );
        assert_eq!(
            rows[0].get("files").and_then(|value| value.as_u64()),
            Some(2)
        );
    }

    #[test]
    fn empty_project_session_history_summary_detects_claude_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("empty-project");
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&project_dir).unwrap();
        let session_dir = claude_dir
            .join("projects")
            .join(claude_project_dir_name(&project_dir));
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("s.jsonl"), b"{\"type\":\"user\"}\n").unwrap();
        let mut config = SyncConfig::new("local");
        config.state_path = Some(tmp.path().join("state.toml"));
        config.claude_config.local = claude_dir;
        config.projects.push(ProjectConfig {
            name: "empty".to_string(),
            local: project_dir,
            peers: HashMap::from([("peer".to_string(), tmp.path().join("remote"))]),
            sync_mode: SyncModeConfig::OneWayPush,
            enabled: true,
            exclude_rules: Vec::new(),
            sync_snapshots: HashMap::new(),
        });

        let summary = history_summary_from_config(&config, "empty", None, None, "session");

        assert_eq!(summary.bytes, 16);
        assert!(summary
            .file_paths
            .iter()
            .any(|path| path.ends_with("s.jsonl")));
    }

    fn manifest_entry(relative_path: &str, hash: &str) -> aisync_core::FileEntry {
        aisync_core::FileEntry {
            relative_path: relative_path.to_string(),
            size: hash.len() as u64,
            blake3_hash: hash.to_string(),
            mtime: 0,
        }
    }
}

/// Start the TLS receive daemon on a dedicated thread so other CodeBaton instances
/// can push to this one. Writes the receiver's self-signed cert (.der) next to
/// the config so a peer can pin it. Returns connection coordinates, or `None`
/// if binding fails (e.g. port in use) — the UI still works, just can't receive.
fn start_serve_daemon(
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
) -> Option<ServeInfo> {
    let receive_dir = receive_root(config_path);
    let cert_path = receiver_cert_path(config_path);
    if let Err(e) = fs::create_dir_all(&receive_dir) {
        eprintln!("[aisync-app] receive dir create failed: {e}");
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
            eprintln!("[aisync-app] receiver identity failed: {e}");
            return None;
        }
    };
    if let Err(e) = fs::write(&cert_out, &identity.cert_der) {
        eprintln!("[aisync-app] write receiver cert failed: {e}");
        return None;
    }

    let service = match runtime.block_on(async {
        let tls = TlsConfig::new(identity, "aisync-receiver");
        ReceiveService::bind(listen, target, &tls).await
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[aisync-app] receive daemon bind failed on {listen}: {e}");
            return None;
        }
    };
    let bound = service.local_addr().ok().map(|a| a.port()).unwrap_or(port);
    log_line(&format!(
        "[aisync-app] receive daemon listening on :{bound} → {} (cert {})",
        receive_dir.display(),
        cert_path.display()
    ));
    let history_config_path = config_path.to_path_buf();
    let history_receive_dir = receive_dir.clone();

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
                if receive_limit
                    .map(|limit| handled >= limit)
                    .unwrap_or(false)
                {
                    log_line(&format!(
                        "[aisync-app] receive daemon test limit reached: {handled}"
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
                        Ok(Err(e)) => eprintln!("[aisync-app] receive daemon error: {e}"),
                        Err(_) => {
                            log_line("[aisync-app] receive daemon test idle timeout");
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
                        Err(e) => eprintln!("[aisync-app] receive daemon error: {e}"),
                    }
                }
            }
        });
    });

    Some(ServeInfo {
        port: bound,
        cert_path,
        receive_dir,
    })
}

/// Map the UI sync-mode label to the config enum.
pub fn sync_mode_from_label(label: &str) -> SyncModeConfig {
    match label {
        "oneWayPush" => SyncModeConfig::OneWayPush,
        "oneWayPull" => SyncModeConfig::OneWayPull,
        _ => SyncModeConfig::TwoWayAuto,
    }
}

fn sync_mode_label(mode: SyncModeConfig) -> &'static str {
    match mode {
        SyncModeConfig::OneWayPush => "oneWayPush",
        SyncModeConfig::OneWayPull => "oneWayPull",
        SyncModeConfig::TwoWayAuto => "twoWayAuto",
    }
}

/// Convenience to add a project mapping to config (D1).
pub fn project_config(
    name: String,
    local: PathBuf,
    peer_name: String,
    remote: PathBuf,
    mode: SyncModeConfig,
) -> ProjectConfig {
    let mut peers = std::collections::HashMap::new();
    peers.insert(peer_name, remote);
    ProjectConfig {
        name,
        local,
        peers,
        sync_mode: mode,
        enabled: true,
        exclude_rules: Vec::new(),
        sync_snapshots: std::collections::HashMap::new(),
    }
}
