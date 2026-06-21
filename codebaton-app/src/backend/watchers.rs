use std::collections::HashMap;
use std::path::{Path, PathBuf};

use codebaton_sync::{FsWatcher, ProjectConfig, SyncConfig, WorkspaceConfig};

use super::{claude_watch_paths, existing_unique_paths, start_project_watcher, start_workspace_watcher};

pub(crate) fn start_project_watchers(config_path: &Path, config: &SyncConfig) -> HashMap<String, FsWatcher> {
    let mut watchers = HashMap::new();
    for project in &config.projects {
        if let Some(watcher) = start_project_watcher(config_path, config, project) {
            watchers.insert(project.name.clone(), watcher);
        }
    }
    watchers
}

pub(crate) fn start_workspace_watchers(config_path: &Path, config: &SyncConfig) -> HashMap<String, FsWatcher> {
    let mut watchers = HashMap::new();
    for workspace in &config.workspaces {
        if let Some(watcher) = start_workspace_watcher(config_path, config, workspace) {
            watchers.insert(workspace.name.clone(), watcher);
        }
    }
    watchers
}

pub(crate) fn workspace_exclude_rules(config: &SyncConfig, workspace: &WorkspaceConfig) -> Vec<String> {
    let mut rules = codebaton_sync::default_exclude_rules();
    rules.extend(config.exclude_rules.clone());
    rules.extend(workspace.exclude_rules.clone());
    codebaton_sync::expand_exclude_rules(&rules)
}

pub(crate) fn project_exclude_rules(config: &SyncConfig, project: &ProjectConfig) -> Vec<String> {
    let mut rules = codebaton_sync::default_exclude_rules();
    rules.extend(config.exclude_rules.clone());
    rules.extend(project.exclude_rules.clone());
    codebaton_sync::expand_exclude_rules(&rules)
}

pub(crate) fn project_watch_paths(config: &SyncConfig, project: &ProjectConfig) -> Vec<PathBuf> {
    let mut paths = vec![project.local.clone()];
    paths.extend(claude_watch_paths(
        config,
        std::slice::from_ref(&project.local),
    ));
    existing_unique_paths(paths)
}

pub(crate) fn workspace_watch_paths(config: &SyncConfig, workspace: &WorkspaceConfig) -> Vec<PathBuf> {
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
