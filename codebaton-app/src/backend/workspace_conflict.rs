//! Workspace split-brain conflict analysis + manifest fingerprinting.
//! Extracted from backend.rs (refactor phase 1, step 5). Pure functions.
//!
//! `manifest_fingerprint` is a SHARED leaf — single owner here, imported by
//! orchestration / sync_push rather than copied (otherwise fingerprint/sync
//! semantics silently diverge).

use codebaton_core::SyncManifest;
use codebaton_sync::{WorkspaceChildConfig, WorkspaceConfig};

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceConflictAnalysis {
    pub(crate) workspace: WorkspaceConfig,
    pub(crate) safe_children: Vec<WorkspaceChildConfig>,
    pub(crate) conflicted_children: Vec<String>,
}

pub(crate) fn analyze_workspace_conflicts(
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

pub(crate) fn child_manifest(manifest: &SyncManifest, child_name: &str) -> SyncManifest {
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

pub(crate) fn manifest_fingerprint(manifest: &SyncManifest) -> String {
    let mut hasher = blake3::Hasher::new();
    for file in &manifest.files {
        hasher.update(file.relative_path.as_bytes());
        hasher.update(file.blake3_hash.as_bytes());
        hasher.update(&file.size.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}
