//! Auto-sync gate / suppression / baseline process-global statics + accessors.
//! Extracted from backend.rs (refactor phase 1, step 4).
//!
//! 4 process-global OnceLock maps + the cooldown override. Point-mutate Mutexes,
//! never held across work. These are NOT Inner fields. Gate fns call
//! `super::app_log` for structured logging.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use codebaton_sync::WorkspaceConfig;

use super::app_log;

const DEFAULT_AUTO_SYNC_COOLDOWN: Duration = Duration::from_secs(90);
/// 自动同步 cooldown 的可覆盖值。测试可在任何 Backend 启动前调一次
/// `set_auto_sync_cooldown_for_test` 设为极短值，使 watcher 路径可确定性测试。
/// 仅在进程启动早期设置一次（所有测试设相同值，不构成并行竞态）。
static AUTO_SYNC_COOLDOWN_OVERRIDE: OnceLock<Duration> = OnceLock::new();

pub(crate) fn auto_sync_cooldown() -> Duration {
    AUTO_SYNC_COOLDOWN_OVERRIDE
        .get()
        .copied()
        .unwrap_or(DEFAULT_AUTO_SYNC_COOLDOWN)
}

/// 测试钩子：设置自动同步 cooldown（仅首次生效；幂等）。
pub fn set_auto_sync_cooldown_for_test(d: Duration) {
    let _ = AUTO_SYNC_COOLDOWN_OVERRIDE.set(d);
}

static INCOMING_SYNC_SUPPRESSIONS: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
static AUTO_SYNC_GATES: OnceLock<Mutex<HashMap<String, AutoSyncGate>>> = OnceLock::new();
static SESSION_BASELINE_SEEDS: OnceLock<Mutex<HashMap<String, SessionBaseline>>> = OnceLock::new();
static WORKSPACE_PROPAGATION_BYPASS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Clone, Copy)]
pub(crate) struct AutoSyncGate {
    pub(crate) in_flight: bool,
    pub(crate) cooldown_until: Instant,
}

#[derive(Clone)]
pub(crate) struct SessionBaseline {
    pub(crate) mtime: SystemTime,
    pub(crate) content_fingerprint: Option<String>,
    pub(crate) sync_fingerprint: Option<String>,
}

pub(crate) fn incoming_sync_suppressions() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    INCOMING_SYNC_SUPPRESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn auto_sync_gates() -> &'static Mutex<HashMap<String, AutoSyncGate>> {
    AUTO_SYNC_GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn session_baseline_seeds() -> &'static Mutex<HashMap<String, SessionBaseline>> {
    SESSION_BASELINE_SEEDS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn workspace_propagation_bypass() -> &'static Mutex<HashSet<String>> {
    WORKSPACE_PROPAGATION_BYPASS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// incoming 防回环抑制窗口：必须 >= watcher debounce，否则接收端写入触发的 watcher
/// 事件在抑制过期后才到达 → 误判为本地变更而反向回推（回环）。取 cooldown 与一个
/// debounce 安全下限的较大者，使其既不被测试的短 cooldown 削穿，也随生产 cooldown 放大。
pub(crate) fn incoming_suppress_window() -> Duration {
    const MIN_WINDOW: Duration = Duration::from_secs(5); // > DEFAULT_DEBOUNCE(2s) + 余量
    auto_sync_cooldown().max(MIN_WINDOW)
}

pub(crate) fn mark_incoming_sync_root(root: &Path) {
    incoming_sync_suppressions()
        .lock()
        .unwrap()
        .insert(root.to_path_buf(), Instant::now() + incoming_suppress_window());
}

pub(crate) fn incoming_sync_recent(root: &Path) -> bool {
    let now = Instant::now();
    let mut guard = incoming_sync_suppressions().lock().unwrap();
    guard.retain(|_, until| *until > now);
    guard
        .keys()
        .any(|incoming| incoming.starts_with(root) || root.starts_with(incoming))
}

pub(crate) fn auto_sync_gate_key(scope: &str, name: &str, peer: &str) -> String {
    format!("{scope}:{name}:{peer}")
}

pub(crate) fn try_begin_auto_sync(
    scope: &str,
    name: &str,
    peer: &str,
    trigger: &str,
) -> Option<String> {
    let key = auto_sync_gate_key(scope, name, peer);
    let now = Instant::now();
    let mut gates = auto_sync_gates().lock().unwrap();
    gates.retain(|_, gate| gate.in_flight || gate.cooldown_until > now);
    if let Some(gate) = gates.get(&key) {
        let reason = if gate.in_flight {
            "in_flight"
        } else {
            "cooldown"
        };
        app_log(
            "auto_sync_suppressed",
            &[
                ("scope", scope.to_string()),
                ("name", name.to_string()),
                ("peer", peer.to_string()),
                ("trigger", trigger.to_string()),
                ("reason", reason.to_string()),
            ],
        );
        return None;
    }
    gates.insert(
        key.clone(),
        AutoSyncGate {
            in_flight: true,
            cooldown_until: now,
        },
    );
    Some(key)
}

pub(crate) fn begin_auto_sync_bypass_cooldown(
    scope: &str,
    name: &str,
    peer: &str,
    trigger: &str,
) -> Option<String> {
    let key = auto_sync_gate_key(scope, name, peer);
    let now = Instant::now();
    let mut gates = auto_sync_gates().lock().unwrap();
    gates.retain(|_, gate| gate.in_flight || gate.cooldown_until > now);
    if gates.get(&key).map(|gate| gate.in_flight).unwrap_or(false) {
        app_log(
            "auto_sync_suppressed",
            &[
                ("scope", scope.to_string()),
                ("name", name.to_string()),
                ("peer", peer.to_string()),
                ("trigger", trigger.to_string()),
                ("reason", "in_flight".to_string()),
            ],
        );
        return None;
    }
    gates.insert(
        key.clone(),
        AutoSyncGate {
            in_flight: true,
            cooldown_until: now,
        },
    );
    Some(key)
}

pub(crate) fn finish_auto_sync(gate_key: &str) {
    auto_sync_gates().lock().unwrap().insert(
        gate_key.to_string(),
        AutoSyncGate {
            in_flight: false,
            cooldown_until: Instant::now() + auto_sync_cooldown(),
        },
    );
}

pub(crate) fn enqueue_workspace_first_propagation(workspace: &WorkspaceConfig) {
    let Some(peer) = workspace.effective_peer() else {
        return;
    };
    workspace_propagation_bypass()
        .lock()
        .unwrap()
        .insert(auto_sync_gate_key("workspace", &workspace.name, peer));
    app_log(
        "workspace_first_propagation_queued",
        &[
            ("workspace", workspace.name.clone()),
            ("peer", peer.to_string()),
        ],
    );
}

pub(crate) fn workspace_first_propagation_pending(workspace_name: &str, peer_name: &str) -> bool {
    workspace_propagation_bypass()
        .lock()
        .unwrap()
        .contains(&auto_sync_gate_key("workspace", workspace_name, peer_name))
}

pub(crate) fn clear_workspace_first_propagation(workspace_name: &str, peer_name: &str) {
    workspace_propagation_bypass()
        .lock()
        .unwrap()
        .remove(&auto_sync_gate_key("workspace", workspace_name, peer_name));
}
