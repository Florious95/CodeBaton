//! Real backend wiring.
//!
//! Bridges the Tauri IPC layer to the actual codebaton crates:
//! - [`codebaton_discovery::MdnsDiscoverer`] for LAN discovery + pairing
//! - [`codebaton_transport::TcpTransporter`] for push transport
//! - [`codebaton_transport`] for manifest / sensitive-file scanning
//!
//! The GUI starts a local receive daemon on launch. Pairing persists the peer's
//! advertised endpoint and pinned receiver certificate so push can connect
//! directly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
#[cfg(test)]
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use codebaton_core::{
    AisyncError, DeviceId, DeviceInfo, Discoverer, OsType, Result,
    SyncManifest,
};
use codebaton_discovery::{
    local_device_addresses, DiscoveryConfig, MdnsDiscoverer, PeerConnectionInfo,
};
use codebaton_session::PathRule;

mod time_util;
use self::time_util::{
    epoch_millis_now, epoch_millis_now_u64, normalize_epoch_millis, unix_nanos_now, unix_secs_now,
};

mod identity;
use self::identity::{default_device_name, is_placeholder_device_name};
// Used only by the in-file `mod tests` (via `use super::*`).
#[cfg(test)]
use self::identity::{load_or_create_receiver_identity, system_hostname, PLACEHOLDER_DEVICE_NAME};

mod events;
use self::events::record_event;
// Test/CLI-visible event + log API: keep stable at `backend::<Name>`.
pub use self::events::{
    current_rss_bytes, event_count, events_for, log_line, reset_event_counts, RecordedEvent,
};

mod auto_sync_gate;
// Gate/suppress/baseline statics + accessors: consumed widely across mod.rs and
// by the in-file `mod tests`. pub(crate) glob keeps them at the backend root.
use self::auto_sync_gate::*;
// Test hook: keep stable at `backend::set_auto_sync_cooldown_for_test`.
pub use self::auto_sync_gate::set_auto_sync_cooldown_for_test;

mod workspace_conflict;
use self::workspace_conflict::{
    analyze_workspace_conflicts, child_manifest, manifest_fingerprint, WorkspaceConflictAnalysis,
};

mod transport;
use self::transport::{
    advertised_local_endpoint, file_transfer_ack_connection,
    peer_transport_connection, send_file_transfer_ack,
    send_file_transfer_data, send_file_transfer_request,
    send_project_mapping_request, send_text_message,
};
// Used only by the in-file `mod tests` (via `use super::*`).
#[cfg(test)]
use self::transport::project_mapping_ack_connection;

mod history;
use self::history::{
    append_json_line, read_jsonl, record_auto_sync_history, record_auto_workspace_child_history,
};
#[cfg(test)]
use self::history::record_receiver_sync_history;
// Used only by the in-file `mod tests` (via `use super::*`).
#[cfg(test)]
use self::history::history_summary_from_config;

mod auto_sync_orchestration;
use self::auto_sync_orchestration::{
    hash_prefix, project_auto_sync_fingerprint, run_project_auto_sync,
    run_workspace_auto_sync_outcome, sync_fingerprint_for_target, workspace_auto_sync_fingerprint,
    WorkspaceSyncOutcome,
};

mod session_stage;
use self::session_stage::{
    claude_project_dir_name, count_files_recursive, increment_child_file_count,
    prepare_claude_session_sync, prepare_claude_workspace_session_sync, prepare_codex_session_sync,
    prepare_codex_workspace_session_sync,
};
// Used only by the in-file `mod tests` (via `use super::*`).
#[cfg(test)]
use self::session_stage::project_rewriter;

