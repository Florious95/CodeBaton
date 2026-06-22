//! Exclude-rule composition for handoff (code + session manifest scans).
//!
//! Migrated out of the old `watchers.rs` (removed with auto-sync). These pure
//! config helpers merge the global defaults (build artifacts, secrets, VCS dirs)
//! with per-project / per-workspace overrides, and are reused by the manual
//! push chain and the handoff-preview manifest so compiled output never ships.

use codebaton_sync::{ProjectConfig, SyncConfig, WorkspaceConfig};

// Kept (per reviewer correction 2) for the workspace handoff-preview manifest,
// mirroring `project_exclude_rules`. The project preview already consumes its
// counterpart; the workspace preview path will use this — retained deliberately.
#[allow(dead_code)]
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
