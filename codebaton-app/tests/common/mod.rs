//! 同步安全测试框架（单机双工 harness + 声明式 fixture + 黑盒断言）。
//!
//! 设计见 `docs/design-test-framework.md`。核心：单机起两个 Backend 实例
//! （A 发送、B 接收带 serve 守护），真实 TLS/TCP 在 localhost 跑同步，
//! 一行 builder 构建场景、`?` 链式断言、TempDir RAII 回收磁盘。

#![allow(dead_code)] // 各用例只用到部分 helper

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::{fs, thread, time::Duration};

use codebaton_app_lib::backend::{Backend, SplitBrainStatus};
use codebaton_core::{Direction, OsType, Result};
use codebaton_sync::{ClaudeConfig, DeviceConfig, SyncConfig, SyncModeConfig, SyncSnapshot};
use codebaton_transport::{manifest_hash, scan_manifest};
use uuid::Uuid;

/// 取一个空闲端口（bind :0 后立即释放，拿到内核分配的端口号）。
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 把路径编码成 Claude projects 子目录名（与 ClaudeCode 解析一致）。
pub fn claude_project_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

pub fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err)?;
    }
    fs::write(path, content).map_err(io_err)
}

fn io_err(e: std::io::Error) -> codebaton_core::AisyncError {
    codebaton_core::AisyncError::Transport(e.to_string())
}

// ──────────────────────────────────────────────────────────────────────────
// TwoBackend harness
// ──────────────────────────────────────────────────────────────────────────

/// 单机双工 harness：A 发送、B 接收（带 serve 守护）。
/// `run_root` 是 RAII TempDir——drop 即递归删除全部测试数据（含 `.bak-*` /
/// `.aisync-trash/`，二者都在 run_root 子树下）。
pub struct TwoBackend {
    pub run_root: tempfile::TempDir,
    pub a: Backend,
    pub a_config_path: PathBuf,
    pub a_project_local: PathBuf,
    pub a_claude_local: PathBuf,
    pub b: Backend,
    pub b_config_path: PathBuf,
    pub b_project_remote: PathBuf,
    pub project_name: String,
    pub peer_name: String,
}

#[derive(Default)]
pub struct TwoBackendBuilder {
    a_files: Vec<(String, String)>,
    b_files: Vec<(String, String)>,
    project_name: Option<String>,
    /// 预置回收站批次：(秒数前, [(相对路径, 内容)])。
    b_trash_batches: Vec<(u64, Vec<(String, String)>)>,
    /// 预置符号链接：(B 项目内相对路径, 链接目标)。
    b_symlinks: Vec<(String, String)>,
    /// 预置 B 同级备份目录 <project>.bak-<ts>：[(相对路径, 内容)]。
    b_existing_backup: Option<Vec<(String, String)>>,
    /// 构建后是否立即跑一次真同步达成「已同步」状态（快照天然正确）。
    synced: bool,
    /// 是否用双向自动同步模式（启用 watcher 自动同步）。
    two_way: bool,
    /// 是否双向：A 也起守护、B 反向映射，用于防回环测试。
    bidirectional: bool,
}

