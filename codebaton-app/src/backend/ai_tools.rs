//! AI-tool abstraction for conversation handoff.
//!
//! The manual handoff must carry the conversation history of every configured
//! AI coding tool, not just Claude. This enum is the single place that lists the
//! supported tools and dispatches to each tool's staging logic. Adding a new
//! tool = add a variant here + its `match` arms; the push chain and the handoff
//! preview both iterate [`AiToolKind::all`] and need no further changes.
//!
//! First version収编 the two existing tools (Claude Code, Codex). Their staging
//! internals are unchanged — Claude rewrites structured paths for cross-device
//! cwd, Codex copies as-is — this layer only unifies the call surface.

use std::collections::HashSet;
use std::path::Path;

use codebaton_core::{ProjectMapping, Result};
use codebaton_sync::SyncConfig;

use super::{
    prepare_claude_session_sync, prepare_claude_workspace_session_sync, prepare_codex_session_sync,
    prepare_codex_workspace_session_sync, SessionSyncPlan, WorkspaceSessionSyncPlan,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AiToolKind {
    Claude,
    Codex,
}

impl AiToolKind {
    /// Every supported AI tool, in push order. Add a tool here.
    pub(crate) fn all() -> [AiToolKind; 2] {
        [AiToolKind::Claude, AiToolKind::Codex]
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            AiToolKind::Claude => "claude",
            AiToolKind::Codex => "codex",
        }
    }

    /// Stage this tool's sessions for a single-project handoff (no transfer).
    pub(crate) fn prepare_project(
        self,
        config_path: &Path,
        config: &SyncConfig,
        peer_name: &str,
        project: &ProjectMapping,
    ) -> Result<Option<SessionSyncPlan>> {
        match self {
            AiToolKind::Claude => prepare_claude_session_sync(config_path, config, peer_name, project),
            AiToolKind::Codex => prepare_codex_session_sync(config_path, peer_name, project),
        }
    }

    /// Stage this tool's sessions for a workspace handoff (no transfer).
    pub(crate) fn prepare_workspace(
        self,
        config_path: &Path,
        config: &SyncConfig,
        peer_name: &str,
        project: &ProjectMapping,
        excluded_children: &HashSet<String>,
    ) -> Result<Option<WorkspaceSessionSyncPlan>> {
        match self {
            AiToolKind::Claude => prepare_claude_workspace_session_sync(
                config_path,
                config,
                peer_name,
                project,
                excluded_children,
            ),
            AiToolKind::Codex => {
                prepare_codex_workspace_session_sync(config_path, peer_name, project, excluded_children)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handoff carries every tool in `all()`; both the push chain and the
    /// preview iterate it. Adding a tool = one variant + its match arms, and
    /// this list grows automatically — no call-site changes. First version
    /// covers Claude + Codex.
    #[test]
    fn registry_lists_claude_and_codex_with_unique_names() {
        let tools = AiToolKind::all();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"claude"), "Claude must be in the registry");
        assert!(names.contains(&"codex"), "Codex must be in the registry");
        // Names must be unique and non-empty (they key remote dirs / logs).
        for (i, a) in names.iter().enumerate() {
            assert!(!a.is_empty(), "tool name must be non-empty");
            for b in &names[i + 1..] {
                assert_ne!(a, b, "tool names must be unique");
            }
        }
    }
}
