//! Split-brain detection/resolution and the sync entry points
//! (`run_sync`/`run_workspace_sync`).
//!
//! ⚠️ Lock discipline here is BEHAVIOR-CRITICAL (documented Rule1 exception):
//! - `run_sync` holds the inner guard ACROSS the network call, exclude
//!   restore, and the in-memory snapshot sync-back — it drops only at fn end.
//! - `run_workspace_sync` uses a scoped block that drops the guard before the
//!   network push.
//! - `probe_peer_target` explicitly drops the guard before its network call.

use codebaton_core::{AisyncError, Direction, Result};
use codebaton_sync::{load_config, save_config, SyncConfig, SyncReport, WorkspaceConfig};
use codebaton_transport::{
    generate_tls_identity, scan_sensitive_files, TargetStatusRequestPayload, TcpTransporter,
    TlsConfig,
};

use super::sync_push::{run_tcp_push, run_workspace_tcp_push};
use super::transport::peer_transport_connection;
use super::{
    app_log, directory_bytes, live_connection_for_config_peer, replace_workspace, Backend,
};

impl Backend {
    pub fn run_workspace_sync(
        &self,
        workspace_name: &str,
        direction: Direction,
    ) -> Result<SyncReport> {
        if direction != Direction::LocalToRemote {
            return Err(AisyncError::Transport(
                "workspace pull over TCP is not implemented".to_string(),
            ));
        }
        let (config_path, config, workspace, live_connection) = {
            let g = self.inner.lock().unwrap();
            let workspace = g
                .config
                .workspaces
                .iter()
                .find(|workspace| workspace.name == workspace_name)
                .cloned()
                .ok_or_else(|| {
                    AisyncError::Config(format!("workspace '{workspace_name}' not found"))
                })?;
            let peer_name = workspace.effective_peer().map(str::to_string);
            let live_connection = peer_name
                .as_deref()
                .and_then(|peer_name| live_connection_for_config_peer(&g, peer_name));
            (
                g.config_path.clone(),
                g.config.clone(),
                workspace,
                live_connection,
            )
        };
        let outcome = run_workspace_tcp_push(&config_path, &config, &workspace, live_connection)?;
        self.persist_workspace_update(outcome.workspace)?;
        Ok(outcome.report)
    }

