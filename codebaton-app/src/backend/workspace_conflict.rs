//! Workspace manifest fingerprinting.
//! Extracted from backend.rs (refactor phase 1, step 5). Pure functions.
//!
//! `manifest_fingerprint` is a SHARED leaf — single owner here, imported by
//! orchestration / sync_push rather than copied (otherwise fingerprint/sync
//! semantics silently diverge).

use codebaton_core::SyncManifest;

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
