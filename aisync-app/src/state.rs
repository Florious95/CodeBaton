//! UI preference state behind the Tauri IPC layer.
//!
//! Real device identity, peers, projects, workspaces and AI-tool status all
//! come from the live [`crate::backend::Backend`] (config + mDNS + on-disk
//! scan). This struct holds only the user-facing *settings* defaults that the
//! backend config does not yet model (notification toggles, log level, etc.).
//! No mock devices/projects are seeded — a fresh install shows empty lists.

use std::sync::Mutex;

use crate::dto::*;

pub struct AppState {
    inner: Mutex<Inner>,
}

struct Inner {
    settings: SettingsDto,
    /// Flipped once the first-run wizard (D12) completes in this session.
    onboarded: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::seed()),
        }
    }

    pub fn settings(&self) -> SettingsDto {
        self.inner.lock().unwrap().settings.clone()
    }

    pub fn save_settings(&self, settings: SettingsDto) {
        self.inner.lock().unwrap().settings = settings;
    }

    pub fn set_onboarded(&self, name: &str) {
        let mut g = self.inner.lock().unwrap();
        g.onboarded = true;
        g.settings.device_name = name.to_string();
    }
}

impl Inner {
    fn seed() -> Self {
        // Settings defaults only — device_name/id/tools are overridden with real
        // values by the command layer before reaching the UI.
        let settings = SettingsDto {
            device_name: String::new(),
            device_id: String::new(),
            tools: Vec::new(),
            debounce_secs: 2,
            refresh_interval_secs: aisync_sync::default_refresh_interval_secs(),
            port: 52000,
            global_excludes: aisync_sync::default_exclude_rules(),
            sensitive_patterns: vec![
                ".env*".to_string(),
                "*credential*".to_string(),
                "*.key".to_string(),
                "*.pem".to_string(),
                "*secret*".to_string(),
            ],
            auto_start: false,
            minimize_to_tray: true,
            notify_on_complete: true,
            log_level: "Info".to_string(),
            log_dir: "~/.aisync/logs/".to_string(),
        };

        Self {
            settings,
            onboarded: false,
        }
    }
}
