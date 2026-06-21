use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use aisync_core::{AisyncError, DeviceId, Direction, ProjectMapping, Result, SyncMode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConfig {
    pub device: DeviceConfig,
    #[serde(default)]
    pub onboarded: bool,
    #[serde(default = "default_receive_port")]
    pub receive_port: u16,
    #[serde(default)]
    pub peers: HashMap<String, PeerConfig>,
    #[serde(default)]
    pub claude_config: ClaudeConfig,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
    #[serde(default = "crate::watcher::default_exclude_rules")]
    pub exclude_rules: Vec<String>,
    #[serde(default)]
    pub default_sync_mode: SyncModeConfig,
    #[serde(default = "default_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
    #[serde(default)]
    pub default_file_receive_dir: Option<PathBuf>,
    /// 接收端落点显式覆盖。None 时退回 `AISYNC_RECEIVE_DIR` env / config 同级 `received/`。
    /// 用于让每个 Backend 实例有独立接收目录，消除并行测试对全局 env 的依赖。
    #[serde(default)]
    pub receive_dir_override: Option<PathBuf>,
    #[serde(default)]
    pub state_path: Option<PathBuf>,
}

impl SyncConfig {
    pub fn new(device_name: impl Into<String>) -> Self {
        Self {
            device: DeviceConfig {
                id: DeviceId::new(),
                name: device_name.into(),
            },
            onboarded: false,
            receive_port: default_receive_port(),
            peers: HashMap::new(),
            claude_config: ClaudeConfig::default(),
            projects: Vec::new(),
            workspaces: Vec::new(),
            exclude_rules: crate::watcher::default_exclude_rules(),
            default_sync_mode: SyncModeConfig::default(),
            refresh_interval_secs: default_refresh_interval_secs(),
            default_file_receive_dir: None,
            receive_dir_override: None,
            state_path: None,
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        load_config(path)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        save_config(path, self)
    }

    pub fn validate(&self) -> Result<()> {
        validate_config(self)
    }

    pub fn state_path(&self) -> PathBuf {
        self.state_path.clone().unwrap_or_else(|| {
            default_state_path().unwrap_or_else(|| PathBuf::from(".aisync-state.toml"))
        })
    }

    pub fn exclude_rules_for_project(&self, project_id: &str) -> Vec<String> {
        let mut rules = crate::watcher::default_exclude_rules();
        rules.extend(self.exclude_rules.clone());
        if let Some(project) = self
            .projects
            .iter()
            .find(|project| project.name == project_id)
        {
            rules.extend(project.exclude_rules.clone());
        }
        crate::watcher::expand_exclude_rules(&rules)
    }

    pub fn project_mapping(&self, project_name: &str, peer_name: &str) -> Result<ProjectMapping> {
        let project = self
            .projects
            .iter()
            .find(|project| project.name == project_name)
            .ok_or_else(|| AisyncError::Config(format!("project '{project_name}' not found")))?;
        let remote_code_dir = project.peers.get(peer_name).cloned().ok_or_else(|| {
            AisyncError::Config(format!(
                "project '{}' has no mapping for peer '{}'",
                project.name, peer_name
            ))
        })?;
        let remote_session_dir = self
            .claude_config
            .peers
            .get(peer_name)
            .cloned()
            .unwrap_or_else(|| sibling_claude_dir(&remote_code_dir));

        Ok(ProjectMapping {
            project_id: project.name.clone(),
            local_code_dir: project.local.clone(),
            local_session_dir: if self.claude_config.local.as_os_str().is_empty() {
                sibling_claude_dir(&project.local)
            } else {
                self.claude_config.local.clone()
            },
            remote_code_dir,
            remote_session_dir,
            original_source_path: project.local.to_string_lossy().into_owned(),
            enabled: project.enabled,
        })
    }

    /// 读取某 (项目, 对端) 的同步快照（脑裂检测用）。无则 None。
    pub fn sync_snapshot(&self, project_name: &str, peer_name: &str) -> Option<SyncSnapshot> {
        self.projects
            .iter()
            .find(|p| p.name == project_name)
            .and_then(|p| p.sync_snapshots.get(peer_name).cloned())
    }

    /// 写入某 (项目, 对端) 的同步快照，覆盖旧值。项目不存在则忽略。
    pub fn set_sync_snapshot(
        &mut self,
        project_name: &str,
        peer_name: &str,
        snapshot: SyncSnapshot,
    ) {
        if let Some(project) = self.projects.iter_mut().find(|p| p.name == project_name) {
            project
                .sync_snapshots
                .insert(peer_name.to_string(), snapshot);
        }
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self::new("aisync-device")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub id: DeviceId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerConfig {
    pub id: DeviceId,
    pub name: String,
    #[serde(default)]
    pub endpoint: Option<SocketAddr>,
    #[serde(default)]
    pub server_cert: Option<PathBuf>,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub last_seen: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeConfig {
    #[serde(default)]
    pub local: PathBuf,
    #[serde(default)]
    pub peers: HashMap<String, PathBuf>,
}

/// 一次成功同步后两端 manifest 指纹的快照，用于脑裂检测。每个 (项目, 对端) 一份。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSnapshot {
    /// 上次同步成功时，对端 manifest 的指纹。
    #[serde(default)]
    pub peer_last_known_hash: String,
    /// 上次同步成功时，本端 manifest 的指纹。
    #[serde(default)]
    pub self_last_synced_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    pub local: PathBuf,
    #[serde(default)]
    pub peers: HashMap<String, PathBuf>,
    #[serde(default)]
    pub sync_mode: SyncModeConfig,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub exclude_rules: Vec<String>,
    /// 按对端名索引的同步快照（脑裂检测）。旧配置无此字段，默认空。
    #[serde(default)]
    pub sync_snapshots: HashMap<String, SyncSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub name: String,
    #[serde(default)]
    pub local_root: PathBuf,
    #[serde(default)]
    pub remote_root: PathBuf,
    #[serde(default)]
    pub peer: String,
    #[serde(default)]
    pub children: Vec<WorkspaceChildConfig>,
    #[serde(default)]
    pub local: PathBuf,
    #[serde(default)]
    pub peers: HashMap<String, PathBuf>,
    #[serde(default = "default_scan_depth")]
    pub scan_depth: usize,
    #[serde(default)]
    pub auto_enable_new: bool,
    #[serde(default)]
    pub sync_mode: SyncModeConfig,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub exclude_rules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceChildConfig {
    pub name: String,
    pub local_dir: PathBuf,
    pub remote_dir: PathBuf,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub conflicted: bool,
    #[serde(default)]
    pub last_fingerprint: Option<String>,
}

impl WorkspaceConfig {
    pub fn effective_local_root(&self) -> &Path {
        if self.local_root.as_os_str().is_empty() {
            &self.local
        } else {
            &self.local_root
        }
    }

    pub fn effective_peer(&self) -> Option<&str> {
        if self.peer.trim().is_empty() {
            self.peers.keys().next().map(String::as_str)
        } else {
            Some(self.peer.as_str())
        }
    }

    pub fn effective_remote_root(&self, peer_name: &str) -> Option<PathBuf> {
        if !self.remote_root.as_os_str().is_empty()
            && (self.peer.is_empty() || self.peer == peer_name)
        {
            Some(self.remote_root.clone())
        } else {
            self.peers.get(peer_name).cloned()
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncModeConfig {
    OneWayPush,
    OneWayPull,
    #[default]
    TwoWayAuto,
}

impl SyncModeConfig {
    pub fn to_sync_mode(self) -> SyncMode {
        match self {
            Self::OneWayPush => SyncMode::OneWayPush {
                direction: Direction::LocalToRemote,
            },
            Self::OneWayPull => SyncMode::OneWayPush {
                direction: Direction::RemoteToLocal,
            },
            Self::TwoWayAuto => SyncMode::TwoWayAuto,
        }
    }
}

#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
    config: SyncConfig,
    last_modified: Option<SystemTime>,
}

impl ConfigStore {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let config = load_config(&path)?;
        let last_modified = modified_at(&path)?;
        Ok(Self {
            path,
            config,
            last_modified,
        })
    }

    pub fn config(&self) -> &SyncConfig {
        &self.config
    }

    pub fn save(&mut self) -> Result<()> {
        save_config(&self.path, &self.config)?;
        self.last_modified = modified_at(&self.path)?;
        Ok(())
    }

    pub fn reload_if_changed(&mut self) -> Result<bool> {
        let modified = modified_at(&self.path)?;
        if modified == self.last_modified {
            return Ok(false);
        }
        self.config = load_config(&self.path)?;
        self.last_modified = modified;
        Ok(true)
    }
}

pub fn load_config(path: impl AsRef<Path>) -> Result<SyncConfig> {
    let text = fs::read_to_string(path.as_ref()).map_err(|error| {
        AisyncError::Config(format!(
            "read config '{}': {error}",
            path.as_ref().display()
        ))
    })?;
    let config: SyncConfig = toml::from_str(&text)
        .map_err(|error| AisyncError::Config(format!("parse TOML config: {error}")))?;
    validate_config(&config)?;
    Ok(config)
}

pub fn save_config(path: impl AsRef<Path>, config: &SyncConfig) -> Result<()> {
    validate_config(config)?;
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(config)
        .map_err(|error| AisyncError::Config(format!("encode TOML config: {error}")))?;
    fs::write(path.as_ref(), text)?;
    Ok(())
}

pub fn default_config_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".aisync").join("config.toml"))
}

pub fn default_state_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".aisync").join("state.toml"))
}