impl TwoBackendBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    /// 在 A 本地项目目录下声明一个文件。
    pub fn a_file(mut self, rel: &str, content: &str) -> Self {
        self.a_files.push((rel.into(), content.into()));
        self
    }
    /// 在 B 接收端目标目录下预置一个文件（模拟「目标非空」）。
    pub fn b_file(mut self, rel: &str, content: &str) -> Self {
        self.b_files.push((rel.into(), content.into()));
        self
    }
    pub fn project_name(mut self, name: &str) -> Self {
        self.project_name = Some(name.into());
        self
    }
    /// 预置 B 回收站批次（`secs_ago` 秒前的时间戳目录），测 7 天保留（CLI-SS-016/AUTO-016）。
    pub fn b_trash_batch(mut self, secs_ago: u64, files: &[(&str, &str)]) -> Self {
        self.b_trash_batches.push((
            secs_ago,
            files.iter().map(|(r, c)| (r.to_string(), c.to_string())).collect(),
        ));
        self
    }
    /// 在 B 项目内预置一个符号链接（AUTO-017：非普通文件不得被物理销毁）。
    pub fn b_symlink(mut self, rel: &str, target: &str) -> Self {
        self.b_symlinks.push((rel.into(), target.into()));
        self
    }
    /// 在 B 项目同级预置一个 `<project>.bak-<ts>` 备份（AUTO-017/§17：exclude 不传输）。
    pub fn b_existing_backup(mut self, files: &[(&str, &str)]) -> Self {
        self.b_existing_backup = Some(
            files.iter().map(|(r, c)| (r.to_string(), c.to_string())).collect(),
        );
        self
    }
    /// 构建后立即跑一次真 push，达成 A/B「已同步」+ 正确快照（用于承接式用例）。
    pub fn synced(mut self) -> Self {
        self.synced = true;
        self
    }
    /// 用双向自动同步模式（启用 watcher 自动同步路径）。
    pub fn two_way(mut self) -> Self {
        self.two_way = true;
        self
    }
    /// 双向：A 也起守护、B 反向映射 + watcher（防回环测试 E-041）。隐含 two_way。
    pub fn bidirectional(mut self) -> Self {
        self.bidirectional = true;
        self.two_way = true;
        self
    }

    pub fn build(self) -> TwoBackend {
        let project_name = self.project_name.unwrap_or_else(|| "proj".into());
        let peer_name = "B".to_string();
        // watcher 自动同步测试需短 cooldown 才能确定性观测（首次设置即定，幂等）。
        codebaton_app_lib::backend::set_auto_sync_cooldown_for_test(Duration::from_millis(200));
        // 测试隔离：把 codex 会话目录指向一个共享的空目录，绝不扫描真实 ~/.codex
        // （AUTO-070）。所有测试指向同一只读空目录，env 全局但内容相同、安全。
        ensure_isolated_codex_dir();
        let sync_mode = if self.two_way {
            SyncModeConfig::TwoWayAuto
        } else {
            SyncModeConfig::OneWayPush
        };
        let run_root = tempfile::tempdir().unwrap();
        let root = run_root.path().to_path_buf();

        // ── B：接收端 ──
        // 每端唯一 config_path → receive_root() 取 config_path 同级 "received"，
        // 天然隔离，无需也不该用全局 env var（但 with_config_serving 仍读 env，
        // 接收落点用 receive_dir_override 显式指定，无 env、无锁，并行安全。
        let b_root = root.join("B");
        let b_config_path = b_root.join("config.toml");
        let b_recv = b_root.join("received");
        let b_project_remote = b_recv.join(&project_name);
        fs::create_dir_all(&b_recv).unwrap();
        // 预置 B 初始文件（目标非空场景）。
        for (rel, content) in &self.b_files {
            write_file(&b_project_remote.join(rel), content).unwrap();
        }
        // 预置回收站批次（时间戳目录名 = now-secs_ago）。
        for (secs_ago, files) in &self.b_trash_batches {
            let ts = now_secs().saturating_sub(*secs_ago);
            let batch = b_project_remote.join(".aisync-trash").join(ts.to_string());
            for (rel, content) in files {
                write_file(&batch.join(rel), content).unwrap();
            }
        }
        // 预置符号链接（仅 unix）。
        #[cfg(unix)]
        for (rel, target) in &self.b_symlinks {
            let link = b_project_remote.join(rel);
            if let Some(p) = link.parent() {
                fs::create_dir_all(p).unwrap();
            }
            let _ = std::os::unix::fs::symlink(target, &link);
        }
        // 预置 B 同级 <project>.bak-<ts> 备份。
        if let Some(files) = &self.b_existing_backup {
            let bak = b_recv.join(format!("{project_name}.bak-{}", now_secs()));
            for (rel, content) in files {
                write_file(&bak.join(rel), content).unwrap();
            }
        }

        let b_port = free_port();
        let mut b_cfg = bare_config("B", b_port, &b_root);
        // 关键：显式接收目录覆盖——并行安全、无全局 env、无锁。
        b_cfg.receive_dir_override = Some(b_recv.clone());

        let backend_b = Backend::with_config_serving(b_cfg, b_config_path.clone()).unwrap();
        let serve = backend_b
            .serve_info()
            .expect("B serve 守护必须启动");

        // ── A：发送端 ──
        let a_root = root.join("A");
        let a_config_path = a_root.join("config.toml");
        let a_project_local = a_root.join(&project_name);
        let a_claude_local = a_root.join(".claude");
        fs::create_dir_all(&a_root).unwrap();
        fs::create_dir_all(&a_claude_local).unwrap();
        for (rel, content) in &self.a_files {
            write_file(&a_project_local.join(rel), content).unwrap();
        }

        let a_recv = a_root.join("received");
        let mut a_cfg = bare_config("A", free_port(), &a_root);
        // A 的 claude 本地目录指向 a_claude_local，使会话同步可定位。
        a_cfg.claude_config = ClaudeConfig {
            local: a_claude_local.clone(),
            peers: {
                let mut m = HashMap::new();
                m.insert(peer_name.clone(), b_root.join(".claude"));
                m
            },
        };
        // 双向模式：A 也起守护并显式接收目录，使 B 能反向推送/防回环可测。
        let backend_a = if self.bidirectional {
            a_cfg.receive_dir_override = Some(a_recv.clone());
            Backend::with_config_serving(a_cfg, a_config_path.clone()).unwrap()
        } else {
            Backend::with_config(a_cfg, a_config_path.clone()).unwrap()
        };

        // A 注册 B 的 endpoint + pinned cert，再加项目映射。
        backend_a
            .add_peer_endpoint(
                peer_name.clone(),
                codebaton_core::DeviceId(Uuid::new_v4()),
                SocketAddr::from(([127, 0, 0, 1], serve.port)),
                Some(serve.cert_path.clone()),
                Some("aisync-receiver".into()),
            )
            .unwrap();
        backend_a
            .add_project(
                project_name.clone(),
                a_project_local.clone(),
                peer_name.clone(),
                b_project_remote.clone(),
                sync_mode,
                false,
            )
            .unwrap();

        // 双向模式：B 也映射「收到的项目目录 → A」并起 watcher，用于防回环测试。
        if self.bidirectional {
            let a_serve = backend_a.serve_info().expect("A 守护应启动");
            backend_b
                .add_peer_endpoint(
                    "A".into(),
                    codebaton_core::DeviceId(Uuid::new_v4()),
                    SocketAddr::from(([127, 0, 0, 1], a_serve.port)),
                    Some(a_serve.cert_path.clone()),
                    Some("aisync-receiver".into()),
                )
                .unwrap();
            // create_local_dir=true：B 的接收项目目录此刻可能尚不存在（未收到推送），
            // 需创建以满足 add_project 的目录存在校验。
            backend_b
                .add_project(
                    project_name.clone(),
                    b_project_remote.clone(),
                    "A".into(),
                    a_project_local.clone(),
                    SyncModeConfig::TwoWayAuto,
                    true,
                )
                .unwrap();
        }

        let harness = TwoBackend {
            run_root,
            a: backend_a,
            a_config_path,
            a_project_local,
            a_claude_local,
            b: backend_b,
            b_config_path,
            b_project_remote,
            project_name,
            peer_name,
        };

        // 达成「已同步」状态：跑一次真 push，快照天然正确（不填假 hash）。
        if self.synced {
            harness.push(false).expect("synced() 的初始 push 应成功");
        }
        harness
    }
}