    fn persist_workspace_update(&self, workspace: WorkspaceConfig) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        replace_workspace(&mut g.config, workspace);
        save_config(&g.config_path, &g.config)
    }

    /// Run a real push/pull through TCP transport.
    ///
    /// G6: sensitive files are excluded by default. Any path in
    /// `confirmed_sensitive` is explicitly re-included; the rest stay excluded
    /// for this run by adding their exact relative paths to the exclude set.
    pub fn run_sync(
        &self,
        project_name: &str,
        peer_name: &str,
        direction: Direction,
        confirmed_sensitive: &[String],
        confirm_overwrite: bool,
    ) -> Result<SyncReport> {
        let mut g = self.inner.lock().unwrap();
        let project = g.config.project_mapping(project_name, peer_name)?;

        // G6 — compute the unconfirmed sensitive files and exclude them by their
        // exact relative path so confirmed ones flow through normally.
        let sensitive = scan_sensitive_files(&project.local_code_dir)?;
        let confirmed: Vec<&str> = confirmed_sensitive.iter().map(|s| s.as_str()).collect();
        let unconfirmed: Vec<String> = sensitive
            .iter()
            .filter(|s| !confirmed.contains(&s.relative_path.as_str()))
            .map(|s| s.relative_path.clone())
            .collect();

        // Inject the per-run excludes onto the project config entry. Restore
        // afterwards so confirmation is scoped to this single sync.
        let saved = inject_excludes(&mut g.config, project_name, &unconfirmed);

        let live_connection = live_connection_for_config_peer(&g, peer_name);
        let coordinator_cfg = g.config.clone();
        let config_path = g.config_path.clone();
        let log_project = project.project_id.clone();
        let log_remote = project.remote_code_dir.display().to_string();
        let log_bytes = directory_bytes(&project.local_code_dir).unwrap_or(0)
            + directory_bytes(&project.local_session_dir).unwrap_or(0);
        app_log(
            "sync_started",
            &[
                ("project", log_project.clone()),
                ("peer", peer_name.to_string()),
                ("remote_dir", log_remote.clone()),
                ("file_count", "0".to_string()),
                ("bytes", log_bytes.to_string()),
            ],
        );
        let result = match direction {
            Direction::LocalToRemote => run_tcp_push(
                &config_path,
                &coordinator_cfg,
                peer_name,
                &project,
                live_connection,
                confirm_overwrite,
            ),
            Direction::RemoteToLocal => Err(AisyncError::Transport(
                "pull over TCP requires a remote control channel; start a local receiver and run send on the peer".to_string(),
            )),
        };

        restore_excludes(&mut g.config, project_name, saved);
        // run_tcp_push 把成功同步的快照写到了磁盘 config；这里同步回内存 config，
        // 否则 check_split_brain 等读内存的逻辑看不到刚写的快照（in-memory/disk 不一致）。
        if result.is_ok() {
            if let Ok(persisted) = load_config(&g.config_path) {
                if let Some(snap) = persisted.sync_snapshot(project_name, peer_name) {
                    g.config.set_sync_snapshot(project_name, peer_name, snap);
                }
            }
        }
        match &result {
            Ok(report) => app_log(
                "sync_complete",
                &[
                    ("project", log_project),
                    ("peer", peer_name.to_string()),
                    ("remote_dir", log_remote),
                    (
                        "file_count",
                        (report.code_files_transferred + report.session_files_transferred)
                            .to_string(),
                    ),
                    ("bytes", log_bytes.to_string()),
                ],
            ),
            Err(error) => app_log(
                "sync_failed",
                &[
                    ("project", log_project),
                    ("peer", peer_name.to_string()),
                    ("remote_dir", log_remote),
                    ("file_count", "0".to_string()),
                    ("bytes", log_bytes.to_string()),
                    ("error", error.to_string()),
                ],
            ),
        }
        result
    }

    /// 连到对端、查询本项目 remote_code_dir 的状态（是否非空 + 当前 manifest 指纹）。
    /// 一次往返同时服务覆盖检测与脑裂检测。对端离线/连接失败时返回 Err。
    fn probe_peer_target(
        &self,
        project_name: &str,
        peer_name: &str,
    ) -> Result<codebaton_transport::TargetStatusResponsePayload> {
        let g = self.inner.lock().unwrap();
        let project = g.config.project_mapping(project_name, peer_name)?;
        let live_connection = live_connection_for_config_peer(&g, peer_name);
        let config = g.config.clone();
        let config_path = g.config_path.clone();
        let local_device = g.discoverer.local_device().clone();
        drop(g);

        let connection =
            peer_transport_connection(&config_path, &config, peer_name, live_connection)?;
        let target_dir = project.remote_code_dir.clone();
        let request_id = codebaton_discovery::new_pairing_request_id();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| AisyncError::Transport(format!("tokio runtime: {error}")))?;
        runtime.block_on(async {
            let identity = generate_tls_identity("aisync-client")?;
            let tls = TlsConfig::new(identity, connection.server_name.clone())
                .with_pinned_peer_cert(connection.receiver_cert_der.clone());
            let mut transporter = TcpTransporter::connect_to_peer(
                &connection.peer,
                connection.endpoint.port(),
                &tls,
            )
            .await?;
            let response = transporter
                .send_target_status_request(TargetStatusRequestPayload {
                    request_id,
                    target_dir,
                    device: local_device,
                })
                .await;
            transporter.shutdown().await;
            response
        })
    }

    /// 推送前覆盖检测（初始场景：从未同步过）：对端目标目录是否已有文件。
    /// 出错时（对端离线/连接失败）返回 false（视为空，不阻断推送），但记日志。
    pub fn check_target_not_empty(&self, project_name: &str, peer_name: &str) -> Result<bool> {
        match self.probe_peer_target(project_name, peer_name) {
            Ok(resp) => {
                app_log(
                    "check_target_not_empty",
                    &[
                        ("project", project_name.to_string()),
                        ("peer", peer_name.to_string()),
                        ("not_empty", resp.not_empty.to_string()),
                        ("file_count", resp.file_count.to_string()),
                    ],
                );
                Ok(resp.not_empty)
            }
            Err(error) => {
                app_log(
                    "check_target_not_empty_failed",
                    &[
                        ("project", project_name.to_string()),
                        ("peer", peer_name.to_string()),
                        ("error", error.to_string()),
                    ],
                );
                Ok(false)
            }
        }
    }
}

/// Temporarily add exact-path excludes to a project. Returns the previous
/// exclude_rules so they can be restored.
pub(crate) fn inject_excludes(
    config: &mut SyncConfig,
    project_name: &str,
    extra: &[String],
) -> Option<Vec<String>> {
    let project = config
        .projects
        .iter_mut()
        .find(|p| p.name == project_name)?;
    let saved = project.exclude_rules.clone();
    project.exclude_rules.extend(extra.iter().cloned());
    Some(saved)
}

pub(crate) fn restore_excludes(config: &mut SyncConfig, project_name: &str, saved: Option<Vec<String>>) {
    if let (Some(saved), Some(project)) = (
        saved,
        config.projects.iter_mut().find(|p| p.name == project_name),
    ) {
        project.exclude_rules = saved;
    }
}