fn validate_config(config: &SyncConfig) -> Result<()> {
    let mut project_names = HashSet::new();
    for project in &config.projects {
        if !project_names.insert(project.name.as_str()) {
            return Err(AisyncError::Config(format!(
                "duplicate project mapping '{}'",
                project.name
            )));
        }
        if project.enabled && !project.local.exists() {
            return Err(AisyncError::Config(format!(
                "project '{}' local path does not exist: {}",
                project.name,
                project.local.display()
            )));
        }
        // NOTE: a peer remote path equal to the local path is intentionally
        // allowed — on the initiating side the "对端目录" is only a reference
        // default (the peer sets its real path on its own machine). Both ends
        // commonly live under /Users/<user>/..., so they're often identical.
    }

    let mut workspace_names = HashSet::new();
    for workspace in &config.workspaces {
        if !workspace_names.insert(workspace.name.as_str()) {
            return Err(AisyncError::Config(format!(
                "duplicate workspace mapping '{}'",
                workspace.name
            )));
        }
        let local_root = workspace.effective_local_root();
        if workspace.enabled && !local_root.exists() {
            return Err(AisyncError::Config(format!(
                "workspace '{}' local path does not exist: {}",
                workspace.name,
                local_root.display()
            )));
        }
        if workspace.enabled && workspace.effective_peer().is_none() {
            return Err(AisyncError::Config(format!(
                "workspace '{}' has no peer",
                workspace.name
            )));
        }
        if workspace.scan_depth != 1 {
            return Err(AisyncError::Config(format!(
                "workspace '{}' scan_depth must be 1 in MVP",
                workspace.name
            )));
        }
    }

    if config
        .exclude_rules
        .iter()
        .any(|rule| rule.trim().is_empty())
    {
        return Err(AisyncError::Config(
            "exclude rules must not contain empty entries".to_string(),
        ));
    }

    Ok(())
}

fn sibling_claude_dir(path: &Path) -> PathBuf {
    path.parent()
        .map(|parent| parent.join(".claude"))
        .unwrap_or_else(|| path.join(".claude"))
}

fn default_true() -> bool {
    true
}

fn default_scan_depth() -> usize {
    1
}

pub fn default_receive_port() -> u16 {
    52000
}

pub fn default_refresh_interval_secs() -> u64 {
    30
}

fn modified_at(path: &Path) -> Result<Option<SystemTime>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.modified().ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
}
