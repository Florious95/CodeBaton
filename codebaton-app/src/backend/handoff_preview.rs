//! Handoff manifest preview — a local, peer-free dry run that lists every file
//! a manual push WOULD carry (code + AI-tool conversations), the build artifacts
//! it WOULD exclude, the total transfer size, and whether the push is
//! incremental (a snapshot for this peer already exists).
//!
//! Reuses the exact code path the real push uses: `scan_manifest_with_patterns`
//! with the same `exclude.rs` rules for code, and the session staging plans for
//! conversations (staged to a temp dir, scanned, then cleaned — nothing is sent).

use std::fs;
use std::path::Path;

use codebaton_core::Result;
use codebaton_transport::scan_manifest_with_patterns;

use super::exclude::project_exclude_rules;
use super::{count_files_recursive, prepare_claude_session_sync, prepare_codex_session_sync};

/// One file that will be carried in the handoff.
pub struct PreviewFile {
    pub rel_path: String,
    pub size: u64,
}

/// A conversation-file group for one AI tool.
pub struct PreviewSessionGroup {
    pub tool: &'static str,
    pub file_count: usize,
    pub bytes: u64,
}

pub struct HandoffPreview {
    pub code_files: Vec<PreviewFile>,
    pub sessions: Vec<PreviewSessionGroup>,
    pub total_size: u64,
    pub incremental: bool,
}

impl super::Backend {
    /// Compute the handoff manifest for `(project, peer)` without contacting the
    /// peer or transferring anything.
    pub fn preview_handoff(&self, project_name: &str, peer_name: &str) -> Result<HandoffPreview> {
        let g = self.inner.lock().unwrap();
        let project = g.config.project_mapping(project_name, peer_name)?;
        let config = g.config.clone();
        let config_path = g.config_path.clone();
        let incremental = g.config.sync_snapshot(project_name, peer_name).is_some();
        // Find the live ProjectConfig entry so excludes match what the push uses.
        let project_cfg = g
            .config
            .projects
            .iter()
            .find(|p| p.name == project_name)
            .cloned();
        drop(g);

        // ── Code files: same exclude rules as the real push ──────────────
        let mut code_files = Vec::new();
        let mut total_size = 0u64;
        if let Some(project_cfg) = project_cfg.as_ref() {
            let rules = project_exclude_rules(&config, project_cfg);
            let patterns: Vec<&str> = rules.iter().map(String::as_str).collect();
            let manifest = scan_manifest_with_patterns(&project.local_code_dir, &patterns)?;
            for entry in manifest.files {
                total_size += entry.size;
                code_files.push(PreviewFile {
                    rel_path: entry.relative_path,
                    size: entry.size,
                });
            }
        }

        // ── Session files: stage (temp), measure, then clean up ──────────
        let mut sessions = Vec::new();
        if let Some(plan) =
            prepare_claude_session_sync(&config_path, &config, peer_name, &project)?
        {
            let file_count = count_files_recursive(&plan.staged_project_dir);
            total_size += plan.bytes;
            sessions.push(PreviewSessionGroup {
                tool: "claude",
                file_count,
                bytes: plan.bytes,
            });
            cleanup_staging(&plan.staging_root);
        }
        if let Some(plan) = prepare_codex_session_sync(&config_path, peer_name, &project)? {
            let file_count = count_files_recursive(&plan.staged_project_dir);
            total_size += plan.bytes;
            sessions.push(PreviewSessionGroup {
                tool: "codex",
                file_count,
                bytes: plan.bytes,
            });
            cleanup_staging(&plan.staging_root);
        }

        Ok(HandoffPreview {
            code_files,
            sessions,
            total_size,
            incremental,
        })
    }
}

fn cleanup_staging(staging_root: &Path) {
    let _ = fs::remove_dir_all(staging_root);
}
