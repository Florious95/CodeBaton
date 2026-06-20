//! Tauri IPC commands. The React frontend invokes these via
//! `@tauri-apps/api/core`'s `invoke`.
//!
//! Operational commands (sync, pairing, workspace scan, sensitive-file scan)
//! are routed through the real [`Backend`] (aisync-sync / aisync-discovery /
//! aisync-transport). Display data that the backend does not yet track
//! structurally (e.g. human-readable sync history, AI-tool session counts) is
//! still seeded from [`AppState`] so the UI stays populated; those seams are
//! marked and shrink as the backend grows.

#[cfg(target_os = "macos")]
use std::process::Command;
use std::thread;

use aisync_core::Direction;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::backend::Backend;
use crate::dto::*;
use crate::state::AppState;
use crate::tray;

// ── Overview / peers — real config + live discovery, no mock data ────

#[tauri::command]
pub fn get_overview(backend: State<Backend>) -> OverviewDto {
    let config = backend.config_with_refreshed_workspaces();
    let tools = ai_tool_dtos(&backend);
    let projects = config
        .projects
        .iter()
        .map(|p| {
            let mut dto = project_dto_from_config(p);
            dto.history = history_entries(&backend, &p.name);
            dto.last_sync = dto.history.first().map(|h| h.timestamp.clone());
            dto
        })
        .collect();
    OverviewDto {
        local: local_info(&backend),
        tools,
        projects,
        workspaces: config
            .workspaces
            .iter()
            .map(|w| workspace_dto_from_config(w, &backend))
            .collect(),
    }
}

/// Read persisted sync history for a project as DTOs (newest first).
fn history_entries(backend: &Backend, project_id: &str) -> Vec<SyncHistoryEntry> {
    backend
        .sync_history(Some(project_id))
        .into_iter()
        .map(|v| SyncHistoryEntry {
            timestamp: v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            project_id: v
                .get("projectId")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            workspace_name: v
                .get("workspaceName")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            child_name: v
                .get("childName")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            direction: v
                .get("direction")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            success: v.get("success").and_then(|t| t.as_bool()).unwrap_or(false),
            files: v.get("files").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
            bytes: v.get("bytes").and_then(|t| t.as_u64()).unwrap_or(0),
            detail: v
                .get("detail")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            trigger: v
                .get("trigger")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            role: v
                .get("role")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            file_type: v
                .get("fileType")
                .or_else(|| v.get("file_type"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
        })
        .collect()
}

fn text_message_entries(backend: &Backend, peer_name: Option<&str>) -> Vec<TextMessageDto> {
    backend
        .text_messages(peer_name)
        .into_iter()
        .map(|v| TextMessageDto {
            sender_name: v
                .get("senderName")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            content: v
                .get("content")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            timestamp: normalize_epoch_millis(
                v.get("timestamp").and_then(|t| t.as_u64()).unwrap_or(0),
            ),
            peer_name: v
                .get("peerName")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            mine: v.get("mine").and_then(|t| t.as_bool()),
        })
        .collect()
}

fn normalize_epoch_millis(timestamp: u64) -> u64 {
    if timestamp > 0 && timestamp < 1_000_000_000_000 {
        timestamp.saturating_mul(1000)
    } else {
        timestamp
    }
}

fn file_transfer_history_entries(
    backend: &Backend,
    peer_name: Option<&str>,
) -> Vec<FileTransferHistoryDto> {
    let mut rows: Vec<FileTransferHistoryDto> = backend
        .file_transfer_history(peer_name)
        .into_iter()
        .map(|v| FileTransferHistoryDto {
            id: v
                .get("transferId")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            timestamp: v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|t| t.parse::<u64>().ok())
                .or_else(|| v.get("timestamp").and_then(|t| t.as_u64()))
                .unwrap_or(0),
            timestamp_text: v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            transfer_id: v
                .get("transferId")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            direction: v
                .get("direction")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            peer_name: v
                .get("peer")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            peer: v
                .get("peer")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            filename: v
                .get("filename")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            size: v.get("bytes").and_then(|t| t.as_u64()).unwrap_or(0),
            path: v
                .get("path")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            save_path: v
                .get("path")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            bytes: v.get("bytes").and_then(|t| t.as_u64()).unwrap_or(0),
            status: v
                .get("status")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            detail: v
                .get("detail")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
        })
        .collect();
    rows.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    rows
}

/// Build the real local-device info from config + live discovery. Hostname and
/// OS come from the discoverer's `local_device`; no hardcoded "MacBook Pro".
fn local_info(backend: &Backend) -> LocalInfoDto {
    let local = backend.local_device();
    let ip = local
        .addresses
        .first()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let info = LocalInfoDto {
        device_id: format!("{}", local.id.0),
        device_name: local.name.clone(),
        os: os_str(&local.os),
        os_version: os_display(&local.os),
        user: std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_default(),
        ip,
    };
    command_log(
        "local_info_detected",
        &[
            ("device_id", info.device_id.clone()),
            ("device_name", info.device_name.clone()),
            ("os", info.os_version.clone()),
            ("username", info.user.clone()),
            ("ip", info.ip.clone()),
        ],
    );
    info
}

/// Real AI-tool DTOs from on-disk config-dir scan.
fn ai_tool_dtos(backend: &Backend) -> Vec<AiToolDto> {
    backend
        .ai_tools()
        .into_iter()
        .map(|t| AiToolDto {
            name: t.name,
            config_dir: t.config_dir,
            session_count: t.session_count,
            installed: t.installed,
        })
        .collect()
}

/// Map a configured project to its display DTO. The first peer mapping (if any)
/// supplies the remote dir + peer name. Status is `Synced` for an enabled,
/// idle project — live sync status is pushed via `sync-progress` events.
fn project_dto_from_config(p: &aisync_sync::ProjectConfig) -> ProjectDto {
    let (peer_name, remote_dir) = p
        .peers
        .iter()
        .next()
        .map(|(name, dir)| (name.clone(), dir.to_string_lossy().into_owned()))
        .unwrap_or_default();
    ProjectDto {
        id: p.name.clone(),
        name: p.name.clone(),
        local_dir: p.local.to_string_lossy().into_owned(),
        remote_dir,
        remote_session_dir: String::new(),
        local_session_dir: String::new(),
        peer_id: peer_name.clone(),
        peer_name,
        mode: match p.sync_mode {
            aisync_sync::SyncModeConfig::OneWayPush => SyncModeDto::OneWayPush,
            aisync_sync::SyncModeConfig::OneWayPull => SyncModeDto::OneWayPull,
            aisync_sync::SyncModeConfig::TwoWayAuto => SyncModeDto::TwoWayAuto,
        },
        target_tool: "Claude Code".to_string(),
        status: if p.enabled {
            ProjectSyncStatus::Synced
        } else {
            ProjectSyncStatus::Disabled
        },
        progress: None,
        last_sync: None,
        exclude_rules: p.exclude_rules.clone(),
        enabled: p.enabled,
        history: Vec::new(),
    }
}

fn workspace_dto_from_config(w: &aisync_sync::WorkspaceConfig, backend: &Backend) -> WorkspaceDto {
    let peer_name = w.effective_peer().unwrap_or_default().to_string();
    let remote_root = w.effective_remote_root(&peer_name).unwrap_or_default();
    let local_root = w.effective_local_root().to_path_buf();
    let mut children: Vec<WorkspaceChildDto> = w
        .children
        .iter()
        .map(|child| WorkspaceChildDto {
            name: child.name.clone(),
            local_dir: child.local_dir.to_string_lossy().into_owned(),
            remote_dir: child.remote_dir.to_string_lossy().into_owned(),
            status: if child.conflicted {
                ProjectSyncStatus::Conflict
            } else if child.enabled {
                ProjectSyncStatus::Synced
            } else {
                ProjectSyncStatus::Disabled
            },
            progress: None,
            peer_name: Some(peer_name.clone()),
            newly_discovered: false,
            discovered_at: None,
        })
        .collect();

    let known: std::collections::HashSet<String> =
        children.iter().map(|child| child.name.clone()).collect();
    if let Ok(entries) = std::fs::read_dir(&local_root) {
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || known.contains(&name) {
                continue;
            }
            children.push(WorkspaceChildDto {
                name: name.clone(),
                local_dir: entry.path().to_string_lossy().into_owned(),
                remote_dir: remote_root.join(&name).to_string_lossy().into_owned(),
                status: if w.auto_enable_new {
                    ProjectSyncStatus::Synced
                } else {
                    ProjectSyncStatus::Disabled
                },
                progress: None,
                peer_name: Some(peer_name.clone()),
                newly_discovered: !w.auto_enable_new,
                discovered_at: None,
            });
        }
    }
    children.sort_by(|left, right| left.name.cmp(&right.name));

    WorkspaceDto {
        id: w.name.clone(),
        name: w.name.clone(),
        local_root: local_root.to_string_lossy().into_owned(),
        remote_root: remote_root.to_string_lossy().into_owned(),
        peer_name,
        default_mode: match w.sync_mode {
            aisync_sync::SyncModeConfig::OneWayPush => SyncModeDto::OneWayPush,
            aisync_sync::SyncModeConfig::OneWayPull => SyncModeDto::OneWayPull,
            aisync_sync::SyncModeConfig::TwoWayAuto => SyncModeDto::TwoWayAuto,
        },
        children,
        history: history_entries(backend, &w.name),
    }
}