/// 把 codex 会话目录指向共享空目录，避免测试扫描真实 ~/.codex（AUTO-070）。
/// OnceLock 确保只建一次；env 全局但所有测试指向同一只读空目录，并行安全。
fn ensure_isolated_codex_dir() {
    use std::sync::OnceLock;
    static CODEX_DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = CODEX_DIR.get_or_init(|| {
        let d = std::env::temp_dir().join("aisync-test-empty-codex");
        let _ = fs::create_dir_all(&d);
        d
    });
    std::env::set_var("AISYNC_CODEX_SESSIONS_DIR", dir);
}

/// 当前 Unix 秒。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ──────────────────────────────────────────────────────────────────────────
// WorkspaceHarness：A 工作区 → B（含子目录），封装映射请求/确认握手
// ──────────────────────────────────────────────────────────────────────────

pub struct WorkspaceHarness {
    pub run_root: tempfile::TempDir,
    pub a: Backend,
    pub a_config_path: PathBuf,
    pub a_workspace_root: PathBuf,
    pub a_claude_local: PathBuf,
    pub b: Backend,
    pub b_remote_root: PathBuf,
    pub workspace_name: String,
    pub peer_name: String,
}

#[derive(Default)]
pub struct WorkspaceBuilder {
    /// 子目录文件：(child, 相对路径, 内容)。
    a_children: Vec<(String, String, String)>,
    auto_enable_new: bool,
    workspace_name: Option<String>,
}