mod sync_push;
use self::sync_push::{run_tcp_push, run_workspace_tcp_push};
mod claude_paths;
use self::claude_paths::*;
mod exclude;
use self::exclude::{project_exclude_rules, workspace_exclude_rules};
mod messaging;
#[cfg(test)]
use self::messaging::record_text_message_history;
mod session_scanner;
use self::session_scanner::{
    refresh_workspaces_in_config, session_mtime_targets, SessionMtimeTarget,
};
#[cfg(test)]
use self::session_scanner::{classify_session_mtime, SessionMtimeDecision};
mod file_transfer;
use self::file_transfer::{safe_filename, FileReceiveState, OutboundFileTransfer};
#[cfg(test)]
use self::file_transfer::{
    ensure_file_receive_target, ensure_file_transfer_source_allowed, file_transfer_tmp_path,
    prepare_default_file_transfer_accept, receive_file_transfer_data,
};
mod projects;
mod workspaces;
mod peers;
// PairingInfo is a public return type (pairing_code); keep stable at backend root.
pub use self::peers::PairingInfo;
// persist_peer_connection is called only by the in-file `mod tests`.
#[cfg(test)]
use self::peers::persist_peer_connection;
// PairingSession is referenced by the non-moved `active_pairing_session` helper.
use self::peers::PairingSession;
// PairingConnection is referenced by the sibling `transport` module via `super::`.
use self::peers::PairingConnection;
mod serve;
// ServeInfo is a public return type (serve_info) and referenced by sibling
// transport.rs via `super::ServeInfo`; keep stable at backend root.
pub use self::serve::ServeInfo;
// ServeShutdownHandle is a Backend field type; start_serve_daemon is called by
// the 3 constructors.
use self::serve::{start_serve_daemon, ServeShutdownHandle};
mod split_brain;
pub use self::split_brain::{SplitBrainResolution, SplitBrainStatus};
use codebaton_sync::{
    default_state_path, load_config, save_config, DiscoveredProject, FsWatcher,
    ProjectConfig, SyncConfig, SyncModeConfig, WatchConfig, WorkspaceChildConfig,
    WorkspaceConfig,
};
#[cfg(test)]
use codebaton_sync::PeerConfig;
#[cfg(test)]
use codebaton_transport::FileTransferDataPayload;
use codebaton_transport::{
    generate_tls_identity, scan_sensitive_files, FileTransferAckPayload,
    FileTransferRequestPayload, PairingRequestPayload,
    ProjectMappingAckPayload, ProjectMappingRequestPayload, SensitiveFile,
    TextMessagePayload, TlsConfig,
    WorkspaceMappingAckPayload, WorkspaceMappingRequestPayload,
};

const HISTORY_FILE_LIMIT: usize = 5;