#[tauri::command]
pub fn get_peers(backend: State<Backend>) -> Vec<PeerDto> {
    build_peers(&backend)
}

fn build_peers(backend: &Backend) -> Vec<PeerDto> {
    let mut peers = Vec::new();
    let local = backend.local_device();
    peers.push(PeerDto {
        id: "self".to_string(),
        name: local.name.clone(),
        os: os_str(&local.os),
        ip: local
            .addresses
            .first()
            .map(|a| a.to_string())
            .unwrap_or_default(),
        status: PeerStatus::Online,
        kind: PeerKind::Local,
        paired_at: None,
    });
    for (device, online) in backend.paired_peers() {
        let ip = device
            .addresses
            .first()
            .map(|a| a.to_string())
            .unwrap_or_default();
        command_log(
            "sidebar_peer_state_updated",
            &[
                ("peer_id", device.id.0.to_string()),
                ("peer_name", device.name.clone()),
                ("ip", ip.clone()),
                ("online", online.to_string()),
            ],
        );
        peers.push(PeerDto {
            id: format!("{}", device.id.0),
            name: device.name.clone(),
            os: os_str(&device.os),
            ip,
            status: if online {
                PeerStatus::Online
            } else {
                PeerStatus::Offline
            },
            kind: PeerKind::Paired,
            paired_at: Some(String::new()),
        });
    }
    for device in backend.discovered_peers() {
        peers.push(PeerDto {
            id: format!("{}", device.id.0),
            name: device.name.clone(),
            os: os_str(&device.os),
            ip: device
                .addresses
                .first()
                .map(|a| a.to_string())
                .unwrap_or_default(),
            status: PeerStatus::Online,
            kind: PeerKind::Discovered,
            paired_at: None,
        });
    }
    // First run shows only the local machine — no fabricated peers.
    peers
}

fn os_str(os: &aisync_core::OsType) -> String {
    match os {
        aisync_core::OsType::Darwin => "darwin".into(),
        aisync_core::OsType::Windows => "windows".into(),
        aisync_core::OsType::Linux => "linux".into(),
        aisync_core::OsType::Other(s) => s.clone(),
    }
}

fn os_display(os: &aisync_core::OsType) -> String {
    match os {
        // Include the real macOS version (e.g. "macOS 15.5") like the prototype.
        aisync_core::OsType::Darwin => match macos_product_version() {
            Some(v) => format!("macOS {v}"),
            None => "macOS".into(),
        },
        aisync_core::OsType::Windows => "Windows".into(),
        aisync_core::OsType::Linux => "Linux".into(),
        aisync_core::OsType::Other(s) if !s.is_empty() => s.clone(),
        aisync_core::OsType::Other(_) => std::env::consts::OS.to_string(),
    }
}

/// Read the macOS product version from SystemVersion.plist — no subprocess, so
/// it works in the sandboxed release build (unlike shelling out to `sw_vers`).
#[cfg(target_os = "macos")]
fn macos_product_version() -> Option<String> {
    let text = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    // Find <key>ProductVersion</key> then the next <string>X.Y[.Z]</string>.
    let idx = text.find("ProductVersion")?;
    let after = &text[idx..];
    let s = after.find("<string>")? + "<string>".len();
    let e = after[s..].find("</string>")?;
    let v = after[s..s + e].trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_product_version() -> Option<String> {
    None
}

/// Epoch milliseconds as a string — the UI formats it into a local date/time
/// with JS `Date`, so we don't pull in a date crate here.
fn epoch_millis() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_default()
}

fn count_files(root: &std::path::Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total += count_files(&path);
        } else if metadata.is_file() {
            total += 1;
        }
    }
    total
}