impl WorkspaceBuilder {
    pub fn new() -> Self {
        let mut b = Self::default();
        b.auto_enable_new = true;
        b
    }
    /// 在工作区子目录 `child` 下声明一个文件。
    pub fn a_child_file(mut self, child: &str, rel: &str, content: &str) -> Self {
        self.a_children
            .push((child.into(), rel.into(), content.into()));
        self
    }
    pub fn workspace_name(mut self, name: &str) -> Self {
        self.workspace_name = Some(name.into());
        self
    }

    pub fn build(self) -> WorkspaceHarness {
        let workspace_name = self.workspace_name.unwrap_or_else(|| "workspace".into());
        let peer_name = "B".to_string();
        codebaton_app_lib::backend::set_auto_sync_cooldown_for_test(Duration::from_millis(200));
        let run_root = tempfile::tempdir().unwrap();
        let root = run_root.path().to_path_buf();

        // ── B：接收端 ──
        let b_root = root.join("B");
        let b_config_path = b_root.join("config.toml");
        let b_recv = b_root.join("received");
        let b_remote_root = b_recv.join(&workspace_name);
        fs::create_dir_all(&b_recv).unwrap();
        let mut b_cfg = bare_config("B", free_port(), &b_root);
        b_cfg.receive_dir_override = Some(b_recv.clone());
        let backend_b = Backend::with_config_serving(b_cfg, b_config_path).unwrap();
        let serve = backend_b.serve_info().expect("B 守护应启动");

        // ── A：发送端（含工作区子目录文件） ──
        let a_root = root.join("A");
        let a_config_path = a_root.join("config.toml");
        let a_workspace_root = a_root.join(&workspace_name);
        let a_claude_local = a_root.join(".claude");
        fs::create_dir_all(&a_root).unwrap();
        fs::create_dir_all(&a_claude_local).unwrap();
        // 至少建一个子目录，保证 workspace local_root 是目录。
        fs::create_dir_all(&a_workspace_root).unwrap();
        for (child, rel, content) in &self.a_children {
            write_file(&a_workspace_root.join(child).join(rel), content).unwrap();
        }

        let mut a_cfg = bare_config("A", free_port(), &a_root);
        a_cfg.claude_config = ClaudeConfig {
            local: a_claude_local.clone(),
            peers: {
                let mut m = HashMap::new();
                m.insert(peer_name.clone(), b_root.join(".claude"));
                m
            },
        };
        let backend_a = Backend::with_config_serving(a_cfg, a_config_path.clone()).unwrap();
        backend_a
            .add_peer_endpoint(
                peer_name.clone(),
                codebaton_core::DeviceId(Uuid::new_v4()),
                std::net::SocketAddr::from(([127, 0, 0, 1], serve.port)),
                Some(serve.cert_path.clone()),
                Some("aisync-receiver".into()),
            )
            .unwrap();

        // 工作区映射握手：A 发请求 → B 确认 → A 处理 ack。
        let request_id = backend_a
            .add_workspace(
                workspace_name.clone(),
                a_workspace_root.clone(),
                peer_name.clone(),
                b_remote_root.clone(),
                SyncModeConfig::TwoWayAuto,
                self.auto_enable_new,
            )
            .expect("add_workspace 应发出请求");
        let request = (0..100)
            .find_map(|_| {
                let r = backend_b.take_pending_workspace_mapping_request();
                if r.is_none() {
                    thread::sleep(Duration::from_millis(30));
                }
                r
            })
            .expect("B 应收到工作区映射请求");
        assert_eq!(request.request_id, request_id);
        backend_b
            .confirm_workspace_mapping_request(&request_id, b_remote_root.clone())
            .expect("B 确认工作区映射");
        (0..100)
            .find_map(|_| {
                let n = backend_a.process_workspace_mapping_acks().unwrap_or(0);
                if n == 0 {
                    thread::sleep(Duration::from_millis(30));
                    None
                } else {
                    Some(n)
                }
            })
            .expect("A 应收到工作区映射 ack");

        WorkspaceHarness {
            run_root,
            a: backend_a,
            a_config_path,
            a_workspace_root,
            a_claude_local,
            b: backend_b,
            b_remote_root,
            workspace_name,
            peer_name,
        }
    }
}

impl WorkspaceHarness {
    pub fn builder() -> WorkspaceBuilder {
        WorkspaceBuilder::new()
    }

    /// A → B 工作区同步（推送整棵子目录树）。
    pub fn sync(&self) -> Result<codebaton_sync::SyncReport> {
        self.a
            .run_workspace_sync(&self.workspace_name, Direction::LocalToRemote)
    }