/// Where an incoming push lands. Resolution order:
/// 1. `config.receive_dir_override` (explicit per-instance dir — parallel-safe, no global state)
/// 2. `AISYNC_RECEIVE_DIR` env (legacy fallback)
/// 3. `config_path` 同级 `received/`
fn receive_root(config: &SyncConfig, config_path: &Path) -> PathBuf {
    config
        .receive_dir_override
        .clone()
        .or_else(|| std::env::var("AISYNC_RECEIVE_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| config_path.with_file_name("received"))
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

impl Drop for Backend {
    fn drop(&mut self) {
        // 停 serve 守护，防 orphan accept 线程在测试目录删除后继续写。
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(handle) = inner.serve_shutdown.take() {
                handle.shutdown();
            }
        }
    }
}

struct Inner {
    config: SyncConfig,
    config_path: PathBuf,
    discoverer: MdnsDiscoverer,
    serve: Option<ServeInfo>,
    serve_shutdown: Option<ServeShutdownHandle>,
    pairing_sessions: HashMap<DeviceId, PairingSession>,
    project_mapping_requests: HashMap<String, ProjectMappingRequestPayload>,
    outbound_project_mappings: HashMap<String, OutboundProjectMapping>,
    workspace_mapping_requests: HashMap<String, WorkspaceMappingRequestPayload>,
    outbound_workspace_mappings: HashMap<String, OutboundWorkspaceMapping>,
    file_transfer_requests: HashMap<String, FileTransferRequestPayload>,
    outbound_file_transfers: HashMap<String, OutboundFileTransfer>,
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

impl Backend {
    /// Build the backend, loading config from `~/.aisync/config.toml` (or a
    /// default in-memory config when absent) and starting live mDNS discovery.
    pub fn new() -> Result<Self> {
        let config_path = codebaton_sync::default_config_path()
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
        let (serve, serve_shutdown) = match start_serve_daemon(
            &config,
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
        ) {
            Some((info, handle)) => (Some(info), Some(handle)),
            None => (None, None),
        };
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

        // Manual handoff: no background watchers or session scanner are started.
        // Sync happens only on explicit user push.
        Ok(Self {
            inner: Mutex::new(Inner {
                config,
                config_path,
                discoverer,
                serve,
                serve_shutdown,
                pairing_sessions: HashMap::new(),
                project_mapping_requests: HashMap::new(),
                outbound_project_mappings: HashMap::new(),
                workspace_mapping_requests: HashMap::new(),
                outbound_workspace_mappings: HashMap::new(),
                file_transfer_requests: HashMap::new(),
                outbound_file_transfers: HashMap::new(),
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
        Ok(Self {
            inner: Mutex::new(Inner {
                config,
                config_path,
                discoverer,
                serve: None,
                serve_shutdown: None,
                pairing_sessions: HashMap::new(),
                project_mapping_requests: HashMap::new(),
                outbound_project_mappings: HashMap::new(),
                workspace_mapping_requests: HashMap::new(),
                outbound_workspace_mappings: HashMap::new(),
                file_transfer_requests: HashMap::new(),
                outbound_file_transfers: HashMap::new(),
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

    /// 停止 serve 守护（若有）：唤醒阻塞的 accept、令守护循环退出、释放端口与线程。
    /// 幂等。测试在 Backend drop 时自动调用以防 orphan 线程。
    pub fn shutdown_serve(&self) {
        let handle = self.inner.lock().unwrap().serve_shutdown.take();
        if let Some(handle) = handle {
            handle.shutdown();
        }
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
        let (serve, serve_shutdown) = match start_serve_daemon(
            &config,
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
        ) {
            Some((info, handle)) => (Some(info), Some(handle)),
            None => (None, None),
        };
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
        Ok(Self {
            inner: Mutex::new(Inner {
                config,
                config_path,
                discoverer,
                serve,
                serve_shutdown,
                pairing_sessions: HashMap::new(),
                project_mapping_requests: HashMap::new(),
                outbound_project_mappings: HashMap::new(),
                workspace_mapping_requests: HashMap::new(),
                outbound_workspace_mappings: HashMap::new(),
                file_transfer_requests: HashMap::new(),
                outbound_file_transfers: HashMap::new(),
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

    fn file_receive_target_path(&self, transfer_id: &str) -> String {
        self.file_receive_states
            .lock()
            .unwrap()
            .get(transfer_id)
            .map(|state| state.target_path.display().to_string())
            .unwrap_or_default()
    }

    pub fn suggested_file_receive_path(&self, filename: &str) -> PathBuf {
        let g = self.inner.lock().unwrap();
        default_file_receive_dir(&g.config_path, &g.config).join(safe_filename(filename))
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
            codebaton_sync::default_refresh_interval_secs()
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

    pub fn local_device(&self) -> DeviceInfo {
        self.inner.lock().unwrap().discoverer.local_device().clone()
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
            let request_id = codebaton_discovery::new_pairing_request_id();
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
}

/// Real AI-tool status surfaced to the overview/settings UI.
pub struct AiTool {
    pub name: String,
    pub config_dir: String,
    pub session_count: u32,
    pub installed: bool,
}

// ── helpers ──────────────────────────────────────────────────────────

pub(crate) fn home_dir() -> Option<PathBuf> {
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

fn live_connection_for_config_peer(g: &Inner, peer_name: &str) -> Option<PeerConnectionInfo> {
    let peer = g.config.peers.get(peer_name)?;
    g.discoverer.peer_connection_info(&peer.id).ok().flatten()
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

pub(crate) fn app_log(event: &str, fields: &[(&str, String)]) {
    record_event(event, fields);
    let mut line = format!("[codebaton-app] event={event}");
    for (key, value) in fields {
        let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"<encode-error>\"".into());
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&encoded);
    }
    log_line(&line);
}

pub(crate) fn refresh_and_save_workspaces(config_path: &Path) -> Option<SyncConfig> {
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
    workspace: &codebaton_sync::WorkspaceConfig,
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

fn replace_workspace(config: &mut SyncConfig, workspace: WorkspaceConfig) {
    config
        .workspaces
        .retain(|existing| existing.name != workspace.name);
    config.workspaces.push(workspace);
}

pub(crate) fn claude_watch_paths(config: &SyncConfig, code_roots: &[PathBuf]) -> Vec<PathBuf> {
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

pub(crate) fn existing_unique_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for path in paths {
        if path.exists() && seen.insert(path.clone()) {
            unique.push(path);
        }
    }
    unique
}

pub(crate) fn session_target_key(target: &SessionMtimeTarget) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        target.scope,
        target.name,
        target.peer,
        target.tool,
        target.path.display()
    )
}

pub(crate) fn session_sync_key(target: &SessionMtimeTarget) -> String {
    format!(
        "{}:{}:{}:{}",
        target.scope, target.name, target.peer, target.tool
    )
}

fn session_seed_key(config_path: &Path, target_key: &str) -> String {
    format!("{}:{target_key}", config_path.display())
}

pub(crate) fn baseline_session_target(
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

pub(crate) fn run_pending_workspace_first_propagations(config_path: &Path, config: &SyncConfig) {
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

pub(crate) fn refresh_interval_secs(config: &SyncConfig) -> u64 {
    match config.refresh_interval_secs {
        0 => codebaton_sync::default_refresh_interval_secs(),
        secs => secs,
    }
}

pub(crate) fn dedupe_mtime_targets(targets: Vec<SessionMtimeTarget>) -> Vec<SessionMtimeTarget> {
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

pub(crate) fn latest_mtime_limited(root: &Path, limit: usize) -> Option<SystemTime> {
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

pub(crate) fn refresh_workspace_children(
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

pub(crate) fn target_content_fingerprint(target: &SessionMtimeTarget) -> Option<String> {
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

pub(crate) fn workspace_project_mapping(
    config: &SyncConfig,
    workspace: &WorkspaceConfig,
    peer_name: &str,
    remote_root: &Path,
) -> Result<codebaton_core::ProjectMapping> {
    Ok(codebaton_core::ProjectMapping {
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

pub(crate) fn local_codex_sessions_dir() -> Option<PathBuf> {
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

pub(crate) fn claude_projects_dir(root: PathBuf) -> PathBuf {
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

fn path_rule_for_project(project: &codebaton_core::ProjectMapping) -> PathRule {
    let source = project.local_code_dir.to_string_lossy().into_owned();
    let target = project.remote_code_dir.to_string_lossy().into_owned();
    if target.contains('\\') {
        PathRule::unix_to_windows(source, target)
    } else {
        PathRule::unix_to_unix(source, target)
    }
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

/// Resolve a human-friendly device name from the host on first run.
///
fn endpoint_online(endpoint: SocketAddr) -> bool {
    TcpStream::connect_timeout(&endpoint, Duration::from_millis(250)).is_ok()
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
        let project = codebaton_core::ProjectMapping {
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
        let project = codebaton_core::ProjectMapping {
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
        let request_id = codebaton_discovery::new_pairing_request_id();

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
        let connection = codebaton_discovery::PeerConnectionInfo {
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

    // Manual handoff: the backend must NOT start any background watchers for
    // existing workspaces — sync happens only on explicit user push.
    #[test]
    fn with_config_starts_no_watchers_for_existing_workspaces() {
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

        // No watcher machinery exists at all (the fields were removed with
        // auto-sync); construction simply loads the config and starts nothing
        // in the background. The workspace is present and untouched.
        let inner = backend.inner.lock().unwrap();
        assert_eq!(inner.config.workspaces.len(), 1);
        assert_eq!(inner.config.workspaces[0].name, "workspace");
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

    fn manifest_entry(relative_path: &str, hash: &str) -> codebaton_core::FileEntry {
        codebaton_core::FileEntry {
            relative_path: relative_path.to_string(),
            size: hash.len() as u64,
            blake3_hash: hash.to_string(),
            mtime: 0,
        }
    }
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