fn workspace_history_scope(
    config: &aisync_sync::SyncConfig,
    project_id: &str,
) -> (
    Option<String>,
    Option<String>,
    Vec<(String, std::path::PathBuf)>,
) {
    for workspace in &config.workspaces {
        if workspace.name == project_id {
            let children = workspace
                .children
                .iter()
                .filter(|child| child.enabled && !child.conflicted)
                .map(|child| (child.name.clone(), child.local_dir.clone()))
                .collect();
            return (Some(workspace.name.clone()), None, children);
        }
        if let Some(child) = workspace
            .children
            .iter()
            .find(|child| child.name == project_id)
        {
            return (
                Some(workspace.name.clone()),
                Some(child.name.clone()),
                vec![(child.name.clone(), child.local_dir.clone())],
            );
        }
    }
    (None, None, Vec::new())
}

fn command_log(event: &str, fields: &[(&str, String)]) {
    let mut line = format!("[aisync-app] event={event}");
    for (key, value) in fields {
        let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"<encode-error>\"".into());
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&encoded);
    }
    // Tee to ~/.aisync/logs/aisync.log so it's visible under `open -a` too.
    crate::backend::log_line(&line);
}

#[tauri::command]
pub fn get_peer_detail(
    backend: State<Backend>,
    peer_id: String,
) -> std::result::Result<
    (
        PeerDto,
        Vec<ProjectDto>,
        Vec<SyncHistoryEntry>,
        Vec<WorkspaceDto>,
    ),
    String,
> {
    let peer = build_peers(&backend)
        .into_iter()
        .find(|p| p.id == peer_id)
        .ok_or("peer not found")?;
    command_log(
        "peer_detail_state_updated",
        &[
            ("peer_id", peer.id.clone()),
            ("peer_name", peer.name.clone()),
            ("ip", peer.ip.clone()),
            ("status", format!("{:?}", peer.status)),
        ],
    );
    // Projects whose first peer mapping targets this peer (by name).
    let config = backend.config_with_refreshed_workspaces();
    let projects: Vec<ProjectDto> = config
        .projects
        .iter()
        .filter(|p| {
            p.peers
                .keys()
                .next()
                .map(|name| *name == peer.name)
                .unwrap_or(false)
        })
        .map(|p| {
            let mut dto = project_dto_from_config(p);
            dto.history = history_entries(&backend, &p.name);
            dto.last_sync = dto.history.first().map(|h| h.timestamp.clone());
            dto
        })
        .collect();
    let workspaces: Vec<WorkspaceDto> = config
        .workspaces
        .iter()
        .filter(|workspace| workspace.effective_peer() == Some(peer.name.as_str()))
        .map(|workspace| workspace_dto_from_config(workspace, &backend))
        .collect();
    // Real, persisted history for all of this peer's projects (newest first).
    let mut history: Vec<SyncHistoryEntry> =
        projects.iter().flat_map(|p| p.history.clone()).collect();
    history.extend(workspaces.iter().flat_map(|w| w.history.clone()));
    history.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok((peer, projects, history, workspaces))
}

#[tauri::command]
pub fn get_settings(state: State<AppState>, backend: State<Backend>) -> SettingsDto {
    let mut settings = state.settings();
    // Real device identity + AI-tool scan override the seeded defaults.
    let local = backend.local_device();
    settings.device_id = format!("{}", local.id.0);
    settings.device_name = local.name;
    let config = backend.config();
    settings.port = config.receive_port;
    settings.refresh_interval_secs = config.refresh_interval_secs;
    settings.tools = ai_tool_dtos(&backend);
    settings
}

#[tauri::command]
pub fn save_settings(state: State<AppState>, backend: State<Backend>, settings: SettingsDto) {
    let _ = backend.set_device_name(&settings.device_name);
    let _ = backend.set_refresh_interval_secs(settings.refresh_interval_secs);
    state.save_settings(settings);
}

#[tauri::command]
pub fn get_status_bar(backend: State<Backend>) -> StatusBarDto {
    // Real status: primary peer = first paired peer (online state from
    // discovery); no syncing/conflict state until a sync emits events.
    let paired = backend.paired_peers();
    let primary = paired.first();
    StatusBarDto {
        primary_peer: primary.map(|(d, _)| d.name.clone()),
        primary_peer_online: primary.map(|(_, online)| *online).unwrap_or(false),
        status: GlobalStatus::Idle,
        syncing_project: None,
        syncing_percent: None,
        conflict_project: None,
        last_sync: None,
        auto_sync_paused: backend.auto_sync_paused(),
    }
}

#[tauri::command]
pub fn is_onboarded(backend: State<Backend>) -> bool {
    backend.is_onboarded()
}

#[tauri::command]
pub fn get_local_info(backend: State<Backend>) -> LocalInfoDto {
    local_info(&backend)
}

#[tauri::command]
pub fn complete_onboarding(
    state: State<AppState>,
    backend: State<Backend>,
    device_name: String,
) -> std::result::Result<(), String> {
    backend
        .complete_onboarding(&device_name)
        .map_err(|error| error.to_string())?;
    state.set_onboarded(&device_name);
    Ok(())
}

// ── Frontend log bridge ──────────────────────────────────────────────

/// Let the React layer write into `~/.aisync/logs/aisync.log`.
///
/// The webview's `console.log` is invisible to qa in a release DMG (no
/// devtools, stderr → /dev/null). Routing UI breadcrumbs through this command
/// makes the *frontend* side of the pairing chain (was beginPairing invoked?
/// did it resolve/throw?) visible in the same file as the backend logs — which
/// is how we prove whether the IPC ever reached Rust.
#[tauri::command]
pub fn log_event(message: String) {
    crate::backend::log_line(&format!("[ui] {message}"));
}

/// Reveal a path in the OS file manager (Finder on macOS).
///
/// Used by P3 settings (修改/打开 a config dir / log dir) and P1 AI-tool 查看.
/// `~` is expanded to $HOME. Returns an error string the UI surfaces as a toast.
#[tauri::command]
pub fn open_path(path: String) -> std::result::Result<(), String> {
    crate::backend::log_line(&format!("[ui] open_path requested path={path}"));
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => std::path::Path::new(&home)
                .join(rest)
                .to_string_lossy()
                .into_owned(),
            None => path.clone(),
        }
    } else {
        path.clone()
    };
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&expanded).status();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer")
        .arg(&expanded)
        .status();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open")
        .arg(&expanded)
        .status();

    match result {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("file manager exited with {s}")),
        Err(e) => Err(format!("failed to open '{expanded}': {e}")),
    }
}

// ── Pairing (D4) — real MdnsDiscoverer ───────────────────────────────