    /// 新建一个工作区子目录（可仅含会话、无代码文件）。
    pub fn add_child_dir(&self, child: &str) {
        fs::create_dir_all(self.a_workspace_root.join(child)).unwrap();
    }

    pub fn write_child(&self, child: &str, rel: &str, content: &str) {
        write_file(&self.a_workspace_root.join(child).join(rel), content).unwrap();
    }

    /// B 接收端工作区根。
    pub fn b_root(&self) -> &Path {
        &self.b_remote_root
    }

    /// 在 A 某子目录的 Claude 会话目录写一条含 marker 的记录。
    pub fn write_child_session(&self, child: &str, session_id: &str, marker: &str) {
        let child_dir = self.a_workspace_root.join(child);
        let dir = self
            .a_claude_local
            .join("projects")
            .join(claude_project_dir_name(&child_dir));
        let line = serde_json::json!({
            "type": "user",
            "cwd": child_dir.to_string_lossy(),
            "message": { "content": [{ "type": "text", "text": marker }] },
            "sessionId": session_id
        })
        .to_string();
        write_file(&dir.join(format!("{session_id}.jsonl")), &format!("{line}\n")).unwrap();
    }

    pub fn b_has_session_marker(&self, marker: &str) -> bool {
        jsonl_contains_marker(self.run_root.path(), marker)
    }
}

impl TwoBackend {
    pub fn builder() -> TwoBackendBuilder {
        TwoBackendBuilder::new()
    }

    /// A → B 推送。confirm_overwrite=true 同时放行覆盖与 >50% 删除安全阀。
    pub fn push(&self, confirm_overwrite: bool) -> Result<codebaton_sync::SyncReport> {
        self.a.run_sync(
            &self.project_name,
            &self.peer_name,
            Direction::LocalToRemote,
            &[],
            confirm_overwrite,
        )
    }

    /// 独立 split-brain 探针（需 B 守护在线）。注意 push 端本身无此守卫。
    /// 返回真实 `SplitBrainStatus`（非 Result，不要加 `?`）。
    pub fn probe_split_brain(&self) -> SplitBrainStatus {
        self.a.check_split_brain(&self.project_name, &self.peer_name)
    }

    pub fn write_a(&self, rel: &str, content: &str) {
        write_file(&self.a_project_local.join(rel), content).unwrap();
    }
    pub fn write_b(&self, rel: &str, content: &str) {
        write_file(&self.b_project_remote.join(rel), content).unwrap();
    }
    pub fn remove_a(&self, rel: &str) {
        fs::remove_file(self.a_project_local.join(rel)).unwrap();
    }

    /// 重新加载 A 持久化 config，黑盒读取快照。
    pub fn a_snapshot(&self) -> Option<SyncSnapshot> {
        let cfg = codebaton_sync::load_config(&self.a_config_path).ok()?;
        cfg.sync_snapshot(&self.project_name, &self.peer_name)
    }

    /// B 接收端项目目录（A push 的落点）。
    pub fn b_dir(&self) -> &Path {
        &self.b_project_remote
    }
    pub fn a_dir(&self) -> &Path {
        &self.a_project_local
    }

    /// A 端项目映射的同步历史（最新在前）。
    pub fn a_history(&self) -> Vec<serde_json::Value> {
        self.a.sync_history(Some(&self.project_name))
    }

    /// B 端（接收端）同步历史——黑盒读 B 的 history.jsonl（最新在前）。
    pub fn b_history(&self) -> Vec<serde_json::Value> {
        let path = self.b_config_path.with_file_name("history.jsonl");
        let Ok(text) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut rows: Vec<serde_json::Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        rows.reverse(); // 最新在前
        rows
    }

    /// 查询已记录的结构化事件（供日志断言）。`events_for(event, Some(project))`。
    pub fn events_for(&self, event: &str) -> Vec<codebaton_app_lib::backend::RecordedEvent> {
        codebaton_app_lib::backend::events_for(event, Some(&self.project_name))
    }

    /// 在 A 的 Claude 会话目录写一条含 `marker` 的 JSONL 记录（用于纯对话同步测试）。
    /// 路径：`<a_claude>/projects/<encoded-a-project>/<session_id>.jsonl`。
    pub fn write_a_claude_session(&self, session_id: &str, marker: &str) {
        let dir = self
            .a_claude_local
            .join("projects")
            .join(claude_project_dir_name(&self.a_project_local));
        let line = serde_json::json!({
            "type": "user",
            "cwd": self.a_project_local.to_string_lossy(),
            "message": { "content": [{ "type": "text", "text": marker }] },
            "sessionId": session_id
        })
        .to_string();
        write_file(&dir.join(format!("{session_id}.jsonl")), &format!("{line}\n")).unwrap();
    }

