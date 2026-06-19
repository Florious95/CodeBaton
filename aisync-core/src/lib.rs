use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, AisyncError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AisyncError {
    ConflictDetected(ConflictDetails),
    Discovery(String),
    Transport(String),
    Session(String),
    PathRewrite(String),
    Config(String),
    Io(String),
    InvalidInput(String),
}

impl Display for AisyncError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConflictDetected(details) => write!(f, "conflict detected: {}", details.summary),
            Self::Discovery(message)
            | Self::Transport(message)
            | Self::Session(message)
            | Self::PathRewrite(message)
            | Self::Config(message)
            | Self::Io(message)
            | Self::InvalidInput(message) => f.write_str(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictDetails {
    pub project_id: String,
    pub local_version: u64,
    pub remote_version: u64,
    pub summary: String,
}

impl Error for AisyncError {}

impl From<std::io::Error> for AisyncError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub Uuid);

impl DeviceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DeviceId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsType {
    Darwin,
    Windows,
    Linux,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub os: OsType,
    pub addresses: Vec<IpAddr>,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMapping {
    pub project_id: String,
    pub local_code_dir: PathBuf,
    pub local_session_dir: PathBuf,
    pub remote_code_dir: PathBuf,
    pub remote_session_dir: PathBuf,
    pub original_source_path: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncManifest {
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub relative_path: String,
    pub size: u64,
    pub blake3_hash: String,
    pub mtime: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    LocalToRemote,
    RemoteToLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    OneWayPush { direction: Direction },
    TwoWayAuto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerChangeKind {
    Discovered,
    Updated,
    Lost,
    Paired,
    Unpaired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerChange {
    pub peer: DeviceInfo,
    pub kind: PeerChangeKind,
}

pub type PeerChangeCallback = Box<dyn Fn(PeerChange) + Send + Sync + 'static>;

pub trait Discoverer {
    fn start(&mut self) -> Result<()>;
    fn peers(&self) -> Result<Vec<DeviceInfo>>;
    fn on_peer_change(&mut self, callback: PeerChangeCallback) -> Result<()>;
}

pub trait Transporter {
    fn connect(&mut self, peer: &DeviceInfo) -> Result<()>;
    fn send_manifest(&mut self, manifest: &SyncManifest) -> Result<()>;
    fn send_files(&mut self, root: &Path, files: &[FileEntry]) -> Result<()>;
    fn receive_files(&mut self, target_dir: &Path) -> Result<SyncManifest>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub tool_name: String,
    pub project_id: Option<String>,
    pub data: serde_json::Value,
}

pub trait SessionParser {
    fn tool_name(&self) -> &str;
    fn detect(&self, path: &Path) -> bool;
    fn parse(&self, config_dir: &Path) -> Result<Vec<Session>>;
    fn rewrite_paths(&self, session: &mut Session, rewriter: &dyn PathRewriter) -> Result<()>;
    fn write_session(&self, session: &Session, target_dir: &Path) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RewriteDirection {
    SourceToTarget,
    TargetToSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRule {
    pub source_prefix: String,
    pub target_prefix: String,
    pub source_separator: char,
    pub target_separator: char,
}

pub trait PathRewriter {
    fn rewrite(&self, content: &str, direction: RewriteDirection) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_round_trips_through_json() {
        let device = DeviceInfo {
            id: DeviceId::new(),
            name: "devbox".to_string(),
            os: OsType::Darwin,
            addresses: vec!["127.0.0.1".parse().unwrap()],
            protocol_version: 1,
        };

        let encoded = serde_json::to_string(&device).unwrap();
        let decoded: DeviceInfo = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, device);
    }
}
