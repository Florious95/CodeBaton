//! Data-transfer objects exchanged over Tauri IPC.
//!
//! These mirror the structures the React UI consumes (see docs/ui-design.md).
//! They are intentionally serialization-friendly and decoupled from the core
//! domain types so the wire format stays stable while the backend evolves.
//! Where a field has a direct counterpart in `aisync-core` / `aisync-sync`, the
//! conversion lives next to the mock state in `state.rs`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerStatus {
    Online,
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerKind {
    /// This machine.
    Local,
    /// A paired peer.
    Paired,
    /// Discovered on the LAN but not yet paired.
    Discovered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDto {
    pub id: String,
    pub name: String,
    pub os: String,
    pub ip: String,
    pub status: PeerStatus,
    pub kind: PeerKind,
    /// Present for paired peers.
    pub paired_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiToolDto {
    pub name: String,
    pub config_dir: String,
    pub session_count: u32,
    pub installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncModeDto {
    TwoWayAuto,
    OneWayPush,
    OneWayPull,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectSyncStatus {
    Synced,
    Syncing,
    Disabled,
    Conflict,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncHistoryEntry {
    pub timestamp: String,
    pub project_id: String,
    pub workspace_name: Option<String>,
    pub child_name: Option<String>,
    pub direction: String,
    pub success: bool,
    pub files: u32,
    pub bytes: u64,
    pub detail: Option<String>,
    pub trigger: Option<String>,
    pub role: Option<String>,
    pub file_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDto {
    pub id: String,
    pub name: String,
    pub local_dir: String,
    pub remote_dir: String,
    pub remote_session_dir: String,
    pub local_session_dir: String,
    pub peer_id: String,
    pub peer_name: String,
    pub mode: SyncModeDto,
    pub target_tool: String,
    pub status: ProjectSyncStatus,
    pub progress: Option<u8>,
    pub last_sync: Option<String>,
    pub exclude_rules: Vec<String>,
    pub enabled: bool,
    pub history: Vec<SyncHistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceChildDto {
    pub name: String,
    pub local_dir: String,
    pub remote_dir: String,
    pub status: ProjectSyncStatus,
    pub progress: Option<u8>,
    pub peer_name: Option<String>,
    /// New child directory discovered after the workspace was created.
    pub newly_discovered: bool,
    pub discovered_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDto {
    pub id: String,
    pub name: String,
    pub local_root: String,
    pub remote_root: String,
    pub peer_name: String,
    pub default_mode: SyncModeDto,
    pub children: Vec<WorkspaceChildDto>,
    pub history: Vec<SyncHistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalInfoDto {
    pub device_id: String,
    pub device_name: String,
    pub os: String,
    pub os_version: String,
    pub user: String,
    pub ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDto {
    pub device_name: String,
    pub device_id: String,
    pub tools: Vec<AiToolDto>,
    pub debounce_secs: u32,
    pub refresh_interval_secs: u64,
    pub port: u16,
    pub global_excludes: Vec<String>,
    pub sensitive_patterns: Vec<String>,
    pub auto_start: bool,
    pub minimize_to_tray: bool,
    pub notify_on_complete: bool,
    pub log_level: String,
    pub log_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewDto {
    pub local: LocalInfoDto,
    pub tools: Vec<AiToolDto>,
    pub projects: Vec<ProjectDto>,
    pub workspaces: Vec<WorkspaceDto>,
}

// ── Sync progress (D9) ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStageDto {
    pub name: String,
    pub percent: u8,
    pub done: bool,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgressDto {
    pub project_id: String,
    pub project_name: String,
    pub peer_name: String,
    pub direction: String,
    pub percent: u8,
    pub phase: String,
    pub files_done: u32,
    pub files_total: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed_bps: u64,
    pub eta_secs: u32,
    pub current_file: Option<String>,
    pub stages: Vec<SyncStageDto>,
    pub finished: bool,
    pub success: bool,
    pub error: Option<String>,
}

// ── Sync result (D9 result view) ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResultDto {
    pub project_id: String,
    pub project_name: String,
    pub peer_name: String,
    pub direction: String,
    pub success: bool,
    pub files: u32,
    pub bytes: u64,
    pub elapsed_secs: f32,
    pub rewritten_paths: u32,
    pub skipped_paths: u32,
    pub workspace_name: Option<String>,
    pub child_name: Option<String>,
    pub error: Option<String>,
}

// ── Path-rewrite report (D10 / G7) ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewriteEntryDto {
    pub location: String,
    pub field: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedRewriteDto {
    pub location: String,
    pub field: String,
    pub snippet: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewriteReportDto {
    pub project_id: String,
    pub project_name: String,
    pub timestamp: String,
    pub direction: String,
    pub rewritten: Vec<RewriteEntryDto>,
    pub skipped: Vec<SkippedRewriteDto>,
}

// ── Conflict (D5) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictFileDto {
    pub path: String,
    pub change: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictSideDto {
    pub device_name: String,
    pub changed_files: u32,
    pub files: Vec<ConflictFileDto>,
    pub session_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictDto {
    pub project_id: String,
    pub project_name: String,
    pub local: ConflictSideDto,
    pub remote: ConflictSideDto,
}

// ── Batch sync (D6) ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItemDto {
    pub project_id: String,
    pub name: String,
    pub changed_files: u32,
    pub bytes: u64,
    pub up_to_date: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchPlanDto {
    pub peer_name: String,
    pub direction: String,
    pub items: Vec<BatchItemDto>,
    /// Files matching the sensitive-file patterns (G6). Need explicit opt-in.
    pub sensitive_files: Vec<String>,
}

// ── Pairing (D4) ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingDto {
    pub peer_id: String,
    pub peer_name: String,
    pub peer_ip: String,
    pub peer_os: String,
    pub code: String,
    pub request_id: String,
    pub expires_at_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMappingRequestDto {
    pub request_id: String,
    pub project_name: String,
    pub peer_name: String,
    pub source_dir: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMappingRequestDto {
    pub request_id: String,
    pub workspace_name: String,
    pub peer_name: String,
    pub source_root: String,
    pub suggested_remote_root: String,
    pub mode: String,
    pub children: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMessageDto {
    pub sender_name: String,
    pub content: String,
    pub timestamp: u64,
    pub peer_name: Option<String>,
    pub mine: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTransferRequestDto {
    pub id: String,
    pub transfer_id: String,
    pub filename: String,
    pub size: u64,
    pub sender_name: String,
    pub suggested_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTransferHistoryDto {
    pub id: String,
    pub timestamp: u64,
    pub timestamp_text: String,
    pub transfer_id: String,
    pub direction: String,
    pub peer_name: String,
    pub peer: String,
    pub filename: String,
    pub size: u64,
    pub path: String,
    pub save_path: Option<String>,
    pub bytes: u64,
    pub status: String,
    pub detail: Option<String>,
}

// ── Workspace scan (D2) ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedChildDto {
    pub local_name: String,
    pub remote_name: String,
    pub matched_remote: bool,
    pub selected: bool,
}

// ── Tray / global status ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GlobalStatus {
    Idle,
    Syncing,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusBarDto {
    pub primary_peer: Option<String>,
    pub primary_peer_online: bool,
    pub status: GlobalStatus,
    pub syncing_project: Option<String>,
    pub syncing_percent: Option<u8>,
    pub conflict_project: Option<String>,
    pub last_sync: Option<String>,
    pub auto_sync_paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeInfoDto {
    pub port: u16,
    pub cert_path: String,
    pub receive_dir: String,
}