    /// 递归在 B 接收端根下任一 .jsonl 中查找 marker。
    pub fn b_has_session_marker(&self, marker: &str) -> bool {
        // B 的会话落点在 b_recv 同级（受 claude_config.peers 映射）；从 run_root 全扫最稳。
        jsonl_contains_marker(self.run_root.path(), marker)
    }

    /// 重新指向 A 的 peer endpoint（用于 TLS 错误注入：错误端口/错误 cert）。
    pub fn repoint_peer(&self, endpoint: std::net::SocketAddr, cert: Option<PathBuf>) {
        self.a
            .add_peer_endpoint(
                self.peer_name.clone(),
                codebaton_core::DeviceId(Uuid::new_v4()),
                endpoint,
                cert,
                Some("aisync-receiver".into()),
            )
            .unwrap();
    }

    /// B serve 守护信息（端口、cert）。
    pub fn b_serve(&self) -> codebaton_app_lib::backend::ServeInfo {
        self.b.serve_info().expect("B 守护应在线")
    }

    /// 停 B 守护（测试可显式调；Backend Drop 也会调，幂等）。
    pub fn shutdown_b(&self) {
        self.b.shutdown_serve();
    }

    /// 读取某事件针对本项目的累计次数（并行隔离：用本 harness 的唯一项目名做 key）。
    /// 传 `"project_auto_sync_complete"` 等事件名即可。
    pub fn event_count_for_project(&self, event: &str) -> u64 {
        codebaton_app_lib::backend::event_count(&format!("{event}:{}", self.project_name))
    }

    /// 用同样内容重写一个文件（触发 watcher FS 事件但内容指纹不变，测「虚空同步」抑制）。
    pub fn rewrite_a_same(&self, rel: &str, content: &str) {
        write_file(&self.a_project_local.join(rel), content).unwrap();
    }

    /// 等待 watcher debounce(2s) + 处理完成。给足余量。
    pub fn wait_watcher(&self) {
        thread::sleep(Duration::from_millis(3500));
    }

    /// 把 B 项目同级目录设为只读，使备份目录创建失败（AUTO-018B）。返回原权限以便恢复。
    #[cfg(unix)]
    pub fn set_b_parent_readonly(&self, readonly: bool) {
        use std::os::unix::fs::PermissionsExt;
        let parent = self.b_project_remote.parent().unwrap();
        let mode = if readonly { 0o555 } else { 0o755 };
        fs::set_permissions(parent, fs::Permissions::from_mode(mode)).unwrap();
    }
}

