use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use codebaton_core::Result;
use codebaton_sync::SyncConfig;

use super::{claude_project_dir_name, claude_projects_dir, existing_unique_paths, home_dir};

pub(crate) fn first_level_dir_names(root: &Path) -> Result<HashSet<String>> {
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

pub(crate) fn local_claude_projects_root(config: &SyncConfig) -> Option<PathBuf> {
    let configured = if config.claude_config.local.as_os_str().is_empty() {
        home_dir()?.join(".claude")
    } else {
        config.claude_config.local.clone()
    };
    local_claude_projects_dir(&configured)
}

pub(crate) fn claude_mtime_paths(config: &SyncConfig, code_roots: &[PathBuf]) -> Vec<PathBuf> {
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

pub(crate) fn local_claude_projects_dir(configured: &Path) -> Option<PathBuf> {
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

pub(crate) fn remote_claude_projects_dir(
    config: &SyncConfig,
    peer_name: &str,
    project: &codebaton_core::ProjectMapping,
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