#[tauri::command]
pub fn begin_pairing(
    backend: State<Backend>,
    peer_id: String,
) -> std::result::Result<PairingDto, String> {
    crate::backend::log_line(&format!("[pair] pair_started peer_id={peer_id}"));
    // Real pairing only — the peer id must be a discovered/known device UUID.
    let uuid = peer_id.parse::<uuid::Uuid>().map_err(|_| {
        crate::backend::log_line(&format!(
            "[pair] pair_failed peer_id={peer_id} reason=invalid_peer_id (not a UUID)"
        ));
        "invalid peer id".to_string()
    })?;
    let pairing = backend
        .pairing_code(&aisync_core::DeviceId(uuid))
        .map_err(|e| {
            crate::backend::log_line(&format!("[pair] pair_failed peer_id={peer_id} reason={e}"));
            e.to_string()
        })?;
    let peer_ip = pairing
        .peer
        .addresses
        .first()
        .map(|a| a.to_string())
        .unwrap_or_default();
    crate::backend::log_line(&format!(
        "[pair] pair_code_ready peer_id={peer_id} peer_name={} peer_ip={peer_ip} code={} request_id={} expires_at={}",
        pairing.peer.name,
        pairing.code,
        pairing.request_id,
        pairing.expires_at_unix_secs
    ));
    Ok(PairingDto {
        peer_id,
        peer_name: pairing.peer.name,
        peer_ip,
        peer_os: os_str(&pairing.peer.os),
        code: pairing.code,
        request_id: pairing.request_id,
        expires_at_unix_secs: pairing.expires_at_unix_secs,
    })
}

#[tauri::command]
pub fn pending_pairing_request(backend: State<Backend>) -> Option<PairingDto> {
    let (peer, code, request_id, expires_at_unix_secs) = backend.take_pending_pairing_request()?;
    let peer_id = format!("{}", peer.id.0);
    let peer_ip = peer
        .addresses
        .first()
        .map(|a| a.to_string())
        .unwrap_or_default();
    crate::backend::log_line(&format!(
        "[pair] pending_pairing_request_returned peer_id={peer_id} peer_name={} peer_ip={peer_ip} request_id={request_id} expires_at={expires_at_unix_secs}",
        peer.name
    ));
    Some(PairingDto {
        peer_id,
        peer_name: peer.name,
        peer_ip,
        peer_os: os_str(&peer.os),
        code,
        request_id,
        expires_at_unix_secs,
    })
}

#[tauri::command]
pub fn pending_project_mapping_request(
    backend: State<Backend>,
) -> Option<ProjectMappingRequestDto> {
    let request = backend.take_pending_project_mapping_request()?;
    crate::backend::log_line(&format!(
        "[project] pending_project_mapping_request_returned request_id={} project={} peer={} source_dir={}",
        request.request_id,
        request.project_name,
        request.device.name,
        request.source_dir.display()
    ));
    Some(ProjectMappingRequestDto {
        request_id: request.request_id,
        project_name: request.project_name,
        peer_name: request.device.name,
        source_dir: request.source_dir.to_string_lossy().into_owned(),
        mode: request.mode,
    })
}

#[tauri::command]
pub fn confirm_project_mapping_request(
    state: State<AppState>,
    backend: State<Backend>,
    app: AppHandle,
    request_id: String,
    local_dir: String,
) -> std::result::Result<(), String> {
    crate::backend::log_line(&format!(
        "[project] project_mapping_confirm_started request_id={request_id} local_dir={local_dir}"
    ));
    backend
        .confirm_project_mapping_request(&request_id, local_dir.into())
        .map_err(|e| {
            crate::backend::log_line(&format!(
                "[project] project_mapping_confirm_failed request_id={request_id} reason={e}"
            ));
            e.to_string()
        })?;
    let _ = tray::refresh(&app, &state, &backend);
    Ok(())
}

#[tauri::command]
pub fn poll_project_mapping_acks(
    state: State<AppState>,
    backend: State<Backend>,
    app: AppHandle,
) -> std::result::Result<usize, String> {
    let count = backend
        .process_project_mapping_acks()
        .map_err(|e| e.to_string())?;
    if count > 0 {
        let _ = tray::refresh(&app, &state, &backend);
    }
    Ok(count)
}

#[tauri::command]
pub fn pending_text_message(backend: State<Backend>) -> Option<TextMessageDto> {
    let message = backend.take_pending_text_message()?;
    Some(TextMessageDto {
        sender_name: message.sender_name,
        content: message.content,
        timestamp: message.timestamp,
        peer_name: None,
        mine: Some(false),
    })
}

#[tauri::command]
pub fn text_messages(backend: State<Backend>, peer_name: Option<String>) -> Vec<TextMessageDto> {
    text_message_entries(&backend, peer_name.as_deref())
}