/// 递归在 root 下任一 .jsonl 文件中查找 marker。
fn jsonl_contains_marker(root: &Path, marker: &str) -> bool {
    let Ok(rd) = fs::read_dir(root) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if jsonl_contains_marker(&p, marker) {
                return true;
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            if fs::read_to_string(&p).map(|c| c.contains(marker)).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// 目录内容指纹（公开版，供用例记录「覆盖前」状态）。
pub fn dir_hash_of(dir: &Path) -> String {
    dir_hash(dir)
}

/// 一个不含项目映射的基础 config（device 名 + 端口 + 各类目录在 root 下）。
fn bare_config(device: &str, receive_port: u16, root: &Path) -> SyncConfig {
    SyncConfig {
        device: DeviceConfig {
            id: codebaton_core::DeviceId(Uuid::new_v4()),
            name: device.into(),
        },
        onboarded: false,
        peers: HashMap::new(),
        claude_config: ClaudeConfig {
            local: root.join("local-claude"),
            peers: HashMap::new(),
        },
        projects: vec![],
        workspaces: vec![],
        exclude_rules: codebaton_sync::default_exclude_rules(),
        default_sync_mode: SyncModeConfig::OneWayPush,
        refresh_interval_secs: 30,
        receive_port,
        default_file_receive_dir: None,
        receive_dir_override: None,
        state_path: Some(root.join("state.toml")),
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 黑盒断言 helper（全部 -> Result<(), String>，可链式 `?`）
// ──────────────────────────────────────────────────────────────────────────

/// 目录内容指纹（排除 .aisync-trash / .bak-* 由 scan_manifest 的 exclude 负责）。
fn dir_hash(dir: &Path) -> String {
    manifest_hash(&scan_manifest(dir).unwrap_or(codebaton_core::SyncManifest { files: vec![] }))
}

/// 两个目录内容（路径+内容哈希）相等。
pub fn assert_dir_tree_eq(a: &Path, b: &Path) -> std::result::Result<(), String> {
    let (ha, hb) = (dir_hash(a), dir_hash(b));
    if ha == hb {
        Ok(())
    } else {
        Err(format!(
            "目录内容不一致:\n  {} = {ha}\n  {} = {hb}",
            a.display(),
            b.display()
        ))
    }
}

pub fn assert_file_content(path: &Path, expected: &str) -> std::result::Result<(), String> {
    match fs::read_to_string(path) {
        Ok(c) if c == expected => Ok(()),
        Ok(c) => Err(format!("{} 内容不符: 期望 {expected:?} 实际 {c:?}", path.display())),
        Err(e) => Err(format!("读 {} 失败: {e}", path.display())),
    }
}

pub fn assert_file_exists(path: &Path) -> std::result::Result<(), String> {
    if path.exists() {
        Ok(())
    } else {
        Err(format!("{} 应存在却缺失", path.display()))
    }
}

pub fn assert_file_not_exists(path: &Path) -> std::result::Result<(), String> {
    if !path.exists() {
        Ok(())
    } else {
        Err(format!("{} 不应存在却仍在", path.display()))
    }
}

/// 同步成功后两端指纹快照存在且非空且相等（单向 push 后两端内容相同）。
pub fn assert_snapshot_synced(snap: Option<SyncSnapshot>) -> std::result::Result<(), String> {
    let s = snap.ok_or("应有同步快照却为 None")?;
    if s.peer_last_known_hash.is_empty() || s.self_last_synced_hash.is_empty() {
        return Err("快照字段不应为空".into());
    }
    if s.peer_last_known_hash != s.self_last_synced_hash {
        return Err("单向 push 后两端指纹应相等".into());
    }
    Ok(())
}

/// abort/取消后快照不应变化（与给定 before 比较）。
pub fn assert_snapshot_unchanged(
    before: &Option<SyncSnapshot>,
    after: &Option<SyncSnapshot>,
) -> std::result::Result<(), String> {
    if before == after {
        Ok(())
    } else {
        Err(format!("快照不应变化: before={before:?} after={after:?}"))
    }
}

/// `<target>/.aisync-trash/<ts>/<relative>` 存在且内容匹配。
pub fn assert_trashed_with_content(
    target_dir: &Path,
    relative: &str,
    expected: &str,
) -> std::result::Result<(), String> {
    let trash_root = target_dir.join(".aisync-trash");
    let entries = fs::read_dir(&trash_root)
        .map_err(|e| format!("无回收站 {}: {e}", trash_root.display()))?;
    for batch in entries.flatten() {
        let candidate = batch.path().join(relative);
        if candidate.exists() {
            return assert_file_content(&candidate, expected);
        }
    }
    Err(format!("回收站未找到 {relative}"))
}

/// 找到 `<parent>/<project>.bak-<ts>` 备份目录。
pub fn find_backup(parent: &Path, project_name: &str) -> Option<PathBuf> {
    let prefix = format!("{project_name}.bak-");
    fs::read_dir(parent)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
}

pub fn assert_backup_exists(
    parent: &Path,
    project_name: &str,
) -> std::result::Result<PathBuf, String> {
    find_backup(parent, project_name)
        .ok_or_else(|| format!("{} 下未找到 {project_name}.bak-*", parent.display()))
}

/// 备份目录内容指纹等于覆盖前记录的指纹。
pub fn assert_backup_matches(
    backup: &Path,
    expected_hash: &str,
) -> std::result::Result<(), String> {
    let h = dir_hash(backup);
    if h == expected_hash {
        Ok(())
    } else {
        Err(format!("备份指纹不符: 期望 {expected_hash} 实际 {h}"))
    }
}

/// B 项目子树未新增 `.bak-*` 目录（备份在兄弟目录，不应进项目内）。
pub fn assert_no_backup_in(dir: &Path) -> std::result::Result<(), String> {
    let has = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.contains(".bak-"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if has {
        Err(format!("{} 内不应有 .bak-* 目录", dir.display()))
    } else {
        Ok(())
    }
}

pub fn assert_no_trash_in(dir: &Path) -> std::result::Result<(), String> {
    if dir.join(".aisync-trash").exists() {
        Err(format!("{} 内不应有 .aisync-trash/", dir.display()))
    } else {
        Ok(())
    }
}

/// 独立 split-brain 探针断言。真实 `SplitBrainStatus`（backend.rs:2813）字段：
/// reachable / has_snapshot / peer_not_empty / split_brain（共 4 个，非 Result）。
pub fn assert_split_brain(status: &SplitBrainStatus) -> std::result::Result<(), String> {
    if !status.reachable {
        return Err("peer 不可达，无法判定 split-brain".into());
    }
    if !status.has_snapshot {
        return Err("应有快照（split-brain 需先有同步历史）".into());
    }
    if !status.split_brain {
        return Err("应检测到 split-brain 却没有".into());
    }
    Ok(())
}

pub fn assert_no_split_brain(status: &SplitBrainStatus) -> std::result::Result<(), String> {
    if !status.reachable {
        return Err("peer 不可达".into());
    }
    if status.split_brain {
        Err("意外检测到 split-brain".into())
    } else {
        Ok(())
    }
}

/// 安全阀 abort 断言：结果须 Err 且错误含 "safety valve"（transport 字面量）。
pub fn assert_safety_valve_aborted(
    result: &Result<codebaton_sync::SyncReport>,
) -> std::result::Result<(), String> {
    let err = result.as_ref().err().ok_or("应被安全阀拦截却成功")?;
    let msg = err.to_string();
    if msg.contains("safety valve") {
        Ok(())
    } else {
        Err(format!("错误应含 'safety valve'，实际: {msg}"))
    }
}

/// 目标目录子树内不含匹配 `pattern` 的路径条目（黑盒断言 exclude 未传输/未生成）。
/// 用于 .bak-* / .aisync-trash / .team 不应出现在同步结果中。
pub fn assert_not_in_target(dir: &Path, pattern: &str) -> std::result::Result<(), String> {
    fn walk(p: &Path, pattern: &str) -> bool {
        let Ok(rd) = fs::read_dir(p) else {
            return false;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.contains(pattern) {
                return true;
            }
            if e.path().is_dir() && walk(&e.path(), pattern) {
                return true;
            }
        }
        false
    }
    if walk(dir, pattern) {
        Err(format!("{} 子树不应含 '{pattern}'", dir.display()))
    } else {
        Ok(())
    }
}

/// 同步历史中最新一条为 success 且方向匹配。`backend.sync_history` 返回 JSON 行。
pub fn assert_sync_history_success(
    history: &[serde_json::Value],
    expect_direction: Option<&str>,
) -> std::result::Result<(), String> {
    let latest = history.first().ok_or("同步历史为空")?;
    let success = latest
        .get("success")
        .and_then(|v| v.as_bool())
        .or_else(|| {
            latest
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s == "success")
        })
        .unwrap_or(false);
    if !success {
        return Err(format!("最新同步历史非 success: {latest}"));
    }
    if let Some(dir) = expect_direction {
        let actual = latest.get("direction").and_then(|v| v.as_str()).unwrap_or("");
        if !actual.contains(dir) {
            return Err(format!("方向不符: 期望含 {dir} 实际 {actual}"));
        }
    }
    Ok(())
}

/// 备份可恢复：把备份目录内容指纹与「覆盖前记录的指纹」比对（= assert_backup_matches 别名，
/// 语义更贴 AUTO-018 恢复验证）。
pub fn assert_backup_recoverable(
    backup: &Path,
    pre_overwrite_hash: &str,
) -> std::result::Result<(), String> {
    assert_backup_matches(backup, pre_overwrite_hash)
}

/// 不可达断言（AUTO-024）：probe 不可达时不得当作「安全无脑裂」。
pub fn assert_unreachable(status: &SplitBrainStatus) -> std::result::Result<(), String> {
    if status.reachable {
        Err("期望对端不可达，实际可达".into())
    } else if status.split_brain {
        Err("不可达却报 split_brain，逻辑错误".into())
    } else {
        Ok(())
    }
}

/// 短暂等待（serve 守护接收完成后磁盘可见性的保险，通常不需要）。
pub fn settle() {
    thread::sleep(Duration::from_millis(50));
}

/// 标记参数已用（避免 OsType 未用告警的占位）。
pub fn _touch_os(_: OsType) {}
