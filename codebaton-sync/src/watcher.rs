use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use codebaton_core::{AisyncError, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchConfig {
    pub paths: Vec<PathBuf>,
    pub debounce: Duration,
    pub exclude_rules: Vec<String>,
}

impl WatchConfig {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            debounce: DEFAULT_DEBOUNCE,
            exclude_rules: default_exclude_rules(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: FileChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeBatch {
    pub changes: Vec<FileChange>,
}

pub struct FsWatcher {
    watcher: RecommendedWatcher,
    stop_tx: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl FsWatcher {
    pub fn start(config: WatchConfig, output: Sender<ChangeBatch>) -> Result<Self> {
        for path in &config.paths {
            if !path.exists() {
                return Err(AisyncError::Config(format!(
                    "watch path does not exist: {}",
                    path.display()
                )));
            }
        }

        let (raw_tx, raw_rx) = mpsc::channel();
        let exclude_rules = config.exclude_rules.clone();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            let Ok(event) = event else {
                return;
            };
            let kind = classify_event(&event.kind);
            for path in event.paths {
                if !is_excluded(&path, &exclude_rules) {
                    let _ = raw_tx.send(FileChange { path, kind });
                }
            }
        })
        .map_err(|error| AisyncError::Io(format!("start watcher: {error}")))?;

        for path in &config.paths {
            watcher
                .watch(path, RecursiveMode::Recursive)
                .map_err(|error| AisyncError::Io(format!("watch '{}': {error}", path.display())))?;
        }

        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || debounce_loop(raw_rx, stop_rx, output, config.debounce));

        Ok(Self {
            watcher,
            stop_tx: Some(stop_tx),
            worker: Some(worker),
        })
    }

    pub fn stop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        let _ = &self.watcher;
        self.stop();
    }
}

pub fn default_exclude_rules() -> Vec<String> {
    [
        ".env*",
        "**/.env*",
        "*.key",
        "**/*.key",
        "*.pem",
        "**/*.pem",
        "credentials.*",
        "**/credentials.*",
        ".git/**",
        "**/.git/**",
        ".git/objects/**",
        "**/.git/objects/**",
        ".git/lfs/**",
        "**/.git/lfs/**",
        ".team/runtime/**",
        "**/.team/runtime/**",
        ".team/logs/**",
        "**/.team/logs/**",
        "node_modules/**",
        "**/node_modules/**",
        "target/**",
        "**/target/**",
        "__pycache__/**",
        "**/__pycache__/**",
        "*.pyc",
        "**/*.pyc",
        ".next/**",
        "**/.next/**",
        "dist/**",
        "**/dist/**",
        "build/**",
        "**/build/**",
        ".DS_Store",
        "**/.DS_Store",
        "Thumbs.db",
        "**/Thumbs.db",
    ]
    .iter()
    .map(|rule| (*rule).to_string())
    .collect()
}

pub fn expand_exclude_rules(rules: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();
    for rule in rules {
        let rule = rule.trim().replace('\\', "/");
        if rule.is_empty() {
            continue;
        }
        if rule.ends_with('/') {
            let dir = rule.trim_end_matches('/');
            push_unique(&mut expanded, format!("{dir}/**"));
            push_unique(&mut expanded, format!("**/{dir}/**"));
            continue;
        }
        push_unique(&mut expanded, rule.clone());
        if !rule.contains('/') && !rule.starts_with("**/") {
            push_unique(&mut expanded, format!("**/{rule}"));
        }
    }
    expanded
}

pub fn is_excluded(path: &Path, rules: &[String]) -> bool {
    let normalized = normalize_path(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    expand_exclude_rules(rules).iter().any(|rule| {
        let rule = rule.trim();
        if rule.is_empty() {
            return false;
        }
        if let Some(dir) = rule
            .strip_prefix("**/")
            .and_then(|rule| rule.strip_suffix("/**"))
        {
            return contains_component_sequence(&normalized, dir);
        }
        if let Some(dir) = rule.strip_suffix("/**") {
            return normalized == dir || normalized.starts_with(&format!("{dir}/"));
        }
        if let Some(file_rule) = rule.strip_prefix("**/") {
            return wildcard_match(file_name, file_rule)
                || normalized.ends_with(&format!("/{file_rule}"));
        }
        if rule.contains('*') {
            return wildcard_match(file_name, rule) || wildcard_match(&normalized, rule);
        }
        normalized == rule
            || normalized.ends_with(&format!("/{rule}"))
            || normalized.contains(&format!("/{rule}/"))
            || file_name == rule
    })
}

fn push_unique(rules: &mut Vec<String>, rule: String) {
    if !rules.contains(&rule) {
        rules.push(rule);
    }
}

fn wildcard_match(value: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return value == pattern;
    }
    let mut remainder = value;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            continue;
        }
        if first && !pattern.starts_with('*') {
            let Some(rest) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = rest;
        } else if let Some(index) = remainder.find(part) {
            remainder = &remainder[index + part.len()..];
        } else {
            return false;
        }
        first = false;
    }
    pattern.ends_with('*') || remainder.is_empty()
}

fn debounce_loop(
    raw_rx: Receiver<FileChange>,
    stop_rx: Receiver<()>,
    output: Sender<ChangeBatch>,
    debounce: Duration,
) {
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let first = match raw_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(change) => change,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let mut changes = BTreeMap::new();
        changes.insert(first.path.clone(), first.kind);

        loop {
            match raw_rx.recv_timeout(debounce) {
                Ok(change) => {
                    changes.insert(change.path.clone(), change.kind);
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        let batch = ChangeBatch {
            changes: changes
                .into_iter()
                .map(|(path, kind)| FileChange { path, kind })
                .collect(),
        };
        if !batch.changes.is_empty() && output.send(batch).is_err() {
            break;
        }
    }
}

fn classify_event(kind: &EventKind) -> FileChangeKind {
    match kind {
        EventKind::Create(_) => FileChangeKind::Created,
        EventKind::Modify(_) => FileChangeKind::Modified,
        EventKind::Remove(_) => FileChangeKind::Deleted,
        _ => FileChangeKind::Other,
    }
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn contains_component_sequence(path: &str, pattern: &str) -> bool {
    let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let pattern: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    if pattern.is_empty() || pattern.len() > components.len() {
        return false;
    }
    components
        .windows(pattern.len())
        .any(|window| window == pattern.as_slice())
}