#[tauri::command]
pub fn send_text_message(
    backend: State<Backend>,
    peer_name: String,
    content: String,
) -> std::result::Result<(), String> {
    backend
        .send_text_message(&peer_name, content)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn request_file_transfer(
    backend: State<Backend>,
    peer_name: String,
    path: String,
    confirmed_sensitive: Option<Vec<String>>,
) -> std::result::Result<String, String> {
    let confirmed = confirmed_sensitive.unwrap_or_default();
    backend
        .request_file_transfer(&peer_name, path.into(), &confirmed)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pick_files_for_transfer(
    app: AppHandle,
    backend: State<'_, Backend>,
    peer_name: String,
    confirmed_sensitive: Option<Vec<String>>,
) -> std::result::Result<Vec<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    crate::backend::log_line("[file] file_picker_opened");
    let paths: Vec<std::path::PathBuf> = app
        .dialog()
        .file()
        .blocking_pick_files()
        .unwrap_or_default()
        .into_iter()
        .map(|path| path.into_path().map_err(|e| e.to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if paths.is_empty() {
        crate::backend::log_line("[file] file_picker_cancelled");
        return Ok(Vec::new());
    }
    crate::backend::log_line(&format!(
        "[file] file_picker_selected count={}",
        paths.len()
    ));
    let confirmed = confirmed_sensitive.unwrap_or_default();
    let mut transfer_ids = Vec::with_capacity(paths.len());
    for path in paths {
        crate::backend::log_line(&format!(
            "[file] file_picker_send_start peer={} path={}",
            peer_name,
            path.display()
        ));
        match backend.request_file_transfer(&peer_name, path.clone(), &confirmed) {
            Ok(id) => transfer_ids.push(id),
            Err(error) => {
                crate::backend::log_line(&format!(
                    "[file] file_picker_send_failed peer={} path={} error={}",
                    peer_name,
                    path.display(),
                    error
                ));
                return Err(error.to_string());
            }
        }
    }
    crate::backend::log_line(&format!(
        "[file] file_picker_send_queued count={} peer={}",
        transfer_ids.len(),
        peer_name
    ));
    Ok(transfer_ids)
}

#[tauri::command]
pub fn paste_files_for_transfer(
    backend: State<Backend>,
    peer_name: String,
    confirmed_sensitive: Option<Vec<String>>,
) -> std::result::Result<Vec<String>, String> {
    let paths = clipboard_file_paths()?;
    if paths.is_empty() {
        crate::backend::log_line("[file] paste_files_empty");
        return Ok(Vec::new());
    }
    crate::backend::log_line(&format!(
        "[file] paste_files_selected count={}",
        paths.len()
    ));
    let confirmed = confirmed_sensitive.unwrap_or_default();
    let mut transfer_ids = Vec::with_capacity(paths.len());
    for path in paths {
        transfer_ids.push(
            backend
                .request_file_transfer(&peer_name, path, &confirmed)
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(transfer_ids)
}

#[cfg(target_os = "macos")]
fn clipboard_file_paths() -> std::result::Result<Vec<std::path::PathBuf>, String> {
    let script = r#"
ObjC.import("AppKit");
ObjC.import("Foundation");
const pb = $.NSPasteboard.generalPasteboard;
const classes = $.NSArray.arrayWithObject($.NSURL.class);
const opts = $.NSDictionary.dictionaryWithObjectForKey(
  $.NSNumber.numberWithBool(true),
  $.NSPasteboardURLReadingFileURLsOnlyKey
);
const urls = pb.readObjectsForClassesOptions(classes, opts);
const out = [];
if (urls) {
  for (let i = 0; i < urls.count; i++) {
    const path = ObjC.unwrap(urls.objectAtIndex(i).path);
    if (path) out.push(path);
  }
}
out.join("\n");
"#;
    let output = Command::new("osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("read clipboard files: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("read clipboard files failed: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(std::path::PathBuf::from)
        .collect())
}

#[cfg(not(target_os = "macos"))]
fn clipboard_file_paths() -> std::result::Result<Vec<std::path::PathBuf>, String> {
    Ok(Vec::new())
}

#[tauri::command]
pub fn pending_file_transfer_request(backend: State<Backend>) -> Option<FileTransferRequestDto> {
    let request = backend.take_pending_file_transfer_request()?;
    let suggested_path = backend.suggested_file_receive_path(&request.filename);
    let id = request.transfer_id.clone();
    Some(FileTransferRequestDto {
        id,
        transfer_id: request.transfer_id,
        filename: request.filename,
        size: request.size,
        sender_name: request.sender_name,
        suggested_path: suggested_path.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
pub fn pending_file_transfers(backend: State<Backend>) -> Vec<FileTransferRequestDto> {
    backend
        .pending_file_transfers()
        .into_iter()
        .map(|request| {
            let suggested_path = backend.suggested_file_receive_path(&request.filename);
            let id = request.transfer_id.clone();
            FileTransferRequestDto {
                id,
                transfer_id: request.transfer_id,
                filename: request.filename,
                size: request.size,
                sender_name: request.sender_name,
                suggested_path: suggested_path.to_string_lossy().into_owned(),
            }
        })
        .collect()
}

#[tauri::command]
pub fn confirm_file_transfer_request(
    backend: State<Backend>,
    transfer_id: String,
    target_path: String,
) -> std::result::Result<(), String> {
    backend
        .confirm_file_transfer_request(&transfer_id, target_path.into())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn accept_file_transfer(
    backend: State<Backend>,
    id: String,
    save_dir: String,
) -> std::result::Result<(), String> {
    backend
        .accept_file_transfer(&id, save_dir.into())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn poll_file_transfer_acks(backend: State<Backend>) -> std::result::Result<usize, String> {
    backend
        .process_file_transfer_acks()
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_default_file_receive_dir(backend: State<Backend>) -> String {
    backend
        .default_file_receive_dir()
        .to_string_lossy()
        .into_owned()
}

#[tauri::command]
pub fn get_default_receive_dir(backend: State<Backend>) -> String {
    get_default_file_receive_dir(backend)
}

#[tauri::command]
pub fn set_default_file_receive_dir(
    backend: State<Backend>,
    path: String,
) -> std::result::Result<(), String> {
    backend
        .set_default_file_receive_dir(path.into())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_default_receive_dir(
    backend: State<Backend>,
    path: String,
) -> std::result::Result<(), String> {
    set_default_file_receive_dir(backend, path)
}

#[tauri::command]
pub fn file_transfer_history(
    backend: State<Backend>,
    peer_name: Option<String>,
) -> Vec<FileTransferHistoryDto> {
    file_transfer_history_entries(&backend, peer_name.as_deref())
}

#[tauri::command]
pub fn pending_workspace_mapping_request(
    backend: State<Backend>,
) -> Option<WorkspaceMappingRequestDto> {
    let request = backend.take_pending_workspace_mapping_request()?;
    crate::backend::log_line(&format!(
        "[workspace] pending_workspace_mapping_request_returned request_id={} workspace={} peer={} source_root={} suggested_remote_root={}",
        request.request_id,
        request.workspace_name,
        request.device.name,
        request.source_root.display(),
        request.suggested_remote_root.display()
    ));
    Some(WorkspaceMappingRequestDto {
        request_id: request.request_id,
        workspace_name: request.workspace_name,
        peer_name: request.device.name,
        source_root: request.source_root.to_string_lossy().into_owned(),
        suggested_remote_root: request.suggested_remote_root.to_string_lossy().into_owned(),
        mode: request.mode,
        children: request.children,
    })
}

#[tauri::command]
pub fn confirm_workspace_mapping_request(
    state: State<AppState>,
    backend: State<Backend>,
    app: AppHandle,
    request_id: String,
    local_root: String,
) -> std::result::Result<(), String> {
    crate::backend::log_line(&format!(
        "[workspace] workspace_mapping_confirm_started request_id={request_id} local_root={local_root}"
    ));
    backend
        .confirm_workspace_mapping_request(&request_id, local_root.into())
        .map_err(|e| {
            crate::backend::log_line(&format!(
                "[workspace] workspace_mapping_confirm_failed request_id={request_id} reason={e}"
            ));
            e.to_string()
        })?;
    let _ = tray::refresh(&app, &state, &backend);
    Ok(())
}

#[tauri::command]
pub fn poll_workspace_mapping_acks(
    state: State<AppState>,
    backend: State<Backend>,
    app: AppHandle,
) -> std::result::Result<usize, String> {
    let count = backend
        .process_workspace_mapping_acks()
        .map_err(|e| e.to_string())?;
    if count > 0 {
        let _ = tray::refresh(&app, &state, &backend);
    }
    Ok(count)
}

#[tauri::command]
pub fn confirm_pairing(
    state: State<AppState>,
    backend: State<Backend>,
    app: AppHandle,
    peer_id: String,
) -> std::result::Result<(), String> {
    crate::backend::log_line(&format!("[pair] pair_confirm_started peer_id={peer_id}"));
    let uuid = peer_id.parse::<uuid::Uuid>().map_err(|_| {
        crate::backend::log_line(&format!(
            "[pair] pair_confirm_failed peer_id={peer_id} reason=invalid_peer_id"
        ));
        "invalid peer id".to_string()
    })?;
    backend
        .confirm_pairing(&aisync_core::DeviceId(uuid))
        .map_err(|e| {
            crate::backend::log_line(&format!(
                "[pair] pair_confirm_failed peer_id={peer_id} reason={e}"
            ));
            e.to_string()
        })?;
    crate::backend::log_line(&format!(
        "[pair] pair_confirmed peer_id={peer_id} (persisted to config [peers])"
    ));
    let _ = tray::refresh(&app, &state, &backend);
    Ok(())
}

#[tauri::command]
pub fn cancel_pairing(_peer_id: String) {}

#[tauri::command]
pub fn unpair(state: State<AppState>, backend: State<Backend>, app: AppHandle, peer_id: String) {
    if let Ok(uuid) = peer_id.parse::<uuid::Uuid>() {
        let _ = backend.unpair(&aisync_core::DeviceId(uuid));
    }
    let _ = tray::refresh(&app, &state, &backend);
}

// ── Workspace scan (D2) — real SyncCoordinator::scan_workspace ───────

#[tauri::command]
pub fn scan_workspace(
    backend: State<Backend>,
    local_root: String,
    remote_root: String,
) -> Vec<ScannedChildDto> {
    // Scan the actual chosen local directory (the "添加工作区" dialog has no
    // configured workspace yet). remote_root only drives matched_remote hints.
    command_log(
        "workspace_scan_started",
        &[
            ("root", local_root.clone()),
            ("remote_root", remote_root.clone()),
        ],
    );
    match backend.scan_workspace_path(
        std::path::Path::new(&local_root),
        std::path::Path::new(&remote_root),
    ) {
        Ok(found) => found
            .into_iter()
            .map(|d| ScannedChildDto {
                local_name: d.name.clone(),
                remote_name: d
                    .remote_code_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or(d.name),
                matched_remote: d.matched_remote,
                selected: d.enabled,
            })
            .collect(),
        Err(e) => {
            crate::backend::log_line(&format!("[ui] scan_workspace failed error={e}"));
            Vec::new()
        }
    }
}

// ── Batch sync (D6) — real sensitive-file scan (G6) ──────────────────

#[tauri::command]
pub fn get_batch_plan(
    backend: State<Backend>,
    peer_id: String,
    direction: String,
) -> std::result::Result<BatchPlanDto, String> {
    let config = backend.config();
    let peer = build_peers(&backend)
        .into_iter()
        .find(|p| p.id == peer_id)
        .ok_or("peer not found")?;

    // Report this peer's configured projects and run the real sensitive-file
    // scan over each project's local dir. No fabricated items.
    let mut items = Vec::new();
    let mut sensitive_files = Vec::new();
    for project in config.projects.iter().filter(|p| {
        p.peers
            .keys()
            .next()
            .map(|name| *name == peer.name)
            .unwrap_or(false)
    }) {
        items.push(BatchItemDto {
            project_id: project.name.clone(),
            name: project.name.clone(),
            changed_files: 0,
            bytes: 0,
            up_to_date: false,
        });
        if let Ok(found) = backend.sensitive_files(&project.name) {
            for f in found {
                sensitive_files.push(format!("{}/{}", project.name, f.relative_path));
            }
        }
    }

    Ok(BatchPlanDto {
        peer_name: peer.name,
        direction,
        items,
        sensitive_files,
    })
}

// ── Conflict (D5) ────────────────────────────────────────────────────

#[tauri::command]
pub fn get_conflict(
    backend: State<Backend>,
    project_id: String,
) -> std::result::Result<ConflictDto, String> {
    let config = backend.config();
    let project = config
        .projects
        .iter()
        .find(|p| p.name == project_id)
        .ok_or("project not found")?;
    let peer_name = project.peers.keys().next().cloned().unwrap_or_default();
    Ok(ConflictDto {
        project_id: project.name.clone(),
        project_name: project.name.clone(),
        local: ConflictSideDto {
            device_name: "本机".to_string(),
            changed_files: 0,
            files: Vec::new(),
            session_summary: "—".to_string(),
        },
        remote: ConflictSideDto {
            device_name: peer_name,
            changed_files: 0,
            files: Vec::new(),
            session_summary: "—".to_string(),
        },
    })
}

#[tauri::command]
pub fn resolve_conflict(_project_id: String, _resolution: String) {}

// ── Path-rewrite report (D10 / G7) ───────────────────────────────────

#[tauri::command]
pub fn get_rewrite_report(
    backend: State<Backend>,
    project_id: String,
) -> std::result::Result<RewriteReportDto, String> {
    let config = backend.config();
    let project = config
        .projects
        .iter()
        .find(|p| p.name == project_id)
        .ok_or("project not found")?;
    Ok(RewriteReportDto {
        project_id: project.name.clone(),
        project_name: project.name.clone(),
        timestamp: String::new(),
        direction: String::new(),
        rewritten: Vec::new(),
        skipped: Vec::new(),
    })
}

// ── Auto-sync pause (X5) ─────────────────────────────────────────────

#[tauri::command]
pub fn set_auto_sync_paused(
    state: State<AppState>,
    backend: State<Backend>,
    app: AppHandle,
    paused: bool,
) {
    backend.set_auto_sync_paused(paused);
    let _ = tray::refresh(&app, &state, &backend);
}

#[tauri::command]
pub fn get_auto_sync_paused(backend: State<Backend>) -> bool {
    backend.auto_sync_paused()
}

// ── Sync (push / pull) — real SyncCoordinator on a worker thread ─────

#[tauri::command]
pub fn start_sync(
    app: AppHandle,
    project_id: String,
    direction: String,
    confirmed_sensitive: Option<Vec<String>>,
) -> std::result::Result<(), String> {
    let dir = if direction == "pull" {
        Direction::RemoteToLocal
    } else {
        Direction::LocalToRemote
    };
    let confirmed = confirmed_sensitive.unwrap_or_default();

    // Resolve a peer name from config for this project, if any.
    let config = app.state::<Backend>().config();
    let peer_name = config
        .projects
        .iter()
        .find(|p| p.name == project_id)
        .and_then(|p| p.peers.keys().next().cloned())
        .or_else(|| {
            config
                .workspaces
                .iter()
                .find(|workspace| {
                    workspace.name == project_id
                        || workspace
                            .children
                            .iter()
                            .any(|child| child.name == project_id)
                })
                .and_then(|workspace| workspace.effective_peer().map(str::to_string))
        });

    let display = (project_id.clone(), peer_name.clone().unwrap_or_default());

    spawn_sync(
        app, project_id, peer_name, dir, confirmed, display.0, display.1,
    );
    Ok(())
}

/// Run the real sync on a worker thread, emitting progress derived from the
/// coordinator's [`SyncReport`] stages and a final result frame.
fn spawn_sync(
    app: AppHandle,
    project_id: String,
    peer_name: Option<String>,
    direction: Direction,
    confirmed_sensitive: Vec<String>,
    project_name: String,
    peer_display: String,
) {
    let dir_label = if matches!(direction, Direction::RemoteToLocal) {
        format!("{} ← {}", project_name, peer_display)
    } else {
        format!("{} → {}", project_name, peer_display)
    };

    thread::spawn(move || {
        let backend = app.state::<Backend>();
        let config = backend.config();
        let (log_workspace_name, log_child_name, _) = workspace_history_scope(&config, &project_id);
        let remote_dir = config
            .projects
            .iter()
            .find(|p| p.name == project_id)
            .and_then(|p| {
                peer_name
                    .as_ref()
                    .and_then(|peer| p.peers.get(peer).cloned())
            })
            .or_else(|| {
                config
                    .workspaces
                    .iter()
                    .find(|workspace| {
                        workspace.name == project_id
                            || workspace
                                .children
                                .iter()
                                .any(|child| child.name == project_id)
                    })
                    .and_then(|workspace| {
                        peer_name
                            .as_ref()
                            .and_then(|peer| workspace.effective_remote_root(peer))
                    })
            })
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let dir_str = if matches!(direction, Direction::RemoteToLocal) {
            "pull"
        } else {
            "push"
        };
        command_log(
            "sync_started",
            &[
                ("project", project_id.clone()),
                ("workspace", log_workspace_name.clone().unwrap_or_default()),
                ("child", log_child_name.clone().unwrap_or_default()),
                ("peer", peer_display.clone()),
                ("direction", dir_str.to_string()),
                ("remote_dir", remote_dir.clone()),
                ("file_count", "0".to_string()),
                ("bytes", "0".to_string()),
            ],
        );
        let report = peer_name
            .as_deref()
            .ok_or_else(|| "project has no configured peer".to_string())
            .and_then(|peer| {
                if backend
                    .config()
                    .projects
                    .iter()
                    .any(|project| project.name == project_id)
                {
                    backend
                        .run_sync(&project_id, peer, direction, &confirmed_sensitive)
                        .map_err(|e| e.to_string())
                } else {
                    let workspace_name = backend
                        .config()
                        .workspaces
                        .iter()
                        .find(|workspace| {
                            workspace.name == project_id
                                || workspace
                                    .children
                                    .iter()
                                    .any(|child| child.name == project_id)
                        })
                        .map(|workspace| workspace.name.clone())
                        .ok_or_else(|| format!("project or workspace '{project_id}' not found"))?;
                    backend
                        .run_workspace_sync(&workspace_name, direction)
                        .map_err(|e| e.to_string())
                }
            });

        match report {
            Ok(report) => {
                // Stream the real stages as progress frames.
                let total = report.code_files_transferred + report.session_files_transferred;
                command_log(
                    "sync_complete",
                    &[
                        ("project", project_id.clone()),
                        ("workspace", log_workspace_name.clone().unwrap_or_default()),
                        ("child", log_child_name.clone().unwrap_or_default()),
                        ("peer", peer_display.clone()),
                        ("direction", dir_str.to_string()),
                        ("remote_dir", remote_dir.clone()),
                        ("file_count", total.to_string()),
                        ("bytes", "0".to_string()),
                    ],
                );
                let stages: Vec<SyncStageDto> = report
                    .stages
                    .iter()
                    .map(|s| SyncStageDto {
                        name: s.name.to_string(),
                        percent: s.percent,
                        done: s.percent >= 100,
                        active: false,
                    })
                    .collect();
                for s in &report.stages {
                    let _ = app.emit(
                        "sync-progress",
                        &SyncProgressDto {
                            project_id: project_id.clone(),
                            project_name: project_name.clone(),
                            peer_name: peer_display.clone(),
                            direction: dir_label.clone(),
                            percent: s.percent,
                            phase: s.name.to_string(),
                            files_done: (total as u32) * s.percent as u32 / 100,
                            files_total: total as u32,
                            bytes_done: 0,
                            bytes_total: 0,
                            speed_bps: 0,
                            eta_secs: 0,
                            current_file: s.current_file.clone(),
                            stages: stages.clone(),
                            finished: false,
                            success: false,
                            error: None,
                        },
                    );
                }
                // Persist to sync history so P1/P2 show it after refresh.
                let timestamp = epoch_millis();
                let config = backend.config();
                let (workspace_name, child_name, child_entries) =
                    workspace_history_scope(&config, &project_id);
                backend.record_sync_scoped(
                    &project_id,
                    dir_str,
                    true,
                    total as u32,
                    0,
                    None,
                    timestamp.clone(),
                    workspace_name.as_deref(),
                    child_name.as_deref(),
                );
                if child_name.is_none() {
                    if let Some(workspace_name) = workspace_name.as_deref() {
                        command_log(
                            "workspace_history_created",
                            &[
                                ("workspace", workspace_name.to_string()),
                                ("peer", peer_display.clone()),
                                ("child_count", child_entries.len().to_string()),
                                ("files", total.to_string()),
                            ],
                        );
                        for (child, path) in child_entries {
                            let files = count_files(&path);
                            backend.record_sync_scoped(
                                &child,
                                dir_str,
                                true,
                                files,
                                0,
                                None,
                                timestamp.clone(),
                                Some(workspace_name),
                                Some(&child),
                            );
                            command_log(
                                "workspace_history_child_added",
                                &[
                                    ("workspace", workspace_name.to_string()),
                                    ("child", child),
                                    ("files", files.to_string()),
                                ],
                            );
                        }
                    }
                }
                let _ = app.emit(
                    "sync-result",
                    &SyncResultDto {
                        project_id,
                        project_name,
                        peer_name: peer_display,
                        direction: dir_str.into(),
                        success: true,
                        files: total as u32,
                        bytes: 0,
                        elapsed_secs: 0.0,
                        rewritten_paths: report.rewritten_sessions as u32,
                        skipped_paths: 0,
                        workspace_name,
                        child_name,
                        error: None,
                    },
                );
            }
            Err(err) => {
                command_log(
                    "sync_failed",
                    &[
                        ("project", project_id.clone()),
                        ("workspace", log_workspace_name.clone().unwrap_or_default()),
                        ("child", log_child_name.clone().unwrap_or_default()),
                        ("peer", peer_display.clone()),
                        ("direction", dir_str.to_string()),
                        ("remote_dir", remote_dir),
                        ("file_count", "0".to_string()),
                        ("bytes", "0".to_string()),
                        ("error", err.clone()),
                    ],
                );
                let config = backend.config();
                let (workspace_name, child_name, _) = workspace_history_scope(&config, &project_id);
                backend.record_sync_scoped(
                    &project_id,
                    dir_str,
                    false,
                    0,
                    0,
                    Some(err.clone()),
                    epoch_millis(),
                    workspace_name.as_deref(),
                    child_name.as_deref(),
                );
                let _ = app.emit(
                    "sync-result",
                    &SyncResultDto {
                        project_id,
                        project_name,
                        peer_name: peer_display,
                        direction: dir_str.into(),
                        success: false,
                        files: 0,
                        bytes: 0,
                        elapsed_secs: 0.0,
                        rewritten_paths: 0,
                        skipped_paths: 0,
                        workspace_name,
                        child_name,
                        error: Some(err),
                    },
                );
            }
        }
    });
}

#[tauri::command]
pub fn cancel_sync(_project_id: String) {}

// ── Project mutations (D1 / D3 / D7) ─────────────────────────────────

#[tauri::command]
pub fn add_project(
    backend: State<Backend>,
    project: serde_json::Value,
) -> std::result::Result<(), String> {
    let name = project
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let local = project
        .get("localDir")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let peer = project
        .get("peer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mode = crate::backend::sync_mode_from_label(
        project
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("twoWayAuto"),
    );
    let name = if name.is_empty() {
        std::path::Path::new(local)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string())
    } else {
        name
    };
    // The GUI sets createLocalDir=true only after the user confirmed the
    // "目录不存在，是否新建" prompt.
    let create_local_dir = project
        .get("createLocalDir")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    crate::backend::log_line(&format!(
        "[ui] add_project_backend name={name} peer={peer} create_local_dir={create_local_dir}"
    ));
    let request_id = backend
        .request_project_mapping(name, local.into(), peer, mode, create_local_dir)
        .map_err(|e| e.to_string())?;
    crate::backend::log_line(&format!(
        "[project] project_mapping_request_queued request_id={request_id}"
    ));
    Ok(())
}

#[tauri::command]
pub fn add_workspace(
    backend: State<Backend>,
    workspace: serde_json::Value,
) -> std::result::Result<(), String> {
    let name = workspace
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let local_root = workspace
        .get("localRoot")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let peer = workspace
        .get("peer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mode = crate::backend::sync_mode_from_label(
        workspace
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("twoWayAuto"),
    );
    let children = workspace
        .get("children")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let selected: Vec<String> = children
        .iter()
        .filter(|c| c.get("selected").and_then(|s| s.as_bool()).unwrap_or(false))
        .filter_map(|c| {
            c.get("localName")
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    let remote_root = workspace
        .get("remoteRoot")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&local_root)
        .to_string();
    let auto_enable = workspace
        .get("autoEnable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    command_log(
        "add_workspace_submit",
        &[
            ("workspace", name.clone()),
            ("local_root", local_root.clone()),
            ("remote_root", remote_root.clone()),
            ("peer", peer.clone()),
            ("selected_count", selected.len().to_string()),
            ("selected", format!("[{}]", selected.join(","))),
        ],
    );

    if peer.trim().is_empty() {
        return Err("请先选择目标设备".to_string());
    }
    let request_id = backend
        .add_workspace(
            name,
            local_root.clone().into(),
            peer.clone(),
            remote_root.into(),
            mode,
            auto_enable,
        )
        .map_err(|error| error.to_string())?;
    command_log(
        "workspace_request_queued",
        &[
            ("request_id", request_id),
            ("peer", peer),
            ("local_root", local_root),
        ],
    );
    Ok(())
}

#[tauri::command]
pub fn enable_child(
    _workspace_id: String,
    _child: String,
    _config: serde_json::Value,
) -> std::result::Result<(), String> {
    Ok(())
}

#[tauri::command]
pub fn set_project_mode(_project_id: String, _mode: String) {}

#[tauri::command]
pub fn save_exclude_rules(_project_id: String, _rules: Vec<String>) {}

#[tauri::command]
pub fn delete_project(_project_id: String) {}

// ── Serve daemon / peer endpoint / directory picker ──────────────────

/// Local receive-daemon coordinates (port + pinned cert + receive dir) so the
/// pairing UI can show "share these with the peer".
#[tauri::command]
pub fn get_serve_info(backend: State<Backend>) -> Option<ServeInfoDto> {
    backend.serve_info().map(|s| ServeInfoDto {
        port: s.port,
        cert_path: s.cert_path.to_string_lossy().into_owned(),
        receive_dir: s.receive_dir.to_string_lossy().into_owned(),
    })
}

/// Register a peer's push endpoint + pinned cert so the GUI can push to it.
#[tauri::command]
pub fn add_peer_endpoint(
    backend: State<Backend>,
    name: String,
    peer_id: Option<String>,
    endpoint: String,
    cert_path: Option<String>,
    server_name: Option<String>,
) -> std::result::Result<(), String> {
    let id = peer_id
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
        .map(aisync_core::DeviceId)
        .unwrap_or_else(aisync_core::DeviceId::new);
    let addr: std::net::SocketAddr = endpoint
        .parse()
        .map_err(|_| format!("invalid endpoint '{endpoint}' (expected ip:port)"))?;
    backend
        .add_peer_endpoint(name, id, addr, cert_path.map(Into::into), server_name)
        .map_err(|e| e.to_string())
}

/// Native directory picker (D1/D2 本机目录浏览). Returns the chosen absolute
/// path, or null if the user cancelled.
#[tauri::command]
pub async fn pick_directory(app: AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().pick_folder(move |path| {
        let _ = tx.send(path.map(|p| p.to_string()));
    });
    rx.recv().ok().flatten()
}
